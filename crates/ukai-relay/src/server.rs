//! The relay process: the agent as a child speaking stdio, client connections
//! over ACP's HTTP/SSE and WebSocket transports, and the [`Hub`] between them.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use agent_client_protocol::{
    AcpAgent, AcpAgentConfig, Channel, Client, ConnectTo, RawJsonRpcMessage, TransportFrame,
};
use agent_client_protocol_http::AcpHttpServer;
use axum::extract::Request;
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use futures::StreamExt;
use futures::channel::mpsc::UnboundedSender;
use serde_json::Value;
use tokio::sync::mpsc;

use crate::hub::{ConnId, Hub, Output};

#[derive(Debug, Clone)]
pub struct Options {
    pub listen: SocketAddr,
    /// Clients must send `Authorization: Bearer <token>` to everything but
    /// `/health`.
    pub token: Option<String>,
    /// Exit after this long with no client connected and no turn running.
    pub idle_timeout: Duration,
    pub command: String,
    pub args: Vec<String>,
    /// Touched after every finished turn, for the pod's workspace sidecar to
    /// take a snapshot.
    pub snapshot_trigger: Option<std::path::PathBuf>,
}

enum Event {
    Open(ConnId, UnboundedSender<TransportFrame>),
    Frame(ConnId, TransportFrame),
    Close(ConnId),
}

/// Serve until the agent exits, the relay has been idle for
/// [`Options::idle_timeout`], or the process is asked to terminate.
pub async fn run(options: Options) -> std::io::Result<()> {
    let agent = AcpAgent::new(AcpAgentConfig::new(&options.command).args(options.args.clone()));
    let (agent_channel, driver) = ConnectTo::<Client>::into_channel_and_future(agent);
    if let Some(driver) = driver {
        tokio::spawn(async move {
            if let Err(error) = driver.await {
                tracing::warn!(%error, "agent connection ended");
            }
        });
    }
    let Channel {
        rx: mut from_agent,
        tx: to_agent,
    } = agent_channel;

    let (events_tx, mut events) = mpsc::unbounded_channel();
    let next_conn = Arc::new(AtomicU64::new(1));
    let server = AcpHttpServer::new(move || {
        let (ours, theirs) = Channel::duplex();
        let conn = next_conn.fetch_add(1, Ordering::Relaxed);
        let events = events_tx.clone();
        let Channel { mut rx, tx } = ours;
        let _ = events.send(Event::Open(conn, tx));
        tokio::spawn(async move {
            while let Some(frame) = rx.next().await {
                let _ = events.send(Event::Frame(conn, frame));
            }
            let _ = events.send(Event::Close(conn));
        });
        theirs
    });
    // The agent's reports live in the directory it works in, and the relay
    // is the only thing that can read them; see `crate::reports`.
    let cwd = std::env::current_dir().ok();
    let token = options.token.clone().map(Arc::<str>::from);
    let router = server
        .into_router()
        .merge(crate::reports::router(
            cwd.clone().unwrap_or_else(|| std::path::PathBuf::from(".")),
        ))
        .layer(middleware::from_fn(move |request, next| {
            authorize(token.clone(), request, next)
        }));
    let listener = tokio::net::TcpListener::bind(options.listen).await?;
    tracing::info!(listen = %options.listen, command = %options.command, "relay serving");
    tokio::spawn(async move {
        if let Err(error) = axum::serve(listener, router).await {
            tracing::error!(%error, "relay server failed");
        }
    });

    let send_agent = |message: Value| {
        if let Some(frame) = frame(message) {
            let _ = to_agent.unbounded_send(frame);
        }
    };
    let mut conns: HashMap<ConnId, UnboundedSender<TransportFrame>> = HashMap::new();
    let trigger = options.snapshot_trigger.clone();
    let dispatch = |outputs: Vec<Output>,
                    conns: &HashMap<ConnId, UnboundedSender<TransportFrame>>| {
        for output in outputs {
            match output {
                Output::Agent(message) => send_agent(message),
                Output::Client(conn, message) => {
                    if let (Some(tx), Some(frame)) = (conns.get(&conn), frame(message)) {
                        let _ = tx.unbounded_send(frame);
                    }
                }
                Output::TurnEnded(session) => {
                    if let Some(path) = &trigger
                        && let Err(error) = std::fs::write(path, session.as_bytes())
                    {
                        tracing::warn!(%error, path = %path.display(), "could not request a snapshot");
                    }
                }
            }
        }
    };

    // The agent starts this binary again as an MCP server that tells it
    // what ukai does with its reports; see `crate::mcp`.
    let mcp_servers = match (&cwd, std::env::current_exe()) {
        (Some(root), Ok(program)) => vec![crate::mcp::server_entry(&program, root)],
        (_, Err(error)) => {
            tracing::warn!(%error, "cannot find the relay binary; sessions get no ukai MCP server");
            Vec::new()
        }
        (None, _) => Vec::new(),
    };
    let (mut hub, init) = Hub::new(
        cwd.map(|dir| dir.to_string_lossy().into_owned()),
        mcp_servers,
    );
    dispatch(init, &conns);
    let mut idle_since = Some(Instant::now());
    let mut tick = tokio::time::interval(Duration::from_secs(5));
    let terminate = terminate_signal();
    tokio::pin!(terminate);
    loop {
        tokio::select! {
            frame = from_agent.next() => {
                let Some(frame) = frame else {
                    tracing::warn!("agent exited");
                    break;
                };
                for message in values(frame) {
                    dispatch(hub.from_agent(message), &conns);
                }
            }
            Some(event) = events.recv() => match event {
                Event::Open(conn, tx) => {
                    tracing::info!(conn, "client connected");
                    conns.insert(conn, tx);
                    hub.connect(conn);
                }
                Event::Frame(conn, frame) => {
                    for message in values(frame) {
                        dispatch(hub.from_client(conn, message), &conns);
                    }
                }
                Event::Close(conn) => {
                    tracing::info!(conn, "client disconnected");
                    conns.remove(&conn);
                    hub.disconnect(conn);
                }
            },
            _ = tick.tick() => {
                if hub.busy() {
                    idle_since = None;
                } else if idle_since.get_or_insert_with(Instant::now).elapsed() >= options.idle_timeout {
                    tracing::info!(idle = ?options.idle_timeout, "idle, exiting");
                    break;
                }
            }
            () = &mut terminate => {
                tracing::info!("terminating");
                break;
            }
        }
    }
    Ok(())
}

