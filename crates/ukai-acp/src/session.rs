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
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{
    CancelNotification, ClientCapabilities, ContentBlock, ContentChunk, EmbeddedResource,
    EmbeddedResourceResource, FileSystemCapabilities, ImageContent, Implementation,
    InitializeRequest, LoadSessionRequest, NewSessionRequest, PlanEntryStatus, PromptRequest,
    ReadTextFileRequest, ReadTextFileResponse, RequestPermissionOutcome, RequestPermissionRequest,
    RequestPermissionResponse, ResourceLink, ResumeSessionRequest, SelectedPermissionOutcome,
    SessionId, SessionModeId, SessionModeState, SessionNotification, SessionUpdate,
    SetSessionModeRequest, TextResourceContents, ToolCallContent, ToolCallStatus,
    WriteTextFileRequest, WriteTextFileResponse,
};
use agent_client_protocol::{Agent, Client, ConnectTo, ConnectionTo, Responder, UntypedMessage};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
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
    /// Progress before the agent can be reached, e.g. while its pod starts.
    Preparing { message: String },
    /// `initialize` and session setup succeeded; prompts can be sent.
    Ready {
        agent: Option<String>,
        /// Pass back as [`SessionOptions::restore`] to continue this session later.
        session_id: String,
        restore: RestoreOutcome,
        /// What a prompt may carry besides text.
        prompt: PromptCaps,
        /// The modes the agent offers, empty when it has none.
        modes: ModeState,
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
        /// File changes the call proposes or made.
        diffs: Vec<FileDiff>,
    },
    ToolCallUpdate {
        id: String,
        title: Option<String>,
        status: Option<ToolStatus>,
        /// Replaces the call's diffs when the update carries content.
        diffs: Option<Vec<FileDiff>>,
    },
    /// The agent's current plan, replacing any earlier one.
    Plan { entries: Vec<PlanItem> },
    /// The session is in another mode, because the agent switched or because
    /// [`SessionHandle::set_mode`] was accepted.
    ModeChanged { mode: String },
    /// The commands a prompt may name, replacing any earlier list.
    CommandsChanged { commands: Vec<AgentCommand> },
    /// Answer with [`SessionHandle::resolve_permission`].
    PermissionRequested {
        id: PermissionId,
        title: String,
        options: Vec<PermissionChoice>,
    },
    /// The agent wrote a file through `fs/write_text_file`.
    FileWritten { path: PathBuf },
    /// A turn this client did not start is running: it reattached to a
    /// session whose turn went on while it was away.
    TurnRunning,
    /// The prompt turn finished, e.g. with `end_turn` or `cancelled`.
    TurnEnded { stop_reason: String },
    /// The agent rejected the prompt; the session is still usable.
    PromptFailed { message: String },
    /// Something else this client asked for failed; the session is still
    /// usable and no turn state changed.
    Warning { message: String },
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

/// A file change a tool call reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileDiff {
    /// Absolute path, as the agent sent it.
    pub path: PathBuf,
    /// `None` for a new file.
    pub old_text: Option<String>,
    pub new_text: String,
}

/// Content types an agent accepts in a prompt beyond text and resource links,
/// from its `promptCapabilities`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PromptCaps {
    pub image: bool,
    pub embedded_context: bool,
}

/// A user turn: text and what was attached to it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Prompt {
    pub text: String,
    pub attachments: Vec<Attachment>,
}

impl From<String> for Prompt {
    fn from(text: String) -> Self {
        Self {
            text,
            attachments: Vec::new(),
        }
    }
}

impl From<&str> for Prompt {
    fn from(text: &str) -> Self {
        text.to_owned().into()
    }
}

/// Something attached to a prompt. Which variant to send is the caller's
/// choice, made against the agent's [`PromptCaps`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Attachment {
    /// Needs [`PromptCaps::image`].
    Image { mime_type: String, data: Vec<u8> },
    /// A text file's contents; needs [`PromptCaps::embedded_context`].
    Text { uri: String, text: String },
    /// A reference the agent resolves itself; every agent accepts it.
    Link { name: String, uri: String },
}

