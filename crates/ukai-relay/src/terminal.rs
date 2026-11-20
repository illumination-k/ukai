//! A shell in the relay's checkout, for a terminal pane of a gateway agent's
//! workspace.
//!
//! `GET /terminal?cols=&rows=` upgrades to a WebSocket that carries a login
//! shell started in the checkout on a pseudo-terminal, with the same
//! [`ukai_acp::terminal::open`] the app uses for a local agent's: binary
//! frames are the terminal's bytes both ways, text frames
//! [`Control`] messages (the client resizes; the relay says the shell exited,
//! then closes). Closing the WebSocket hangs up on the shell. Like
//! everything but `/health`, it wants the relay's token.
//!
//! While a terminal is open the relay counts as busy, so it does not exit
//! idle under someone typing ([`Terminals::open`]).

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::Router;
use axum::extract::rejection::QueryRejection;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::response::Response;
use axum::routing::get;
use futures::{SinkExt, StreamExt};
use ukai_acp::terminal::{self, Control, Size, TERMINAL_PATH, TerminalEvent, TerminalPlace};

/// The terminals open in the relay's checkout.
#[derive(Debug, Clone, Default)]
pub struct Terminals {
    open: Arc<AtomicUsize>,
}

impl Terminals {
    /// How many terminals are open now.
    pub fn open(&self) -> usize {
        self.open.load(Ordering::Relaxed)
    }
}

/// Counts a terminal as open for as long as it lives.
struct Open(Arc<AtomicUsize>);

impl Open {
    fn new(terminals: &Terminals) -> Self {
        terminals.open.fetch_add(1, Ordering::Relaxed);
        Self(terminals.open.clone())
    }
}

impl Drop for Open {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

#[derive(Clone)]
struct Shared {
    root: PathBuf,
    terminals: Terminals,
}

/// `GET /terminal`.
pub fn router(root: PathBuf, terminals: Terminals) -> Router {
    Router::new()
        .route(TERMINAL_PATH, get(upgrade))
        .with_state(Shared { root, terminals })
}

async fn upgrade(
    State(shared): State<Shared>,
    size: Result<Query<Size>, QueryRejection>,
    upgrade: WebSocketUpgrade,
) -> Response {
    // A size that does not parse is the default one: the client resizes.
    let size = size.map(|Query(size)| size).unwrap_or_default();
    upgrade.on_upgrade(move |socket| serve(socket, shared, size))
}

async fn serve(socket: WebSocket, shared: Shared, size: Size) {
    let _open = Open::new(&shared.terminals);
    let (terminal, mut events) = terminal::open(TerminalPlace::Local(shared.root), size);
    let (mut tx, mut rx) = socket.split();
    loop {
        tokio::select! {
            message = rx.next() => match message {
                Some(Ok(Message::Binary(bytes))) => terminal.write(bytes.to_vec()),
                Some(Ok(Message::Text(text))) => {
                    if let Ok(Control::Resize(size)) = serde_json::from_str(&text) {
                        terminal.resize(size);
                    }
                }
                Some(Ok(Message::Close(_)) | Err(_)) | None => return,
                Some(Ok(_)) => {}
            },
            event = events.recv() => match event {
                Some(TerminalEvent::Output(bytes)) => {
                    if tx.send(Message::Binary(bytes.into())).await.is_err() {
                        return;
                    }
                }
                Some(TerminalEvent::Exited(code)) => {
                    if let Ok(text) = serde_json::to_string(&Control::Exit { code }) {
                        let _ = tx.send(Message::Text(text.into())).await;
                    }
                    break;
                }
                Some(TerminalEvent::Failed(why)) => {
                    tracing::warn!(%why, "a terminal failed");
                    break;
                }
                None => break,
            },
        }
    }
    let _ = tx.send(Message::Close(None)).await;
}

#[cfg(all(test, unix))]
mod tests {
    use std::time::Duration;

    use tokio_tungstenite::tungstenite;

    use super::*;

    async fn serve_router(router: Router) -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap_or_else(|e| panic!("bind: {e}"));
        let port = listener.local_addr().map(|a| a.port()).unwrap_or_default();
        tokio::spawn(async move { axum::serve(listener, router).await });
        port
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_terminal_runs_a_shell_in_the_checkout() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
        std::fs::write(dir.path().join("in-the-checkout"), b"")
            .unwrap_or_else(|e| panic!("write: {e}"));
        let terminals = Terminals::default();
        let relay = serve_router(router(dir.path().to_owned(), terminals.clone())).await;
        let (mut ws, _) = tokio_tungstenite::connect_async(format!(
            "ws://127.0.0.1:{relay}/terminal?cols=100&rows=30"
        ))
        .await
        .unwrap_or_else(|e| panic!("connect: {e}"));
        let resize = serde_json::to_string(&Control::Resize(Size { cols: 90, rows: 20 }))
            .unwrap_or_default();
        ws.send(tungstenite::Message::text(resize))
            .await
            .unwrap_or_else(|e| panic!("send: {e}"));
        ws.send(tungstenite::Message::binary(
            b"ls; stty size; exit 5\n".to_vec(),
        ))
        .await
        .unwrap_or_else(|e| panic!("send: {e}"));
        let mut output = Vec::new();
        let mut exit = None;
        let reading = async {
            while let Some(Ok(message)) = ws.next().await {
                match message {
                    tungstenite::Message::Binary(bytes) => output.extend_from_slice(&bytes),
                    tungstenite::Message::Text(text) => {
                        exit = serde_json::from_str::<Control>(&text).ok();
                    }
                    tungstenite::Message::Close(_) => break,
                    _ => {}
                }
            }
        };
        tokio::time::timeout(Duration::from_secs(20), reading)
            .await
            .unwrap_or_else(|_| panic!("timed out: {}", String::from_utf8_lossy(&output)));
        let output = String::from_utf8_lossy(&output);
        assert!(output.contains("in-the-checkout"), "{output}");
        assert!(output.contains("20 90"), "{output}");
        assert_eq!(exit, Some(Control::Exit { code: Some(5) }));
        // The count drops once the connection is served.
        for _ in 0..50 {
            if terminals.open() == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(terminals.open(), 0);
    }
}
