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

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use agent_client_protocol::schema::v1::{
    AvailableCommandInput, BooleanConfigOptionCapabilities, CancelNotification, ClientCapabilities,
    ClientSessionCapabilities, ContentBlock, ContentChunk, EmbeddedResource,
    EmbeddedResourceResource, FileSystemCapabilities, ImageContent, Implementation,
    InitializeRequest, ListSessionsRequest, LoadSessionRequest, NewSessionRequest, PlanEntryStatus,
    PromptRequest, ReadTextFileRequest, ReadTextFileResponse, RequestPermissionOutcome,
    RequestPermissionRequest, RequestPermissionResponse, ResourceLink, ResumeSessionRequest,
    SelectedPermissionOutcome, SessionConfigKind, SessionConfigOption, SessionConfigOptionValue,
    SessionConfigOptionsCapabilities, SessionConfigSelectOptions, SessionId, SessionNotification,
    SessionUpdate, SetSessionConfigOptionRequest, TextResourceContents, ToolCallContent,
    ToolCallStatus, WriteTextFileRequest, WriteTextFileResponse,
};
use agent_client_protocol::schema::{MaybeUndefined, ProtocolVersion};
use agent_client_protocol::{Agent, Client, ConnectTo, ConnectionTo, Responder, UntypedMessage};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use tokio::sync::mpsc;

use crate::Transport;
use crate::fs::FsRoot;
use crate::reconnect::{Next, Reconnect, ReconnectPolicy};
use crate::updates::{Update, UpdateCheck};

/// The receiving end of a session's events.
pub type SessionEvents = mpsc::UnboundedReceiver<SessionEvent>;

/// Identifies one pending permission request within a session.
pub type PermissionId = u64;

/// Something the agent did, translated for the UI.
#[derive(Debug, Clone, PartialEq)]
pub enum SessionEvent {
    /// Progress before the agent can be reached, e.g. while its pod starts.
    Preparing { message: String },
    /// The agent is behind its latest release. A new session waits for
    /// [`SessionHandle::proceed`] before it is created; to update instead,
    /// drop the handle and restart the agent.
    UpdateAvailable(Update),
    /// The agent was restarted to reach [`UpdateCheck::expecting`]'s release
    /// and is still behind it, so its command or image fixes the version.
    /// The session starts without asking.
    UpdateMissed(Update),
    /// `initialize` and session setup succeeded; prompts can be sent.
    Ready {
        agent: Option<String>,
        /// Pass back as [`SessionOptions::restore`] to continue this session later.
        session_id: String,
        restore: RestoreOutcome,
        /// What a prompt may carry besides text.
        prompt: PromptCaps,
        /// Settings the user may change, e.g. model or reasoning effort.
        config: Vec<ConfigOption>,
        /// The agent answers [`SessionHandle::list_sessions`].
        list_sessions: bool,
    },
    /// The sessions the agent keeps for this working directory, from
    /// [`SessionHandle::list_sessions`].
    Sessions { sessions: Vec<PastSession> },
    /// [`SessionHandle::list_sessions`] failed.
    SessionsFailed { message: String },
    /// The agent's settings changed, replacing every earlier [`ConfigOption`].
    ConfigChanged { options: Vec<ConfigOption> },
    /// The agent rejected a [`SessionHandle::set_config`]; nothing changed.
    ConfigFailed { message: String },
    /// A chunk of a user message the agent replays while loading a session.
    UserMessage { text: String },
    /// An image of a user message replayed likewise, as a `data:` URL.
    UserImage { url: String },
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
    /// A message, thought, or tool call of a subagent the agent runs, which
    /// streams alongside the agent's own and those of its other subagents.
    /// `parent` is the id of the tool call that started the subagent.
    Subagent {
        parent: String,
        event: Box<SessionEvent>,
    },
    /// The agent's current plan, replacing any earlier one.
    Plan { entries: Vec<PlanItem> },
    /// Context window use and session cost, replacing any earlier report.
    Usage(Usage),
    /// The slash commands the agent offers, replacing any earlier list. Run
    /// one by sending a prompt that starts with `/name`.
    Commands { commands: Vec<SlashCommand> },
    /// What the agent calls the session, replacing any earlier title; `None`
    /// when it takes the title back.
    Title { title: Option<String> },
    /// Answer with [`SessionHandle::resolve_permission`].
    PermissionRequested {
        id: PermissionId,
        title: String,
        options: Vec<PermissionChoice>,
    },
    /// The agent wrote a file through `fs/write_text_file`.
    FileWritten { path: PathBuf },
    /// A turn the UI did not start with [`SessionHandle::prompt`] is running:
    /// the client reattached to a session whose turn went on while it was
    /// away, or a [`SessionHandle::steer`] found the turn over and started one.
    TurnRunning,
    /// The prompt turn finished, e.g. with `end_turn` or `cancelled`.
    TurnEnded { stop_reason: String },
    /// The agent rejected the prompt; the session is still usable.
    PromptFailed { message: String },
    /// The agent rejected a [`SessionHandle::steer`]; the turn goes on without it.
    SteerFailed { message: String },
    /// The connection dropped while no turn ran. The session lives on at the
    /// agent and is loaded again over a new connection when a command needs
    /// it; until then nothing is wrong that the user should see.
    Detached,
    /// The connection dropped (or a command needs the agent while
    /// [`Self::Detached`]): attempt `attempt` (1-based) at reaching the
    /// session again starts. Once it does, the agent replays the session's
    /// history, then [`Self::Ready`] with [`RestoreOutcome::Replayed`] and,
    /// if the turn goes on, [`Self::TurnRunning`]. Fold the replay into a
    /// fresh history rather than onto what is shown: it repeats all of it.
    Reconnecting { attempt: u32 },
    /// The connection is gone; no more events follow. A remote session ends
    /// so only once reconnecting has failed (see [`crate::reconnect`]).
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
    /// The agent does not have the session any more, so a new one was created.
    Missing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolStatus {
    Pending,
    InProgress,
    Completed,
    Failed,
}

/// A session an agent keeps, as `session/list` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PastSession {
    /// Pass as [`SessionOptions::restore`] to continue it.
    pub id: String,
    pub title: Option<String>,
    /// ISO 8601, as the agent sent it.
    pub updated_at: Option<String>,
}

