//! The visible history of one agent session, folded from [`SessionEvent`]s.

use std::path::PathBuf;
use std::sync::Arc;

use ukai_acp::checkout::CommandOutput;
use ukai_acp::{
    Attachment, ConfigOption, FileDiff, PastSession, PermissionChoice, PermissionId, PlanItem,
    Prompt, PromptCaps, RestoreOutcome, SessionEvent, SlashCommand, ToolStatus, Update, Usage,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Connecting,
    /// Ready for a prompt.
    Idle,
    /// A prompt turn is running.
    Busy,
    Closed {
        error: Option<String>,
    },
}

/// How the session reaches its agent, apart from [`Status`]: a dropped
/// connection to an agent that keeps sessions is reconnected quietly, and only
/// becomes [`Status::Closed`] once that fails.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Connection {
    #[default]
    Connected,
    /// The connection dropped while idle; the next prompt reaches the
    /// session again first. Nothing for the user to act on.
    Detached,
    /// Reaching the session again, for a running turn or a prompt.
    Reconnecting { attempt: u32 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entry {
    User {
        text: String,
        /// What was attached to the turn.
        attachments: Vec<Sent>,
    },
    Agent {
        text: String,
    },
    Thought {
        text: String,
    },
    Tool {
        id: String,
        title: String,
        kind: String,
        status: ToolStatus,
        diffs: Vec<FileDiff>,
        /// What the subagent the call started said and did, folded like the
        /// transcript's own entries; empty for other calls.
        children: Vec<Entry>,
    },
    Plan {
        items: Vec<PlanItem>,
    },
    Permission {
        id: PermissionId,
        title: String,
        options: Vec<PermissionChoice>,
        /// Name of the chosen option, or `cancelled`; `None` while pending.
        answer: Option<String>,
    },
    FileWritten {
        path: PathBuf,
    },
    Notice {
        text: String,
        error: bool,
    },
    /// A command the user ran in the checkout with `!` (`crate::shell`).
    Shell {
        /// Tells the run apart, for its output to land in its entry.
        id: u64,
        command: String,
        /// `None` while it runs; why it could not run, or what it printed.
        result: Option<Result<CommandOutput, String>>,
        /// What it printed went to the agent with a prompt.
        sent: bool,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Transcript {
    pub agent: Option<String>,
    pub status: Status,
    pub entries: Vec<Entry>,
    /// What the agent accepts in a prompt, known once it is ready.
    pub prompt_caps: PromptCaps,
    /// The agent's latest report of context use and cost, if it sends any.
    pub usage: Option<Usage>,
    /// Settings the agent lets the user change, e.g. model or reasoning effort.
    pub config: Vec<ConfigOption>,
    /// The slash commands the agent last said it offers.
    pub commands: Vec<SlashCommand>,
    /// What the user sent while a turn ran, for an agent that cannot be
    /// steered: the turn is being stopped, and this goes out once it has.
    pub queued: Option<Queued>,
    /// The ACP session this transcript is of, known once it is ready.
    pub session_id: Option<String>,
    /// What the agent calls the session, if it names it (see [`Self::title`]).
    pub session_title: Option<String>,
    /// The agent's other sessions the pane could switch to.
    pub history: History,
    /// A newer release of the agent, offered before the session is created:
    /// restart to update, or [`Self::skip_update`] and start as it is.
    pub update: Option<Update>,
    pub connection: Connection,
    /// While reconnecting, the history the agent replays, which replaces
    /// [`Self::entries`] once the session is back (see [`reconcile`]).
    replay: Option<Vec<Entry>>,
    /// Prompts recorded while not connected: they go out once the session is
    /// back, after its replay, so the replay does not have them.
    unsent: usize,
}

/// The agent's earlier sessions, as far as the pane has asked for them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum History {
    /// The agent cannot list its sessions (or is not ready yet).
    #[default]
    Unsupported,
    /// It can; nobody has asked yet.
    Available,
    Loading,
    /// Newest first, without the pane's own session.
    Loaded(Vec<PastSession>),
    Failed(String),
}

/// Something attached to a user's message, as the message shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sent {
    pub name: String,
    /// An image, as a `data:` URL: it is shown rather than named. Shared,
    /// since entries are cloned to render them.
    pub image: Option<Arc<str>>,
}

impl Sent {
    /// `attachment`, sent under `name`.
    pub fn new(name: impl Into<String>, attachment: &Attachment) -> Self {
        Self {
            name: name.into(),
            image: attachment.data_url().map(Arc::from),
        }
    }

    /// An image the agent replays, which comes without a name.
    fn replayed(url: String) -> Self {
        Self {
            name: "image".to_owned(),
            image: Some(Arc::from(url)),
        }
    }
}

/// A message sent while a turn runs to an agent without
/// [`PromptCaps::steering`]: the turn is stopped, and this is sent as the next one.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Queued {
    pub text: String,
    /// Each attachment with the name it is shown under.
    pub attachments: Vec<(String, Attachment)>,
}

impl Queued {
    /// The attachments as the message shows them.
    pub fn shown(&self) -> Vec<Sent> {
        self.attachments
            .iter()
            .map(|(name, attachment)| Sent::new(name.clone(), attachment))
            .collect()
    }

    pub fn prompt(&self) -> Prompt {
        Prompt {
            text: self.text.clone(),
            attachments: self.attachments.iter().map(|(_, a)| a.clone()).collect(),
        }
    }
}

impl Default for Transcript {
    fn default() -> Self {
        Self {
            agent: None,
            status: Status::Connecting,
            entries: Vec::new(),
            prompt_caps: PromptCaps::default(),
            session_id: None,
            session_title: None,
            history: History::Unsupported,
            usage: None,
            config: Vec::new(),
            commands: Vec::new(),
            queued: None,
            update: None,
            connection: Connection::Connected,
            replay: None,
            unsent: 0,
        }
    }
}

/// Something in a session the user should hear about even when not looking
/// at its pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Alert {
    /// A turn ended on its own (not by the user's cancel).
    Finished,
    /// The agent is blocked on a permission request.
    NeedsInput { title: String },
    /// A new session waits on whether to update the agent first.
    UpdateAvailable { package: String, latest: String },
    /// A prompt failed or the connection dropped.
    Failed { message: String },
}

impl Alert {
    /// The alert `event` raises, if any.
    pub fn of(event: &SessionEvent) -> Option<Self> {
        match event {
            SessionEvent::TurnEnded { stop_reason } if stop_reason != "cancelled" => {
                Some(Self::Finished)
            }
            SessionEvent::PermissionRequested { title, .. } => Some(Self::NeedsInput {
                title: title.clone(),
            }),
            SessionEvent::PromptFailed { message } => Some(Self::Failed {
                message: message.clone(),
            }),
            SessionEvent::UpdateAvailable(update) => Some(Self::UpdateAvailable {
                package: update.package.clone(),
                latest: update.latest.clone(),
            }),
            SessionEvent::Closed { error: Some(error) } => Some(Self::Failed {
                message: format!("disconnected: {error}"),
            }),
            _ => None,
        }
    }

    /// One line for a notification body.
    pub fn summary(&self) -> String {
        match self {
            Self::Finished => "Finished its turn".to_owned(),
            Self::NeedsInput { title } => format!("Needs permission: {title}"),
            Self::UpdateAvailable { package, latest } => {
                format!("{package} {latest} is out; update before starting?")
            }
            Self::Failed { message } => message.clone(),
        }
    }
}