impl Attachment {
    fn into_block(self) -> ContentBlock {
        match self {
            Self::Image { mime_type, data } => {
                ContentBlock::Image(ImageContent::new(BASE64.encode(data), mime_type))
            }
            Self::Text { uri, text } => ContentBlock::Resource(EmbeddedResource::new(
                EmbeddedResourceResource::TextResourceContents(TextResourceContents::new(
                    text, uri,
                )),
            )),
            Self::Link { name, uri } => ContentBlock::ResourceLink(ResourceLink::new(name, uri)),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanItem {
    pub content: String,
    pub done: bool,
    pub active: bool,
}

/// One of the ways an agent can operate, e.g. `plan` or `edit`.
///
/// See [Session Modes](https://agentclientprotocol.com/protocol/session-modes).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentMode {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
}

/// The modes an agent offers and the one it is in. Empty for an agent with
/// no modes, which is most of them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModeState {
    /// `None` until the agent names a mode.
    pub current: Option<String>,
    pub available: Vec<AgentMode>,
}

impl ModeState {
    /// The mode the session is in, if it is one of the available ones.
    pub fn current(&self) -> Option<&AgentMode> {
        let current = self.current.as_deref()?;
        self.available.iter().find(|mode| mode.id == current)
    }
}

/// A command the agent offers for a prompt, named in it as `/<name>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentCommand {
    pub name: String,
    pub description: String,
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
    Prompt(Prompt),
    Cancel,
    SetMode(String),
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
    pub fn prompt(&self, prompt: impl Into<Prompt>) -> bool {
        self.send(SessionCommand::Prompt(prompt.into()))
    }

    /// Cancel the running turn; pending permission requests are answered as cancelled.
    pub fn cancel(&self) -> bool {
        self.send(SessionCommand::Cancel)
    }

