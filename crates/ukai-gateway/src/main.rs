//! `ukai-gateway`: one endpoint in a namespace for the agents in its
//! `gateway.toml`. `GET /agents/<id>/acp` upgrades to ACP's WebSocket profile;
//! the gateway finds or starts that agent's pod for the workspace and relays
//! frames to the `ukai-relay` in it, which keeps sessions across connections.
//! While the pod starts, the client hears `_ukai/progress` notifications (and
//! `_ukai/heartbeat` while nothing changes, such as during a long image pull)
//! and its own messages wait.
//!
//! A client asks for a pod of a pane's own with the headers
//! `Ukai-Workspace-Scope: pod` and `Ukai-Workspace: <pane key>`; otherwise
//! every connection to an agent shares one workspace.
//!
//! `GET /agents/<id>/reports` and `/agents/<id>/reports/files/<path>` read
//! the reports that workspace's agent wrote, from the `ukai-relay` serving
//! them in its pod. Same token, same workspace headers; a report request
//! never starts a pod.
//!
//! `POST /agents/<id>/restart` stops that workspace's pod, so the next
//! connection starts a fresh one, e.g. to pick up a newer agent release.
//! Every session in the workspace ends with it.
//!
//! `/agents/<id>/schedules` keeps the agent's [`schedules`]: commands and
//! prompts the gateway runs on a cron pattern, in a workspace of each
//! schedule's own. `GET` lists them and `POST` adds one; `GET`, `PUT`, and
//! `DELETE` on `<it>/<schedule>` read, change, and remove one;
//! `GET <schedule>/runs` lists its recent runs, and `POST <schedule>/run` and
//! `<schedule>/stop` start and stop one now.
//!
//! `/agents/<id>/knowledge` is the [`knowledge`] of the agent's scope (its
//! project in `gateway.toml`, else its repository): notes kept outside the
//! repositories. `GET` lists them; `GET`, `PUT`, and
//! `DELETE ?revision=<n>` on `<it>/<name>` read, write, and remove one, a
//! change being refused with 409 when the note is no longer at the revision
//! the writer read; `GET <name>/revisions` is its history, and
//! `POST <name>/approve?revision=<n>` approves what an agent wrote. Agent pods reach
//! the same, with a cap on how many changes each makes, at
//! `/workspace/knowledge` with their relay's token instead of the gateway's,
//! which names the scope of the pod it belongs to and no other.
//!
//! Environment:
//! - `UKAI_GATEWAY_CONFIG`: path of `gateway.toml` (default `/etc/ukai/gateway.toml`)
//! - `UKAI_GATEWAY_LISTEN`: address to serve (default `0.0.0.0:8080`)
//! - `UKAI_GATEWAY_TOKEN`: if set, clients must send `Authorization: Bearer <it>`
//! - `UKAI_RELAY_IMAGE`: image whose `/usr/local/bin/ukai-relay` agent pods
//!   run; the gateway's own image (read from its pod, `HOSTNAME`) when unset
//! - `UKAI_KNOWLEDGE_URL`: the SurrealDB server keeping the [`knowledge`]
//!   (`ws://ukai-surrealdb:8000` in `deploy/gateway`); without it the
//!   gateway keeps none, and agent pods are not offered any
//! - `UKAI_KNOWLEDGE_USER`, `UKAI_KNOWLEDGE_PASS`: its root user
//! - `UKAI_GATEWAY_INTERNAL_URL`: where agent pods reach the gateway for the
//!   knowledge (default `http://ukai-gateway`, the Service in `deploy/gateway`)
//!
//! Each agent runs in a Job per workspace, in the gateway's namespace, with
//! nothing installed cluster-side: no CRD, no PersistentVolumeClaim of its
//! own. [`job`] builds the Job from the config without touching the cluster;
//! [`cluster`] finds or creates it and waits for its pod.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::process::ExitCode;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use futures::stream::{SplitSink, SplitStream};
use futures::{Sink, SinkExt, Stream, StreamExt};
use k8s_openapi::api::core::v1::Pod;
use serde_json::json;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, tungstenite};
mod cluster;
mod config;
mod job;
mod knowledge;
mod schedules;

use crate::cluster::{Cluster, RelayEndpoint};
use crate::config::KubernetesAgent;
use crate::job::{Plan, Runtime};
use crate::knowledge::{Knowledge, KnowledgeError, StoreConfig};
use crate::schedules::Scheduler;
use ukai_core::knowledge::{Conflict, Editor, Listing, NoteWrite};
use ukai_core::schedule::{ScheduleSpec, Trigger};