impl Transcript {
    /// What the session is about, for where its pane is listed: the title
    /// the agent gave it, else the first line of the first prompt.
    pub fn title(&self) -> Option<&str> {
        let named = self.session_title.as_deref().map(str::trim);
        named.filter(|title| !title.is_empty()).or_else(|| {
            // While reconnecting, the replay is the session's history.
            let entries = match &self.replay {
                Some(replay) if !replay.is_empty() => replay,
                _ => &self.entries,
            };
            entries.iter().find_map(|entry| match entry {
                Entry::User { text, .. } => {
                    text.lines().map(str::trim).find(|line| !line.is_empty())
                }
                _ => None,
            })
        })
    }

    /// Note that the session list was asked for; `false` if the agent cannot list.
    pub fn history_requested(&mut self) -> bool {
        if self.history == History::Unsupported {
            return false;
        }
        self.history = History::Loading;
        true
    }

    /// Record a prompt the user sent, with what was attached to it. It
    /// carried the commands [`Self::unsent_shell`] lists.
    pub fn push_prompt(&mut self, text: impl Into<String>, attachments: Vec<Sent>) {
        for entry in &mut self.entries {
            if let Entry::Shell {
                result: Some(Ok(_)),
                sent,
                ..
            } = entry
            {
                *sent = true;
            }
        }
        self.entries.push(Entry::User {
            text: text.into(),
            attachments,
        });
        if self.connection != Connection::Connected {
            self.unsent += 1;
        }
        self.status = Status::Busy;
    }

    /// Record a command the user started with `!`; its id finishes it.
    pub fn start_shell(&mut self, command: impl Into<String>) -> u64 {
        let id = self
            .entries
            .iter()
            .filter(|entry| matches!(entry, Entry::Shell { .. }))
            .count() as u64;
        self.entries.push(Entry::Shell {
            id,
            command: command.into(),
            result: None,
            sent: false,
        });
        id
    }

    /// Record how the command `id` went.
    pub fn finish_shell(&mut self, id: u64, outcome: Result<CommandOutput, String>) {
        if let Some(Entry::Shell { result, .. }) = self
            .entries
            .iter_mut()
            .rev()
            .find(|entry| matches!(entry, Entry::Shell { id: run, .. } if *run == id))
        {
            *result = Some(outcome);
        }
    }

    /// The commands that finished and no prompt told the agent about yet,
    /// for the next one to; those that could not run are left out, and
    /// [`Self::push_prompt`] counts these as told.
    pub fn unsent_shell(&self) -> Vec<(&str, &CommandOutput)> {
        self.entries
            .iter()
            .filter_map(|entry| match entry {
                Entry::Shell {
                    command,
                    result: Some(Ok(output)),
                    sent: false,
                    ..
                } => Some((command.as_str(), output)),
                _ => None,
            })
            .collect()
    }

    /// Record the user's answer to a permission request.
    pub fn answer_permission(&mut self, id: PermissionId, answer: impl Into<String>) {
        if let Some(Entry::Permission { answer: slot, .. }) = self
            .entries
            .iter_mut()
            .rev()
            .find(|entry| matches!(entry, Entry::Permission { id: pid, .. } if *pid == id))
        {
            *slot = Some(answer.into());
        }
    }

    /// Whether the agent is blocked on a permission request nobody answered.
    pub fn awaiting_permission(&self) -> bool {
        self.entries
            .iter()
            .any(|entry| matches!(entry, Entry::Permission { answer: None, .. }))
    }

    /// Record a cancel: every pending permission request is answered with it.
    pub fn cancel(&mut self) {
        for entry in &mut self.entries {
            if let Entry::Permission {
                answer: answer @ None,
                ..
            } = entry
            {
                *answer = Some("cancelled".to_owned());
            }
        }
    }

    /// Queue `message` to follow the running turn, which the caller stops. A
    /// second message before the turn has stopped joins the first.
    pub fn queue(&mut self, message: Queued) {
        self.cancel();
        match &mut self.queued {
            Some(queued) => {
                if queued.text.is_empty() {
                    queued.text = message.text;
                } else if !message.text.is_empty() {
                    queued.text = format!("{}\n\n{}", queued.text, message.text);
                }
                queued.attachments.extend(message.attachments);
            }
            None => self.queued = Some(message),
        }
    }

    /// The queued [`Queued`], once the turn it waits on is over: send it when
    /// the status is [`Status::Idle`], hand it back to the user otherwise.
    pub fn take_queued(&mut self) -> Option<Queued> {
        match self.status {
            Status::Busy | Status::Connecting => None,
            Status::Idle | Status::Closed { .. } => self.queued.take(),
        }
    }

    /// Start the session on the agent as it is, past [`Self::update`].
    pub fn skip_update(&mut self) {
        self.update = None;
    }

