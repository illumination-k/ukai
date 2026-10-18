//! The ports of the relay's pod: what listens there, and a way in.
//!
//! An agent starts servers in its pod — a web app's dev server, a docs
//! preview — that the user cannot reach, the pod being inside a cluster. The
//! relay serves, next to `/acp`, and `ukai-gateway` proxies per workspace:
//!
//! - `GET /ports` … the TCP ports something in the pod listens on (read from
//!   `/proc/net/tcp*`, so only on Linux; the relay's own is left out), and
//!   the previews the agent asked the user to look at through the MCP server
//!   ([`crate::mcp`]), which records them in a file the relay reads back.
//! - `GET /forward/<port>` … a WebSocket carrying one TCP connection to
//!   `localhost:<port>` in the pod, as binary frames. Connecting from inside
//!   reaches a server bound to `127.0.0.1` too, as most dev servers are. When
//!   nothing listens there the answer is 502, before any upgrade, so the
//!   client can tell that apart from a workspace that is gone.
//!
//! Both want the relay's token, like everything but `/health`.

use std::collections::{BTreeMap, HashMap};
use std::io::Write;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::Router;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path as UrlPath, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use futures::{SinkExt, StreamExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use ukai_acp::ports::{
    FORWARD_PATH, ListeningPort, PORTS_PATH, PREVIEWS_KEPT, PortListing, Preview,
};

/// What the routes need: the port the relay itself serves on, to leave out
/// of the listing, and the file the MCP server records previews in.
#[derive(Debug, Clone)]
pub struct Ports {
    pub own: u16,
    pub previews: PathBuf,
}

/// `GET /ports` and `GET /forward/{port}`.
pub fn router(ports: Ports) -> Router {
    Router::new()
        .route(PORTS_PATH, get(listing))
        .route(&format!("{FORWARD_PATH}/{{port}}"), get(forward))
        .with_state(ports)
}

async fn listing(State(ports): State<Ports>) -> Response {
    let listening = scan()
        .unwrap_or_default()
        .into_iter()
        .filter(|port| port.port != ports.own)
        .collect();
    axum::Json(PortListing {
        ports: listening,
        previews: previews(&ports.previews),
    })
    .into_response()
}

async fn forward(UrlPath(port): UrlPath<u16>, upgrade: WebSocketUpgrade) -> Response {
    match connect(port).await {
        Ok(tcp) => upgrade.on_upgrade(move |socket| splice(socket, tcp)),
        Err(error) => {
            tracing::debug!(port, %error, "nothing to forward to");
            (
                StatusCode::BAD_GATEWAY,
                format!("nothing listens on port {port} in the workspace"),
            )
                .into_response()
        }
    }
}

/// `localhost:port`, over IPv4 or else IPv6: a server bound to `::1` only is
/// as local as one on `127.0.0.1`.
async fn connect(port: u16) -> std::io::Result<TcpStream> {
    match TcpStream::connect((Ipv4Addr::LOCALHOST, port)).await {
        Ok(tcp) => Ok(tcp),
        Err(error) => TcpStream::connect((Ipv6Addr::LOCALHOST, port))
            .await
            .map_err(|_| error),
    }
}

/// Pass bytes between the client's WebSocket and the port until either side
/// ends, as the client's end of the tunnel does (`ukai_acp::ports::splice`).
async fn splice(socket: WebSocket, tcp: TcpStream) {
    let (mut ws_tx, mut ws_rx) = socket.split();
    let (mut tcp_rx, mut tcp_tx) = tcp.into_split();
    let up = async {
        let mut buffer = vec![0u8; 16 * 1024];
        loop {
            match tcp_rx.read(&mut buffer).await {
                Ok(0) | Err(_) => break,
                Ok(read) => {
                    let frame = Message::Binary(buffer[..read].to_vec().into());
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

/// What listens in this network namespace, lowest port first, with the
/// process holding each socket when it can be found. `None` where there is
/// no `/proc/net/tcp` to read, as on anything but Linux.
pub fn scan() -> Option<Vec<ListeningPort>> {
    let mut tables = String::new();
    let mut readable = false;
    for table in ["/proc/net/tcp", "/proc/net/tcp6"] {
        if let Ok(text) = std::fs::read_to_string(table) {
            readable = true;
            tables.push_str(&text);
            tables.push('\n');
        }
    }
    if !readable {
        return None;
    }
    let sockets = listening(&tables);
    let owners = socket_owners();
    let mut ports: BTreeMap<u16, Option<String>> = BTreeMap::new();
    for (port, inode) in sockets {
        let owner = owners.get(&inode).cloned();
        let entry = ports.entry(port).or_default();
        if entry.is_none() {
            *entry = owner;
        }
    }
    Some(
        ports
            .into_iter()
            .map(|(port, process)| ListeningPort { port, process })
            .collect(),
    )
}

/// The listening sockets in the text of `/proc/net/tcp` and `tcp6`: each
/// local port with the socket's inode. Lines that do not parse are skipped.
pub fn listening(tables: &str) -> Vec<(u16, u64)> {
    /// `st` of a socket in `LISTEN`.
    const LISTEN: &str = "0A";
    tables
        .lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            let local = fields.get(1)?;
            if *fields.get(3)? != LISTEN {
                return None;
            }
            let (_, port) = local.rsplit_once(':')?;
            let port = u16::from_str_radix(port, 16).ok()?;
            let inode = fields.get(9)?.parse().ok()?;
            Some((port, inode))
        })
        .collect()
}

/// The name of the process holding each socket inode, from the links in
/// `/proc/<pid>/fd`. Processes the relay may not look into are skipped.
fn socket_owners() -> HashMap<u64, String> {
    let mut owners = HashMap::new();
    let Ok(processes) = std::fs::read_dir("/proc") else {
        return owners;
    };
    for process in processes.flatten() {
        let pid = process.path();
        let numeric = process
            .file_name()
            .to_str()
            .is_some_and(|name| name.bytes().all(|b| b.is_ascii_digit()));
        if !numeric {
            continue;
        }
        let Ok(fds) = std::fs::read_dir(pid.join("fd")) else {
            continue;
        };
        let inodes: Vec<u64> = fds
            .flatten()
            .filter_map(|fd| std::fs::read_link(fd.path()).ok())
            .filter_map(|link| socket_inode(&link.to_string_lossy()))
            .collect();
        if inodes.is_empty() {
            continue;
        }
        let Ok(name) = std::fs::read_to_string(pid.join("comm")) else {
            continue;
        };
        for inode in inodes {
            owners
                .entry(inode)
                .or_insert_with(|| name.trim().to_owned());
        }
    }
    owners
}

/// The inode of a `/proc/<pid>/fd` link to a socket, `socket:[<inode>]`.
pub fn socket_inode(link: &str) -> Option<u64> {
    link.strip_prefix("socket:[")?
        .strip_suffix(']')?
        .parse()
        .ok()
}

/// The previews recorded in `file`, the last [`PREVIEWS_KEPT`] of them,
/// oldest first. A file that is missing or a line that does not parse holds
/// none: the agent can write to the file as well as the MCP server can.
pub fn previews(file: &Path) -> Vec<Preview> {
    let Ok(text) = std::fs::read_to_string(file) else {
        return Vec::new();
    };
    let all: Vec<Preview> = text
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    let skip = all.len().saturating_sub(PREVIEWS_KEPT);
    all.into_iter().skip(skip).collect()
}

/// Record that the agent asks the user to look at `port` from `path`, for
/// [`previews`] to list. One JSON line, appended.
pub fn record_preview(
    file: &Path,
    port: u16,
    path: String,
    title: Option<String>,
) -> std::io::Result<Preview> {
    let preview = Preview {
        port,
        path,
        title,
        at: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| {
                u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
            }),
    };
    let mut line = serde_json::to_vec(&preview)?;
    line.push(b'\n');
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(file)?
        .write_all(&line)?;
    Ok(preview)
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::Request;
    use rstest::rstest;
    use tokio_tungstenite::tungstenite;
    use tower::ServiceExt;

    use super::*;

    /// Two listeners (one over IPv6, both on 5173, and 8080), a connection
    /// that is not listening, and the header line.
    const TABLES: &str = "\
  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 0100007F:1435 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 4242 1 0000000000000000 100 0 0 10 0
   1: 0100007F:9C40 0100007F:1435 01 00000000:00000000 00:00000000 00000000  1000        0 4343 1 0000000000000000 20 4 30 10 -1
   0: 00000000000000000000000000000000:1F90 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 5151 1 0000000000000000 100 0 0 10 0
   1: 00000000000000000000000001000000:1435 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 5252 1 0000000000000000 100 0 0 10 0
";

    #[test]
    fn reads_the_listening_sockets() {
        assert_eq!(
            listening(TABLES),
            [(5173, 4242), (8080, 5151), (5173, 5252)]
        );
        assert!(listening("garbage\n\n").is_empty());
    }

    #[rstest]
    #[case::socket("socket:[4242]", Some(4242))]
    #[case::pipe("pipe:[4242]", None)]
    #[case::file("/workspace/ukai.toml", None)]
    #[case::not_a_number("socket:[x]", None)]
    fn reads_a_socket_link(#[case] link: &str, #[case] expected: Option<u64>) {
        assert_eq!(socket_inode(link), expected);
    }

    #[test]
    fn keeps_the_latest_previews() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
        let file = dir.path().join("previews.jsonl");
        assert!(previews(&file).is_empty());
        for port in 0..(PREVIEWS_KEPT as u16 + 2) {
            record_preview(&file, 3000 + port, "/".into(), None)
                .unwrap_or_else(|e| panic!("record: {e}"));
        }
        // A line the agent garbled is skipped, not the whole file.
        std::fs::OpenOptions::new()
            .append(true)
            .open(&file)
            .and_then(|mut f| f.write_all(b"{not json\n"))
            .unwrap_or_else(|e| panic!("append: {e}"));
        let kept = previews(&file);
        assert_eq!(kept.len(), PREVIEWS_KEPT);
        assert_eq!(kept.first().map(|p| p.port), Some(3002));
        assert_eq!(
            kept.last().map(|p| p.port),
            Some(3000 + PREVIEWS_KEPT as u16 + 1)
        );
        assert!(kept.windows(2).all(|pair| pair[0].at <= pair[1].at));
    }

    #[tokio::test]
    async fn lists_ports_and_previews() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
        let file = dir.path().join("previews.jsonl");
        record_preview(&file, 5173, "/docs".into(), Some("Docs".into()))
            .unwrap_or_else(|e| panic!("record: {e}"));
        let response = router(Ports {
            own: 7070,
            previews: file,
        })
        .oneshot(
            Request::get(PORTS_PATH)
                .body(Body::empty())
                .unwrap_or_else(|_| Request::new(Body::empty())),
        )
        .await
        .unwrap_or_else(|e| panic!("request: {e}"));
        assert_eq!(response.status().as_u16(), 200);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap_or_default();
        let listing: PortListing = serde_json::from_slice(&body).unwrap_or_default();
        assert_eq!(listing.previews.len(), 1);
        assert_eq!(listing.previews[0].path, "/docs");
        assert!(listing.ports.iter().all(|port| port.port != 7070));
    }

    async fn serve(router: Router) -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap_or_else(|e| panic!("bind: {e}"));
        let port = listener.local_addr().map(|a| a.port()).unwrap_or_default();
        tokio::spawn(async move { axum::serve(listener, router).await });
        port
    }

    fn ports() -> Ports {
        Ports {
            own: 7070,
            previews: PathBuf::from("/nonexistent/previews.jsonl"),
        }
    }

    #[tokio::test]
    async fn forwards_a_connection_to_a_local_port() {
        // A server bound to 127.0.0.1 only, as dev servers usually are.
        let upstream = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap_or_else(|e| panic!("bind: {e}"));
        let upstream_port = upstream.local_addr().map(|a| a.port()).unwrap_or_default();
        tokio::spawn(async move {
            if let Ok((mut socket, _)) = upstream.accept().await {
                let mut request = [0u8; 5];
                if socket.read_exact(&mut request).await.is_ok() {
                    let _ = socket.write_all(b"pong:").await;
                    let _ = socket.write_all(&request).await;
                }
            }
        });
        let relay = serve(router(ports())).await;
        let (mut ws, _) = tokio_tungstenite::connect_async(format!(
            "ws://127.0.0.1:{relay}/forward/{upstream_port}"
        ))
        .await
        .unwrap_or_else(|e| panic!("connect: {e}"));
        ws.send(tungstenite::Message::binary(b"hello".to_vec()))
            .await
            .unwrap_or_else(|e| panic!("send: {e}"));
        let mut received = Vec::new();
        while received.len() < 10 {
            match ws.next().await {
                Some(Ok(tungstenite::Message::Binary(bytes))) => received.extend_from_slice(&bytes),
                Some(Ok(_)) => {}
                other => panic!("unexpected {other:?}"),
            }
        }
        assert_eq!(received, b"pong:hello");
    }

    #[tokio::test]
    async fn refuses_a_port_nothing_listens_on() {
        // Take a port and let it go, so nothing is on it.
        let free = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .and_then(|l| l.local_addr())
            .map(|a| a.port())
            .unwrap_or_else(|e| panic!("bind: {e}"));
        let relay = serve(router(ports())).await;
        let refused =
            tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{relay}/forward/{free}"))
                .await;
        match refused {
            Err(tungstenite::Error::Http(response)) => assert_eq!(response.status().as_u16(), 502),
            other => panic!("expected a 502, got {:?}", other.map(|_| ())),
        }
    }
}
