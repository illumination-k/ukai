//! The relay process: the agent as a child speaking stdio, client connections
//! over ACP's HTTP/SSE and WebSocket transports, and the [`Hub`] between them.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
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
    /// The gateway that started this pod: with `token`, the MCP server
    /// hands the agent the knowledge it keeps for the project.
    pub gateway: Option<String>,
    /// Exit after this long with no client connected, no turn running, and
    /// no terminal open.
    pub idle_timeout: Duration,
    pub command: String,
    pub args: Vec<String>,
    /// Touched after every finished turn, for the pod's workspace sidecar to
    /// take a snapshot.
    pub snapshot_trigger: Option<PathBuf>,
    /// The agent is a secretary: its MCP server offers the tools that read
    /// and prompt the other workspaces, which the gateway answers only for
    /// a secretary's pod.
    pub secretary: bool,
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
    let records = Records::start();
    // What the gateway asks about the sessions for a secretary; see
    // `crate::sessions`.
    let (queries_tx, mut queries) = mpsc::unbounded_channel();
    // The runs of flows that sessions started and wait for: the relay tells
    // each when its run has ended; see `crate::flows::watch`.
    let flows_awaited = match (&cwd, options.secretary) {
        (Some(root), false) => {
            let waits = crate::mcp::flows::waits_file(&records.sessions);
            if let Err(error) = std::fs::write(&waits, b"") {
                tracing::warn!(%error, path = %waits.display(), "cannot record flows' runs");
            }
            Some(crate::flows::watch(root.clone(), waits, queries_tx.clone()))
        }
        _ => None,
    };
    // A shell in the checkout, for a terminal pane; see `crate::terminal`.
    let terminals = crate::terminal::Terminals::default();
    let router = routes(
        server.into_router(),
        &options,
        cwd.clone().unwrap_or_else(|| PathBuf::from(".")),
        &records,
        queries_tx,
        terminals.clone(),
    );
    let listener = tokio::net::TcpListener::bind(options.listen).await?;
    tracing::info!(listen = %options.listen, command = %options.command, "relay serving");
    tokio::spawn(async move {
        if let Err(error) = axum::serve(listener, router).await {
            tracing::error!(%error, "relay server failed");
        }
    });

    // The agent starts this binary again as an MCP server that tells it
    // what ukai does with its reports, and hands it the project's
    // knowledge; see `crate::mcp`.
    let gateway = match (&options.gateway, &options.token) {
        (Some(url), Some(token)) => Some(crate::mcp::Gateway {
            url: url.clone(),
            token: token.clone(),
        }),
        _ => None,
    };
    let mcp_servers = mcp_servers(&options, cwd.as_deref(), gateway.as_ref(), &records);
    let mut outlet = Outlet {
        to_agent,
        conns: HashMap::new(),
        answers: crate::sessions::Answers::default(),
        sessions: records.sessions,
        // What each turn took, for the gateway to keep; see `crate::metrics`.
        meter: crate::metrics::Meter::new(match (&options.gateway, &options.token) {
            (Some(url), Some(token)) => {
                ukai_acp::metrics::MetricsEndpoint::for_workspace(url, token)
                    .inspect_err(
                        |error| tracing::warn!(%error, "cannot reach the gateway for metrics"),
                    )
                    .ok()
            }
            _ => None,
        }),
        snapshot_trigger: options.snapshot_trigger.clone(),
    };
    let (mut hub, init) = Hub::new(
        cwd.map(|dir| dir.to_string_lossy().into_owned()),
        mcp_servers,
    );
    // Only a secretary with the gateway to reach has the tools its role needs.
    if options.secretary && gateway.is_some() {
        hub.append_system_prompt(crate::mcp::secretary::system_prompt());
    }
    outlet.dispatch(init);
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
                    outlet.dispatch(hub.from_agent(message));
                }
            }
            Some(event) = events.recv() => match event {
                Event::Open(conn, tx) => {
                    tracing::info!(conn, "client connected");
                    outlet.conns.insert(conn, tx);
                    hub.connect(conn);
                }
                Event::Frame(conn, frame) => {
                    for message in values(frame) {
                        outlet.dispatch(hub.from_client(conn, message));
                    }
                }
                Event::Close(conn) => {
                    tracing::info!(conn, "client disconnected");
                    outlet.conns.remove(&conn);
                    hub.disconnect(conn);
                }
            },
            Some(query) = queries.recv() => {
                let outputs = outlet.answers.answer(&mut hub, query);
                outlet.dispatch(outputs);
            }
            _ = tick.tick() => {
                // Someone typing in a terminal is as busy as a turn, and so
                // is a flow whose end a session waits for.
                let flows = flows_awaited
                    .as_ref()
                    .map_or(0, |awaited| awaited.load(Ordering::Relaxed));
                if hub.busy() || terminals.open() > 0 || flows > 0 {
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

/// The files this relay's MCP servers write and its routes read, under the
/// temporary directory and out of the checkout.
struct Records {
    /// The servers the agent runs in the pod are as out of reach as its
    /// reports; the MCP server records the ones it asks the user to look
    /// at here. See `crate::ports`.
    previews: PathBuf,
    /// The agent's review of its changes, written through the MCP server
    /// and read with the diff; see `crate::review`.
    review: PathBuf,
    /// Which session each of its MCP servers serves, for them to publish
    /// reports for it; see `crate::mcp::record_session`.
    sessions: PathBuf,
}

impl Records {
    /// This relay's files, with no session recorded yet.
    fn start() -> Self {
        let previews = previews_file();
        let sessions = previews.with_file_name("sessions.jsonl");
        if let Err(error) = std::fs::write(&sessions, b"") {
            tracing::warn!(%error, path = %sessions.display(), "cannot record sessions");
        }
        Self {
            review: previews.with_file_name("review.json"),
            sessions,
            previews,
        }
    }
}

/// Everything the relay serves: ACP at `acp`, and what a pane shows of the
/// checkout at `root`, all behind the relay's token.
fn routes(
    acp: axum::Router,
    options: &Options,
    root: PathBuf,
    records: &Records,
    queries: mpsc::UnboundedSender<crate::sessions::Query>,
    terminals: crate::terminal::Terminals,
) -> axum::Router {
    let token = options.token.clone().map(Arc::<str>::from);
    acp.merge(crate::reports::router(root.clone()))
        .merge(crate::checkout::router(
            root.clone(),
            records.review.clone(),
            // What the user attaches, next to the previews: this relay's,
            // and out of the checkout.
            records.previews.with_file_name("attachments"),
        ))
        .merge(crate::review::router(records.review.clone()))
        .merge(crate::sessions::router(queries))
        .merge(crate::terminal::router(root.clone(), terminals))
        // The runs of the flows the agent starts in the checkout; see
        // `crate::flows`.
        .merge(crate::flows::router(root))
        .merge(crate::ports::router(crate::ports::Ports {
            own: options.listen.port(),
            previews: records.previews.clone(),
        }))
        .layer(middleware::from_fn(move |request, next| {
            authorize(token.clone(), request, next)
        }))
}

/// The MCP servers every session gets: this binary, started again by the
/// agent.
fn mcp_servers(
    options: &Options,
    cwd: Option<&Path>,
    gateway: Option<&crate::mcp::Gateway>,
    records: &Records,
) -> Vec<Value> {
    match (cwd, std::env::current_exe()) {
        (Some(root), Ok(program)) => {
            vec![crate::mcp::server_entry(
                &program,
                root,
                gateway,
                // A secretary hands work off rather than doing it, so it is
                // told nothing about showing servers or reviewing changes.
                (!options.secretary).then_some(records.previews.as_path()),
                (!options.secretary).then_some(records.review.as_path()),
                Some(&records.sessions),
                options.secretary,
                // Nor about running workflows of agents.
                !options.secretary,
            )]
        }
        (_, Err(error)) => {
            tracing::warn!(%error, "cannot find the relay binary; sessions get no ukai MCP server");
            Vec::new()
        }
        (None, _) => Vec::new(),
    }
}

/// Where what the hub puts out goes.
struct Outlet {
    to_agent: UnboundedSender<TransportFrame>,
    conns: HashMap<ConnId, UnboundedSender<TransportFrame>>,
    answers: crate::sessions::Answers,
    /// [`Records::sessions`].
    sessions: PathBuf,
    meter: crate::metrics::Meter,
    /// [`Options::snapshot_trigger`].
    snapshot_trigger: Option<PathBuf>,
}

impl Outlet {
    fn dispatch(&mut self, outputs: Vec<Output>) {
        for output in outputs {
            match output {
                Output::Agent(message) => {
                    if let Some(frame) = frame(message) {
                        let _ = self.to_agent.unbounded_send(frame);
                    }
                }
                Output::Client(conn, message) => {
                    if let (Some(tx), Some(frame)) = (self.conns.get(&conn), frame(message)) {
                        let _ = tx.unbounded_send(frame);
                    }
                }
                Output::SessionKey { key, session } => {
                    if let Err(error) = crate::mcp::record_session(&self.sessions, &key, &session) {
                        tracing::warn!(%error, path = %self.sessions.display(), "cannot record a session");
                    }
                }
                Output::Started { ticket, outcome } => self.answers.started(ticket, outcome),
                Output::Metered { session, event } => self.meter.observe(&session, &event),
                Output::TurnEnded(session) => {
                    if let Some(path) = &self.snapshot_trigger
                        && let Err(error) = std::fs::write(path, session.as_bytes())
                    {
                        tracing::warn!(%error, path = %path.display(), "could not request a snapshot");
                    }
                }
            }
        }
    }
}

/// Where this relay's MCP servers record previews: a file of its own under
/// the temporary directory, emptied at start, so the previews listed are
/// this relay's and the checkout stays clean.
fn previews_file() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ukai-relay-{}", std::process::id()));
    let file = dir.join("previews.jsonl");
    if let Err(error) = std::fs::create_dir_all(&dir).and_then(|()| std::fs::write(&file, b"")) {
        tracing::warn!(%error, path = %file.display(), "cannot keep previews");
    }
    file
}

async fn authorize(token: Option<Arc<str>>, request: Request, next: Next) -> Response {
    let Some(token) = token else {
        return next.run(request).await;
    };
    // `/health` stays open for the kubelet's probes; `/acp`, the reports
    // of the agent's checkout, its files and commands, its terminals, its flows, and its ports all
    // want the token.
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