struct Gateway {
    agents: BTreeMap<String, KubernetesAgent>,
    cluster: Cluster,
    user: String,
    runtime: Runtime,
    token: Option<String>,
    schedules: Scheduler,
    /// Whether the gateway keeps knowledge at all.
    keeps_knowledge: bool,
    /// Set once the store is reached.
    knowledge: tokio::sync::OnceCell<Knowledge>,
    /// When each agent pod's workspace last changed notes, for [`POD_WRITES`].
    pod_writes: Mutex<HashMap<String, VecDeque<std::time::Instant>>>,
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "ukai_gateway=info".into()),
        )
        .init();
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!("{error}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let env = |name: &str| std::env::var(name).ok().filter(|value| !value.is_empty());
    let path = env("UKAI_GATEWAY_CONFIG").unwrap_or_else(|| "/etc/ukai/gateway.toml".to_owned());
    let src = std::fs::read_to_string(&path).map_err(|error| format!("{path}: {error}"))?;
    let config = config::parse(&src).map_err(|error| format!("{path}: {error}"))?;
    let listen = env("UKAI_GATEWAY_LISTEN").unwrap_or_else(|| "0.0.0.0:8080".to_owned());

    let cluster = Cluster::infer().await?;
    let relay_image = match env("UKAI_RELAY_IMAGE") {
        Some(image) => image,
        None => {
            let pod = env("HOSTNAME").ok_or("set UKAI_RELAY_IMAGE or HOSTNAME")?;
            cluster
                .container_image(&pod)
                .await?
                .ok_or("UKAI_RELAY_IMAGE is not set and the gateway pod has no image")?
        }
    };
    let store = env("UKAI_KNOWLEDGE_URL").map(|url| StoreConfig {
        url,
        credentials: env("UKAI_KNOWLEDGE_USER").zip(env("UKAI_KNOWLEDGE_PASS")),
    });
    // Agent pods are told where the gateway is only when it keeps knowledge.
    let gateway_url = store.as_ref().map(|_| {
        env("UKAI_GATEWAY_INTERNAL_URL").unwrap_or_else(|| "http://ukai-gateway".to_owned())
    });
    let user = cluster.whoami().await?;
    tracing::info!(namespace = cluster.namespace(), %user, %relay_image, ?gateway_url, knowledge = ?store.as_ref().map(|s| &s.url), agents = ?config.agents.keys().collect::<Vec<_>>(), "gateway starting");
    let (schedules, saver) = Scheduler::load(&cluster).await?;
    let gateway = Arc::new(Gateway {
        agents: config.agents,
        cluster,
        user,
        runtime: Runtime {
            relay_image,
            gateway_url,
        },
        token: env("UKAI_GATEWAY_TOKEN"),
        schedules,
        keeps_knowledge: store.is_some(),
        knowledge: tokio::sync::OnceCell::new(),
        pod_writes: Mutex::default(),
    });
    tokio::spawn(saver.run(gateway.clone()));
    tokio::spawn(schedules::tick(gateway.clone()));
    if let Some(store) = store {
        tokio::spawn(reach_knowledge(gateway.clone(), store));
    }
    let router = Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/agents", get(list))
        .route("/agents/{id}/acp", get(connect))
        .route("/agents/{id}/restart", post(restart))
        .route("/agents/{id}/reports", get(reports))
        .route("/agents/{id}/reports/files/{*path}", get(reports))
        .route(
            "/agents/{id}/schedules",
            get(list_schedules).post(create_schedule),
        )
        .route(
            "/agents/{id}/schedules/{schedule}",
            get(get_schedule)
                .put(update_schedule)
                .delete(delete_schedule),
        )
        .route("/agents/{id}/schedules/{schedule}/runs", get(schedule_runs))
        .route("/agents/{id}/schedules/{schedule}/run", post(run_schedule))
        .route(
            "/agents/{id}/schedules/{schedule}/stop",
            post(stop_schedule),
        )
        .route("/agents/{id}/knowledge", get(agent_notes))
        .route(
            "/agents/{id}/knowledge/{name}",
            get(agent_note)
                .put(put_agent_note)
                .delete(delete_agent_note),
        )
        .route(
            "/agents/{id}/knowledge/{name}/revisions",
            get(agent_note_revisions),
        )
        // Approval is a person's: there is no route for it under /workspace.
        .route(
            "/agents/{id}/knowledge/{name}/approve",
            post(approve_agent_note),
        )
        .route("/workspace/knowledge", get(workspace_notes))
        .route(
            "/workspace/knowledge/{name}",
            get(workspace_note)
                .put(put_workspace_note)
                .delete(delete_workspace_note),
        )
        .route(
            "/workspace/knowledge/{name}/revisions",
            get(workspace_note_revisions),
        )
        .with_state(gateway);
    let listener = tokio::net::TcpListener::bind(&listen).await?;
    tracing::info!(%listen, "serving");
    axum::serve(listener, router).await?;
    Ok(())
}

/// How long the gateway waits between attempts to reach the knowledge store.
const KNOWLEDGE_RETRY: std::time::Duration = std::time::Duration::from_secs(10);

/// Reach the knowledge store, trying until it answers: the gateway serves
/// agents meanwhile, and knowledge requests are 503 until then.
async fn reach_knowledge(gateway: Arc<Gateway>, store: StoreConfig) {
    loop {
        match Knowledge::connect(&store).await {
            Ok(knowledge) => {
                tracing::info!(url = %store.url, "knowledge store reached");
                let _ = gateway.knowledge.set(knowledge);
                return;
            }
            Err(error) => {
                tracing::warn!(url = %store.url, %error, "cannot reach the knowledge store; trying again");
                tokio::time::sleep(KNOWLEDGE_RETRY).await;
            }
        }
    }
}

