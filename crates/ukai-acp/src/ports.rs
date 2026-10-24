//! Ports a gateway agent's workspace listens on, and tunnels to them.
//!
//! An agent behind `ukai-gateway` runs a dev server in its pod, where the
//! user cannot reach it. The relay in the pod lists what listens there and
//! carries a TCP connection to one of those ports over a WebSocket; the
//! gateway passes both through per workspace, as it does reports. This is
//! the client side: [`PortsEndpoint`] reads the listing (the gateway's
//! `ukai.gateway.v1.Ports`, gRPC-Web; see `crate::grpc`), and a [`Tunnel`]
//! listens on this machine and carries each connection it accepts to the
//! port in the pod, so a browser pane sees an ordinary `127.0.0.1` origin —
//! absolute paths and a dev server's hot-reload socket work as they do
//! locally, which proxying HTTP under a path prefix would break.
//!
//! The listing also carries the previews the agent asked the user to look
//! at (through the relay's MCP server), which the app opens in a pane.

use std::net::{Ipv4Addr, SocketAddr};
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use reqwest::header::{CONNECTION, HeaderValue, SEC_WEBSOCKET_ACCEPT, SEC_WEBSOCKET_KEY, UPGRADE};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpSocket, TcpStream};
use tokio::task::JoinSet;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::client::generate_key;
use tokio_tungstenite::tungstenite::handshake::derive_accept_key;
use tokio_tungstenite::tungstenite::protocol::Role;
use ukai_config::{AgentSpec, Source};
use ukai_proto::gateway::v1 as gw;
use ukai_proto::gateway::v1::ports_client::PortsClient;

use crate::grpc::{self, Channel};
use crate::reports::endpoint_base;
use crate::{PaneWorkspace, ResolveError, header_map, workspace_headers};

/// Where a relay lists what listens in its pod (JSON, for the gateway).
pub const PORTS_PATH: &str = "/ports";

/// Suffix of an agent's ACP endpoint under which `/<port>` upgrades to a
/// WebSocket carrying one TCP connection to that port in the workspace.
pub const FORWARD_PATH: &str = "/forward";

/// How many previews a listing carries at most, newest last.
pub const PREVIEWS_KEPT: usize = 20;

/// How long a listing may take.
const TIMEOUT: Duration = Duration::from_secs(20);

/// How long opening a tunnelled connection may take before the browser is
/// told it failed: client → gateway → pod, and the port's own accept.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

/// What a relay or gateway answers at [`PORTS_PATH`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortListing {
    /// TCP ports something in the workspace listens on, lowest first; the
    /// relay's own is left out.
    pub ports: Vec<ListeningPort>,
    /// What the agent asked the user to look at, oldest first.
    #[serde(default)]
    pub previews: Vec<Preview>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListeningPort {
    pub port: u16,
    /// The name of the process listening, when the relay could tell.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process: Option<String>,
}

/// A page the agent asked the user to look at: a server it started in the
/// workspace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Preview {
    pub port: u16,
    /// Where on that server to start: `/`-prefixed, with any query.
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// When it was asked for, in milliseconds since the epoch by the pod's
    /// clock; a client compares it only with other previews' times.
    pub at: u64,
}

/// `path` as a preview may start at: `/`-prefixed and naming nothing but a
/// path and query on the same server, so it can only be appended to an
/// origin. Empty means the root. `None` for anything else, such as a full URL.
pub fn preview_path(path: &str) -> Option<String> {
    let path = path.trim();
    if path.is_empty() {
        return Some("/".to_owned());
    }
    let valid = path.starts_with('/')
        && !path.starts_with("//")
        && !path.contains('\\')
        && !path.chars().any(char::is_control);
    valid.then(|| path.to_owned())
}