/// Context window use and cumulative cost an agent reports for its session.
#[derive(Debug, Clone, PartialEq)]
pub struct Usage {
    /// Tokens currently in context.
    pub used: u64,
    /// The context window, in tokens.
    pub size: u64,
    pub cost: Option<Cost>,
}

/// Cumulative cost of a session.
#[derive(Debug, Clone, PartialEq)]
pub struct Cost {
    pub amount: f64,
    /// ISO 4217 code, e.g. `USD`.
    pub currency: String,
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

/// How an agent takes prompts: content types it accepts beyond text and
/// resource links (its `promptCapabilities`), and whether a running turn can
/// be steered.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PromptCaps {
    pub image: bool,
    pub embedded_context: bool,
    /// The agent takes [`STEER_METHOD`]: a message sent while a turn runs
    /// joins that turn instead of stopping it.
    pub steering: bool,
}

/// A user turn: text and what was attached to it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Prompt {
    pub text: String,
    pub attachments: Vec<Attachment>,
}

impl Prompt {
    fn into_blocks(self) -> Vec<ContentBlock> {
        (!self.text.is_empty())
            .then(|| ContentBlock::from(self.text))
            .into_iter()
            .chain(self.attachments.into_iter().map(Attachment::into_block))
            .collect()
    }
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
    /// The image as a `data:` URL, to show it; `None` for anything else.
    pub fn data_url(&self) -> Option<String> {
        match self {
            Self::Image { mime_type, data } => Some(data_url(mime_type, &BASE64.encode(data))),
            Self::Text { .. } | Self::Link { .. } => None,
        }
    }

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

/// A session setting the agent lets the user change (ACP `configOptions`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigOption {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    /// `mode`, `model`, `model_config`, `thought_level`, or the agent's own;
    /// a hint for presentation only.
    pub category: Option<String>,
    pub value: ConfigValue,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigValue {
    Select {
        current: String,
        choices: Vec<ConfigChoice>,
    },
    Toggle(bool),
}

/// One value of a [`ConfigValue::Select`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigChoice {
    pub value: String,
    pub name: String,
    pub description: Option<String>,
    /// Name of the group the agent listed it under, if any.
    pub group: Option<String>,
}

/// A new value for a [`ConfigOption`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigInput {
    /// One of a select's [`ConfigChoice::value`]s.
    Select(String),
    Toggle(bool),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanItem {
    pub content: String,
    pub done: bool,
    pub active: bool,
}

/// A command from the agent's `available_commands_update`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlashCommand {
    /// Without the leading `/`.
    pub name: String,
    pub description: String,
    /// What to write after the name, when the command takes input.
    pub hint: Option<String>,
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
    Steer(Prompt),
    Cancel,
    ResolvePermission {
        id: PermissionId,
        option: Option<String>,
    },
    SetConfig {
        id: String,
        value: ConfigInput,
    },
    ListSessions,
    Proceed,
}

