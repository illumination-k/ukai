//! One ACP session, driven from a background task.
//!
//! The UI talks to a session through two channels: [`SessionCommand`]s go in
//! through a [`SessionHandle`], [`SessionEvent`]s come out. Neither side sees
//! ACP schema types, so protocol changes stop at this module.
//!
//! Everything the agent sends is handled by connection-level callbacks that
//! only translate and forward, so the SDK's dispatch loop is never held
//! waiting on the user. A permission request parks its responder with the
//! command loop until the user answers.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{
    CancelNotification, ClientCapabilities, ContentBlock, ContentChunk, FileSystemCapabilities,
    Implementation, InitializeRequest, LoadSessionRequest, NewSessionRequest, PlanEntryStatus,
    PromptRequest, ReadTextFileRequest, ReadTextFileResponse, RequestPermissionOutcome,
    RequestPermissionRequest, RequestPermissionResponse, ResumeSessionRequest,
    SelectedPermissionOutcome, SessionId, SessionNotification, SessionUpdate, ToolCallStatus,
    WriteTextFileRequest, WriteTextFileResponse,
};
use agent_client_protocol::{Agent, Client, ConnectTo, ConnectionTo, Responder};
use tokio::sync::mpsc;

use crate::Transport;
use crate::fs::FsRoot;

/// The receiving end of a session's events.
pub type SessionEvents = mpsc::UnboundedReceiver<SessionEvent>;

/// Identifies one pending permission request within a session.
pub type PermissionId = u64;

/// Something the agent did, translated for the UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionEvent {
    /// `initialize` and session setup succeeded; prompts can be sent.
    Ready {
        agent: Option<String>,
        /// Pass back as [`SessionOptions::restore`] to continue this session later.
        session_id: String,
        restore: RestoreOutcome,
    },
    /// A chunk of a user message the agent replays while loading a session.
    UserMessage { text: String },
    /// A chunk of the agent's reply; consecutive chunks form one message.
    AgentMessage { text: String },
    /// A chunk of the agent's visible reasoning.
    AgentThought { text: String },
    ToolCall {
        id: String,
        title: String,
        kind: String,
        status: ToolStatus,
    },
    ToolCallUpdate {
        id: String,
        title: Option<String>,
        status: Option<ToolStatus>,
    },
    /// The agent's current plan, replacing any earlier one.
    Plan { entries: Vec<PlanItem> },
    /// Answer with [`SessionHandle::resolve_permission`].
    PermissionRequested {
        id: PermissionId,
        title: String,
        options: Vec<PermissionChoice>,
    },
    /// The agent wrote a file through `fs/write_text_file`.
    FileWritten { path: PathBuf },
    /// The prompt turn finished, e.g. with `end_turn` or `cancelled`.
    TurnEnded { stop_reason: String },
    /// The agent rejected the prompt; the session is still usable.
    PromptFailed { message: String },
    /// The connection is gone; no more events follow.
    Closed { error: Option<String> },
}