async fn authorize(token: Option<Arc<str>>, request: Request, next: Next) -> Response {
    let Some(token) = token else {
        return next.run(request).await;
    };
    // `/health` stays open for the kubelet's probes; `/acp` and the reports
    // of the agent's checkout both want the token.
    if request.uri().path().starts_with("/health") {
        return next.run(request).await;
    }
    let presented = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    if presented.is_some_and(|presented| constant_time_eq(presented.as_bytes(), token.as_bytes())) {
        next.run(request).await
    } else {
        StatusCode::UNAUTHORIZED.into_response()
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

async fn terminate_signal() {
    #[cfg(unix)]
    {
        if let Ok(mut term) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            tokio::select! {
                _ = term.recv() => {}
                _ = tokio::signal::ctrl_c() => {}
            }
            return;
        }
    }
    let _ = tokio::signal::ctrl_c().await;
}

fn values(frame: TransportFrame) -> Vec<Value> {
    match frame {
        TransportFrame::Single(message) => serde_json::to_value(message).into_iter().collect(),
        TransportFrame::Batch(_) | TransportFrame::Malformed { .. } => {
            tracing::warn!("dropping a batch or malformed frame");
            Vec::new()
        }
    }
}

fn frame(message: Value) -> Option<TransportFrame> {
    match serde_json::from_value::<RawJsonRpcMessage>(message) {
        Ok(message) => Some(TransportFrame::Single(message)),
        Err(error) => {
            tracing::warn!(%error, "dropping a message that is not JSON-RPC");
            None
        }
    }
}
