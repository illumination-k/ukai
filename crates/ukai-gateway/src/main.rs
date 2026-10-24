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
//! never starts a pod. With a bucket ([`archive`]), the gateway keeps a copy
//! of every running workspace's reports and answers from it once the pod is
//! gone.
//!
//! `GET /agents/<id>/forward/<port>` upgrades to a WebSocket carrying one
//! TCP connection to a port in that workspace's pod, through the pod's
//! `ukai-relay`. Like reports, it never starts a pod.
//!
//! The rest is request and answer, served as gRPC and gRPC-Web on the same
//! port ([`api`], `ukai.gateway.v1`): `Agents` lists the agents and restarts
//! a workspace (stopping its pod, so the next connection starts a fresh
//! one, e.g. to pick up a newer agent release); `Ports` lists what listens
//! in a workspace's pod and the previews its agent asked for; `Schedules`
//! keeps the agent's [`schedules`], commands and prompts the gateway runs on
//! a cron pattern, in a workspace of each schedule's own. These name the
//! workspace in the request rather than in headers.
//!
//! `ukai.gateway.v1.Knowledge` (gRPC and gRPC-Web, on the same port) is the
//! knowledge of an agent's scope (its project in `gateway.toml`, else its
//! repository): notes kept outside the repositories by `ukai-knowledge`,
//! which the gateway asks on behalf of whoever it finds is asking
//! ([`knowledge`]). Agent pods reach their own scope's at
//! `WorkspaceKnowledge`, with their relay's token instead of the gateway's;
//! they cannot review.
//!
//! Environment:
//! - `UKAI_GATEWAY_CONFIG`: path of `gateway.toml` (default `/etc/ukai/gateway.toml`)
//! - `UKAI_GATEWAY_LISTEN`: address to serve (default `0.0.0.0:8080`)
//! - `UKAI_GATEWAY_TOKEN`: if set, clients must send `Authorization: Bearer <it>`
//! - `UKAI_RELAY_IMAGE`: image whose `/usr/local/bin/ukai-relay` agent pods
//!   run; the gateway's own image (read from its pod, `HOSTNAME`) when unset
//! - `UKAI_KNOWLEDGE_URL`: `ukai-knowledge`, which keeps the notes
//!   (`http://ukai-knowledge:50051` in `deploy/gateway`); without it the
//!   gateway keeps none, and agent pods are not offered any
//! - `UKAI_KNOWLEDGE_TOKEN`: the token `ukai-knowledge` answers; knowledge
//!   needs it too
//! - `UKAI_GATEWAY_INTERNAL_URL`: where agent pods reach the gateway for the
//!   knowledge (default `http://ukai-gateway`, the Service in `deploy/gateway`)
//! - `UKAI_REPORTS_BUCKET`: the S3 bucket keeping a copy of the workspaces'
//!   reports ([`archive`]), reached as the `AWS_*` variables say
//!   (`AWS_ENDPOINT`, `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`,
//!   `AWS_REGION`); without it no copy is kept
//!
//! Each agent runs in a Job per workspace, in the gateway's namespace, with
//! nothing installed cluster-side: no CRD, no PersistentVolumeClaim of its
//! own. [`job`] builds the Job from the config without touching the cluster;
//! [`cluster`] finds or creates it and waits for its pod.

use std::collections::BTreeMap;
use std::process::ExitCode;
use std::sync::Arc;

use axum::Router;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use futures::stream::{SplitSink, SplitStream};
use futures::{Sink, SinkExt, Stream, StreamExt};
use k8s_openapi::api::core::v1::Pod;
use serde_json::json;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, tungstenite};
mod api;
mod archive;
mod cluster;
mod config;
mod job;
mod knowledge;
mod schedules;

use crate::archive::Archive;
use crate::cluster::{Cluster, RelayEndpoint};
use crate::config::KubernetesAgent;
use crate::job::{Plan, Runtime};
use crate::knowledge::KnowledgeClient;
use crate::schedules::Scheduler;