#[derive(Debug, thiserror::Error)]
pub enum PortsError {
    #[error("cannot reach the gateway: {0}")]
    Unreachable(String),
    #[error("the agent's workspace is not running")]
    Gone,
    #[error("nothing listens on that port in the agent's workspace")]
    Refused,
    #[error("the gateway answered {0}")]
    Status(u16),
    #[error("the gateway sent a listing that cannot be read: {0}")]
    Listing(String),
    #[error("the gateway refused: {0}")]
    Failed(String),
}

impl From<tonic::Status> for PortsError {
    fn from(status: tonic::Status) -> Self {
        match grpc::Failure::from(status) {
            grpc::Failure::Unreachable(why) => Self::Unreachable(why),
            grpc::Failure::NotRunning => Self::Gone,
            grpc::Failure::NotFound => Self::Failed("the gateway has no such agent".to_owned()),
            grpc::Failure::Denied(why) | grpc::Failure::Failed(why) => Self::Failed(why),
            grpc::Failure::TooMany => Self::Failed("too many requests".to_owned()),
        }
    }
}

impl From<PortListing> for gw::PortListing {
    fn from(listing: PortListing) -> Self {
        Self {
            ports: listing
                .ports
                .into_iter()
                .map(|port| gw::ListeningPort {
                    port: port.port.into(),
                    process: port.process,
                })
                .collect(),
            previews: listing
                .previews
                .into_iter()
                .map(|preview| gw::Preview {
                    port: preview.port.into(),
                    path: preview.path,
                    title: preview.title,
                    at: preview.at,
                })
                .collect(),
        }
    }
}

impl TryFrom<gw::PortListing> for PortListing {
    type Error = PortsError;

    fn try_from(listing: gw::PortListing) -> Result<Self, PortsError> {
        let port = |port: u32| {
            u16::try_from(port).map_err(|_| PortsError::Listing(format!("{port} is not a port")))
        };
        Ok(Self {
            ports: listing
                .ports
                .into_iter()
                .map(|listening| {
                    Ok(ListeningPort {
                        port: port(listening.port)?,
                        process: listening.process,
                    })
                })
                .collect::<Result<_, PortsError>>()?,
            previews: listing
                .previews
                .into_iter()
                .map(|preview| {
                    Ok(Preview {
                        port: port(preview.port)?,
                        path: preview.path,
                        title: preview.title,
                        at: preview.at,
                    })
                })
                .collect::<Result<_, PortsError>>()?,
        })
    }
}

/// Where one workspace's ports are listed and reached: a gateway agent's
/// endpoint with the agent's headers and the pane's workspace among them.
#[derive(Debug, Clone)]
pub struct PortsEndpoint {
    channel: Channel,
    root: String,
    agent: String,
    workspace: gw::Workspace,
    /// For upgrades: no total timeout, which would cut a tunnelled
    /// connection that outlives it.
    forward: reqwest::Client,
    forward_base: String,
}

impl PartialEq for PortsEndpoint {
    /// Two endpoints are the same when they reach the same workspace.
    fn eq(&self, other: &Self) -> bool {
        (&self.root, &self.agent, &self.workspace) == (&other.root, &other.agent, &other.workspace)
    }
}

impl PortsEndpoint {
    /// The ports of `workspace` for `spec`, or `None` when the agent is not
    /// a gateway agent: a local agent's ports are this machine's own.
    pub fn resolve(
        spec: &AgentSpec,
        lookup: impl Fn(Source, &str) -> Option<String> + Copy,
        workspace: Option<&PaneWorkspace>,
    ) -> Result<Option<Self>, ResolveError> {
        let AgentSpec::Gateway {
            url,
            headers,
            workspace: default_scope,
        } = spec
        else {
            return Ok(None);
        };
        let url = ukai_config::interpolate(url, lookup)?;
        let (Some((root, agent)), Some(forward_base)) =
            (grpc::agent_root(&url), endpoint_base(&url, FORWARD_PATH))
        else {
            return Ok(None);
        };
        let client = reqwest::Client::builder()
            .default_headers(header_map(headers, &[], lookup)?)
            .timeout(TIMEOUT)
            .build()
            .map_err(|error| ResolveError::Client(error.to_string()))?;
        // The upgrade is not RPC: it names the workspace as `/acp` does.
        let extra = workspace_headers(workspace, *default_scope);
        let forward = reqwest::Client::builder()
            .default_headers(header_map(headers, &extra, lookup)?)
            .connect_timeout(CONNECT_TIMEOUT)
            .build()
            .map_err(|error| ResolveError::Client(error.to_string()))?;
        Ok(Some(Self {
            channel: grpc::channel(client, root.clone()),
            root,
            agent,
            workspace: grpc::workspace(workspace, *default_scope),
            forward,
            forward_base,
        }))
    }