impl SessionCommand {
    /// Whether the command needs a connection to the agent. Those that do
    /// not mean nothing while detached: no turn runs and nothing is asked.
    fn needs_agent(&self) -> bool {
        match self {
            Self::Prompt(_) | Self::Steer(_) | Self::SetConfig { .. } | Self::ListSessions => true,
            Self::Cancel | Self::ResolvePermission { .. } | Self::Proceed => false,
        }
    }
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

    /// Add `prompt` to the running turn; needs [`PromptCaps::steering`]. When
    /// the turn has ended by the time the agent sees it, it is sent as a new
    /// turn, announced with [`SessionEvent::TurnRunning`].
    pub fn steer(&self, prompt: impl Into<Prompt>) -> bool {
        self.send(SessionCommand::Steer(prompt.into()))
    }

    /// Cancel the running turn; pending permission requests are answered as cancelled.
    pub fn cancel(&self) -> bool {
        self.send(SessionCommand::Cancel)
    }

    /// Answer a [`SessionEvent::PermissionRequested`]; `None` declines without choosing.
    pub fn resolve_permission(&self, id: PermissionId, option: Option<String>) -> bool {
        self.send(SessionCommand::ResolvePermission { id, option })
    }

    /// Change one of the agent's [`ConfigOption`]s. The result arrives as
    /// [`SessionEvent::ConfigChanged`] or [`SessionEvent::ConfigFailed`].
    pub fn set_config(&self, id: impl Into<String>, value: ConfigInput) -> bool {
        self.send(SessionCommand::SetConfig {
            id: id.into(),
            value,
        })
    }

    /// Ask for the sessions the agent keeps for this working directory; needs
    /// `list_sessions` in [`SessionEvent::Ready`]. The result arrives as
    /// [`SessionEvent::Sessions`] or [`SessionEvent::SessionsFailed`].
    pub fn list_sessions(&self) -> bool {
        self.send(SessionCommand::ListSessions)
    }

    /// Create the session despite a [`SessionEvent::UpdateAvailable`].
    pub fn proceed(&self) -> bool {
        self.send(SessionCommand::Proceed)
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
    /// Look up the agent's latest release before a new session (never a
    /// restored one, which may be in use elsewhere) and ask first when it
    /// is behind.
    pub update_check: Option<UpdateCheck>,
}

/// Start a session on the current tokio runtime.
///
/// A remote agent's dropped connection does not end the session: it is
/// reconnected and the session loaded again, as [`crate::reconnect`]
/// describes, when the agent can load sessions. A local agent's process
/// ending does end it, since whatever turn it ran ended with it.
pub fn spawn_session(
    transport: Transport,
    options: SessionOptions,
) -> (SessionHandle, SessionEvents) {
    spawn_session_with(transport, options, ReconnectPolicy::default())
}

/// [`spawn_session`], reconnecting a remote agent as `policy` says;
/// [`ReconnectPolicy::NEVER`] ends the session with its first connection.
pub fn spawn_session_with(
    transport: Transport,
    options: SessionOptions,
    policy: ReconnectPolicy,
) -> (SessionHandle, SessionEvents) {
    match transport {
        Transport::Remote(client) => {
            // Each clone opens a connection of its own over the shared pool.
            let redial = client.clone();
            spawn_on(
                client,
                Some(Box::new(move || redial.clone())),
                policy,
                options,
            )
        }
        Transport::Local(agent) => spawn_on(agent, None, ReconnectPolicy::NEVER, options),
    }
}

/// Makes a new connection to the agent the session's first one reached.
pub(crate) type Redial<T> = Box<dyn FnMut() -> T + Send>;

/// [`spawn_session`] over any ACP transport; tests use an in-memory channel.
pub(crate) fn spawn_on<T: ConnectTo<Client> + Send + 'static>(
    transport: T,
    redial: Option<Redial<T>>,
    policy: ReconnectPolicy,
    options: SessionOptions,
) -> (SessionHandle, SessionEvents) {
    let (command_tx, command_rx) = mpsc::unbounded_channel();
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let events = Events::new(event_tx.clone());
        let result = run_session(transport, redial, policy, options, events, command_rx).await;
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

/// Where a session's events go, noting on the way whether a turn is running,
/// which decides how eagerly a dropped connection is reconnected.
#[derive(Debug, Clone)]
struct Events {
    tx: mpsc::UnboundedSender<SessionEvent>,
    running: Arc<AtomicBool>,
}

impl Events {
    fn new(tx: mpsc::UnboundedSender<SessionEvent>) -> Self {
        Self {
            tx,
            running: Arc::new(AtomicBool::new(false)),
        }
    }

    fn send(&self, event: SessionEvent) -> Result<(), mpsc::error::SendError<SessionEvent>> {
        match event {
            SessionEvent::TurnRunning => self.running.store(true, Ordering::SeqCst),
            SessionEvent::TurnEnded { .. } | SessionEvent::PromptFailed { .. } => {
                self.running.store(false, Ordering::SeqCst);
            }
            _ => {}
        }
        self.tx.send(event)
    }

    /// A prompt went out; its turn runs until it is answered.
    fn turn_started(&self) {
        self.running.store(true, Ordering::SeqCst);
    }

    /// Before a connection is set up: whether a turn runs is the agent's to say.
    fn turn_unknown(&self) {
        self.running.store(false, Ordering::SeqCst);
    }

    fn running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }
}

