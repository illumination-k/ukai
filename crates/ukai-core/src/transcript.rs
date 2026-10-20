//! The visible history of one agent session, folded from [`SessionEvent`]s.

use std::path::PathBuf;

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entry {
    User {
        text: String,
        /// Names of the files attached to the turn.
        attachments: Vec<String>,
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
    /// The agent's other sessions the pane could switch to.
    pub history: History,
    /// A newer release of the agent, offered before the session is created:
    /// restart to update, or [`Self::skip_update`] and start as it is.
    pub update: Option<Update>,
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

/// A message sent while a turn runs to an agent without
/// [`PromptCaps::steering`]: the turn is stopped, and this is sent as the next one.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Queued {
    pub text: String,
    /// Each attachment with the name it is shown under.
    pub attachments: Vec<(String, Attachment)>,
}

impl Queued {
    pub fn names(&self) -> Vec<String> {
        self.attachments
            .iter()
            .map(|(name, _)| name.clone())
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
            history: History::Unsupported,
            usage: None,
            config: Vec::new(),
            commands: Vec::new(),
            queued: None,
            update: None,
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
    /// Note that the session list was asked for; `false` if the agent cannot list.
    pub fn history_requested(&mut self) -> bool {
        if self.history == History::Unsupported {
            return false;
        }
        self.history = History::Loading;
        true
    }

    /// Record a prompt the user sent, with the names of its attachments.
    pub fn push_prompt(&mut self, text: impl Into<String>, attachments: Vec<String>) {
        self.entries.push(Entry::User {
            text: text.into(),
            attachments,
        });
        self.status = Status::Busy;
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
            SessionEvent::Preparing { message } => self.entries.push(Entry::Notice {
                text: message,
                error: false,
            }),
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
                self.status = Status::Idle;
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
            SessionEvent::UserMessage { text } => match self.entries.last_mut() {
                Some(Entry::User { text: last, .. }) => last.push_str(&text),
                _ => self.entries.push(Entry::User {
                    text,
                    attachments: Vec::new(),
                }),
            },
            SessionEvent::AgentMessage { text } => match self.entries.last_mut() {
                Some(Entry::Agent { text: last }) => last.push_str(&text),
                _ => self.entries.push(Entry::Agent { text }),
            },
            SessionEvent::AgentThought { text } => match self.entries.last_mut() {
                Some(Entry::Thought { text: last }) => last.push_str(&text),
                _ => self.entries.push(Entry::Thought { text }),
            },
            SessionEvent::ToolCall {
                id,
                title,
                kind,
                status,
                diffs,
            } => match self.tool_mut(&id) {
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
                _ => self.entries.push(Entry::Tool {
                    id,
                    title,
                    kind,
                    status,
                    diffs,
                }),
            },
            SessionEvent::ToolCallUpdate {
                id,
                title,
                status,
                diffs,
            } => match self.tool_mut(&id) {
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
                _ => self.entries.push(Entry::Tool {
                    title: title.unwrap_or_else(|| id.clone()),
                    id,
                    kind: "other".to_owned(),
                    status: status.unwrap_or(ToolStatus::Pending),
                    diffs: diffs.unwrap_or_default(),
                }),
            },
            SessionEvent::Plan { entries: items } => {
                // One plan per turn: later plans replace the earlier one in place.
                let turn_start = self
                    .entries
                    .iter()
                    .rposition(|entry| matches!(entry, Entry::User { .. }))
                    .map_or(0, |index| index + 1);
                match self.entries[turn_start..]
                    .iter_mut()
                    .find(|entry| matches!(entry, Entry::Plan { .. }))
                {
                    Some(Entry::Plan { items: current }) => *current = items,
                    _ => self.entries.push(Entry::Plan { items }),
                }
            }
            SessionEvent::Commands { commands } => self.commands = commands,
            SessionEvent::PermissionRequested { id, title, options } => {
                self.entries.push(Entry::Permission {
                    id,
                    title,
                    options,
                    answer: None,
                });
            }
            SessionEvent::FileWritten { path } => self.entries.push(Entry::FileWritten { path }),
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
                self.status = Status::Closed { error };
            }
        }
    }

    fn tool_mut(&mut self, id: &str) -> Option<&mut Entry> {
        self.entries
            .iter_mut()
            .rev()
            .find(|entry| matches!(entry, Entry::Tool { id: tid, .. } if tid == id))
    }
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
                Entry::Tool {
                    id: "1".into(),
                    title: "tool 1".into(),
                    kind: "read".into(),
                    status: ToolStatus::Pending,
                    diffs: Vec::new(),
                },
                Entry::Agent { text: "c".into() },
            ]
        );
    }

    #[rstest]
    #[case::known_call(
        vec![tool("1", ToolStatus::Pending)],
        Entry::Tool { id: "1".into(), title: "renamed".into(), kind: "read".into(), status: ToolStatus::Completed, diffs: vec![diff()] },
    )]
    #[case::unknown_call(
        vec![],
        Entry::Tool { id: "1".into(), title: "renamed".into(), kind: "other".into(), status: ToolStatus::Completed, diffs: vec![diff()] },
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
            queued.as_ref().map(Queued::names),
            Some(vec!["x".to_owned()])
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
        transcript.push_prompt("hi", vec!["a.png".into()]);
        assert_eq!(transcript.status, Status::Busy);
        transcript.apply(SessionEvent::TurnEnded {
            stop_reason: "end_turn".into(),
        });
        assert_eq!(transcript.status, Status::Idle);
        assert_eq!(
            transcript.entries,
            vec![Entry::User {
                text: "hi".into(),
                attachments: vec!["a.png".into()]
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
}