    /// What listens in the workspace, and the previews asked for so far.
    pub async fn list(&self) -> Result<PortListing, PortsError> {
        let request = gw::ListPortsRequest {
            agent: self.agent.clone(),
            workspace: Some(self.workspace.clone()),
        };
        let listing = PortsClient::new(self.channel.clone())
            .list(request)
            .await?
            .into_inner();
        listing.try_into()
    }

    fn forward_url(&self, port: u16) -> String {
        format!("{}/{port}", self.forward_base)
    }

    /// One connection to `port` in the workspace, as a WebSocket of binary
    /// frames. The upgrade goes through the same client as everything else,
    /// so it carries the agent's headers and its TLS configuration.
    pub async fn connect(
        &self,
        port: u16,
    ) -> Result<WebSocketStream<reqwest::Upgraded>, PortsError> {
        let key = generate_key();
        let response = self
            .forward
            .get(self.forward_url(port))
            .version(reqwest::Version::HTTP_11)
            .header(CONNECTION, "Upgrade")
            .header(UPGRADE, "websocket")
            .header("Sec-WebSocket-Version", "13")
            .header(SEC_WEBSOCKET_KEY, &key)
            .send();
        let response = tokio::time::timeout(CONNECT_TIMEOUT, response)
            .await
            .map_err(|_| PortsError::Unreachable("timed out".to_owned()))?
            .map_err(|error| PortsError::Unreachable(error.to_string()))?;
        let status = response.status().as_u16();
        if status != 101 {
            return Err(failure(status));
        }
        let accepted = response.headers().get(SEC_WEBSOCKET_ACCEPT);
        let expected = HeaderValue::from_str(&derive_accept_key(key.as_bytes())).ok();
        if accepted.is_none() || accepted != expected.as_ref() {
            return Err(PortsError::Unreachable(
                "the gateway did not accept the WebSocket".to_owned(),
            ));
        }
        let upgraded = response
            .upgrade()
            .await
            .map_err(|error| PortsError::Unreachable(error.to_string()))?;
        Ok(WebSocketStream::from_raw_socket(upgraded, Role::Client, None).await)
    }
}

/// What a status that is not the expected one means. 404 and 410 are a
/// workspace whose pod is gone; 502 is the relay failing to reach the port.
fn failure(status: u16) -> PortsError {
    match status {
        404 | 410 => PortsError::Gone,
        502 => PortsError::Refused,
        other => PortsError::Status(other),
    }
}