/// How a session came to be, relative to [`SessionOptions::restore`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestoreOutcome {
    /// No restore was asked for; a new session was created.
    Fresh,
    /// `session/load`: the agent replayed the history before `Ready`.
    Replayed,
    /// `session/resume`: the session continues, but its history was not replayed.
    Resumed,
    /// The agent can neither load nor resume, so a new session was created.
    Unsupported,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolStatus {
    Pending,
    InProgress,
    Completed,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanItem {
    pub content: String,
    pub done: bool,
    pub active: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionChoice {
    pub id: String,
    pub name: String,
    /// `allow_once`, `allow_always`, `reject_once`, or `reject_always`.
    pub kind: String,
}

#[derive(Debug)]
enum SessionCommand {
    Prompt(String),
    Cancel,
    ResolvePermission {
        id: PermissionId,
        option: Option<String>,
    },
}

/// Sends commands to a running session. Dropping every handle ends it.
#[derive(Debug, Clone)]
pub struct SessionHandle {
    commands: mpsc::UnboundedSender<SessionCommand>,
}

impl SessionHandle {
    /// Returns `false` when the session has already ended.
    pub fn prompt(&self, text: impl Into<String>) -> bool {
        self.send(SessionCommand::Prompt(text.into()))
    }

    /// Cancel the running turn; pending permission requests are answered as cancelled.
    pub fn cancel(&self) -> bool {
        self.send(SessionCommand::Cancel)
    }

    /// Answer a [`SessionEvent::PermissionRequested`]; `None` declines without choosing.
    pub fn resolve_permission(&self, id: PermissionId, option: Option<String>) -> bool {
        self.send(SessionCommand::ResolvePermission { id, option })
    }

    fn send(&self, command: SessionCommand) -> bool {
        self.commands.send(command).is_ok()
    }
}

#[derive(Debug, Clone)]
pub struct SessionOptions {
    /// Working directory announced in `session/new`.
    pub cwd: PathBuf,
    /// Where `fs/*` requests are allowed to read and write.
    pub fs_root: FsRoot,
    /// Session id from an earlier [`SessionEvent::Ready`] to continue instead
    /// of starting a new session.
    pub restore: Option<String>,
}

/// Start a session on the current tokio runtime.
pub fn spawn_session(
    transport: Transport,
    options: SessionOptions,
) -> (SessionHandle, SessionEvents) {
    match transport {
        Transport::Remote(client) => spawn_on(client, options),
        Transport::Local(agent) => spawn_on(agent, options),
    }
}

/// [`spawn_session`] over any ACP transport; tests use an in-memory channel.
pub(crate) fn spawn_on<T: ConnectTo<Client> + Send + 'static>(
    transport: T,
    options: SessionOptions,
) -> (SessionHandle, SessionEvents) {
    let (command_tx, command_rx) = mpsc::unbounded_channel();
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let events = event_tx.clone();
        let result = run_session(transport, options, events, command_rx).await;
        if let Err(error) = &result {
            tracing::warn!(%error, "session ended with an error");
        }
        let _ = event_tx.send(SessionEvent::Closed {
            error: result.err().map(|error| error.to_string()),
        });
    });
    (
        SessionHandle {
            commands: command_tx,
        },
        event_rx,
    )
}

type Events = mpsc::UnboundedSender<SessionEvent>;

/// Generous enough for `npx` to fetch an adapter on first run.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(120);
type Parked = (PermissionId, Responder<RequestPermissionResponse>);