fn authorized(gateway: &Gateway, headers: &HeaderMap) -> bool {
    let Some(token) = &gateway.token else {
        return true;
    };
    headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .is_some_and(|presented| cluster::same_secret(token, presented))
}

async fn list(State(gateway): State<Arc<Gateway>>, headers: HeaderMap) -> Response {
    if !authorized(&gateway, &headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    axum::Json(gateway.agents.keys().collect::<Vec<_>>()).into_response()
}

async fn connect(
    State(gateway): State<Arc<Gateway>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    if !authorized(&gateway, &headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let Some(agent) = gateway.agents.get(&id).cloned() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let pane = match pane_key(&headers) {
        Ok(pane) => pane,
        Err(problem) => return (StatusCode::BAD_REQUEST, problem).into_response(),
    };
    upgrade.on_upgrade(move |socket| async move {
        let plan = Plan::new(
            agent,
            &gateway.user,
            gateway.cluster.namespace(),
            pane.as_deref(),
        );
        if let Err(error) = relay(&gateway, plan, socket).await {
            tracing::warn!(agent = %id, %error, "connection ended with an error");
        }
    })
}

/// `POST /agents/<id>/restart`: stop the workspace's pod, resolved as for
/// `/acp`. The next connection starts a new one.
async fn restart(
    State(gateway): State<Arc<Gateway>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    if !authorized(&gateway, &headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let Some(agent) = gateway.agents.get(&id).cloned() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let pane = match pane_key(&headers) {
        Ok(pane) => pane,
        Err(problem) => return (StatusCode::BAD_REQUEST, problem).into_response(),
    };
    let plan = Plan::new(
        agent,
        &gateway.user,
        gateway.cluster.namespace(),
        pane.as_deref(),
    );
    match gateway.cluster.stop(&plan).await {
        Ok(()) => {
            tracing::info!(agent = %id, job = %plan.job_name(), "restarting the agent's workspace");
            StatusCode::NO_CONTENT.into_response()
        }
        Err(error) => {
            tracing::warn!(agent = %id, %error, "cannot stop the workspace's pod");
            StatusCode::BAD_GATEWAY.into_response()
        }
    }
}

/// `GET /agents/<id>/reports[/files/<path>]`: what the agent wrote in the
/// workspace this client asks for, read from the `ukai-relay` in its pod.
///
/// The workspace is resolved exactly as for `/acp`, and the same token is
/// required, but a report request never starts a pod: it is a read of work
/// that is already there. A workspace whose pod has idled out is 410, so a
/// pane can say so instead of waiting out an image pull.
async fn reports(State(gateway): State<Arc<Gateway>>, headers: HeaderMap, uri: Uri) -> Response {
    if !authorized(&gateway, &headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    // The path is read from the URI rather than through `Path`, which cannot
    // serve both routes at once, and so the file part stays percent-encoded
    // for the relay to decode exactly once.
    let Some((id, suffix)) = reports_target(uri.path()) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Some(agent) = gateway.agents.get(&id).cloned() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let pane = match pane_key(&headers) {
        Ok(pane) => pane,
        Err(problem) => return (StatusCode::BAD_REQUEST, problem).into_response(),
    };
    let plan = Plan::new(
        agent,
        &gateway.user,
        gateway.cluster.namespace(),
        pane.as_deref(),
    );
    let pod = match gateway.cluster.serving_pod(&plan).await {
        Ok(Some(pod)) => pod,
        Ok(None) => return gone(),
        Err(error) => {
            tracing::warn!(agent = %id, %error, "cannot look up the workspace's pod");
            return StatusCode::BAD_GATEWAY.into_response();
        }
    };
    let Some(endpoint) = RelayEndpoint::of(&pod) else {
        return gone();
    };
    match fetch(&endpoint, suffix).await {
        Ok(response) => response,
        Err(error) => {
            tracing::warn!(agent = %id, %error, "cannot read the workspace's reports");
            StatusCode::BAD_GATEWAY.into_response()
        }
    }
}

/// The agent a report request names and the part of it the relay serves
/// under its own `/reports`: nothing for the listing, `/files/<path>` for one
/// file. `None` for anything else, so only these two are ever proxied.
///
/// The agent id is decoded, as axum decodes it for `/acp`; the file path is
/// left as it arrived, for the relay to decode once.
fn reports_target(path: &str) -> Option<(String, &str)> {
    let (id, rest) = path.strip_prefix("/agents/")?.split_once('/')?;
    let suffix = rest.strip_prefix("reports")?;
    let proxied = suffix.is_empty()
        || suffix
            .strip_prefix("/files/")
            .is_some_and(|path| !path.is_empty());
    if !proxied || id.is_empty() {
        return None;
    }
    let id = percent_encoding::percent_decode_str(id)
        .decode_utf8()
        .ok()?;
    Some((id.into_owned(), suffix))
}

/// A workspace that is not running: there is nothing to read, and a report
/// request must not bring its pod back.
fn gone() -> Response {
    (
        StatusCode::GONE,
        "the agent's workspace is not running; open the agent to start it",
    )
        .into_response()
}

/// Read one report response from the pod's relay and pass it on, with the
/// headers a web view needs to render it and nothing else.
async fn fetch(endpoint: &RelayEndpoint, suffix: &str) -> Result<Response, BoxError> {
    let response = reqwest::Client::new()
        .get(endpoint.reports_url(suffix))
        .bearer_auth(&endpoint.token)
        .timeout(REPORT_TIMEOUT)
        .send()
        .await?;
    let status = StatusCode::from_u16(response.status().as_u16())?;
    let passed = |name: &header::HeaderName| {
        response
            .headers()
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
    };
    let content_type = passed(&header::CONTENT_TYPE);
    let etag = passed(&header::ETAG);
    let body = response.bytes().await?;
    let mut builder = Response::builder()
        .status(status)
        .header(header::CACHE_CONTROL, "no-store")
        .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff");
    if let Some(content_type) = content_type {
        builder = builder.header(header::CONTENT_TYPE, content_type);
    }
    if let Some(etag) = etag {
        builder = builder.header(header::ETAG, etag);
    }
    Ok(builder.body(body.into())?)
}

/// How long the gateway waits on the pod for one report.
const REPORT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// Answers for one agent's schedules: `Err` is the response when the
/// request is not authorized or names no agent of the gateway's.
fn schedules_of<'g>(
    gateway: &'g Gateway,
    headers: &HeaderMap,
    agent: &str,
) -> Result<&'g Scheduler, StatusCode> {
    if !authorized(gateway, headers) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    if !gateway.agents.contains_key(agent) {
        return Err(StatusCode::NOT_FOUND);
    }
    Ok(&gateway.schedules)
}

fn refused(problems: Vec<String>) -> Response {
    (
        StatusCode::UNPROCESSABLE_ENTITY,
        axum::Json(json!({ "problems": problems })),
    )
        .into_response()
}

fn found<T: serde::Serialize>(value: Option<T>) -> Response {
    match value {
        Some(value) => axum::Json(value).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn list_schedules(
    State(gateway): State<Arc<Gateway>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    match schedules_of(&gateway, &headers, &id) {
        Ok(schedules) => axum::Json(schedules.list(&id)).into_response(),
        Err(status) => status.into_response(),
    }
}

async fn create_schedule(
    State(gateway): State<Arc<Gateway>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    axum::Json(spec): axum::Json<ScheduleSpec>,
) -> Response {
    let schedules = match schedules_of(&gateway, &headers, &id) {
        Ok(schedules) => schedules,
        Err(status) => return status.into_response(),
    };
    match schedules.create(&id, spec) {
        Ok(info) => (StatusCode::CREATED, axum::Json(info)).into_response(),
        Err(problems) => refused(problems),
    }
}

async fn get_schedule(
    State(gateway): State<Arc<Gateway>>,
    Path((id, schedule)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    match schedules_of(&gateway, &headers, &id) {
        Ok(schedules) => found(schedules.info(&id, &schedule)),
        Err(status) => status.into_response(),
    }
}

async fn update_schedule(
    State(gateway): State<Arc<Gateway>>,
    Path((id, schedule)): Path<(String, String)>,
    headers: HeaderMap,
    axum::Json(spec): axum::Json<ScheduleSpec>,
) -> Response {
    let schedules = match schedules_of(&gateway, &headers, &id) {
        Ok(schedules) => schedules,
        Err(status) => return status.into_response(),
    };
    match schedules.update(&id, &schedule, spec) {
        Ok(info) => found(info),
        Err(problems) => refused(problems),
    }
}

async fn delete_schedule(
    State(gateway): State<Arc<Gateway>>,
    Path((id, schedule)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    match schedules_of(&gateway, &headers, &id) {
        Ok(schedules) => found(schedules.delete(&id, &schedule).then(|| json!({}))),
        Err(status) => status.into_response(),
    }
}

async fn schedule_runs(
    State(gateway): State<Arc<Gateway>>,
    Path((id, schedule)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    match schedules_of(&gateway, &headers, &id) {
        Ok(schedules) => found(schedules.runs(&id, &schedule)),
        Err(status) => status.into_response(),
    }
}

/// Start a run now. One already going is not an error: the answer shows it.
async fn run_schedule(
    State(gateway): State<Arc<Gateway>>,
    Path((id, schedule)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let schedules = match schedules_of(&gateway, &headers, &id) {
        Ok(schedules) => schedules,
        Err(status) => return status.into_response(),
    };
    if schedules.info(&id, &schedule).is_none() {
        return StatusCode::NOT_FOUND.into_response();
    }
    schedules::fire(&gateway, &schedule, Trigger::Manual);
    found(gateway.schedules.info(&id, &schedule))
}

async fn stop_schedule(
    State(gateway): State<Arc<Gateway>>,
    Path((id, schedule)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    match schedules_of(&gateway, &headers, &id) {
        Ok(schedules) => found(schedules.stop(&id, &schedule)),
        Err(status) => status.into_response(),
    }
}

/// Whose knowledge a request is about, and who is asking.
struct Caller {
    scope: String,
    editor: Editor,
    /// The agent pod's workspace, for a pod.
    workspace: Option<String>,
}

/// The scope whose knowledge a client asks for through `agent`, or the
/// status when it may not.
fn client_caller(
    gateway: &Gateway,
    headers: &HeaderMap,
    agent: &str,
) -> Result<Caller, StatusCode> {
    if !authorized(gateway, headers) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let agent = gateway.agents.get(agent).ok_or(StatusCode::NOT_FOUND)?;
    Ok(Caller {
        scope: agent.knowledge_scope(),
        editor: Editor::User,
        workspace: None,
    })
}

/// The agent pod whose relay token the request carries, or the response
/// when it carries none that the gateway knows.
async fn workspace_caller(gateway: &Gateway, headers: &HeaderMap) -> Result<Caller, StatusCode> {
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .filter(|token| !token.is_empty())
        .ok_or(StatusCode::UNAUTHORIZED)?;
    match gateway.cluster.pod_of(token).await {
        Ok(Some(pod)) => Ok(Caller {
            scope: pod.scope,
            editor: Editor::Agent,
            workspace: Some(pod.workspace),
        }),
        Ok(None) => Err(StatusCode::UNAUTHORIZED),
        Err(error) => {
            tracing::warn!(%error, "cannot look up the pod asking for knowledge");
            Err(StatusCode::BAD_GATEWAY)
        }
    }
}

/// Changes one agent pod's workspace may make to the knowledge per
/// [`POD_WRITE_WINDOW`]: a session that read something hostile cannot flood
/// the store with notes that outnumber the real ones.
const POD_WRITES: usize = 20;
const POD_WRITE_WINDOW: std::time::Duration = std::time::Duration::from_secs(3600);

/// Count a change by `caller`; `false` when its workspace has made too many.
fn admit_write(gateway: &Gateway, caller: &Caller) -> bool {
    let Some(workspace) = &caller.workspace else {
        return true;
    };
    let now = std::time::Instant::now();
    let mut writes = gateway
        .pod_writes
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let recent = writes.entry(workspace.clone()).or_default();
    recent.retain(|at| now.duration_since(*at) < POD_WRITE_WINDOW);
    if recent.len() >= POD_WRITES {
        return false;
    }
    recent.push_back(now);
    true
}

/// The knowledge store, or why there is none to use.
fn knowledge_of(gateway: &Gateway) -> Result<&Knowledge, (StatusCode, &'static str)> {
    let unavailable = |why| (StatusCode::SERVICE_UNAVAILABLE, why);
    if !gateway.keeps_knowledge {
        return Err(unavailable(
            "this gateway keeps no knowledge: set UKAI_KNOWLEDGE_URL",
        ));
    }
    gateway
        .knowledge
        .get()
        .ok_or_else(|| unavailable("the gateway has not reached its knowledge store yet"))
}

/// The response to a failed knowledge request.
fn knowledge_failed(error: KnowledgeError) -> Response {
    match error {
        KnowledgeError::Refused(problems) => refused(problems),
        KnowledgeError::NotFound => StatusCode::NOT_FOUND.into_response(),
        KnowledgeError::Conflict(current) => (
            StatusCode::CONFLICT,
            axum::Json(Conflict { current: *current }),
        )
            .into_response(),
        error @ (KnowledgeError::Store(_) | KnowledgeError::Unreadable(_)) => {
            tracing::warn!(%error, "knowledge request failed");
            StatusCode::BAD_GATEWAY.into_response()
        }
    }
}

/// What can be asked of one scope's knowledge.
enum Ask {
    List,
    Read(String),
    Revisions(String),
    Write(String, NoteWrite),
    Delete(String, u64),
    Approve(String, u64),
}

async fn answer(gateway: &Gateway, caller: Result<Caller, StatusCode>, ask: Ask) -> Response {
    let caller = match caller {
        Ok(caller) => caller,
        Err(status) => return status.into_response(),
    };
    let knowledge = match knowledge_of(gateway) {
        Ok(knowledge) => knowledge,
        Err(unavailable) => return unavailable.into_response(),
    };
    if matches!(ask, Ask::Write(..) | Ask::Delete(..)) && !admit_write(gateway, &caller) {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            "this workspace has changed too many notes for now",
        )
            .into_response();
    }
    let scope = &caller.scope;
    let result = match ask {
        Ask::List => knowledge.list(scope).await.map(|notes| {
            axum::Json(Listing {
                scope: scope.clone(),
                notes,
            })
            .into_response()
        }),
        Ask::Read(name) => knowledge.get(scope, &name).await.map(found),
        Ask::Revisions(name) => knowledge
            .revisions(scope, &name)
            .await
            .map(|revisions| found((!revisions.is_empty()).then_some(revisions))),
        Ask::Write(name, write) => knowledge
            .put(scope, &name, write, caller.editor, caller.workspace)
            .await
            .map(|note| axum::Json(note).into_response()),
        Ask::Delete(name, revision) => knowledge
            .delete(scope, &name, revision, caller.editor, caller.workspace)
            .await
            .map(|()| axum::Json(json!({})).into_response()),
        Ask::Approve(name, revision) => {
            if caller.editor != Editor::User {
                return StatusCode::FORBIDDEN.into_response();
            }
            knowledge
                .approve(scope, &name, revision)
                .await
                .map(|note| axum::Json(note).into_response())
        }
    };
    result.unwrap_or_else(knowledge_failed)
}

/// `?revision=<n>` on a delete or an approval: the revision the person or
/// agent read.
#[derive(serde::Deserialize)]
struct RevisionQuery {
    revision: u64,
}

async fn agent_notes(
    State(gateway): State<Arc<Gateway>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let caller = client_caller(&gateway, &headers, &id);
    answer(&gateway, caller, Ask::List).await
}

async fn agent_note(
    State(gateway): State<Arc<Gateway>>,
    Path((id, name)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let caller = client_caller(&gateway, &headers, &id);
    answer(&gateway, caller, Ask::Read(name)).await
}

async fn agent_note_revisions(
    State(gateway): State<Arc<Gateway>>,
    Path((id, name)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let caller = client_caller(&gateway, &headers, &id);
    answer(&gateway, caller, Ask::Revisions(name)).await
}

async fn put_agent_note(
    State(gateway): State<Arc<Gateway>>,
    Path((id, name)): Path<(String, String)>,
    headers: HeaderMap,
    axum::Json(write): axum::Json<NoteWrite>,
) -> Response {
    let caller = client_caller(&gateway, &headers, &id);
    answer(&gateway, caller, Ask::Write(name, write)).await
}

async fn delete_agent_note(
    State(gateway): State<Arc<Gateway>>,
    Path((id, name)): Path<(String, String)>,
    Query(query): Query<RevisionQuery>,
    headers: HeaderMap,
) -> Response {
    let caller = client_caller(&gateway, &headers, &id);
    answer(&gateway, caller, Ask::Delete(name, query.revision)).await
}

async fn approve_agent_note(
    State(gateway): State<Arc<Gateway>>,
    Path((id, name)): Path<(String, String)>,
    Query(query): Query<RevisionQuery>,
    headers: HeaderMap,
) -> Response {
    let caller = client_caller(&gateway, &headers, &id);
    answer(&gateway, caller, Ask::Approve(name, query.revision)).await
}

async fn workspace_notes(State(gateway): State<Arc<Gateway>>, headers: HeaderMap) -> Response {
    let caller = workspace_caller(&gateway, &headers).await;
    answer(&gateway, caller, Ask::List).await
}

async fn workspace_note(
    State(gateway): State<Arc<Gateway>>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> Response {
    let caller = workspace_caller(&gateway, &headers).await;
    answer(&gateway, caller, Ask::Read(name)).await
}

async fn workspace_note_revisions(
    State(gateway): State<Arc<Gateway>>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> Response {
    let caller = workspace_caller(&gateway, &headers).await;
    answer(&gateway, caller, Ask::Revisions(name)).await
}

async fn put_workspace_note(
    State(gateway): State<Arc<Gateway>>,
    Path(name): Path<String>,
    headers: HeaderMap,
    axum::Json(write): axum::Json<NoteWrite>,
) -> Response {
    let caller = workspace_caller(&gateway, &headers).await;
    answer(&gateway, caller, Ask::Write(name, write)).await
}

async fn delete_workspace_note(
    State(gateway): State<Arc<Gateway>>,
    Path(name): Path<String>,
    Query(query): Query<RevisionQuery>,
    headers: HeaderMap,
) -> Response {
    let caller = workspace_caller(&gateway, &headers).await;
    answer(&gateway, caller, Ask::Delete(name, query.revision)).await
}

/// The pane key when the client asks for a pod of the pane's own.
fn pane_key(headers: &HeaderMap) -> Result<Option<String>, &'static str> {
    let header = |name| headers.get(name).and_then(|value| value.to_str().ok());
    match header("ukai-workspace-scope") {
        None | Some("shared") => Ok(None),
        Some("pod") => {
            let key = header("ukai-workspace").ok_or("scope `pod` needs a Ukai-Workspace key")?;
            let valid = !key.is_empty()
                && key.len() <= 64
                && key
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
            if valid {
                Ok(Some(key.to_owned()))
            } else {
                Err("Ukai-Workspace must be 1-64 letters, digits, `-`, or `_`")
            }
        }
        Some(_) => Err("Ukai-Workspace-Scope must be `shared` or `pod`"),
    }
}

/// How often a client waiting on a pod hears that the gateway is still there.
const HEARTBEAT: std::time::Duration = std::time::Duration::from_secs(20);

fn heartbeat() -> Message {
    let notification = json!({ "jsonrpc": "2.0", "method": "_ukai/heartbeat", "params": {} });
    Message::Text(notification.to_string().into())
}

fn progress(message: &str) -> Message {
    let notification = json!({
        "jsonrpc": "2.0",
        "method": "_ukai/progress",
        "params": { "message": message }
    });
    Message::Text(notification.to_string().into())
}

type BoxError = Box<dyn std::error::Error + Send + Sync>;
type ClientTx = SplitSink<WebSocket, Message>;
type ClientRx = SplitStream<WebSocket>;
type Upstream = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Bring up the agent's pod while telling the client how it goes, then pass
/// frames both ways until either side closes.
async fn relay(gateway: &Gateway, plan: Plan, socket: WebSocket) -> Result<(), BoxError> {
    let (mut client_tx, mut client_rx) = socket.split();
    let Some((pod, waiting)) = prepare(gateway, &plan, &mut client_tx, &mut client_rx).await?
    else {
        return Ok(());
    };
    let (mut relay_tx, relay_rx) = connect_relay(&pod).await?.split();
    for message in waiting.into_iter().filter_map(to_relay) {
        relay_tx.send(message).await?;
    }
    pump(client_tx, client_rx, relay_tx, relay_rx).await
}

/// Wait for a ready pod, forwarding progress to the client and holding what
/// it sends meanwhile (usually `initialize`). `None` if the client leaves.
async fn prepare(
    gateway: &Gateway,
    plan: &Plan,
    client_tx: &mut ClientTx,
    client_rx: &mut ClientRx,
) -> Result<Option<(Pod, Vec<Message>)>, BoxError> {
    let (progress_tx, mut progress_rx) = mpsc::unbounded_channel::<String>();
    let ensure = gateway
        .cluster
        .ensure_pod(plan, &gateway.runtime, move |message| {
            let _ = progress_tx.send(message);
        });
    tokio::pin!(ensure);
    let mut beat = tokio::time::interval_at(tokio::time::Instant::now() + HEARTBEAT, HEARTBEAT);
    let mut waiting = Vec::new();
    let pod = loop {
        tokio::select! {
            pod = &mut ensure => break pod,
            Some(message) = progress_rx.recv() => {
                client_tx.send(progress(&message)).await?;
            }
            _ = beat.tick() => {
                client_tx.send(heartbeat()).await?;
            }
            message = client_rx.next() => match message {
                Some(Ok(Message::Close(_))) | None => return Ok(None),
                Some(Ok(message)) => waiting.push(message),
                Some(Err(error)) => return Err(error.into()),
            },
        }
    };
    while let Ok(message) = progress_rx.try_recv() {
        client_tx.send(progress(&message)).await?;
    }
    match pod {
        Ok(pod) => Ok(Some((pod, waiting))),
        Err(error) => {
            client_tx.send(progress(&error.to_string())).await?;
            client_tx.send(Message::Close(None)).await?;
            Err(error.into())
        }
    }
}

async fn connect_relay(pod: &Pod) -> Result<Upstream, BoxError> {
    let endpoint = RelayEndpoint::of(pod).ok_or("the agent pod has no address yet")?;
    connect_relay_at(&endpoint).await
}

async fn connect_relay_at(endpoint: &RelayEndpoint) -> Result<Upstream, BoxError> {
    let mut request = endpoint.acp_url().as_str().into_client_request()?;
    request.headers_mut().insert(
        header::AUTHORIZATION,
        format!("Bearer {}", endpoint.token).parse()?,
    );
    let (upstream, _) = tokio_tungstenite::connect_async(request).await?;
    Ok(upstream)
}

/// How often an open connection pings its client. The proxies in front of a
/// gateway (an ingress, an identity-aware proxy) drop a WebSocket that stays
/// quiet past their idle timeout, as one does while the agent thinks or the
/// user reads; a ping well under a minute apart keeps it open.
const KEEPALIVE: std::time::Duration = std::time::Duration::from_secs(20);

/// Pass frames both ways until either side closes, then close the other,
/// pinging the client every [`KEEPALIVE`] meanwhile.
async fn pump<CT, CR, RT, RR>(
    mut client_tx: CT,
    mut client_rx: CR,
    mut relay_tx: RT,
    mut relay_rx: RR,
) -> Result<(), BoxError>
where
    CT: Sink<Message, Error = axum::Error> + Unpin,
    CR: Stream<Item = Result<Message, axum::Error>> + Unpin,
    RT: Sink<tungstenite::Message, Error = tungstenite::Error> + Unpin,
    RR: Stream<Item = Result<tungstenite::Message, tungstenite::Error>> + Unpin,
{
    let mut keepalive =
        tokio::time::interval_at(tokio::time::Instant::now() + KEEPALIVE, KEEPALIVE);
    loop {
        tokio::select! {
            message = client_rx.next() => {
                let Some(message) = message.transpose()? else { break };
                let close = matches!(message, Message::Close(_));
                if let Some(message) = to_relay(message) {
                    relay_tx.send(message).await?;
                }
                if close {
                    break;
                }
            }
            message = relay_rx.next() => {
                let Some(message) = message.transpose()? else { break };
                let close = matches!(message, tungstenite::Message::Close(_));
                if let Some(message) = to_client(message) {
                    client_tx.send(message).await?;
                }
                if close {
                    break;
                }
            }
            _ = keepalive.tick() => {
                client_tx.send(Message::Ping(Default::default())).await?;
            }
        }
    }
    let _ = relay_tx.close().await;
    let _ = client_tx.close().await;
    Ok(())
}

/// Data and close frames cross; pings are answered on each hop.
fn to_relay(message: Message) -> Option<tungstenite::Message> {
    match message {
        Message::Text(text) => Some(tungstenite::Message::text(text.as_str())),
        Message::Binary(bytes) => Some(tungstenite::Message::binary(bytes)),
        Message::Close(_) => Some(tungstenite::Message::Close(None)),
        Message::Ping(_) | Message::Pong(_) => None,
    }
}

fn to_client(message: tungstenite::Message) -> Option<Message> {
    match message {
        tungstenite::Message::Text(text) => Some(Message::Text(text.as_str().into())),
        tungstenite::Message::Binary(bytes) => Some(Message::Binary(bytes)),
        tungstenite::Message::Close(_) => Some(Message::Close(None)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case::listing("/agents/claude/reports", Some(("claude", "")))]
    #[case::file(
        "/agents/claude/reports/files/reports/run%201.html",
        Some(("claude", "/files/reports/run%201.html"))
    )]
    #[case::encoded_agent_id("/agents/team%2Fclaude/reports", Some(("team/claude", "")))]
    #[case::acp("/agents/claude/acp", None)]
    #[case::no_file("/agents/claude/reports/files/", None)]
    #[case::other_suffix("/agents/claude/reports/../acp", None)]
    #[case::no_agent("/agents//reports", None)]
    #[case::elsewhere("/health", None)]
    fn reads_what_a_report_request_asks_for(
        #[case] path: &str,
        #[case] expected: Option<(&str, &str)>,
    ) {
        assert_eq!(
            reports_target(path)
                .as_ref()
                .map(|(id, suffix)| (id.as_str(), *suffix)),
            expected,
            "{path}"
        );
    }

    #[rstest]
    #[case::none(None, None, Ok(None))]
    #[case::shared(Some("shared"), Some("k1"), Ok(None))]
    #[case::pod(Some("pod"), Some("0f3a-b_c"), Ok(Some("0f3a-b_c".to_owned())))]
    #[case::pod_without_key(Some("pod"), None, Err(()))]
    #[case::bad_key(Some("pod"), Some("../etc"), Err(()))]
    #[case::bad_scope(Some("tab"), Some("k1"), Err(()))]
    fn reads_the_panes_workspace(
        #[case] scope: Option<&'static str>,
        #[case] key: Option<&'static str>,
        #[case] expected: Result<Option<String>, ()>,
    ) {
        let mut headers = HeaderMap::new();
        if let Some(scope) = scope {
            headers.insert("ukai-workspace-scope", HeaderValue::from_static(scope));
        }
        if let Some(key) = key {
            headers.insert("ukai-workspace", HeaderValue::from_static(key));
        }
        assert_eq!(pane_key(&headers).map_err(|_| ()), expected);
    }

    /// A quiet connection pings its client every keepalive, and frames still
    /// cross while it does.
    #[tokio::test(start_paused = true)]
    async fn pings_a_quiet_client_and_still_relays() -> Result<(), BoxError> {
        let (client_tx, mut client_out) = futures::channel::mpsc::unbounded::<Message>();
        let (client_in, client_rx) =
            futures::channel::mpsc::unbounded::<Result<Message, axum::Error>>();
        let (relay_tx, mut relay_out) = futures::channel::mpsc::unbounded::<tungstenite::Message>();
        let (_relay_in, relay_rx) =
            futures::channel::mpsc::unbounded::<Result<tungstenite::Message, tungstenite::Error>>();
        let pump = tokio::spawn(pump(
            client_tx.sink_map_err(axum::Error::new),
            client_rx,
            relay_tx.sink_map_err(|_| tungstenite::Error::ConnectionClosed),
            relay_rx,
        ));

        tokio::time::sleep(KEEPALIVE * 2 + std::time::Duration::from_millis(1)).await;
        for _ in 0..2 {
            assert!(matches!(client_out.next().await, Some(Message::Ping(_))));
        }

        client_in.unbounded_send(Ok(Message::Text("{}".into())))?;
        assert_eq!(
            relay_out.next().await,
            Some(tungstenite::Message::text("{}"))
        );

        client_in.unbounded_send(Ok(Message::Close(None)))?;
        pump.await??;
        assert_eq!(
            relay_out.next().await,
            Some(tungstenite::Message::Close(None))
        );
        Ok(())
    }
}
