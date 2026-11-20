//! A shell in the checkout an agent works in, for the terminal pane.
//!
//! ACP's `terminal/*` goes the other way (the agent asks the client to run
//! something), so ukai opens the user's own shell itself, as it runs `!`
//! commands ([`crate::checkout`]). A local agent's checkout is the project
//! root: the shell runs here, on a pseudo-terminal. A gateway agent's is in
//! its pod: the relay there serves [`TERMINAL_PATH`] with the same [`open`],
//! and the gateway passes the WebSocket through per workspace, as it does a
//! port's (`crate::ports`).
//!
//! On that WebSocket, binary frames are the terminal's bytes both ways, and
//! text frames are [`Control`] messages: the client resizes the terminal,
//! and the server says the shell exited before it closes. The size it starts
//! with is in the query (`?cols=&rows=`).

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use futures::{SinkExt, StreamExt};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use ukai_config::{AgentSpec, Source};

use crate::gateway::GatewayAgent;
use crate::ports::{UpgradeError, upgrade};
use crate::reports::endpoint_base;
use crate::{PaneWorkspace, ResolveError};

/// Suffix of an agent's ACP endpoint (and the relay's path) that upgrades to
/// a WebSocket carrying a shell in the workspace's checkout.
pub const TERMINAL_PATH: &str = "/terminal";

/// Bytes read from the terminal at a time.
const CHUNK: usize = 16 * 1024;

/// A terminal's size in character cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Size {
    pub cols: u16,
    pub rows: u16,
}

impl Default for Size {
    fn default() -> Self {
        Self { cols: 80, rows: 24 }
    }
}

impl Size {
    /// The size kept within what a terminal can be: at least one cell, and
    /// not so large that a bad request costs the shell its memory.
    pub fn clamped(self) -> Self {
        Self {
            cols: self.cols.clamp(1, 1000),
            rows: self.rows.clamp(1, 500),
        }
    }

    fn pty(self) -> PtySize {
        PtySize {
            rows: self.rows,
            cols: self.cols,
            pixel_width: 0,
            pixel_height: 0,
        }
    }
}

/// A text frame on a terminal's WebSocket.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Control {
    /// From the client: the terminal is now this size.
    Resize(Size),
    /// From the server: the shell exited, with this code when it has one.
    Exit { code: Option<i32> },
}

/// What a terminal does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalEvent {
    /// Bytes for the screen.
    Output(Vec<u8>),
    /// The shell exited; nothing follows.
    Exited(Option<i32>),
    /// The terminal could not start or was cut off; nothing follows.
    Failed(String),
}

enum Input {
    Bytes(Vec<u8>),
    Resize(Size),
}

/// The keyboard end of an open terminal. Dropping it ends the shell (a
/// local one is hung up on; a workspace's is closed, which the relay does
/// the same with).
#[derive(Debug, Clone)]
pub struct Terminal {
    input: mpsc::UnboundedSender<Input>,
}

impl std::fmt::Debug for Input {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Bytes(bytes) => write!(f, "Bytes({})", bytes.len()),
            Self::Resize(size) => write!(f, "Resize({size:?})"),
        }
    }
}

impl Terminal {
    /// Type `bytes` into the terminal.
    pub fn write(&self, bytes: Vec<u8>) {
        let _ = self.input.send(Input::Bytes(bytes));
    }

    pub fn resize(&self, size: Size) {
        let _ = self.input.send(Input::Resize(size.clamped()));
    }
}

/// Where a terminal's shell runs.
#[derive(Debug, Clone)]
pub enum TerminalPlace {
    /// In this directory, on this machine.
    Local(PathBuf),
    /// In a gateway agent's workspace, in its pod.
    Gateway(TerminalEndpoint),
}

impl TerminalPlace {
    /// Where a terminal of `spec`'s checkout runs: in `root` for a local
    /// agent, in `workspace` for a gateway agent. `None` for a gateway URL
    /// that names no agent.
    pub fn resolve(
        spec: &AgentSpec,
        lookup: impl Fn(Source, &str) -> Option<String> + Copy,
        workspace: Option<&PaneWorkspace>,
        root: &Path,
    ) -> Result<Option<Self>, ResolveError> {
        if !matches!(spec, AgentSpec::Gateway { .. }) {
            return Ok(Some(Self::Local(root.to_owned())));
        }
        Ok(TerminalEndpoint::resolve(spec, lookup, workspace)?.map(Self::Gateway))
    }
}