    /// Ask for one of the modes in [`ModeState::available`]. The session
    /// reports the outcome as [`SessionEvent::ModeChanged`] or
    /// [`SessionEvent::Warning`].
    pub fn set_mode(&self, mode: impl Into<String>) -> bool {
        self.send(SessionCommand::SetMode(mode.into()))
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
    /// Where `fs/*` requests are allowed to read and write; `None` does not
    /// offer `fs/*` at all, for agents that are not on this machine's files.
    pub fs_root: Option<FsRoot>,
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

/// How long setup may go without progress: generous enough for `npx` to
/// fetch an adapter on first run. A remote agent that sends `_ukai/progress`
/// or `_ukai/heartbeat` (e.g. while its pod starts) restarts the clock each
/// time.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(120);

/// Progress before the agent is reachable: `{ "message": string }`.
pub const PROGRESS_METHOD: &str = "_ukai/progress";
/// A session's turn state, after ACP v2's `state_update`:
/// `{ "sessionId", "state": "running" | "idle", "stopReason"? }`.
pub const TURN_STATE_METHOD: &str = "_ukai/turn_state";
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
    let last_progress = Arc::new(Mutex::new(Instant::now()));
    // A reattached session's turn state can arrive before the load response
    // completes setup; it is held until `Ready` so the UI sees them in order.
    let ready = Arc::new(AtomicBool::new(false));
    let running_before_ready = Arc::new(AtomicBool::new(false));

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
        .on_receive_notification(
            {
                let events = events.clone();
                let last_progress = last_progress.clone();
                let ready = ready.clone();
                let running_before_ready = running_before_ready.clone();
                async move |message: UntypedMessage, _cx| {
                    // Any of ukai's own notifications (progress, heartbeats)
                    // shows that setup is still moving.
                    if message.method().starts_with("_ukai/")
                        && let Ok(mut last) = last_progress.lock()
                    {
                        *last = Instant::now();
                    }
                    match translate_extension(&message) {
                        Some(
                            event @ (SessionEvent::TurnRunning | SessionEvent::TurnEnded { .. }),
                        ) if !ready.load(Ordering::SeqCst) => {
                            let running = matches!(event, SessionEvent::TurnRunning);
                            running_before_ready.store(running, Ordering::SeqCst);
                        }
                        Some(event) => {
                            let _ = events.send(event);
                        }
                        None => {}
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
                async move |request: ReadTextFileRequest, responder, _cx| {
                    let Some(fs_root) = &fs_root else {
                        return responder
                            .respond_with_error(agent_client_protocol::Error::method_not_found());
                    };
                    match fs_root.read(&request.path, request.line, request.limit) {
                        Ok(content) => responder.respond(ReadTextFileResponse::new(content)),
                        Err(error) => responder.respond_with_error(fs_error(&error)),
                    }
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let events = events.clone();
                let fs_root = fs_root.clone();
                async move |request: WriteTextFileRequest, responder, _cx| {
                    let Some(fs_root) = &fs_root else {
                        return responder
                            .respond_with_error(agent_client_protocol::Error::method_not_found());
                    };
                    match fs_root.write(&request.path, &request.content) {
                        Ok(path) => {
                            let _ = events.send(SessionEvent::FileWritten { path });
                            responder.respond(WriteTextFileResponse::new())
                        }
                        Err(error) => responder.respond_with_error(fs_error(&error)),
                    }
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(transport, async |cx: ConnectionTo<Agent>| {
            let offer_fs = fs_root.is_some();
            let capabilities = ClientCapabilities::new().fs(FileSystemCapabilities::new()
                .read_text_file(offer_fs)
                .write_text_file(offer_fs));
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
                let (session_id, outcome, modes) = match restore {
                    Some(id) if can_load => {
                        // History arrives as session/update notifications before
                        // the response; user turns are only forwarded meanwhile.
                        replaying.store(true, Ordering::SeqCst);
                        let loaded = cx
                            .send_request(LoadSessionRequest::new(id.clone(), cwd.clone()))
                            .block_task()
                            .await;
                        replaying.store(false, Ordering::SeqCst);
                        let loaded = loaded?;
                        (
                            SessionId::new(id),
                            RestoreOutcome::Replayed,
                            loaded.modes,
                        )
                    }
                    Some(id) if can_resume => {
                        let resumed = cx
                            .send_request(ResumeSessionRequest::new(id.clone(), cwd.clone()))
                            .block_task()
                            .await?;
                        (SessionId::new(id), RestoreOutcome::Resumed, resumed.modes)
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
                        (session.session_id, outcome, session.modes)
                    }
                };
                Ok::<_, agent_client_protocol::Error>((init, session_id, outcome, modes))
            };
            // An agent that never answers (e.g. stuck on expired credentials)
            // would otherwise leave the pane connecting forever.
            let (init, session_id, restore, modes) = tokio::select! {
                result = handshake => result?,
                () = stalled(&last_progress) => {
                    return Err(agent_client_protocol::util::internal_error(format!(
                        "agent made no progress on initialize and session setup for {}s",
                        HANDSHAKE_TIMEOUT.as_secs()
                    )));
                }
            };
            let prompt = &init.agent_capabilities.prompt_capabilities;
            let _ = events.send(SessionEvent::Ready {
                agent: init.agent_info.map(|info| info.title.unwrap_or(info.name)),
                session_id: session_id.0.to_string(),
                restore,
                prompt: PromptCaps {
                    image: prompt.image,
                    embedded_context: prompt.embedded_context,
                },
                modes: mode_state(modes),
            });
            ready.store(true, Ordering::SeqCst);
            if running_before_ready.load(Ordering::SeqCst) {
                let _ = events.send(SessionEvent::TurnRunning);
            }

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
                            Some(SessionCommand::Prompt(Prompt { text, attachments })) => {
                                let events = events.clone();
                                let blocks = (!text.is_empty())
                                    .then(|| ContentBlock::from(text))
                                    .into_iter()
                                    .chain(attachments.into_iter().map(Attachment::into_block))
                                    .collect();
                                // Prepared so the turn-end callback is ordered after
                                // every update the agent sent before its response.
                                cx.prepare_request(PromptRequest::new(session_id.clone(), blocks))
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
                            Some(SessionCommand::SetMode(mode)) => {
                                let events = events.clone();
                                // Prepared like a prompt: a rejected mode must not
                                // take the connection down with it.
                                cx.prepare_request(SetSessionModeRequest::new(
                                    session_id.clone(),
                                    SessionModeId::new(mode.clone()),
                                ))
                                .on_receiving_result(async move |result| {
                                    let event = match result {
                                        Ok(_) => SessionEvent::ModeChanged { mode },
                                        Err(error) => SessionEvent::Warning {
                                            message: format!("cannot switch mode: {error}"),
                                        },
                                    };
                                    let _ = events.send(event);
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
/// Resolves once [`HANDSHAKE_TIMEOUT`] has passed since `last_progress`,
/// which progress notifications keep moving forward.
async fn stalled(last_progress: &Mutex<Instant>) {
    loop {
        let deadline = last_progress
            .lock()
            .map_or_else(|_| Instant::now(), |last| *last + HANDSHAKE_TIMEOUT);
        if deadline <= Instant::now() {
            return;
        }
        tokio::time::sleep_until(deadline.into()).await;
    }
}

/// ukai's own notifications from a relay or gateway in front of an agent.
fn translate_extension(message: &UntypedMessage) -> Option<SessionEvent> {
    let params = message.params();
    match message.method() {
        PROGRESS_METHOD => Some(SessionEvent::Preparing {
            message: params.get("message")?.as_str()?.to_owned(),
        }),
        TURN_STATE_METHOD => match params.get("state")?.as_str()? {
            "running" => Some(SessionEvent::TurnRunning),
            "idle" => Some(SessionEvent::TurnEnded {
                stop_reason: params
                    .get("stopReason")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("end_turn")
                    .to_owned(),
            }),
            _ => None,
        },
        _ => None,
    }
}

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
            diffs: file_diffs(call.content),
        },
        SessionUpdate::ToolCallUpdate(update) => SessionEvent::ToolCallUpdate {
            id: update.tool_call_id.0.to_string(),
            title: update.fields.title,
            status: update.fields.status.map(tool_status),
            diffs: update.fields.content.map(file_diffs),
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
        SessionUpdate::CurrentModeUpdate(update) => SessionEvent::ModeChanged {
            mode: update.current_mode_id.0.to_string(),
        },
        SessionUpdate::AvailableCommandsUpdate(update) => SessionEvent::CommandsChanged {
            commands: update
                .available_commands
                .into_iter()
                .map(|command| AgentCommand {
                    name: command.name,
                    description: command.description,
                })
                .collect(),
        },
        // Echoes of our own prompt, usage, and the rest are not shown yet.
        _ => return None,
    })
}

/// The modes a `session/new`, `session/load`, or `session/resume` response
/// reported; empty for an agent without modes.
fn mode_state(modes: Option<SessionModeState>) -> ModeState {
    modes.map_or_else(ModeState::default, |modes| ModeState {
        current: Some(modes.current_mode_id.0.to_string()),
        available: modes
            .available_modes
            .into_iter()
            .map(|mode| AgentMode {
                id: mode.id.0.to_string(),
                name: mode.name,
                description: mode.description,
            })
            .collect(),
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

fn file_diffs(content: Vec<ToolCallContent>) -> Vec<FileDiff> {
    content
        .into_iter()
        .filter_map(|content| match content {
            ToolCallContent::Diff(diff) => Some(FileDiff {
                path: diff.path,
                old_text: diff.old_text,
                new_text: diff.new_text,
            }),
            _ => None,
        })
        .collect()
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