/// A port of this machine that leads to a port in a workspace: every
/// connection accepted on it is carried to the pod over a WebSocket of its
/// own. It listens on `127.0.0.1` only, and stops — connections and all —
/// when dropped.
#[derive(Debug)]
pub struct Tunnel {
    local: SocketAddr,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Tunnel {
    /// Listen for `remote` in `endpoint`'s workspace: on the same port when
    /// it is free here, so the page keeps its origin (its storage, an OAuth
    /// redirect registered for it) from one tunnel to the next, else on any.
    pub async fn open(endpoint: PortsEndpoint, remote: u16) -> std::io::Result<Self> {
        let listener = match listen(remote) {
            Ok(listener) => listener,
            Err(_) => listen(0)?,
        };
        let local = listener.local_addr()?;
        let task = tokio::spawn(accept(listener, endpoint, remote));
        Ok(Self { local, task })
    }

    /// The port on `127.0.0.1` that leads to the workspace.
    pub fn local_port(&self) -> u16 {
        self.local.port()
    }
}

/// A listener on `127.0.0.1:port` without `SO_REUSEADDR`, which on macOS
/// would let it bind beside a server already on `*:port` and take its
/// connections to `127.0.0.1`.
fn listen(port: u16) -> std::io::Result<TcpListener> {
    let socket = TcpSocket::new_v4()?;
    socket.bind(SocketAddr::from((Ipv4Addr::LOCALHOST, port)))?;
    socket.listen(64)
}

async fn accept(listener: TcpListener, endpoint: PortsEndpoint, remote: u16) {
    // Held here so that aborting the tunnel aborts its connections too.
    let mut connections = JoinSet::new();
    loop {
        while connections.try_join_next().is_some() {}
        match listener.accept().await {
            Ok((socket, _)) => {
                let endpoint = endpoint.clone();
                connections.spawn(async move { carry(&endpoint, remote, socket).await });
            }
            Err(error) => {
                // Out of descriptors, usually; give the open ones time to end.
                tracing::warn!(%error, "a tunnel cannot accept a connection");
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
    }
}

/// Carry one connection to the workspace. When the workspace cannot be
/// reached, the connection is answered with a page saying why: the tunnel
/// serves browser panes, which would otherwise show only a reset.
async fn carry(endpoint: &PortsEndpoint, remote: u16, mut socket: TcpStream) {
    match endpoint.connect(remote).await {
        Ok(ws) => splice(ws, socket).await,
        Err(error) => {
            tracing::debug!(%error, port = remote, "a tunnelled connection failed");
            // Read the request first: closing with it unread resets the
            // connection, and the browser would drop the answer.
            let mut request = [0u8; 8192];
            let _ = tokio::time::timeout(Duration::from_secs(1), socket.read(&mut request)).await;
            let _ = socket.write_all(problem_page(&error).as_bytes()).await;
            let _ = socket.shutdown().await;
        }
    }
}

/// An HTTP response telling the browser why the port cannot be reached.
fn problem_page(error: &PortsError) -> String {
    let (status, message) = match error {
        PortsError::Gone => (
            "410 Gone",
            "The agent's workspace is not running. Open the agent to start it.".to_owned(),
        ),
        PortsError::Refused => (
            "502 Bad Gateway",
            "Nothing listens on this port in the agent's workspace yet. Reload once the server is up."
                .to_owned(),
        ),
        other => ("502 Bad Gateway", other.to_string()),
    };
    format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\n\
         Content-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{message}",
        message.len()
    )
}

/// Pass bytes between `ws` and `tcp` until either side ends, then end both.
/// Bytes travel as binary frames; HTTP never half-closes a connection it
/// still wants an answer on, so the first end is the end.
pub async fn splice<S>(ws: WebSocketStream<S>, tcp: TcpStream)
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (mut ws_tx, mut ws_rx) = ws.split();
    let (mut tcp_rx, mut tcp_tx) = tcp.into_split();
    let up = async {
        let mut buffer = vec![0u8; 16 * 1024];
        loop {
            match tcp_rx.read(&mut buffer).await {
                Ok(0) | Err(_) => break,
                Ok(read) => {
                    let frame = Message::binary(buffer[..read].to_vec());
                    if ws_tx.send(frame).await.is_err() {
                        return;
                    }
                }
            }
        }
        let _ = ws_tx.send(Message::Close(None)).await;
    };
    let down = async {
        while let Some(Ok(message)) = ws_rx.next().await {
            let bytes = match message {
                Message::Binary(bytes) => bytes,
                Message::Text(text) => text.as_bytes().to_vec().into(),
                Message::Close(_) => break,
                _ => continue,
            };
            if tcp_tx.write_all(&bytes).await.is_err() {
                break;
            }
        }
        let _ = tcp_tx.shutdown().await;
    };
    tokio::select! {
        () = up => {}
        () = down => {}
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use rstest::rstest;
    use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
    use ukai_config::WorkspaceScope;

    use super::*;

    fn gateway(url: &str) -> AgentSpec {
        AgentSpec::Gateway {
            url: url.to_owned(),
            headers: BTreeMap::from([("Authorization".to_owned(), "Bearer t".to_owned())]),
            workspace: WorkspaceScope::Shared,
        }
    }

    fn endpoint(url: &str) -> PortsEndpoint {
        PortsEndpoint::resolve(&gateway(url), |_, _| None, None)
            .unwrap_or_else(|e| panic!("resolve: {e}"))
            .unwrap_or_else(|| panic!("a gateway agent has ports"))
    }

    #[test]
    fn derives_the_ports_endpoints() {
        let endpoint = endpoint("wss://gw.example.com/agents/a/acp");
        assert_eq!(
            (endpoint.root.as_str(), endpoint.agent.as_str()),
            ("https://gw.example.com", "a")
        );
        assert_eq!(
            endpoint.forward_url(5173),
            "https://gw.example.com/agents/a/forward/5173"
        );
    }

    #[test]
    fn local_agents_have_no_ports_endpoint() {
        let spec = AgentSpec::Stdio {
            command: "agent".into(),
            args: Vec::new(),
            env: BTreeMap::new(),
        };
        assert!(matches!(
            PortsEndpoint::resolve(&spec, |_, _| None, None),
            Ok(None)
        ));
    }

    #[rstest]
    #[case::empty("", Some("/"))]
    #[case::root("/", Some("/"))]
    #[case::path("/docs/index.html?x=1#top", Some("/docs/index.html?x=1#top"))]
    #[case::relative("docs", None)]
    #[case::url("http://evil.example/", None)]
    #[case::scheme_relative("//evil.example/", None)]
    #[case::backslash("/\\evil.example", None)]
    #[case::newline("/a\r\nHost: x", None)]
    fn previews_start_at_a_path_of_the_same_server(
        #[case] path: &str,
        #[case] expected: Option<&str>,
    ) {
        assert_eq!(preview_path(path).as_deref(), expected);
    }

    #[test]
    fn listings_round_trip() {
        let listing = PortListing {
            ports: vec![
                ListeningPort {
                    port: 3000,
                    process: Some("node".into()),
                },
                ListeningPort {
                    port: 8000,
                    process: None,
                },
            ],
            previews: vec![Preview {
                port: 3000,
                path: "/".into(),
                title: None,
                at: 1,
            }],
        };
        let json = serde_json::to_string(&listing).unwrap_or_default();
        assert!(!json.contains("null"), "{json}");
        assert_eq!(
            serde_json::from_str::<PortListing>(&json).ok(),
            Some(listing)
        );
        assert_eq!(
            serde_json::from_str::<PortListing>(r#"{"ports":[]}"#).ok(),
            Some(PortListing::default())
        );
    }

    #[rstest]
    #[case::gone(410, "410 Gone")]
    #[case::not_found(404, "410 Gone")]
    #[case::refused(502, "502 Bad Gateway")]
    #[case::other(401, "502 Bad Gateway")]
    fn failures_answer_the_browser(#[case] status: u16, #[case] line: &str) {
        let page = problem_page(&failure(status));
        assert!(page.starts_with(&format!("HTTP/1.1 {line}\r\n")), "{page}");
        let (head, body) = page.split_once("\r\n\r\n").unwrap_or_default();
        assert!(
            head.contains(&format!("Content-Length: {}", body.len())),
            "{page}"
        );
    }

    /// A gateway that upgrades `/agents/a/forward/<port>` when the agent's
    /// header is there and echoes every frame, or answers `status` instead.
    // The handshake callback's error type is tungstenite's, not ours to shrink.
    #[allow(clippy::result_large_err)]
    async fn fake_gateway(status: Option<u16>) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap_or_else(|e| panic!("bind: {e}"));
        let port = listener.local_addr().map(|a| a.port()).unwrap_or_default();
        tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let check = |request: &Request, response: Response| {
                        let authorized = request
                            .headers()
                            .get("authorization")
                            .is_some_and(|value| value == "Bearer t");
                        let path_ok = request.uri().path() == "/agents/a/forward/5173";
                        match status {
                            None if authorized && path_ok => Ok(response),
                            _ => {
                                let mut refused =
                                    tokio_tungstenite::tungstenite::http::Response::new(None);
                                *refused.status_mut() =
                                    tokio_tungstenite::tungstenite::http::StatusCode::from_u16(
                                        status.unwrap_or(403),
                                    )
                                    .unwrap_or_default();
                                Err(refused)
                            }
                        }
                    };
                    let Ok(mut ws) = tokio_tungstenite::accept_hdr_async(socket, check).await
                    else {
                        return;
                    };
                    while let Some(Ok(message)) = ws.next().await {
                        if message.is_close() {
                            break;
                        }
                        if ws.send(message).await.is_err() {
                            break;
                        }
                    }
                });
            }
        });
        port
    }

    #[tokio::test]
    async fn a_tunnel_carries_bytes_both_ways() {
        let gateway = fake_gateway(None).await;
        let endpoint = endpoint(&format!("ws://127.0.0.1:{gateway}/agents/a/acp"));
        let tunnel = Tunnel::open(endpoint, 5173)
            .await
            .unwrap_or_else(|e| panic!("open: {e}"));
        let mut socket = TcpStream::connect(("127.0.0.1", tunnel.local_port()))
            .await
            .unwrap_or_else(|e| panic!("connect: {e}"));
        socket
            .write_all(b"GET / HTTP/1.1\r\n\r\n")
            .await
            .unwrap_or_else(|e| panic!("write: {e}"));
        let mut echoed = vec![0u8; 18];
        socket
            .read_exact(&mut echoed)
            .await
            .unwrap_or_else(|e| panic!("read: {e}"));
        assert_eq!(echoed, b"GET / HTTP/1.1\r\n\r\n");
    }

    #[tokio::test]
    async fn a_tunnel_tells_the_browser_why_it_cannot_reach_the_port() {
        let gateway = fake_gateway(Some(410)).await;
        let endpoint = endpoint(&format!("ws://127.0.0.1:{gateway}/agents/a/acp"));
        let tunnel = Tunnel::open(endpoint, 5173)
            .await
            .unwrap_or_else(|e| panic!("open: {e}"));
        let mut socket = TcpStream::connect(("127.0.0.1", tunnel.local_port()))
            .await
            .unwrap_or_else(|e| panic!("connect: {e}"));
        socket
            .write_all(b"GET / HTTP/1.1\r\n\r\n")
            .await
            .unwrap_or_else(|e| panic!("write: {e}"));
        let mut answer = String::new();
        socket
            .read_to_string(&mut answer)
            .await
            .unwrap_or_else(|e| panic!("read: {e}"));
        assert!(answer.starts_with("HTTP/1.1 410 Gone\r\n"), "{answer}");
        assert!(answer.contains("not running"), "{answer}");
    }

    #[tokio::test]
    async fn a_tunnel_takes_another_port_when_its_own_is_taken() {
        let taken = TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap_or_else(|e| panic!("bind: {e}"));
        let port = taken.local_addr().map(|a| a.port()).unwrap_or_default();
        let tunnel = Tunnel::open(endpoint("ws://127.0.0.1:9/agents/a/acp"), port)
            .await
            .unwrap_or_else(|e| panic!("open: {e}"));
        assert_ne!(tunnel.local_port(), port);
        assert_ne!(tunnel.local_port(), 0);
    }
}