    pub fn apply(&mut self, event: SessionEvent) {
        match event {
            SessionEvent::UpdateAvailable(update) => self.update = Some(update),
            SessionEvent::UpdateMissed(Update {
                package,
                current,
                latest,
            }) => self.entries.push(Entry::Notice {
                text: format!(
                    "restarted, but {package} is still {current} (latest {latest}): the agent's \
                     command or image fixes its version; run it as `npx -y {package}@latest` \
                     to follow releases"
                ),
                error: true,
            }),
            // Reaching a session again is quiet, however long it takes.
            SessionEvent::Preparing { .. } if self.connection != Connection::Connected => {}
            SessionEvent::Preparing { message } => self.entries.push(Entry::Notice {
                text: message,
                error: false,
            }),
            SessionEvent::Detached => self.connection = Connection::Detached,
            SessionEvent::Reconnecting { attempt } => {
                self.connection = Connection::Reconnecting { attempt };
                // A failed attempt may have replayed part of the history.
                self.replay = Some(Vec::new());
            }
            SessionEvent::TurnRunning => self.status = Status::Busy,
            SessionEvent::Ready {
                agent,
                restore,
                prompt,
                config,
                session_id,
                list_sessions,
                ..
            } => {
                self.agent = agent;
                self.session_id = Some(session_id);
                self.history = if list_sessions {
                    History::Available
                } else {
                    History::Unsupported
                };
                self.prompt_caps = prompt;
                self.config = config;
                if let Some(replay) = self.replay.take() {
                    self.entries = match restore {
                        RestoreOutcome::Replayed => {
                            reconcile(std::mem::take(&mut self.entries), replay, self.unsent)
                        }
                        // Nothing was replayed: what is shown stays, but
                        // requests of the old connection cannot be answered.
                        _ => std::mem::take(&mut self.entries)
                            .into_iter()
                            .filter(|entry| {
                                !matches!(entry, Entry::Permission { answer: None, .. })
                            })
                            .collect(),
                    };
                }
                self.connection = Connection::Connected;
                // Prompts sent while away go out now, so their turn is on.
                self.status = if self.unsent > 0 {
                    Status::Busy
                } else {
                    Status::Idle
                };
                self.unsent = 0;
                let notice = match restore {
                    RestoreOutcome::Fresh | RestoreOutcome::Replayed => None,
                    RestoreOutcome::Resumed => {
                        Some("session resumed; earlier messages were not replayed")
                    }
                    RestoreOutcome::Unsupported => {
                        Some("this agent cannot restore sessions; started a new one")
                    }
                    RestoreOutcome::Missing => {
                        Some("the agent no longer has this session; started a new one")
                    }
                };
                if let Some(text) = notice {
                    self.entries.push(Entry::Notice {
                        text: text.to_owned(),
                        error: false,
                    });
                }
            }
            SessionEvent::ConfigChanged { options } => self.config = options,
            SessionEvent::Sessions { mut sessions } => {
                sessions.retain(|past| self.session_id.as_ref() != Some(&past.id));
                // ISO 8601 in one format sorts as text; undated sessions go last.
                sessions.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
                self.history = History::Loaded(sessions);
            }
            SessionEvent::SessionsFailed { message } => self.history = History::Failed(message),
            SessionEvent::ConfigFailed { message } => self.entries.push(Entry::Notice {
                text: format!("could not change the setting: {message}"),
                error: true,
            }),
            SessionEvent::UserMessage { text } => match self.history_mut().last_mut() {
                Some(Entry::User { text: last, .. }) => last.push_str(&text),
                _ => self.history_mut().push(Entry::User {
                    text,
                    attachments: Vec::new(),
                }),
            },
            SessionEvent::UserImage { url } => match self.history_mut().last_mut() {
                Some(Entry::User { attachments, .. }) => attachments.push(Sent::replayed(url)),
                _ => self.history_mut().push(Entry::User {
                    text: String::new(),
                    attachments: vec![Sent::replayed(url)],
                }),
            },
            event @ (SessionEvent::AgentMessage { .. }
            | SessionEvent::AgentThought { .. }
            | SessionEvent::ToolCall { .. }
            | SessionEvent::ToolCallUpdate { .. }) => fold(self.history_mut(), event),
            SessionEvent::Subagent { parent, event } => {
                match tool_mut(self.history_mut(), &parent) {
                    Some(Entry::Tool { children, .. }) => fold(children, *event),
                    // A subagent of a call we never saw: show it as the agent's own.
                    _ => self.apply(*event),
                }
            }
            SessionEvent::Plan { entries: items } => {
                // One plan per turn: later plans replace the earlier one in place.
                let entries = self.history_mut();
                let turn_start = entries
                    .iter()
                    .rposition(|entry| matches!(entry, Entry::User { .. }))
                    .map_or(0, |index| index + 1);
                match entries[turn_start..]
                    .iter_mut()
                    .find(|entry| matches!(entry, Entry::Plan { .. }))
                {
                    Some(Entry::Plan { items: current }) => *current = items,
                    _ => entries.push(Entry::Plan { items }),
                }
            }
            SessionEvent::Commands { commands } => self.commands = commands,
            SessionEvent::Title { title } => self.session_title = title,
            // A request asked again on reconnecting follows the replay.
            SessionEvent::PermissionRequested { id, title, options } => {
                self.history_mut().push(Entry::Permission {
                    id,
                    title,
                    options,
                    answer: None,
                });
            }
            SessionEvent::FileWritten { path } => {
                self.history_mut().push(Entry::FileWritten { path });
            }
            SessionEvent::Usage(usage) => self.usage = Some(usage),
            SessionEvent::TurnEnded { stop_reason } => {
                if stop_reason != "end_turn" {
                    self.entries.push(Entry::Notice {
                        text: format!("turn ended: {stop_reason}"),
                        error: false,
                    });
                }
                self.status = Status::Idle;
            }
            SessionEvent::SteerFailed { message } => self.entries.push(Entry::Notice {
                text: format!("could not steer the turn: {message}"),
                error: true,
            }),
            SessionEvent::PromptFailed { message } => {
                self.entries.push(Entry::Notice {
                    text: message,
                    error: true,
                });
                self.status = Status::Idle;
            }
            SessionEvent::Closed { error } => {
                if let Some(error) = &error {
                    self.entries.push(Entry::Notice {
                        text: format!("disconnected: {error}"),
                        error: true,
                    });
                }
                self.update = None;
                self.replay = None;
                self.unsent = 0;
                self.connection = Connection::Connected;
                self.status = Status::Closed { error };
            }
        }
    }

    /// Where the agent's history goes: the replay while reconnecting, else
    /// what is shown.
    fn history_mut(&mut self) -> &mut Vec<Entry> {
        self.replay.as_mut().unwrap_or(&mut self.entries)
    }
}

/// What a reconnect shows: the history as the agent `replay`ed it, which has
/// everything the agent did (also while the connection was down), with what
/// only this client knew put back where it was (`shown`).
///
/// - Notices, `!` commands, written files, and answered permission requests
///   keep their place after the agent's entries that came before them.
/// - Prompts keep their attachments as they were shown: a replay has the
///   images only, and without their names.
/// - Unanswered permission requests go: they were the old connection's, and
///   the agent asks again over the new one.
/// - Prompts the replay does not have stay at the end: the last `unsent` go
///   out now; any before them were lost with the connection, which a notice
///   says.
fn reconcile(shown: Vec<Entry>, replay: Vec<Entry>, unsent: usize) -> Vec<Entry> {
    // Each entry only this client knows, with how many of the agent's
    // entries came before it.
    let mut local = Vec::new();
    let mut agent = Vec::new();
    for entry in shown {
        match entry {
            Entry::Permission { answer: None, .. } => {}
            Entry::Notice { .. }
            | Entry::Shell { .. }
            | Entry::FileWritten { .. }
            | Entry::Permission { .. } => local.push((agent.len(), entry)),
            Entry::User { .. }
            | Entry::Agent { .. }
            | Entry::Thought { .. }
            | Entry::Tool { .. }
            | Entry::Plan { .. } => agent.push(entry),
        }
    }
    let is_user = |entry: &Entry| matches!(entry, Entry::User { .. });
    let replayed_prompts = replay.iter().filter(|entry| is_user(entry)).count();
    // Where the prompts the replay lacks begin among the agent's entries.
    let cut = agent
        .iter()
        .enumerate()
        .filter(|(_, entry)| is_user(entry))
        .nth(replayed_prompts)
        .map_or(agent.len(), |(index, _)| index);
    let beyond = agent.split_off(cut);
    let mut attachments = agent.into_iter().filter_map(|entry| match entry {
        Entry::User { attachments, .. } => Some(attachments),
        _ => None,
    });
    let mut local = local.into_iter().peekable();
    let mut merged = Vec::new();
    for (index, mut entry) in replay.into_iter().enumerate() {
        while let Some((_, kept)) = local.next_if(|(at, _)| *at <= index && *at < cut) {
            merged.push(kept);
        }
        if let Entry::User {
            attachments: replayed,
            ..
        } = &mut entry
            && let Some(shown) = attachments.next()
            && !shown.is_empty()
        {
            *replayed = shown;
        }
        merged.push(entry);
    }
    while let Some((_, kept)) = local.next_if(|(at, _)| *at <= cut) {
        merged.push(kept);
    }
    let lost = beyond
        .iter()
        .filter(|entry| is_user(entry))
        .count()
        .saturating_sub(unsent);
    for (index, entry) in beyond.into_iter().enumerate() {
        while let Some((_, kept)) = local.next_if(|(at, _)| *at <= cut + index) {
            merged.push(kept);
        }
        merged.push(entry);
    }
    merged.extend(local.map(|(_, kept)| kept));
    if lost > 0 {
        merged.push(Entry::Notice {
            text: "the connection dropped before the agent got the last message; send it again"
                .to_owned(),
            error: true,
        });
    }
    merged
}