/// How long setup may go without progress: generous enough for `npx` to
/// fetch an adapter on first run. A remote agent that sends `_ukai/progress`
/// or `_ukai/heartbeat` (e.g. while its pod starts) restarts the clock each
/// time.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(120);

/// Progress before the agent is reachable: `{ "message": string }`.
pub const PROGRESS_METHOD: &str = "_ukai/progress";
/// Adds a message to the running turn: `{ "sessionId", "prompt": ContentBlock[],
/// "_meta"? }`, answered with `{ "outcome": "injected" | "promptRequired" }`.
/// An agent that takes it says so with `_meta.steering.supported` in its
/// `initialize` response. This is the steering extension `claude-agent-acp`
/// implements, which Zed uses.
pub const STEER_METHOD: &str = "_session/steering";
/// A session's turn state, after ACP v2's `state_update`:
/// `{ "sessionId", "state": "running" | "idle", "stopReason"? }`.
pub const TURN_STATE_METHOD: &str = "_ukai/turn_state";
type Parked = (PermissionId, Responder<RequestPermissionResponse>);

/// What a session carries from one connection to the next.
struct Link {
    cwd: PathBuf,
    fs_root: Option<FsRoot>,
    /// The session to continue on the first connection, from [`SessionOptions::restore`].
    restore: Option<String>,
    update_check: Option<UpdateCheck>,
    /// Permission ids stay unique across connections.
    next_permission: Arc<AtomicU64>,
    /// The session, once a connection has set it up: later connections load it.
    session: Option<SessionId>,
    /// The agent advertised `loadSession`, so a later connection can pick the
    /// session up with its history.
    can_load: bool,
    /// The current connection got as far as [`SessionEvent::Ready`].
    reached: bool,
    /// Commands that arrived while no connection was up, to run once one is.
    pending: VecDeque<SessionCommand>,
}

/// How one connection ended without an error.
enum Ended {
    /// Every [`SessionHandle`] was dropped: the session is over.
    Handle,
    /// The agent closed the connection.
    Lost,
}