struct Gateway {
    agents: BTreeMap<String, KubernetesAgent>,
    cluster: Cluster,
    user: String,
    runtime: Runtime,
    token: Option<String>,
    schedules: Scheduler,
    /// `ukai-knowledge`, when the gateway is given one.
    knowledge: Option<KnowledgeClient>,
    /// The copy of the workspaces' reports, when there is a bucket for it.
    archive: Option<Arc<Archive>>,
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
    let knowledge_url = env("UKAI_KNOWLEDGE_URL");
    let knowledge = match (&knowledge_url, env("UKAI_KNOWLEDGE_TOKEN")) {
        (Some(url), Some(token)) => Some(
            KnowledgeClient::new(url, &token)
                .map_err(|error| format!("UKAI_KNOWLEDGE_URL: {error}"))?,
        ),
        (Some(_), None) => {
            tracing::warn!(
                "UKAI_KNOWLEDGE_URL is set without UKAI_KNOWLEDGE_TOKEN; keeping no knowledge"
            );
            None
        }
        (None, _) => None,
    };
    // Agent pods are told where the gateway is only when it keeps knowledge.
    let gateway_url = knowledge.as_ref().map(|_| {
        env("UKAI_GATEWAY_INTERNAL_URL").unwrap_or_else(|| "http://ukai-gateway".to_owned())
    });
    let user = cluster.whoami().await?;
    tracing::info!(namespace = cluster.namespace(), %user, %relay_image, ?gateway_url, knowledge = ?knowledge.as_ref().and(knowledge_url.as_ref()), agents = ?config.agents.keys().collect::<Vec<_>>(), "gateway starting");
    let archive = match env("UKAI_REPORTS_BUCKET") {
        Some(bucket) => {
            let archive = Archive::connect(&bucket)
                .await
                .map_err(|error| format!("UKAI_REPORTS_BUCKET {bucket}: {error}"))?;
            tracing::info!(%bucket, "keeping a copy of the workspaces' reports");
            Some(Arc::new(archive))
        }
        None => None,
    };
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
        knowledge,
        archive,
    });
    if let Some(archive) = &gateway.archive {
        tokio::spawn(archive::run(archive.clone(), gateway.cluster.clone()));
    }
    tokio::spawn(saver.run(gateway.clone()));
    tokio::spawn(schedules::tick(gateway.clone()));
    let router = Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/agents/{id}/acp", get(connect))
        .route("/agents/{id}/reports", get(reports))
        .route("/agents/{id}/reports/files/{*path}", get(reports))
        .route("/agents/{id}/forward/{port}", get(forward))
        .with_state(gateway.clone())
        .merge(grpc(gateway));
    let listener = tokio::net::TcpListener::bind(&listen).await?;
    tracing::info!(%listen, "serving");
    axum::serve(listener, router).await?;
    Ok(())
}

