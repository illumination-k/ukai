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
//! Environment:
//! - `UKAI_GATEWAY_CONFIG`: path of `gateway.toml` (default `/etc/ukai/gateway.toml`)
//! - `UKAI_GATEWAY_LISTEN`: address to serve (default `0.0.0.0:8080`)
//! - `UKAI_GATEWAY_TOKEN`: if set, clients must send `Authorization: Bearer <it>`
//! - `UKAI_RELAY_IMAGE`: image whose `/usr/local/bin/ukai-relay` agent pods
//!   run; the gateway's own image (read from its pod, `HOSTNAME`) when unset
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
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use futures::stream::{SplitSink, SplitStream};
use futures::{SinkExt, StreamExt};
use k8s_openapi::api::core::v1::Pod;
use serde_json::json;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, tungstenite};
mod cluster;
mod config;
mod job;

use crate::cluster::{Cluster, RelayEndpoint};
use crate::config::KubernetesAgent;
use crate::job::Plan;

struct Gateway {
    agents: BTreeMap<String, KubernetesAgent>,
    cluster: Cluster,
    user: String,
    relay_image: String,
    token: Option<String>,
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
    let user = cluster.whoami().await?;
    tracing::info!(namespace = cluster.namespace(), %user, %relay_image, agents = ?config.agents.keys().collect::<Vec<_>>(), "gateway starting");
    let gateway = Arc::new(Gateway {
        agents: config.agents,
        cluster,
        user,
        relay_image,
        token: env("UKAI_GATEWAY_TOKEN"),
    });
    let router = Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/agents", get(list))
        .route("/agents/{id}/acp", get(connect))
        .with_state(gateway);
    let listener = tokio::net::TcpListener::bind(&listen).await?;
    tracing::info!(%listen, "serving");
    axum::serve(listener, router).await?;
    Ok(())
}

fn authorized(gateway: &Gateway, headers: &HeaderMap) -> bool {
    let Some(token) = &gateway.token else {
        return true;
    };
    headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .is_some_and(|presented| {
            presented.len() == token.len()
                && presented
                    .bytes()
                    .zip(token.bytes())
                    .fold(0u8, |acc, (a, b)| acc | (a ^ b))
                    == 0
        })
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
        .ensure_pod(plan, &gateway.relay_image, move |message| {
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
    let mut request = endpoint.url.as_str().into_client_request()?;
    request.headers_mut().insert(
        header::AUTHORIZATION,
        format!("Bearer {}", endpoint.token).parse()?,
    );
    let (upstream, _) = tokio_tungstenite::connect_async(request).await?;
    Ok(upstream)
}

/// Pass frames both ways until either side closes, then close the other.
async fn pump(
    mut client_tx: ClientTx,
    mut client_rx: ClientRx,
    mut relay_tx: SplitSink<Upstream, tungstenite::Message>,
    mut relay_rx: SplitStream<Upstream>,
) -> Result<(), BoxError> {
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
}