async fn run_session<T: ConnectTo<Client> + 'static>(
    first: T,
    mut redial: Option<Redial<T>>,
    policy: ReconnectPolicy,
    options: SessionOptions,
    events: Events,
    mut commands: mpsc::UnboundedReceiver<SessionCommand>,
) -> Result<(), agent_client_protocol::Error> {
    let SessionOptions {
        cwd,
        fs_root,
        restore,
        update_check,
    } = options;
    let mut link = Link {
        cwd,
        fs_root,
        restore,
        update_check,
        next_permission: Arc::new(AtomicU64::new(0)),
        session: None,
        can_load: false,
        reached: false,
        pending: VecDeque::new(),
    };
    let mut reconnect = Reconnect::new(policy);
    let mut transport = first;
    loop {
        let error = match connect(transport, &mut link, &events, &mut commands).await {
            Ok(Ended::Handle) => return Ok(()),
            Ok(Ended::Lost) => None,
            Err(error) => Some(error),
        };
        // A session that never came up, or that a new connection could not
        // pick up with its history, ends here as it always has.
        let Some(redial) = redial
            .as_mut()
            .filter(|_| link.session.is_some() && link.can_load)
        else {
            return match error {
                Some(error) => Err(error),
                // The turn is gone with the connection, so say so.
                None if events.running() => Err(agent_client_protocol::util::internal_error(
                    "the agent closed the connection during the turn".to_owned(),
                )),
                None => Ok(()),
            };
        };
        let mut next = if link.reached {
            reconnect.connected();
            let running = events.running();
            match &error {
                Some(error) => tracing::info!(%error, running, "lost the connection to the agent"),
                None => tracing::info!(running, "the agent closed the connection"),
            }
            reconnect.dropped(running, Instant::now())
        } else {
            if let Some(error) = &error {
                tracing::info!(%error, "could not reconnect to the agent");
            }
            reconnect.failed(Instant::now(), crate::reconnect::spread())
        };
        'wait: loop {
            match next {
                Next::GiveUp { attempts } => {
                    for command in link.pending.drain(..) {
                        if matches!(
                            command,
                            SessionCommand::Prompt(_) | SessionCommand::Steer(_)
                        ) {
                            let _ = events.send(SessionEvent::PromptFailed {
                                message: "not sent: the agent could not be reached".to_owned(),
                            });
                        }
                    }
                    let reason = error.map_or_else(
                        || "the agent closed the connection".to_owned(),
                        |error| error.to_string(),
                    );
                    return Err(agent_client_protocol::util::internal_error(format!(
                        "lost the connection and could not reconnect in {attempts} attempts: {reason}"
                    )));
                }
                Next::WhenNeeded => {
                    let _ = events.send(SessionEvent::Detached);
                    loop {
                        match commands.recv().await {
                            None => return Ok(()),
                            Some(command) if command.needs_agent() => {
                                link.pending.push_back(command);
                                break;
                            }
                            // Nothing is running or waiting on an answer.
                            Some(_) => {}
                        }
                    }
                    next = reconnect.needed(Instant::now());
                }
                Next::Attempt { attempt, after } => {
                    let _ = events.send(SessionEvent::Reconnecting { attempt });
                    let wake = tokio::time::sleep(after);
                    tokio::pin!(wake);
                    loop {
                        tokio::select! {
                            () = &mut wake => break 'wait,
                            command = commands.recv() => match command {
                                None => return Ok(()),
                                Some(command) => link.pending.push_back(command),
                            },
                        }
                    }
                }
            }
        }
        transport = redial();
    }
}