/// Folds a message, thought, or tool call into `entries`, the transcript's
/// own or those of a subagent: consecutive chunks join, and a tool call
/// updates its entry wherever it is. Other events are left out.
fn fold(entries: &mut Vec<Entry>, event: SessionEvent) {
    match event {
        SessionEvent::AgentMessage { text } => match entries.last_mut() {
            Some(Entry::Agent { text: last }) => last.push_str(&text),
            _ => entries.push(Entry::Agent { text }),
        },
        SessionEvent::AgentThought { text } => match entries.last_mut() {
            Some(Entry::Thought { text: last }) => last.push_str(&text),
            _ => entries.push(Entry::Thought { text }),
        },
        SessionEvent::ToolCall {
            id,
            title,
            kind,
            status,
            diffs,
        } => match tool_mut(entries, &id) {
            Some(Entry::Tool {
                title: t,
                kind: k,
                status: s,
                diffs: d,
                ..
            }) => {
                *t = title;
                *k = kind;
                *s = status;
                *d = diffs;
            }
            _ => entries.push(Entry::Tool {
                id,
                title,
                kind,
                status,
                diffs,
                children: Vec::new(),
            }),
        },
        SessionEvent::ToolCallUpdate {
            id,
            title,
            status,
            diffs,
        } => match tool_mut(entries, &id) {
            Some(Entry::Tool {
                title: t,
                status: s,
                diffs: d,
                ..
            }) => {
                if let Some(title) = title {
                    *t = title;
                }
                if let Some(status) = status {
                    *s = status;
                }
                if let Some(diffs) = diffs {
                    *d = diffs;
                }
            }
            // An update for a call we never saw: show it rather than drop it.
            _ => entries.push(Entry::Tool {
                title: title.unwrap_or_else(|| id.clone()),
                id,
                kind: "other".to_owned(),
                status: status.unwrap_or(ToolStatus::Pending),
                diffs: diffs.unwrap_or_default(),
                children: Vec::new(),
            }),
        },
        _ => {}
    }
}

/// The latest entry of tool call `id`, among `entries` or the entries of
/// their subagents.
fn tool_mut<'a>(entries: &'a mut [Entry], id: &str) -> Option<&'a mut Entry> {
    entries.iter_mut().rev().find_map(|entry| {
        if matches!(entry, Entry::Tool { id: tid, .. } if tid == id) {
            return Some(entry);
        }
        match entry {
            Entry::Tool { children, .. } => tool_mut(children, id),
            _ => None,
        }
    })
}

/// A past session's title, unless it has none worth showing.
pub fn session_label(past: &PastSession) -> Option<&str> {
    past.title
        .as_deref()
        .map(str::trim)
        .filter(|title| !title.is_empty())
}

/// `2026-10-14T12:00:00Z` as `2026-10-14 12:00`; anything else as sent.
pub fn updated_label(updated_at: &str) -> String {
    let bytes = updated_at.as_bytes();
    let shaped = bytes.len() >= 16
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes[10] == b'T'
        && bytes[13] == b':';
    if shaped {
        format!("{} {}", &updated_at[..10], &updated_at[11..16])
    } else {
        updated_at.to_owned()
    }
}

/// Short text for a usage report, e.g. `53k / 200k (27%) · $0.05`.
pub fn usage_label(usage: &Usage) -> String {
    let mut label = format!("{} / {}", tokens(usage.used), tokens(usage.size));
    if usage.size > 0 {
        let percent = (usage.used as f64 / usage.size as f64 * 100.0).round();
        label.push_str(&format!(" ({percent}%)"));
    }
    if let Some(cost) = &usage.cost {
        let amount = format!("{:.2}", cost.amount);
        match cost.currency.as_str() {
            "USD" => label.push_str(&format!(" · ${amount}")),
            currency => label.push_str(&format!(" · {amount} {currency}")),
        }
    }
    label
}