async fn run_session<T: ConnectTo<Client> + 'static>(
    transport: T,
    options: SessionOptions,
    events: Events,
    mut commands: mpsc::UnboundedReceiver<SessionCommand>,
) -> Result<(), agent_client_protocol::Error> {
    let (park_tx, mut park_rx) = mpsc::unbounded_channel::<Parked>();
    let next_permission = Arc::new(AtomicU64::new(0));
    let SessionOptions {
        cwd,
        fs_root,
        restore,
    } = options;
    let replaying = Arc::new(AtomicBool::new(false));

    Client
        .builder()
        .name("ukai")
        .on_receive_notification(
            {
                let events = events.clone();
                let replaying = replaying.clone();
                async move |notification: SessionNotification, _cx| {
                    let replaying = replaying.load(Ordering::SeqCst);
                    if let Some(event) = translate_update(notification.update, replaying) {
                        let _ = events.send(event);
                    }
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            {
                let events = events.clone();
                async move |request: RequestPermissionRequest, responder, _cx| {
                    let id = next_permission.fetch_add(1, Ordering::Relaxed);
                    let fields = request.tool_call.fields;
                    let title = fields
                        .title
                        .unwrap_or_else(|| request.tool_call.tool_call_id.0.to_string());
                    let options = request
                        .options
                        .into_iter()
                        .map(|option| PermissionChoice {
                            id: option.option_id.0.to_string(),
                            name: option.name,
                            kind: serde_label(&option.kind),
                        })
                        .collect();
                    let _ = events.send(SessionEvent::PermissionRequested { id, title, options });
                    // If the command loop is gone the responder drops unanswered,
                    // which is fine: the connection is closing anyway.
                    let _ = park_tx.send((id, responder));
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let fs_root = fs_root.clone();
                async move |request: ReadTextFileRequest, responder, _cx| match fs_root.read(
                    &request.path,
                    request.line,
                    request.limit,
                ) {
                    Ok(content) => responder.respond(ReadTextFileResponse::new(content)),
                    Err(error) => responder.respond_with_error(fs_error(&error)),
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let events = events.clone();
                async move |request: WriteTextFileRequest, responder, _cx| match fs_root
                    .write(&request.path, &request.content)
                {
                    Ok(path) => {
                        let _ = events.send(SessionEvent::FileWritten { path });
                        responder.respond(WriteTextFileResponse::new())
                    }
                    Err(error) => responder.respond_with_error(fs_error(&error)),
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(transport, async |cx: ConnectionTo<Agent>| {
            let capabilities = ClientCapabilities::new().fs(FileSystemCapabilities::new()
                .read_text_file(true)
                .write_text_file(true));
            let handshake = async {
                let init = cx
                    .send_request(
                        InitializeRequest::new(ProtocolVersion::V1)
                            .client_capabilities(capabilities)
                            .client_info(Implementation::new("ukai", env!("CARGO_PKG_VERSION"))),
                    )
                    .block_task()
                    .await?;
                let can_load = init.agent_capabilities.load_session;
                let can_resume = init
                    .agent_capabilities
                    .session_capabilities
                    .resume
                    .is_some();
                let (session_id, outcome) = match restore {
                    Some(id) if can_load => {
                        // History arrives as session/update notifications before
                        // the response; user turns are only forwarded meanwhile.
                        replaying.store(true, Ordering::SeqCst);
                        let loaded = cx
                            .send_request(LoadSessionRequest::new(id.clone(), cwd.clone()))
                            .block_task()
                            .await;
                        replaying.store(false, Ordering::SeqCst);
                        loaded?;
                        (SessionId::new(id), RestoreOutcome::Replayed)
                    }
                    Some(id) if can_resume => {
                        cx.send_request(ResumeSessionRequest::new(id.clone(), cwd.clone()))
                            .block_task()
                            .await?;
                        (SessionId::new(id), RestoreOutcome::Resumed)
                    }
                    restore => {
                        let session = cx
                            .send_request(NewSessionRequest::new(cwd.clone()))
                            .block_task()
                            .await?;
                        let outcome = if restore.is_some() {
                            RestoreOutcome::Unsupported
                        } else {
                            RestoreOutcome::Fresh
                        };
                        (session.session_id, outcome)
                    }
                };
                Ok::<_, agent_client_protocol::Error>((init, session_id, outcome))
            };
            // An agent that never answers (e.g. stuck on expired credentials)
            // would otherwise leave the pane connecting forever.
            let (init, session_id, restore) = tokio::time::timeout(HANDSHAKE_TIMEOUT, handshake)
                .await
                .map_err(|_| {
                    agent_client_protocol::util::internal_error(format!(
                        "agent did not finish initialize and session setup within {}s",
                        HANDSHAKE_TIMEOUT.as_secs()
                    ))
                })??;
            let _ = events.send(SessionEvent::Ready {
                agent: init.agent_info.map(|info| info.title.unwrap_or(info.name)),
                session_id: session_id.0.to_string(),
                restore,
            });

            let mut parked: HashMap<PermissionId, Responder<RequestPermissionResponse>> =
                HashMap::new();
            loop {
                tokio::select! {
                    Some((id, responder)) = park_rx.recv() => {
                        parked.insert(id, responder);
                    }
                    command = commands.recv() => {
                        // A permission request parked an instant ago must be
                        // visible to the command that answers it.
                        while let Ok((id, responder)) = park_rx.try_recv() {
                            parked.insert(id, responder);
                        }
                        match command {
                            None => break,
                            Some(SessionCommand::Prompt(text)) => {
                                let events = events.clone();
                                // Prepared so the turn-end callback is ordered after
                                // every update the agent sent before its response.
                                cx.prepare_request(PromptRequest::new(
                                    session_id.clone(),
                                    vec![ContentBlock::from(text)],
                                ))
                                .on_receiving_result(async move |result| {
                                    let event = match result {
                                        Ok(response) => SessionEvent::TurnEnded {
                                            stop_reason: serde_label(&response.stop_reason),
                                        },
                                        Err(error) => SessionEvent::PromptFailed {
                                            message: error.to_string(),
                                        },
                                    };
                                    let _ = events.send(event);
                                    // An error here would end the connection.
                                    Ok(())
                                })?;
                            }
                            Some(SessionCommand::Cancel) => {
                                cx.send_notification(CancelNotification::new(session_id.clone()))?;
                                for (_, responder) in parked.drain() {
                                    responder.respond(RequestPermissionResponse::new(
                                        RequestPermissionOutcome::Cancelled,
                                    ))?;
                                }
                            }
                            Some(SessionCommand::ResolvePermission { id, option }) => {
                                if let Some(responder) = parked.remove(&id) {
                                    let outcome = match option {
                                        Some(option) => RequestPermissionOutcome::Selected(
                                            SelectedPermissionOutcome::new(option),
                                        ),
                                        None => RequestPermissionOutcome::Cancelled,
                                    };
                                    responder.respond(RequestPermissionResponse::new(outcome))?;
                                }
                            }
                        }
                    }
                    () = cx.incoming_closed() => break,
                }
            }
            Ok(())
        })
        .await
}

/// `replaying`: a `session/load` is in progress, so user turns are history
/// to show rather than echoes of a prompt the UI already displays.
fn translate_update(update: SessionUpdate, replaying: bool) -> Option<SessionEvent> {
    Some(match update {
        SessionUpdate::UserMessageChunk(ContentChunk { content, .. }) if replaying => {
            SessionEvent::UserMessage {
                text: content_text(content)?,
            }
        }
        SessionUpdate::AgentMessageChunk(ContentChunk { content, .. }) => {
            SessionEvent::AgentMessage {
                text: content_text(content)?,
            }
        }
        SessionUpdate::AgentThoughtChunk(ContentChunk { content, .. }) => {
            SessionEvent::AgentThought {
                text: content_text(content)?,
            }
        }
        SessionUpdate::ToolCall(call) => SessionEvent::ToolCall {
            id: call.tool_call_id.0.to_string(),
            title: call.title,
            kind: serde_label(&call.kind),
            status: tool_status(call.status),
        },
        SessionUpdate::ToolCallUpdate(update) => SessionEvent::ToolCallUpdate {
            id: update.tool_call_id.0.to_string(),
            title: update.fields.title,
            status: update.fields.status.map(tool_status),
        },
        SessionUpdate::Plan(plan) => SessionEvent::Plan {
            entries: plan
                .entries
                .into_iter()
                .map(|entry| PlanItem {
                    content: entry.content,
                    done: matches!(entry.status, PlanEntryStatus::Completed),
                    active: matches!(entry.status, PlanEntryStatus::InProgress),
                })
                .collect(),
        },
        // Echoes of our own prompt, mode/command lists, usage, and the rest
        // are not shown yet.
        _ => return None,
    })
}

/// Text to show for a content block; `None` for content not rendered yet.
fn content_text(content: ContentBlock) -> Option<String> {
    match content {
        ContentBlock::Text(text) => Some(text.text),
        ContentBlock::ResourceLink(link) => Some(format!("[{}]({})", link.name, link.uri)),
        ContentBlock::Image(_) => Some("[image]".to_owned()),
        _ => None,
    }
}

fn tool_status(status: ToolCallStatus) -> ToolStatus {
    match status {
        ToolCallStatus::InProgress => ToolStatus::InProgress,
        ToolCallStatus::Completed => ToolStatus::Completed,
        ToolCallStatus::Failed => ToolStatus::Failed,
        _ => ToolStatus::Pending,
    }
}

/// The wire name of a unit-like schema enum, e.g. `end_turn` for `StopReason::EndTurn`.
fn serde_label(value: &impl serde::Serialize) -> String {
    match serde_json::to_value(value) {
        Ok(serde_json::Value::String(label)) => label,
        _ => "unknown".to_owned(),
    }
}

fn fs_error(error: &crate::fs::FsError) -> agent_client_protocol::Error {
    agent_client_protocol::Error::invalid_params().data(error.to_string())
}

#[cfg(test)]
mod tests;