/// Run the session over one connection: set it up (or load it again), then
/// pass commands and events until the connection or the handle goes.
async fn connect<T: ConnectTo<Client> + 'static>(
    transport: T,
    link: &mut Link,
    events: &Events,
    commands: &mut mpsc::UnboundedReceiver<SessionCommand>,
) -> Result<Ended, agent_client_protocol::Error> {
    link.reached = false;
    let (park_tx, mut park_rx) = mpsc::unbounded_channel::<Parked>();
    let next_permission = link.next_permission.clone();
    let cwd = link.cwd.clone();
    let fs_root = link.fs_root.clone();
    let restore = match &link.session {
        Some(session) => Some(session.0.to_string()),
        None => link.restore.clone(),
    };
    let update_check = link.update_check.clone();
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
            let capabilities = ClientCapabilities::new()
                .fs(FileSystemCapabilities::new()
                    .read_text_file(offer_fs)
                    .write_text_file(offer_fs))
                .session(
                    ClientSessionCapabilities::new().config_options(
                        SessionConfigOptionsCapabilities::new()
                            .boolean(BooleanConfigOptionCapabilities::new()),
                    ),
                );
            let init = progressing(
                &last_progress,
                cx.send_request(
                    InitializeRequest::new(ProtocolVersion::V1)
                        .client_capabilities(capabilities)
                        .client_info(Implementation::new("ukai", env!("CARGO_PKG_VERSION"))),
                )
                .block_task(),
            )
            .await?;
            let update = match (&update_check, &init.agent_info, &restore) {
                (Some(check), Some(info), None) => check.check(&info.name, &info.version).await,
                _ => None,
            };
            match update {
                Some(update) if update_check.as_ref().is_some_and(|c| c.missed(&update)) => {
                    let _ = events.send(SessionEvent::UpdateMissed(update));
                }
                Some(update) => {
                    let _ = events.send(SessionEvent::UpdateAvailable(update));
                    loop {
                        tokio::select! {
                            command = commands.recv() => match command {
                                None => return Ok(Ended::Handle),
                                Some(SessionCommand::Proceed) => break,
                                // Nothing else means anything before the session exists.
                                Some(_) => {}
                            },
                            () = cx.incoming_closed() => return Ok(Ended::Lost),
                        }
                    }
                    // The time spent deciding is not the agent's.
                    if let Ok(mut last) = last_progress.lock() {
                        *last = Instant::now();
                    }
                }
                None => {}
            }
            let can_load = init.agent_capabilities.load_session;
            link.can_load = can_load;
            let can_resume = init
                .agent_capabilities
                .session_capabilities
                .resume
                .is_some();
            let setup = async {
                let restored = match &restore {
                    Some(id) if can_load => {
                        // History arrives as session/update notifications before
                        // the response; user turns are only forwarded meanwhile.
                        replaying.store(true, Ordering::SeqCst);
                        let loaded = cx
                            .send_request(LoadSessionRequest::new(id.clone(), cwd.clone()))
                            .block_task()
                            .await;
                        replaying.store(false, Ordering::SeqCst);
                        Some(loaded.map(|loaded| {
                            (
                                SessionId::new(id.clone()),
                                RestoreOutcome::Replayed,
                                loaded.config_options,
                            )
                        }))
                    }
                    Some(id) if can_resume => Some(
                        cx.send_request(ResumeSessionRequest::new(id.clone(), cwd.clone()))
                            .block_task()
                            .await
                            .map(|resumed| {
                                (
                                    SessionId::new(id.clone()),
                                    RestoreOutcome::Resumed,
                                    resumed.config_options,
                                )
                            }),
                    ),
                    _ => None,
                };
                let outcome = match restored {
                    Some(Ok(restored)) => return Ok(restored),
                    // The agent no longer keeps it, e.g. its pod's disk is new.
                    Some(Err(e))
                        if e.code == agent_client_protocol::ErrorCode::ResourceNotFound =>
                    {
                        RestoreOutcome::Missing
                    }
                    Some(Err(e)) => return Err(e),
                    None if restore.is_some() => RestoreOutcome::Unsupported,
                    None => RestoreOutcome::Fresh,
                };
                let session = cx
                    .send_request(NewSessionRequest::new(cwd.clone()))
                    .block_task()
                    .await?;
                Ok((session.session_id, outcome, session.config_options))
            };
            let (session_id, restore, config) = progressing(&last_progress, setup).await?;
            let prompt = &init.agent_capabilities.prompt_capabilities;
            // A turn that ran before the connection dropped may have ended
            // since; the agent says when one is still running.
            events.turn_unknown();
            link.session = Some(session_id.clone());
            link.reached = true;
            let _ = events.send(SessionEvent::Ready {
                agent: init.agent_info.map(|info| info.title.unwrap_or(info.name)),
                session_id: session_id.0.to_string(),
                restore,
                prompt: PromptCaps {
                    image: prompt.image,
                    embedded_context: prompt.embedded_context,
                    steering: init
                        .meta
                        .as_ref()
                        .and_then(|meta| meta.get("steering")?.get("supported")?.as_bool())
                        .unwrap_or(false),
                },
                config: config_options(config.unwrap_or_default()),
                list_sessions: init.agent_capabilities.session_capabilities.list.is_some(),
            });
            ready.store(true, Ordering::SeqCst);
            if running_before_ready.load(Ordering::SeqCst) {
                let _ = events.send(SessionEvent::TurnRunning);
            }

            let mut parked: HashMap<PermissionId, Responder<RequestPermissionResponse>> =
                HashMap::new();
            // Steers that reached the agent after their turn ended, to send as turns.
            let (late_tx, mut late_rx) = mpsc::unbounded_channel::<Vec<ContentBlock>>();
            loop {
                // What arrived while no connection was up goes first.
                let command = match link.pending.pop_front() {
                    Some(command) => Some(command),
                    None => tokio::select! {
                        Some(blocks) = late_rx.recv() => {
                            let _ = events.send(SessionEvent::TurnRunning);
                            send_prompt(&cx, &session_id, blocks, events)?;
                            continue;
                        }
                        Some((id, responder)) = park_rx.recv() => {
                            parked.insert(id, responder);
                            continue;
                        }
                        command = commands.recv() => command,
                        () = cx.incoming_closed() => return Ok(Ended::Lost),
                    },
                };
                // A permission request parked an instant ago must be
                // visible to the command that answers it.
                while let Ok((id, responder)) = park_rx.try_recv() {
                    parked.insert(id, responder);
                }
                match command {
                    None => return Ok(Ended::Handle),
                    Some(SessionCommand::Prompt(prompt)) => {
                        send_prompt(&cx, &session_id, prompt.into_blocks(), events)?;
                    }
                    Some(SessionCommand::Steer(prompt)) => {
                        let blocks = prompt.into_blocks();
                        let params = serde_json::json!({
                            "sessionId": session_id,
                            "prompt": blocks,
                            // Hand the message back rather than have the
                            // agent start a turn this client would not see end.
                            "_meta": { "steering": { "idleBehavior": "promptRequired" } },
                        });
                        let events = events.clone();
                        let late = late_tx.clone();
                        cx.send_request(UntypedMessage::new(STEER_METHOD, params)?)
                            .on_receiving_result(async move |result| {
                                match result {
                                    Ok(response) if response["outcome"] == "promptRequired" => {
                                        let _ = late.send(blocks);
                                    }
                                    Ok(_) => {}
                                    // Recorded or not, the reconnect's replay shows it.
                                    Err(error)
                                        if agent_client_protocol::is_incoming_transport_closed(
                                            &error,
                                        ) => {}
                                    Err(error) => {
                                        let _ = events.send(SessionEvent::SteerFailed {
                                            message: error.to_string(),
                                        });
                                    }
                                }
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
                    Some(SessionCommand::SetConfig { id, value }) => {
                        let events = events.clone();
                        let value = match value {
                            ConfigInput::Select(value) => SessionConfigOptionValue::value_id(value),
                            ConfigInput::Toggle(on) => SessionConfigOptionValue::boolean(on),
                        };
                        cx.send_request(SetSessionConfigOptionRequest::new(
                            session_id.clone(),
                            id,
                            value,
                        ))
                        .on_receiving_result(async move |result| {
                            let event = match result {
                                Ok(response) => SessionEvent::ConfigChanged {
                                    options: config_options(response.config_options),
                                },
                                // The reconnect's `Ready` carries the settings as they are.
                                Err(error)
                                    if agent_client_protocol::is_incoming_transport_closed(
                                        &error,
                                    ) =>
                                {
                                    return Ok(());
                                }
                                Err(error) => SessionEvent::ConfigFailed {
                                    message: error.to_string(),
                                },
                            };
                            let _ = events.send(event);
                            Ok(())
                        })?;
                    }
                    Some(SessionCommand::ListSessions) => {
                        let events = events.clone();
                        let request = list_sessions(cx.clone(), cwd.clone());
                        cx.spawn(async move {
                            let event = match request.await {
                                Ok(sessions) => SessionEvent::Sessions { sessions },
                                Err(error) => SessionEvent::SessionsFailed {
                                    message: error.to_string(),
                                },
                            };
                            let _ = events.send(event);
                            Ok(())
                        })?;
                    }
                    Some(SessionCommand::Proceed) => {}
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
        })
        .await
}

/// Pages of `session/list` to follow before giving up on the rest.
const MAX_SESSION_PAGES: usize = 10;

/// Every session the agent keeps for `cwd`, following `nextCursor`.
async fn list_sessions(
    cx: ConnectionTo<Agent>,
    cwd: PathBuf,
) -> Result<Vec<PastSession>, agent_client_protocol::Error> {
    let mut sessions = Vec::new();
    let mut cursor = None;
    for _ in 0..MAX_SESSION_PAGES {
        let page = cx
            .send_request(ListSessionsRequest::new().cwd(cwd.clone()).cursor(cursor))
            .block_task()
            .await?;
        sessions.extend(page.sessions.into_iter().map(|info| PastSession {
            id: info.session_id.0.to_string(),
            title: info.title,
            updated_at: info.updated_at,
        }));
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    Ok(sessions)
}

/// Start a turn. The request is prepared so the turn-end event is ordered
/// after every update the agent sent before its response.
fn send_prompt(
    cx: &ConnectionTo<Agent>,
    session_id: &SessionId,
    blocks: Vec<ContentBlock>,
    events: &Events,
) -> Result<(), agent_client_protocol::Error> {
    let events = events.clone();
    events.turn_started();
    cx.prepare_request(PromptRequest::new(session_id.clone(), blocks))
        .on_receiving_result(async move |result| {
            let event = match result {
                Ok(response) => SessionEvent::TurnEnded {
                    stop_reason: serde_label(&response.stop_reason),
                },
                // The connection dropped, not the turn: a new connection
                // hears how it goes.
                Err(error) if agent_client_protocol::is_incoming_transport_closed(&error) => {
                    return Ok(());
                }
                Err(error) => SessionEvent::PromptFailed {
                    message: error.to_string(),
                },
            };
            let _ = events.send(event);
            // An error here would end the connection.
            Ok(())
        })
}

/// `work`, unless the agent stalls first. An agent that never answers (e.g.
/// stuck on expired credentials) would otherwise leave the pane connecting
/// forever.
async fn progressing<T>(
    last_progress: &Mutex<Instant>,
    work: impl Future<Output = Result<T, agent_client_protocol::Error>>,
) -> Result<T, agent_client_protocol::Error> {
    tokio::select! {
        result = work => result,
        () = stalled(last_progress) => Err(agent_client_protocol::util::internal_error(format!(
            "agent made no progress on initialize and session setup for {}s",
            HANDSHAKE_TIMEOUT.as_secs()
        ))),
    }
}

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

/// `replaying`: a `session/load` is in progress, so user turns are history
/// to show rather than echoes of a prompt the UI already displays.
fn translate_update(update: SessionUpdate, replaying: bool) -> Option<SessionEvent> {
    let parent = subagent_parent(&update);
    let event = translate_own_update(update, replaying)?;
    Some(match parent {
        Some(parent) => SessionEvent::Subagent {
            parent,
            event: Box::new(event),
        },
        None => event,
    })
}

/// The tool call that started the subagent `update` comes from, which
/// claude-agent-acp names in `_meta.claudeCode.parentToolUseId`.
fn subagent_parent(update: &SessionUpdate) -> Option<String> {
    let meta = match update {
        SessionUpdate::AgentMessageChunk(chunk) | SessionUpdate::AgentThoughtChunk(chunk) => {
            chunk.meta.as_ref()
        }
        SessionUpdate::ToolCall(call) => call.meta.as_ref(),
        SessionUpdate::ToolCallUpdate(update) => update.meta.as_ref(),
        _ => None,
    }?;
    let parent = meta.get("claudeCode")?.get("parentToolUseId")?.as_str()?;
    Some(parent.to_owned())
}

fn translate_own_update(update: SessionUpdate, replaying: bool) -> Option<SessionEvent> {
    Some(match update {
        SessionUpdate::UserMessageChunk(ContentChunk { content, .. }) if replaying => match content
        {
            ContentBlock::Image(image) => SessionEvent::UserImage {
                url: data_url(&image.mime_type, &image.data),
            },
            content => SessionEvent::UserMessage {
                text: content_text(content)?,
            },
        },
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
        SessionUpdate::UsageUpdate(usage) => SessionEvent::Usage(Usage {
            used: usage.used,
            size: usage.size,
            cost: usage.cost.map(|cost| Cost {
                amount: cost.amount,
                currency: cost.currency,
            }),
        }),
        SessionUpdate::ConfigOptionUpdate(update) => SessionEvent::ConfigChanged {
            options: config_options(update.config_options),
        },
        SessionUpdate::AvailableCommandsUpdate(update) => SessionEvent::Commands {
            commands: update
                .available_commands
                .into_iter()
                .map(|command| SlashCommand {
                    hint: match command.input {
                        Some(AvailableCommandInput::Unstructured(input)) => Some(input.hint),
                        _ => None,
                    },
                    name: command.name,
                    description: command.description,
                })
                .collect(),
        },
        SessionUpdate::SessionInfoUpdate(info) => match info.title {
            MaybeUndefined::Undefined => return None,
            MaybeUndefined::Null => SessionEvent::Title { title: None },
            MaybeUndefined::Value(title) => SessionEvent::Title { title: Some(title) },
        },
        // Echoes of our own prompt, modes, and the rest are not shown yet.
        _ => return None,
    })
}

/// The options of kinds this client knows; others are left out.
fn config_options(options: Vec<SessionConfigOption>) -> Vec<ConfigOption> {
    options
        .into_iter()
        .filter_map(|option| {
            let value = match option.kind {
                SessionConfigKind::Select(select) => ConfigValue::Select {
                    current: select.current_value.0.to_string(),
                    choices: match select.options {
                        SessionConfigSelectOptions::Ungrouped(options) => options
                            .into_iter()
                            .map(|choice| (None, choice))
                            .collect::<Vec<_>>(),
                        SessionConfigSelectOptions::Grouped(groups) => groups
                            .into_iter()
                            .flat_map(|group| {
                                let name = group.name;
                                group
                                    .options
                                    .into_iter()
                                    .map(move |choice| (Some(name.clone()), choice))
                            })
                            .collect(),
                        _ => Vec::new(),
                    }
                    .into_iter()
                    .map(|(group, choice)| ConfigChoice {
                        value: choice.value.0.to_string(),
                        name: choice.name,
                        description: choice.description,
                        group,
                    })
                    .collect(),
                },
                SessionConfigKind::Boolean(boolean) => ConfigValue::Toggle(boolean.current_value),
                _ => return None,
            };
            Some(ConfigOption {
                id: option.id.0.to_string(),
                name: option.name,
                description: option.description,
                category: option.category.as_ref().map(serde_label),
                value,
            })
        })
        .collect()
}

/// A `data:` URL of `base64` data.
fn data_url(mime_type: &str, base64: &str) -> String {
    format!("data:{mime_type};base64,{base64}")
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