/// Start a shell at `place`, `size` large. Its output, and how it ends,
/// arrive on the receiver; the [`Terminal`] types into it.
pub fn open(
    place: TerminalPlace,
    size: Size,
) -> (Terminal, mpsc::UnboundedReceiver<TerminalEvent>) {
    let (input_tx, input) = mpsc::unbounded_channel();
    let (events, events_rx) = mpsc::unbounded_channel();
    let size = size.clamped();
    tokio::spawn(async move {
        let end = match place {
            TerminalPlace::Local(root) => run_local(&root, size, input, &events).await,
            TerminalPlace::Gateway(endpoint) => run_remote(&endpoint, size, input, &events).await,
        };
        if let Some(end) = end {
            let _ = events.send(end);
        }
    });
    (Terminal { input: input_tx }, events_rx)
}

/// The user's shell, as a login shell, in `root`.
fn shell(root: &Path) -> CommandBuilder {
    let mut command = CommandBuilder::new_default_prog();
    command.cwd(root);
    command.env("TERM", "xterm-256color");
    command.env("COLORTERM", "truecolor");
    command.env("TERM_PROGRAM", "ukai");
    command
}

/// Run a shell on a pseudo-terminal here until it exits or the [`Terminal`]
/// is dropped. The pty's ends block, so each has a thread of its own.
async fn run_local(
    root: &Path,
    size: Size,
    mut input: mpsc::UnboundedReceiver<Input>,
    events: &mpsc::UnboundedSender<TerminalEvent>,
) -> Option<TerminalEvent> {
    let pair = match native_pty_system().openpty(size.pty()) {
        Ok(pair) => pair,
        Err(error) => {
            return Some(TerminalEvent::Failed(format!(
                "cannot open a terminal: {error}"
            )));
        }
    };
    let mut child = match pair.slave.spawn_command(shell(root)) {
        Ok(child) => child,
        Err(error) => {
            return Some(TerminalEvent::Failed(format!(
                "cannot start a shell: {error}"
            )));
        }
    };
    // Only the shell holds the terminal's end now, so the output ends with it.
    drop(pair.slave);
    let master = pair.master;
    let (mut reader, mut writer) = match (master.try_clone_reader(), master.take_writer()) {
        (Ok(reader), Ok(writer)) => (reader, writer),
        (Err(error), _) | (_, Err(error)) => {
            let _ = child.kill();
            return Some(TerminalEvent::Failed(format!(
                "cannot use the terminal: {error}"
            )));
        }
    };
    let mut killer = child.clone_killer();
    let (output_tx, mut output) = mpsc::unbounded_channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut buffer = vec![0u8; CHUNK];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(read) => {
                    if output_tx.send(buffer[..read].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });
    let (keys, typed) = std::sync::mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        for bytes in typed {
            if writer
                .write_all(&bytes)
                .and_then(|()| writer.flush())
                .is_err()
            {
                break;
            }
        }
    });
    let exit = tokio::task::spawn_blocking(move || child.wait());
    loop {
        tokio::select! {
            command = input.recv() => match command {
                Some(Input::Bytes(bytes)) => {
                    let _ = keys.send(bytes);
                }
                Some(Input::Resize(size)) => {
                    if let Err(error) = master.resize(size.pty()) {
                        tracing::debug!(%error, "cannot resize a terminal");
                    }
                }
                None => {
                    // No one is looking: hang up.
                    let _ = killer.kill();
                    return None;
                }
            },
            bytes = output.recv() => match bytes {
                Some(bytes) => {
                    if events.send(TerminalEvent::Output(bytes)).is_err() {
                        let _ = killer.kill();
                        return None;
                    }
                }
                None => break,
            },
        }
    }
    let code = exit
        .await
        .ok()
        .and_then(Result::ok)
        .and_then(|status| i32::try_from(status.exit_code()).ok());
    Some(TerminalEvent::Exited(code))
}