/// The gateway's gRPC services, each answering gRPC and gRPC-Web on the
/// same port as everything else: the app speaks gRPC-Web, which passes
/// HTTP/1.1 proxies such as IAP.
fn grpc(gateway: Arc<Gateway>) -> Router {
    use tonic_web::GrpcWebLayer;
    use tower::Layer;
    use ukai_proto::gateway::v1::agents_server::AgentsServer;
    use ukai_proto::gateway::v1::knowledge_server::KnowledgeServer;
    use ukai_proto::gateway::v1::ports_server::PortsServer;
    use ukai_proto::gateway::v1::schedules_server::SchedulesServer;
    use ukai_proto::gateway::v1::workspace_knowledge_server::WorkspaceKnowledgeServer;

    let web = GrpcWebLayer::new();
    let api = api::Api {
        gateway: gateway.clone(),
    };
    tonic::service::Routes::new(web.layer(KnowledgeServer::new(knowledge::ClientKnowledge {
        gateway: gateway.clone(),
    })))
    .add_service(
        web.layer(WorkspaceKnowledgeServer::new(knowledge::PodKnowledge {
            gateway,
        })),
    )
    .add_service(web.layer(AgentsServer::new(api.clone())))
    .add_service(web.layer(PortsServer::new(api.clone())))
    .add_service(web.layer(SchedulesServer::new(api)))
    .prepare()
    .into_axum_router()
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

/// `GET /agents/<id>/reports[/files/<path>]`: what the agent wrote in the
/// workspace this client asks for, read from the `ukai-relay` in its pod.
///
/// The workspace is resolved exactly as for `/acp`, and the same token is
/// required, but a report request never starts a pod: it is a read of work
/// that is already there. A workspace whose pod has idled out is answered
/// from the copy the gateway kept of it, when there is a bucket for one, and
/// is 410 otherwise, so a pane can say so instead of waiting out an image
/// pull.
async fn reports(State(gateway): State<Arc<Gateway>>, headers: HeaderMap, uri: Uri) -> Response {
    // The path is read from the URI rather than through `Path`, which cannot
    // serve both routes at once, and so the file part stays percent-encoded
    // for the relay to decode exactly once.
    let Some((id, suffix)) = reports_target(uri.path()) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let plan = match workspace_plan(&gateway, &id, &headers) {
        Ok(plan) => plan,
        Err(refused) => return refused.into_response(),
    };
    let endpoint = match running_relay(&gateway, &id, &plan).await {
        Ok(Some(endpoint)) => endpoint,
        Ok(None) => {
            return match &gateway.archive {
                Some(archive) => from_archive(archive, &id, plan.workspace_id(), suffix).await,
                None => gone(),
            };
        }
        Err(response) => return response,
    };
    match fetch(&endpoint.reports_url(suffix), &endpoint.token).await {
        Ok(response) => response,
        Err(error) => {
            tracing::warn!(agent = %id, %error, "cannot read the workspace's reports");
            StatusCode::BAD_GATEWAY.into_response()
        }
    }
}

/// A report request answered from the copy of a workspace that is not
/// running: the listing, saying when the copy was taken, or one file. 410
/// as before when there is no copy to read.
async fn from_archive(archive: &Archive, id: &str, workspace: &str, suffix: &str) -> Response {
    let answer = match suffix.strip_prefix(ukai_acp::reports::FILES_PATH) {
        None => archive
            .listing(workspace)
            .await
            .map(|listing| listing.map(|listing| axum::Json(listing).into_response())),
        Some(path) => archive.file(workspace, path).await.map(|file| {
            file.map(|file| {
                let mut builder = Response::builder()
                    .header(header::CACHE_CONTROL, "no-store")
                    .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff");
                if let Some(content_type) = file.content_type {
                    builder = builder.header(header::CONTENT_TYPE, content_type);
                }
                if let Some(etag) = file.etag {
                    builder = builder.header(header::ETAG, etag);
                }
                builder
                    .body(file.body.into())
                    .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
            })
        }),
    };
    match answer {
        Ok(Some(response)) => response,
        Ok(None) if suffix.is_empty() => gone(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(error) => {
            tracing::warn!(agent = %id, %error, "cannot read the copy of the workspace's reports");
            StatusCode::BAD_GATEWAY.into_response()
        }
    }
}

/// The workspace a request asks for, resolved as for `/acp` and with the
/// same token, or the status and message saying why it may not.
fn workspace_plan(
    gateway: &Gateway,
    id: &str,
    headers: &HeaderMap,
) -> Result<Plan, (StatusCode, &'static str)> {
    if !authorized(gateway, headers) {
        return Err((StatusCode::UNAUTHORIZED, ""));
    }
    let Some(agent) = gateway.agents.get(id).cloned() else {
        return Err((StatusCode::NOT_FOUND, ""));
    };
    let pane = pane_key(headers).map_err(|problem| (StatusCode::BAD_REQUEST, problem))?;
    Ok(Plan::new(
        agent,
        &gateway.user,
        gateway.cluster.namespace(),
        pane.as_deref(),
    ))
}

/// The relay of the workspace's running pod; `None` when it has none. It
/// never starts a pod: what asks this way reads work that is already there.
async fn running_relay(
    gateway: &Gateway,
    id: &str,
    plan: &Plan,
) -> Result<Option<RelayEndpoint>, Response> {
    match gateway.cluster.serving_pod(plan).await {
        Ok(pod) => Ok(pod.as_ref().and_then(RelayEndpoint::of)),
        Err(error) => {
            tracing::warn!(agent = %id, %error, "cannot look up the workspace's pod");
            Err(StatusCode::BAD_GATEWAY.into_response())
        }
    }
}

/// [`running_relay`] of the workspace a request asks for, with a workspace
/// that is not running being 410.
async fn serving_relay(
    gateway: &Gateway,
    id: &str,
    headers: &HeaderMap,
) -> Result<RelayEndpoint, Response> {
    let plan = workspace_plan(gateway, id, headers).map_err(IntoResponse::into_response)?;
    running_relay(gateway, id, &plan).await?.ok_or_else(gone)
}

/// `GET /agents/<id>/forward/<port>`: one TCP connection to `port` in the
/// workspace's pod, as a WebSocket. The relay is reached before the client
/// is upgraded, so a port nothing listens on is the relay's 502 rather than
/// a socket that closes at once.
async fn forward(
    State(gateway): State<Arc<Gateway>>,
    Path((id, port)): Path<(String, u16)>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    let endpoint = match serving_relay(&gateway, &id, &headers).await {
        Ok(endpoint) => endpoint,
        Err(response) => return response,
    };
    let upstream = match connect_relay_url(&endpoint.forward_url(port), &endpoint.token).await {
        Ok(upstream) => upstream,
        Err(tungstenite::Error::Http(response)) => {
            return StatusCode::from_u16(response.status().as_u16())
                .unwrap_or(StatusCode::BAD_GATEWAY)
                .into_response();
        }
        Err(error) => {
            tracing::warn!(agent = %id, port, %error, "cannot reach the workspace's relay");
            return StatusCode::BAD_GATEWAY.into_response();
        }
    };
    upgrade.on_upgrade(move |socket| async move {
        let (client_tx, client_rx) = socket.split();
        let (relay_tx, relay_rx) = upstream.split();
        if let Err(error) = pump(client_tx, client_rx, relay_tx, relay_rx).await {
            tracing::debug!(agent = %id, port, %error, "a forwarded connection ended with an error");
        }
    })
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

/// Read one response from the pod's relay at `url` and pass it on, with the
/// headers a web view needs to render it and nothing else.
async fn fetch(url: &str, token: &str) -> Result<Response, BoxError> {
    let response = reqwest::Client::new()
        .get(url)
        .bearer_auth(token)
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

/// How long the gateway waits on the pod for one report or listing.
const REPORT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// Whether `key` can name a pane's own workspace: 1-64 letters, digits,
/// `-`, or `_`.
fn valid_pane_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 64
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// The pane key when the client asks for a pod of the pane's own.
fn pane_key(headers: &HeaderMap) -> Result<Option<String>, &'static str> {
    let header = |name| headers.get(name).and_then(|value| value.to_str().ok());
    match header("ukai-workspace-scope") {
        None | Some("shared") => Ok(None),
        Some("pod") => {
            let key = header("ukai-workspace").ok_or("scope `pod` needs a Ukai-Workspace key")?;
            if valid_pane_key(key) {
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
    Ok(connect_relay_url(&endpoint.acp_url(), &endpoint.token).await?)
}

async fn connect_relay_url(url: &str, token: &str) -> Result<Upstream, tungstenite::Error> {
    let mut request = url.into_client_request()?;
    let authorization = format!("Bearer {token}")
        .parse()
        .map_err(|_| tungstenite::Error::Url(tungstenite::error::UrlError::NoHostName))?;
    request
        .headers_mut()
        .insert(header::AUTHORIZATION, authorization);
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