/// A token count rounded for display: `950`, `1.2k`, `53k`, `1.5M`.
fn tokens(count: u64) -> String {
    let (value, unit) = match count {
        0..1_000 => return count.to_string(),
        // Below 999.5k so rounding never shows `1000k`.
        1_000..999_500 => (count as f64 / 1_000.0, "k"),
        _ => (count as f64 / 1_000_000.0, "M"),
    };
    if value < 10.0 {
        let text = format!("{value:.1}");
        format!("{}{unit}", text.trim_end_matches(".0"))
    } else {
        format!("{}{unit}", value.round())
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    fn folded(events: impl IntoIterator<Item = SessionEvent>) -> Transcript {
        let mut transcript = Transcript::default();
        for event in events {
            transcript.apply(event);
        }
        transcript
    }

    fn ready(agent: Option<&str>, restore: RestoreOutcome) -> SessionEvent {
        SessionEvent::Ready {
            agent: agent.map(str::to_owned),
            session_id: "s".into(),
            restore,
            prompt: PromptCaps::default(),
            config: Vec::new(),
            list_sessions: false,
        }
    }

    fn past(id: &str, title: Option<&str>, updated_at: Option<&str>) -> PastSession {
        PastSession {
            id: id.into(),
            title: title.map(str::to_owned),
            updated_at: updated_at.map(str::to_owned),
        }
    }

    #[test]
    fn history_lists_other_sessions_newest_first() {
        let mut transcript = folded([SessionEvent::Ready {
            agent: None,
            session_id: "now".into(),
            restore: RestoreOutcome::Fresh,
            prompt: PromptCaps::default(),
            config: Vec::new(),
            list_sessions: true,
        }]);
        assert_eq!(transcript.history, History::Available);
        assert!(transcript.history_requested());
        assert_eq!(transcript.history, History::Loading);
        transcript.apply(SessionEvent::Sessions {
            sessions: vec![
                past("undated", None, None),
                past("old", None, Some("2026-10-01T00:00:00Z")),
                past("now", None, Some("2026-10-15T00:00:00Z")),
                past("new", None, Some("2026-10-14T00:00:00Z")),
            ],
        });
        let History::Loaded(sessions) = &transcript.history else {
            panic!("{:?}", transcript.history);
        };
        let ids: Vec<_> = sessions.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["new", "old", "undated"]);
        transcript.apply(SessionEvent::SessionsFailed {
            message: "boom".into(),
        });
        assert_eq!(transcript.history, History::Failed("boom".into()));
    }

    #[rstest]
    #[case::unnamed(vec![], None)]
    #[case::first_prompt(vec![user_said("\n fix the build \nplease"), user_said("and the docs")], Some("fix the build"))]
    #[case::named(vec![user_said("fix the build"), named(Some(" Build fix "))], Some("Build fix"))]
    #[case::renamed(vec![named(Some("a")), named(Some("b"))], Some("b"))]
    #[case::blank_name(vec![user_said("fix the build"), named(Some("  "))], Some("fix the build"))]
    #[case::name_taken_back(vec![user_said("fix the build"), named(Some("a")), named(None)], Some("fix the build"))]
    fn title_is_the_agents_or_the_first_prompt(
        #[case] events: Vec<SessionEvent>,
        #[case] expected: Option<&str>,
    ) {
        assert_eq!(folded(events).title(), expected);
    }

    #[test]
    fn title_follows_the_prompt_the_user_sent() {
        let mut transcript = folded([ready(None, RestoreOutcome::Fresh)]);
        transcript.push_prompt("fix the build", Vec::new());
        assert_eq!(transcript.title(), Some("fix the build"));
    }

    fn user_said(text: &str) -> SessionEvent {
        SessionEvent::UserMessage { text: text.into() }
    }

    fn named(title: Option<&str>) -> SessionEvent {
        SessionEvent::Title {
            title: title.map(str::to_owned),
        }
    }

    #[test]
    fn history_needs_the_capability() {
        let mut transcript = folded([ready(None, RestoreOutcome::Fresh)]);
        assert_eq!(transcript.history, History::Unsupported);
        assert!(!transcript.history_requested());
        assert_eq!(transcript.history, History::Unsupported);
    }

    #[rstest]
    #[case::title(Some(" Fix the build "), Some("Fix the build"))]
    #[case::blank_title(Some("  "), None)]
    #[case::no_title(None, None)]
    fn session_labels(#[case] title: Option<&str>, #[case] expected: Option<&str>) {
        assert_eq!(session_label(&past("abc", title, None)), expected);
    }

    #[rstest]
    #[case::iso("2026-10-14T12:34:56Z", "2026-10-14 12:34")]
    #[case::offset("2026-10-14T12:34:56.789+09:00", "2026-10-14 12:34")]
    #[case::other("yesterday", "yesterday")]
    fn updated_labels(#[case] updated_at: &str, #[case] expected: &str) {
        assert_eq!(updated_label(updated_at), expected);
    }

    #[test]
    fn replayed_history_builds_turns() {
        let transcript = folded([
            SessionEvent::UserMessage { text: "q".into() },
            SessionEvent::UserMessage { text: "?".into() },
            msg("a"),
            ready(None, RestoreOutcome::Replayed),
        ]);
        assert_eq!(
            transcript.entries,
            vec![user("q?"), Entry::Agent { text: "a".into() }]
        );
    }

    #[test]
    fn replayed_images_join_the_users_message() {
        let image = |url: &str| SessionEvent::UserImage { url: url.into() };
        let shown = |url: &str| Sent {
            name: "image".into(),
            image: Some(url.into()),
        };
        let transcript = folded([
            image("data:image/png;base64,YQ=="),
            msg("a"),
            SessionEvent::UserMessage { text: "q".into() },
            image("data:image/png;base64,Yg=="),
            ready(None, RestoreOutcome::Replayed),
        ]);
        assert_eq!(
            transcript.entries,
            vec![
                Entry::User {
                    text: String::new(),
                    attachments: vec![shown("data:image/png;base64,YQ==")],
                },
                Entry::Agent { text: "a".into() },
                Entry::User {
                    text: "q".into(),
                    attachments: vec![shown("data:image/png;base64,Yg==")],
                },
            ]
        );
    }

    #[rstest]
    #[case::resumed(RestoreOutcome::Resumed)]
    #[case::unsupported(RestoreOutcome::Unsupported)]
    #[case::missing(RestoreOutcome::Missing)]
    fn lossy_restores_leave_a_notice(#[case] restore: RestoreOutcome) {
        let transcript = folded([ready(None, restore)]);
        assert!(matches!(
            transcript.entries.as_slice(),
            [Entry::Notice { error: false, .. }]
        ));
    }

    fn user(text: &str) -> Entry {
        Entry::User {
            text: text.into(),
            attachments: Vec::new(),
        }
    }

    fn msg(text: &str) -> SessionEvent {
        SessionEvent::AgentMessage { text: text.into() }
    }

    fn tool(id: &str, status: ToolStatus) -> SessionEvent {
        SessionEvent::ToolCall {
            id: id.into(),
            title: format!("tool {id}"),
            kind: "read".into(),
            status,
            diffs: Vec::new(),
        }
    }

    fn plan(contents: &[&str]) -> SessionEvent {
        SessionEvent::Plan {
            entries: contents
                .iter()
                .map(|content| PlanItem {
                    content: (*content).into(),
                    done: false,
                    active: false,
                })
                .collect(),
        }
    }

    #[test]
    fn usage_keeps_the_latest_report() {
        let report = |used| {
            SessionEvent::Usage(Usage {
                used,
                size: 100,
                cost: None,
            })
        };
        let transcript = folded([report(10), msg("a"), report(20)]);
        assert_eq!(transcript.usage.map(|u| u.used), Some(20));
        assert_eq!(transcript.entries, vec![Entry::Agent { text: "a".into() }]);
    }

    #[rstest]
    #[case::small(950, 1_000, None, "950 / 1k (95%)")]
    #[case::thousands(53_000, 200_000, Some((0.045, "USD")), "53k / 200k (27%) · $0.04")]
    #[case::fraction(1_234, 1_000_000, None, "1.2k / 1M (0%)")]
    #[case::near_million(999_600, 2_000_000, None, "1M / 2M (50%)")]
    #[case::other_currency(0, 0, Some((1.5, "EUR")), "0 / 0 · 1.50 EUR")]
    fn usage_labels(
        #[case] used: u64,
        #[case] size: u64,
        #[case] cost: Option<(f64, &str)>,
        #[case] expected: &str,
    ) {
        let usage = Usage {
            used,
            size,
            cost: cost.map(|(amount, currency)| ukai_acp::Cost {
                amount,
                currency: currency.into(),
            }),
        };
        assert_eq!(usage_label(&usage), expected);
    }

    fn update() -> Update {
        Update {
            package: "@scope/agent".into(),
            current: "1.0.0".into(),
            latest: "1.2.0".into(),
        }
    }

    #[test]
    fn an_offered_update_waits_until_skipped_or_closed() {
        let mut transcript = folded([SessionEvent::UpdateAvailable(update())]);
        assert_eq!(transcript.update, Some(update()));
        assert_eq!(transcript.status, Status::Connecting);
        assert_eq!(
            Alert::of(&SessionEvent::UpdateAvailable(update())),
            Some(Alert::UpdateAvailable {
                package: "@scope/agent".into(),
                latest: "1.2.0".into(),
            })
        );
        transcript.skip_update();
        assert_eq!(transcript.update, None);

        let closed = folded([
            SessionEvent::UpdateAvailable(update()),
            SessionEvent::Closed { error: None },
        ]);
        assert_eq!(closed.update, None);
    }

    #[test]
    fn a_missed_update_is_an_error_notice() {
        let transcript = folded([
            SessionEvent::UpdateMissed(update()),
            ready(None, RestoreOutcome::Fresh),
        ]);
        assert!(matches!(
            transcript.entries.as_slice(),
            [Entry::Notice { text, error: true }] if text.contains("still 1.0.0 (latest 1.2.0)")
        ));
        assert_eq!(transcript.update, None);
        assert_eq!(transcript.status, Status::Idle);
    }

    #[test]
    fn ready_sets_agent_and_idle() {
        let transcript = folded([ready(Some("Claude"), RestoreOutcome::Fresh)]);
        assert_eq!(transcript.agent.as_deref(), Some("Claude"));
        assert_eq!(transcript.status, Status::Idle);
    }

    #[test]
    fn consecutive_chunks_merge_but_not_across_other_entries() {
        let transcript = folded([msg("a"), msg("b"), tool("1", ToolStatus::Pending), msg("c")]);
        assert_eq!(
            transcript.entries,
            vec![
                Entry::Agent { text: "ab".into() },
                tool_entry("1", ToolStatus::Pending, vec![]),
                Entry::Agent { text: "c".into() },
            ]
        );
    }

    fn tool_entry(id: &str, status: ToolStatus, children: Vec<Entry>) -> Entry {
        Entry::Tool {
            id: id.into(),
            title: format!("tool {id}"),
            kind: "read".into(),
            status,
            diffs: Vec::new(),
            children,
        }
    }

    fn subagent(parent: &str, event: SessionEvent) -> SessionEvent {
        SessionEvent::Subagent {
            parent: parent.into(),
            event: Box::new(event),
        }
    }

    #[test]
    fn subagents_fold_under_the_call_that_started_them() {
        let transcript = folded([
            msg("looking"),
            tool("a", ToolStatus::InProgress),
            tool("b", ToolStatus::InProgress),
            subagent("a", msg("found ")),
            subagent("b", tool("grep", ToolStatus::InProgress)),
            subagent("a", msg("it")),
            // An update that does not name its subagent still finds its call.
            SessionEvent::ToolCallUpdate {
                id: "grep".into(),
                title: None,
                status: Some(ToolStatus::Completed),
                diffs: None,
            },
            subagent("gone", msg("orphan")),
        ]);
        assert_eq!(
            transcript.entries,
            vec![
                Entry::Agent {
                    text: "looking".into()
                },
                tool_entry(
                    "a",
                    ToolStatus::InProgress,
                    vec![Entry::Agent {
                        text: "found it".into()
                    }],
                ),
                tool_entry(
                    "b",
                    ToolStatus::InProgress,
                    vec![tool_entry("grep", ToolStatus::Completed, vec![])],
                ),
                Entry::Agent {
                    text: "orphan".into()
                },
            ]
        );
    }

    #[rstest]
    #[case::known_call(
        vec![tool("1", ToolStatus::Pending)],
        Entry::Tool { id: "1".into(), title: "renamed".into(), kind: "read".into(), status: ToolStatus::Completed, diffs: vec![diff()], children: vec![] },
    )]
    #[case::unknown_call(
        vec![],
        Entry::Tool { id: "1".into(), title: "renamed".into(), kind: "other".into(), status: ToolStatus::Completed, diffs: vec![diff()], children: vec![] },
    )]
    fn tool_updates_patch_in_place(#[case] before: Vec<SessionEvent>, #[case] expected: Entry) {
        let mut transcript = folded(before);
        transcript.apply(SessionEvent::ToolCallUpdate {
            id: "1".into(),
            title: Some("renamed".into()),
            status: Some(ToolStatus::Completed),
            diffs: Some(vec![diff()]),
        });
        assert_eq!(transcript.entries, vec![expected]);
    }

    fn diff() -> FileDiff {
        FileDiff {
            path: "/p/a.txt".into(),
            old_text: None,
            new_text: "a".into(),
        }
    }

    #[test]
    fn tool_updates_without_content_keep_diffs() {
        let mut transcript = folded([SessionEvent::ToolCall {
            id: "1".into(),
            title: "edit".into(),
            kind: "edit".into(),
            status: ToolStatus::Pending,
            diffs: vec![diff()],
        }]);
        transcript.apply(SessionEvent::ToolCallUpdate {
            id: "1".into(),
            title: None,
            status: Some(ToolStatus::Completed),
            diffs: None,
        });
        assert!(matches!(
            transcript.entries.as_slice(),
            [Entry::Tool { diffs, .. }] if *diffs == vec![diff()]
        ));
    }

    #[rstest]
    #[case::finished(SessionEvent::TurnEnded { stop_reason: "end_turn".into() }, Some(Alert::Finished))]
    #[case::refused(SessionEvent::TurnEnded { stop_reason: "refusal".into() }, Some(Alert::Finished))]
    #[case::cancelled(SessionEvent::TurnEnded { stop_reason: "cancelled".into() }, None)]
    #[case::permission(
        SessionEvent::PermissionRequested { id: 0, title: "rm".into(), options: vec![] },
        Some(Alert::NeedsInput { title: "rm".into() }),
    )]
    #[case::failed(
        SessionEvent::PromptFailed { message: "boom".into() },
        Some(Alert::Failed { message: "boom".into() }),
    )]
    #[case::dropped(
        SessionEvent::Closed { error: Some("eof".into()) },
        Some(Alert::Failed { message: "disconnected: eof".into() }),
    )]
    #[case::closed_cleanly(SessionEvent::Closed { error: None }, None)]
    #[case::chunk(SessionEvent::AgentMessage { text: "x".into() }, None)]
    fn alerts(#[case] event: SessionEvent, #[case] alert: Option<Alert>) {
        assert_eq!(Alert::of(&event), alert);
    }

    #[test]
    fn ready_records_prompt_caps() {
        let caps = PromptCaps {
            image: true,
            embedded_context: false,
            steering: false,
        };
        let transcript = folded([SessionEvent::Ready {
            agent: None,
            session_id: "s".into(),
            restore: RestoreOutcome::Fresh,
            prompt: caps,
            config: Vec::new(),
            list_sessions: false,
        }]);
        assert_eq!(transcript.prompt_caps, caps);
    }

    fn effort(current: &str) -> ConfigOption {
        ConfigOption {
            id: "effort".into(),
            name: "Effort".into(),
            description: None,
            category: Some("thought_level".into()),
            value: ukai_acp::ConfigValue::Select {
                current: current.into(),
                choices: Vec::new(),
            },
        }
    }

    #[test]
    fn config_follows_ready_and_updates_and_failures_leave_it() {
        let mut transcript = folded([SessionEvent::Ready {
            agent: None,
            session_id: "s".into(),
            restore: RestoreOutcome::Fresh,
            prompt: PromptCaps::default(),
            config: vec![effort("low")],
            list_sessions: false,
        }]);
        assert_eq!(transcript.config, [effort("low")]);
        transcript.apply(SessionEvent::ConfigChanged {
            options: vec![effort("high")],
        });
        assert_eq!(transcript.config, [effort("high")]);
        transcript.apply(SessionEvent::ConfigFailed {
            message: "no".into(),
        });
        assert_eq!(transcript.config, [effort("high")]);
        assert!(matches!(
            transcript.entries.as_slice(),
            [Entry::Notice { error: true, .. }]
        ));
        assert_eq!(transcript.status, Status::Idle);
    }

    fn queued(text: &str, files: &[&str]) -> Queued {
        Queued {
            text: text.into(),
            attachments: files
                .iter()
                .map(|name| {
                    let link = Attachment::Link {
                        name: (*name).into(),
                        uri: format!("file:///{name}"),
                    };
                    ((*name).to_owned(), link)
                })
                .collect(),
        }
    }

    fn running() -> Transcript {
        let mut transcript = folded([ready(None, RestoreOutcome::Fresh)]);
        transcript.push_prompt("first", Vec::new());
        transcript
    }

    #[test]
    fn queued_waits_for_the_turn_to_stop() {
        let mut transcript = running();
        transcript.apply(SessionEvent::PermissionRequested {
            id: 1,
            title: "write".into(),
            options: Vec::new(),
        });
        transcript.queue(queued("instead", &[]));
        assert!(!transcript.awaiting_permission());
        assert_eq!(transcript.take_queued(), None);

        transcript.apply(SessionEvent::TurnEnded {
            stop_reason: "cancelled".into(),
        });
        assert_eq!(transcript.take_queued(), Some(queued("instead", &[])));
        assert_eq!(transcript.take_queued(), None);
    }

    #[rstest]
    #[case::both(queued("a", &["x"]), queued("b", &["y"]), queued("a\n\nb", &["x", "y"]))]
    #[case::first_empty(queued("", &["x"]), queued("b", &[]), queued("b", &["x"]))]
    #[case::second_empty(queued("a", &[]), queued("", &["y"]), queued("a", &["y"]))]
    fn queued_before_the_stop_join(
        #[case] first: Queued,
        #[case] second: Queued,
        #[case] joined: Queued,
    ) {
        let mut transcript = running();
        transcript.queue(first);
        transcript.queue(second);
        transcript.apply(SessionEvent::PromptFailed {
            message: "boom".into(),
        });
        assert_eq!(transcript.take_queued(), Some(joined));
    }

    #[test]
    fn queued_is_handed_back_when_the_session_closes() {
        let mut transcript = running();
        transcript.queue(queued("instead", &["x"]));
        transcript.apply(SessionEvent::Closed { error: None });
        let queued = transcript.take_queued();
        assert_eq!(
            queued.as_ref().map(Queued::shown),
            Some(vec![Sent {
                name: "x".into(),
                image: None
            }])
        );
        assert_eq!(queued.map(|q| q.prompt().text).as_deref(), Some("instead"));
    }

    #[test]
    fn commands_are_replaced_without_entries() {
        let command = |name: &str| SlashCommand {
            name: name.into(),
            description: String::new(),
            hint: None,
        };
        let transcript = folded([
            SessionEvent::Commands {
                commands: vec![command("a"), command("b")],
            },
            SessionEvent::Commands {
                commands: vec![command("c")],
            },
        ]);
        assert_eq!(transcript.commands, vec![command("c")]);
        assert!(transcript.entries.is_empty());
    }

    #[test]
    fn plan_is_replaced_within_a_turn_and_new_per_turn() {
        let mut transcript = folded([plan(&["a"]), msg("x"), plan(&["a", "b"])]);
        assert_eq!(transcript.entries.len(), 2);
        assert!(matches!(&transcript.entries[0], Entry::Plan { items } if items.len() == 2));

        transcript.push_prompt("next", Vec::new());
        transcript.apply(plan(&["c"]));
        assert_eq!(transcript.entries.len(), 4);
    }

    #[test]
    fn prompt_and_turn_end_toggle_busy() {
        let mut transcript = folded([ready(None, RestoreOutcome::Fresh)]);
        let image = Attachment::Image {
            mime_type: "image/png".into(),
            data: b"hi".to_vec(),
        };
        let sent = Sent::new("a.png", &image);
        assert_eq!(sent.image.as_deref(), Some("data:image/png;base64,aGk="));
        transcript.push_prompt("hi", vec![sent.clone()]);
        assert_eq!(transcript.status, Status::Busy);
        transcript.apply(SessionEvent::TurnEnded {
            stop_reason: "end_turn".into(),
        });
        assert_eq!(transcript.status, Status::Idle);
        assert_eq!(
            transcript.entries,
            vec![Entry::User {
                text: "hi".into(),
                attachments: vec![sent]
            }]
        );
    }

    #[rstest]
    #[case::cancelled(
        SessionEvent::TurnEnded { stop_reason: "cancelled".into() },
        Entry::Notice { text: "turn ended: cancelled".into(), error: false },
        Status::Idle,
    )]
    #[case::failed(
        SessionEvent::PromptFailed { message: "auth required".into() },
        Entry::Notice { text: "auth required".into(), error: true },
        Status::Idle,
    )]
    #[case::closed(
        SessionEvent::Closed { error: Some("eof".into()) },
        Entry::Notice { text: "disconnected: eof".into(), error: true },
        Status::Closed { error: Some("eof".into()) },
    )]
    fn abnormal_endings_leave_a_notice(
        #[case] event: SessionEvent,
        #[case] notice: Entry,
        #[case] status: Status,
    ) {
        let mut transcript = Transcript::default();
        transcript.push_prompt("hi", Vec::new());
        transcript.apply(event);
        assert_eq!(transcript.entries.last(), Some(&notice));
        assert_eq!(transcript.status, status);
    }

    #[test]
    fn permissions_are_answered_individually_or_by_cancel() {
        let request = |id| SessionEvent::PermissionRequested {
            id,
            title: "t".into(),
            options: vec![],
        };
        let mut transcript = folded([request(0), request(1), request(2)]);
        assert!(transcript.awaiting_permission());
        transcript.answer_permission(1, "Allow");
        assert!(transcript.awaiting_permission());
        transcript.cancel();
        assert!(!transcript.awaiting_permission());
        let answers: Vec<_> = transcript
            .entries
            .iter()
            .map(|entry| match entry {
                Entry::Permission { answer, .. } => answer.as_deref(),
                _ => None,
            })
            .collect();
        assert_eq!(
            answers,
            [Some("cancelled"), Some("Allow"), Some("cancelled")]
        );
    }

    #[test]
    fn finished_commands_go_with_the_next_prompt_once() {
        let ran = |text: &str| CommandOutput {
            exit_code: Some(0),
            output: text.into(),
            ..CommandOutput::default()
        };
        let mut transcript = Transcript::default();
        let old = transcript.start_shell("ls");
        transcript.finish_shell(old, Ok(ran("a")));
        transcript.push_prompt("look", Vec::new());
        let done = transcript.start_shell("cat a");
        let failed = transcript.start_shell("nope");
        let running = transcript.start_shell("sleep 9");
        transcript.finish_shell(done, Ok(ran("hello")));
        transcript.finish_shell(failed, Err("no workspace".into()));
        let unsent: Vec<(&str, &str)> = transcript
            .unsent_shell()
            .into_iter()
            .map(|(command, output)| (command, output.output.as_str()))
            .collect();
        assert_eq!(unsent, [("cat a", "hello")]);
        transcript.push_prompt("and now", Vec::new());
        assert!(transcript.unsent_shell().is_empty());
        // One that was still running then goes with the prompt after it ends.
        transcript.finish_shell(running, Ok(ran("woke")));
        let unsent: Vec<&str> = transcript
            .unsent_shell()
            .into_iter()
            .map(|(c, _)| c)
            .collect();
        assert_eq!(unsent, ["sleep 9"]);
    }

    /// A file attached as `name`, as a message shows it.
    fn file(name: &str) -> Sent {
        Sent {
            name: name.into(),
            image: None,
        }
    }

    fn prompted(text: &str, attachments: &[&str]) -> Entry {
        Entry::User {
            text: text.into(),
            attachments: attachments.iter().copied().map(file).collect(),
        }
    }

    fn agent(text: &str) -> Entry {
        Entry::Agent { text: text.into() }
    }

    fn said(text: &str) -> SessionEvent {
        SessionEvent::AgentMessage { text: text.into() }
    }

    fn asked(id: PermissionId) -> SessionEvent {
        SessionEvent::PermissionRequested {
            id,
            title: "write".into(),
            options: Vec::new(),
        }
    }

    /// A session that ran `!ls`, then a prompt with an attachment that the
    /// agent began answering.
    fn mid_turn() -> Transcript {
        let mut transcript = folded([ready(None, RestoreOutcome::Fresh)]);
        let shell = transcript.start_shell("ls");
        transcript.finish_shell(shell, Err("no such command".into()));
        transcript.push_prompt("slow", vec![file("a.png")]);
        transcript.apply(said("part 1"));
        transcript
    }

    #[test]
    fn a_reconnect_shows_the_replay_once_without_losing_what_only_the_client_knew() {
        let mut transcript = mid_turn();
        let shown = transcript.entries.clone();
        transcript.apply(SessionEvent::Reconnecting { attempt: 1 });
        for event in [
            SessionEvent::UserMessage {
                text: "slow".into(),
            },
            // The replay has the image, without the name it was sent under.
            SessionEvent::UserImage {
                url: "data:image/png;base64,aGk=".into(),
            },
            said("part 1"),
            said(", part 2"),
        ] {
            transcript.apply(event);
        }
        // The replay waits for the session to be back; the old history stays up.
        assert_eq!(transcript.entries, shown);
        assert_eq!(transcript.status, Status::Busy);
        assert_eq!(
            transcript.connection,
            Connection::Reconnecting { attempt: 1 }
        );

        transcript.apply(ready(None, RestoreOutcome::Replayed));
        transcript.apply(SessionEvent::TurnRunning);
        assert_eq!(
            transcript.entries,
            vec![
                shown[0].clone(),
                prompted("slow", &["a.png"]),
                agent("part 1, part 2"),
            ]
        );
        assert_eq!(transcript.status, Status::Busy);
        assert_eq!(transcript.connection, Connection::Connected);
        transcript.apply(SessionEvent::TurnEnded {
            stop_reason: "end_turn".into(),
        });
        assert_eq!(transcript.status, Status::Idle);
    }

    #[test]
    fn a_turn_that_ended_while_away_is_idle_after_the_reconnect() {
        let mut transcript = mid_turn();
        for event in [
            SessionEvent::Reconnecting { attempt: 1 },
            SessionEvent::UserMessage {
                text: "slow".into(),
            },
            said("part 1, done"),
            ready(None, RestoreOutcome::Replayed),
        ] {
            transcript.apply(event);
        }
        assert_eq!(transcript.status, Status::Idle);
        assert_eq!(transcript.entries.last(), Some(&agent("part 1, done")));
    }

    #[test]
    fn a_prompt_while_detached_goes_after_the_replay() {
        let mut transcript = folded([ready(None, RestoreOutcome::Fresh)]);
        transcript.push_prompt("hello", Vec::new());
        transcript.apply(said("hi"));
        transcript.apply(SessionEvent::TurnEnded {
            stop_reason: "end_turn".into(),
        });
        transcript.apply(SessionEvent::Detached);
        // Detached is no state for the user to act on.
        assert_eq!(transcript.status, Status::Idle);
        assert_eq!(transcript.connection, Connection::Detached);

        transcript.push_prompt("again", Vec::new());
        for event in [
            SessionEvent::Reconnecting { attempt: 1 },
            SessionEvent::UserMessage {
                text: "hello".into(),
            },
            said("hi"),
            ready(None, RestoreOutcome::Replayed),
        ] {
            transcript.apply(event);
        }
        assert_eq!(
            transcript.entries,
            vec![prompted("hello", &[]), agent("hi"), prompted("again", &[])]
        );
        // The prompt goes out now, so its turn is on.
        assert_eq!(transcript.status, Status::Busy);
        transcript.apply(said("ok"));
        assert_eq!(transcript.entries.last(), Some(&agent("ok")));
    }

    #[test]
    fn a_prompt_the_replay_lacks_was_lost() {
        let mut transcript = folded([ready(None, RestoreOutcome::Fresh)]);
        transcript.push_prompt("hello", Vec::new());
        transcript.apply(SessionEvent::Reconnecting { attempt: 1 });
        transcript.apply(ready(None, RestoreOutcome::Replayed));
        assert!(matches!(
            transcript.entries.as_slice(),
            [Entry::User { .. }, Entry::Notice { error: true, .. }]
        ));
        assert_eq!(transcript.status, Status::Idle);
    }

    #[test]
    fn a_permission_request_is_asked_again_over_the_new_connection() {
        let mut transcript = mid_turn();
        transcript.apply(asked(0));
        transcript.apply(asked(1));
        transcript.answer_permission(0, "Allow");
        for event in [
            SessionEvent::Reconnecting { attempt: 1 },
            SessionEvent::UserMessage {
                text: "slow".into(),
            },
            said("part 1"),
            asked(2),
            ready(None, RestoreOutcome::Replayed),
        ] {
            transcript.apply(event);
        }
        let permissions: Vec<(PermissionId, Option<&str>)> = transcript
            .entries
            .iter()
            .filter_map(|entry| match entry {
                Entry::Permission { id, answer, .. } => Some((*id, answer.as_deref())),
                _ => None,
            })
            .collect();
        assert_eq!(permissions, [(2, None), (0, Some("Allow"))]);
        assert!(transcript.awaiting_permission());
    }

    #[rstest]
    #[case::resumed(RestoreOutcome::Resumed)]
    #[case::missing(RestoreOutcome::Missing)]
    fn a_reconnect_without_a_replay_keeps_what_is_shown(#[case] restore: RestoreOutcome) {
        let mut transcript = mid_turn();
        transcript.apply(asked(0));
        let mut shown = transcript.entries.clone();
        shown.pop();
        transcript.apply(SessionEvent::Reconnecting { attempt: 2 });
        transcript.apply(ready(None, restore));
        assert_eq!(transcript.entries[..shown.len()], shown[..]);
        assert!(matches!(
            transcript.entries[shown.len()..],
            [Entry::Notice { error: false, .. }]
        ));
    }

    #[test]
    fn reconnecting_is_quiet_until_it_gives_up() {
        let mut transcript = mid_turn();
        let shown = transcript.entries.clone();
        for event in [
            SessionEvent::Reconnecting { attempt: 1 },
            SessionEvent::Preparing {
                message: "Starting the workspace".into(),
            },
            SessionEvent::UserMessage {
                text: "slow".into(),
            },
            SessionEvent::Reconnecting { attempt: 2 },
        ] {
            assert_eq!(Alert::of(&event), None);
            transcript.apply(event);
        }
        assert_eq!(transcript.entries, shown);
        transcript.apply(SessionEvent::Closed {
            error: Some("could not reconnect".into()),
        });
        assert_eq!(
            transcript.status,
            Status::Closed {
                error: Some("could not reconnect".into())
            }
        );
        assert_eq!(transcript.connection, Connection::Connected);
        assert_eq!(transcript.entries.len(), shown.len() + 1);
    }
}