/// Carry a workspace's terminal over its WebSocket until the shell exits,
/// the connection ends, or the [`Terminal`] is dropped.
async fn run_remote(
    endpoint: &TerminalEndpoint,
    size: Size,
    mut input: mpsc::UnboundedReceiver<Input>,
    events: &mpsc::UnboundedSender<TerminalEvent>,
) -> Option<TerminalEvent> {
    let ws = match endpoint.connect(size).await {
        Ok(ws) => ws,
        Err(error) => return Some(TerminalEvent::Failed(error.to_string())),
    };
    let (mut tx, mut rx) = ws.split();
    let mut exited = None;
    loop {
        tokio::select! {
            command = input.recv() => {
                let message = match command {
                    Some(Input::Bytes(bytes)) => Message::binary(bytes),
                    Some(Input::Resize(size)) => match serde_json::to_string(&Control::Resize(size)) {
                        Ok(text) => Message::text(text),
                        Err(_) => continue,
                    },
                    None => {
                        let _ = tx.send(Message::Close(None)).await;
                        return None;
                    }
                };
                if let Err(error) = tx.send(message).await {
                    return Some(TerminalEvent::Failed(format!(
                        "lost the workspace's terminal: {error}"
                    )));
                }
            }
            message = rx.next() => match message {
                Some(Ok(Message::Binary(bytes))) => {
                    if events.send(TerminalEvent::Output(bytes.to_vec())).is_err() {
                        let _ = tx.send(Message::Close(None)).await;
                        return None;
                    }
                }
                Some(Ok(Message::Text(text))) => {
                    if let Ok(Control::Exit { code }) = serde_json::from_str(&text) {
                        exited = Some(code);
                    }
                }
                Some(Ok(Message::Close(_))) | None => break,
                Some(Ok(_)) => {}
                Some(Err(error)) => {
                    return Some(TerminalEvent::Failed(format!(
                        "lost the workspace's terminal: {error}"
                    )));
                }
            },
        }
    }
    Some(match exited {
        Some(code) => TerminalEvent::Exited(code),
        None => TerminalEvent::Failed("the workspace closed the terminal".to_owned()),
    })
}

#[derive(Debug, thiserror::Error)]
pub enum TerminalError {
    #[error("cannot reach the gateway: {0}")]
    Unreachable(String),
    #[error("the agent's workspace is not running. Open the agent to start it.")]
    NotRunning,
    #[error("the gateway answered {0}")]
    Status(u16),
}

/// Where a gateway agent's workspace opens a terminal: its endpoint, with the
/// agent's headers and the pane's workspace among them.
#[derive(Debug, Clone)]
pub struct TerminalEndpoint {
    /// No total timeout, which would cut a terminal that outlives it.
    client: reqwest::Client,
    base: String,
}

impl TerminalEndpoint {
    /// The terminal of `workspace` for `spec`, or `None` when the agent is
    /// not a gateway agent.
    pub fn resolve(
        spec: &AgentSpec,
        lookup: impl Fn(Source, &str) -> Option<String> + Copy,
        workspace: Option<&PaneWorkspace>,
    ) -> Result<Option<Self>, ResolveError> {
        let Some(agent) = GatewayAgent::resolve(spec, lookup)? else {
            return Ok(None);
        };
        let Some(base) = endpoint_base(&agent.gateway.url, TERMINAL_PATH) else {
            return Ok(None);
        };
        let client = agent
            .gateway
            .workspace_client(workspace, |builder| builder)?;
        Ok(Some(Self { client, base }))
    }

    fn url(&self, size: Size) -> String {
        format!("{}?cols={}&rows={}", self.base, size.cols, size.rows)
    }

    /// A shell in the workspace's checkout, `size` large, as a WebSocket.
    pub async fn connect(
        &self,
        size: Size,
    ) -> Result<WebSocketStream<reqwest::Upgraded>, TerminalError> {
        upgrade(&self.client, &self.url(size))
            .await
            .map_err(|error| match error {
                UpgradeError::Unreachable(why) => TerminalError::Unreachable(why),
                UpgradeError::Status(404 | 410) => TerminalError::NotRunning,
                UpgradeError::Status(status) => TerminalError::Status(status),
            })
    }
}

#[cfg(test)]
mod tests;
