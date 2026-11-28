//! What each turn of a session took — time, tokens, tool calls — to find
//! where agents spend their turns (see "ターンのメトリクス" in
//! `docs/design.md`). A gateway agent's relay folds its sessions' events into
//! a [`TurnRecord`] per turn with [`Recorder`] and sends each to the gateway
//! ([`MetricsEndpoint`]), which has `ukai-store` keep it. The app reads a
//! workspace's turns back through the gateway ([`MetricsReader`]).
//!
//! Times are taken as events arrive: a phase runs from its first event until
//! the next event of another kind.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::{ConfigOption, ConfigValue, ResolveError, SessionEvent, TokenUsage, ToolStatus, Usage};

/// One turn, as [`Recorder`] measured it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TurnRecord {
    pub session_id: Option<String>,
    pub agent: Option<String>,
    /// The model and reasoning effort the agent's settings named at the end.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// When the turn started, in milliseconds since the Unix epoch.
    pub started_at_ms: u64,
    /// The agent's stop reason, or `failed` / `closed`.
    pub outcome: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub duration_ms: u64,
    /// From the start until the agent's first message, thought, or tool call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_output_ms: Option<u64>,
    /// Spent streaming the agent's reasoning.
    pub thinking_ms: u64,
    /// Spent streaming the agent's reply.
    pub message_ms: u64,
    /// Spent waiting for the user to answer permission requests.
    pub permission_ms: u64,
    pub tools: Vec<ToolRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<Tokens>,
    /// The latest context use and cumulative cost the agent reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<Context>,
    /// The turn was running before the pane joined it, so it started earlier.
    #[serde(default, skip_serializing_if = "is_false")]
    pub joined: bool,
    /// The connection dropped during the turn; its times include the gap.
    #[serde(default, skip_serializing_if = "is_false")]
    pub reconnected: bool,
}

/// A tool call of a turn, the agent's own or one of its subagents'.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolRecord {
    pub kind: String,
    pub title: String,
    /// `completed`, `failed`, or the status it had when the turn ended.
    pub status: String,
    /// From the call until it completed or failed; `None` if it did not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// The tool call that started the subagent making this one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
}

/// The tokens the agent says the turn took.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tokens {
    pub total: u64,
    pub input: u64,
    pub output: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thought: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_read: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_write: Option<u64>,
}

impl From<TokenUsage> for Tokens {
    fn from(usage: TokenUsage) -> Self {
        Self {
            total: usage.total,
            input: usage.input,
            output: usage.output,
            thought: usage.thought,
            cached_read: usage.cached_read,
            cached_write: usage.cached_write,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Context {
    pub used: u64,
    pub size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub currency: Option<String>,
}

impl From<&Usage> for Context {
    fn from(usage: &Usage) -> Self {
        Self {
            used: usage.used,
            size: usage.size,
            cost: usage.cost.as_ref().map(|cost| cost.amount),
            currency: usage.cost.as_ref().map(|cost| cost.currency.clone()),
        }
    }
}

#[expect(clippy::trivially_copy_pass_by_ref, reason = "serde's signature")]
fn is_false(value: &bool) -> bool {
    !value
}

/// Folds a session's events into a [`TurnRecord`] per turn.
#[derive(Debug, Default)]
pub struct Recorder {
    session_id: Option<String>,
    agent: Option<String>,
    model: Option<String>,
    effort: Option<String>,
    usage: Option<Usage>,
    /// Events are live: the session is ready and not replaying its history.
    live: bool,
    turn: Option<Turn>,
}

#[derive(Debug)]
struct Turn {
    started: Instant,
    started_at: SystemTime,
    joined: bool,
    reconnected: bool,
    first_output: Option<Duration>,
    /// The phase streaming now and since when.
    phase: Option<(Phase, Instant)>,
    thinking: Duration,
    message: Duration,
    permission: Duration,
    tools: Vec<Tool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Thinking,
    Message,
    Permission,
}

#[derive(Debug)]
struct Tool {
    id: String,
    parent: Option<String>,
    kind: String,
    title: String,
    status: ToolStatus,
    started: Instant,
    duration: Option<Duration>,
}

impl Turn {
    fn new(now: Instant, wall: SystemTime, joined: bool) -> Self {
        Self {
            started: now,
            started_at: wall,
            joined,
            reconnected: false,
            first_output: None,
            phase: None,
            thinking: Duration::ZERO,
            message: Duration::ZERO,
            permission: Duration::ZERO,
            tools: Vec::new(),
        }
    }

    /// An event of the turn arrived at `now`: the phase streaming so far
    /// ends unless the event continues it, and `next` starts if it is one.
    fn mark(&mut self, now: Instant, next: Option<Phase>) {
        match self.phase {
            Some((phase, _)) if Some(phase) == next && phase != Phase::Permission => return,
            Some((phase, since)) => {
                let spent = now.saturating_duration_since(since);
                match phase {
                    Phase::Thinking => self.thinking += spent,
                    Phase::Message => self.message += spent,
                    Phase::Permission => self.permission += spent,
                }
            }
            None => {}
        }
        self.phase = next.map(|phase| (phase, now));
    }

    fn output(&mut self, now: Instant) {
        if self.first_output.is_none() {
            self.first_output = Some(now.saturating_duration_since(self.started));
        }
    }

    fn tool_started(
        &mut self,
        id: &str,
        parent: Option<&str>,
        kind: &str,
        title: &str,
        status: ToolStatus,
        now: Instant,
    ) {
        // An agent may send a call again to fill it in.
        if let Some(tool) = self.tools.iter_mut().find(|tool| tool.id == id) {
            tool.title = title.to_owned();
            tool.kind = kind.to_owned();
            tool.update(status, now);
            return;
        }
        let mut tool = Tool {
            id: id.to_owned(),
            parent: parent.map(str::to_owned),
            kind: kind.to_owned(),
            title: title.to_owned(),
            status: ToolStatus::Pending,
            started: now,
            duration: None,
        };
        tool.update(status, now);
        self.tools.push(tool);
    }

    fn tool_updated(
        &mut self,
        id: &str,
        title: Option<&str>,
        status: Option<ToolStatus>,
        now: Instant,
    ) {
        let Some(tool) = self.tools.iter_mut().find(|tool| tool.id == id) else {
            return;
        };
        if let Some(title) = title {
            title.clone_into(&mut tool.title);
        }
        if let Some(status) = status {
            tool.update(status, now);
        }
    }

    fn end(
        mut self,
        now: Instant,
        outcome: String,
        error: Option<String>,
        tokens: Option<TokenUsage>,
        recorder: &Recorder,
    ) -> TurnRecord {
        self.mark(now, None);
        TurnRecord {
            session_id: recorder.session_id.clone(),
            agent: recorder.agent.clone(),
            model: recorder.model.clone(),
            effort: recorder.effort.clone(),
            started_at_ms: millis(
                self.started_at
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default(),
            ),
            outcome,
            error,
            duration_ms: millis(now.saturating_duration_since(self.started)),
            first_output_ms: self.first_output.map(millis),
            thinking_ms: millis(self.thinking),
            message_ms: millis(self.message),
            permission_ms: millis(self.permission),
            tools: self
                .tools
                .into_iter()
                .map(|tool| ToolRecord {
                    kind: tool.kind,
                    title: tool.title,
                    status: status_label(tool.status).to_owned(),
                    duration_ms: tool.duration.map(millis),
                    parent: tool.parent,
                })
                .collect(),
            tokens: tokens.map(Tokens::from),
            context: recorder.usage.as_ref().map(Context::from),
            joined: self.joined,
            reconnected: self.reconnected,
        }
    }
}

impl Tool {
    fn update(&mut self, status: ToolStatus, now: Instant) {
        self.status = status;
        if matches!(status, ToolStatus::Completed | ToolStatus::Failed) && self.duration.is_none() {
            self.duration = Some(now.saturating_duration_since(self.started));
        }
    }
}

impl Recorder {
    /// A recorder that waits for the session's `Ready`: what comes before it
    /// is the history a restored session replays.
    pub fn new() -> Self {
        Self::default()
    }

    /// A recorder of `session`, whose events are live from the start, as a
    /// relay hears them from the agent.
    pub fn of_session(session: impl Into<String>) -> Self {
        Self {
            session_id: Some(session.into()),
            live: true,
            ..Self::default()
        }
    }

    /// Take `event`, which arrived at `now` (`wall` on the clock); returns the
    /// turn it ended, if it ended one.
    pub fn observe(
        &mut self,
        event: &SessionEvent,
        now: Instant,
        wall: SystemTime,
    ) -> Option<TurnRecord> {
        match event {
            SessionEvent::Ready {
                agent,
                session_id,
                config,
                ..
            } => {
                self.agent.clone_from(agent);
                self.session_id = Some(session_id.clone());
                self.settings(config);
                self.live = true;
                return None;
            }
            SessionEvent::ConfigChanged { options } => self.settings(options),
            SessionEvent::Usage(usage) => self.usage = Some(usage.clone()),
            SessionEvent::Reconnecting { .. } | SessionEvent::Detached => {
                self.live = false;
                if let Some(turn) = &mut self.turn {
                    turn.reconnected = true;
                }
                return None;
            }
            SessionEvent::Closed { error } => {
                self.live = false;
                let turn = self.turn.take()?;
                return Some(turn.end(now, "closed".to_owned(), error.clone(), None, self));
            }
            _ => {}
        }
        if !self.live {
            // The history a restored session replays, or a reconnect's.
            return None;
        }
        match event {
            SessionEvent::TurnStarted => {
                // A prompt sent while one ran ends that turn's measure.
                let ended = self
                    .turn
                    .take()
                    .map(|turn| turn.end(now, "superseded".to_owned(), None, None, self));
                self.turn = Some(Turn::new(now, wall, false));
                return ended;
            }
            SessionEvent::TurnRunning => {
                self.turn.get_or_insert_with(|| Turn::new(now, wall, true));
                return None;
            }
            SessionEvent::TurnEnded {
                stop_reason,
                tokens,
            } => {
                let turn = self.turn.take()?;
                return Some(turn.end(now, stop_reason.clone(), None, *tokens, self));
            }
            SessionEvent::PromptFailed { message } => {
                let turn = self.turn.take()?;
                return Some(turn.end(now, "failed".to_owned(), Some(message.clone()), None, self));
            }
            _ => {}
        }
        let turn = self.turn.as_mut()?;
        match event {
            SessionEvent::AgentThought { .. } => {
                turn.output(now);
                turn.mark(now, Some(Phase::Thinking));
            }
            SessionEvent::AgentMessage { .. } => {
                turn.output(now);
                turn.mark(now, Some(Phase::Message));
            }
            SessionEvent::PermissionRequested { .. } => turn.mark(now, Some(Phase::Permission)),
            SessionEvent::ToolCall {
                id,
                title,
                kind,
                status,
                ..
            } => {
                turn.output(now);
                turn.mark(now, None);
                turn.tool_started(id, None, kind, title, *status, now);
            }
            SessionEvent::ToolCallUpdate {
                id, title, status, ..
            } => {
                turn.mark(now, None);
                turn.tool_updated(id, title.as_deref(), *status, now);
            }
            SessionEvent::Subagent { parent, event } => {
                // A subagent's thoughts run alongside the agent's, so only
                // its tool calls are measured.
                turn.mark(now, None);
                match event.as_ref() {
                    SessionEvent::ToolCall {
                        id,
                        title,
                        kind,
                        status,
                        ..
                    } => turn.tool_started(id, Some(parent), kind, title, *status, now),
                    SessionEvent::ToolCallUpdate {
                        id, title, status, ..
                    } => turn.tool_updated(id, title.as_deref(), *status, now),
                    _ => {}
                }
            }
            SessionEvent::Plan { .. } | SessionEvent::FileWritten { .. } => turn.mark(now, None),
            _ => {}
        }
        None
    }

    fn settings(&mut self, options: &[ConfigOption]) {
        for option in options {
            let ConfigValue::Select { current, .. } = &option.value else {
                continue;
            };
            match option.category.as_deref() {
                Some("model") => self.model = Some(current.clone()),
                Some("thought_level") => self.effort = Some(current.clone()),
                _ => {}
            }
        }
    }
}

fn status_label(status: ToolStatus) -> &'static str {
    match status {
        ToolStatus::Pending => "pending",
        ToolStatus::InProgress => "in_progress",
        ToolStatus::Completed => "completed",
        ToolStatus::Failed => "failed",
    }
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// Recording the turns of an agent pod's own workspace, as its relay does:
/// the gateway knows the workspace and agent by the relay's token.
#[derive(Debug, Clone)]
pub struct MetricsEndpoint {
    channel: crate::grpc::Channel,
}

#[derive(Debug, thiserror::Error)]
pub enum RecordError {
    #[error("{}", .0.join("; "))]
    Refused(Vec<String>),
    #[error("the gateway keeps no metrics")]
    Unavailable,
    #[error("the gateway's metrics failed: {0}")]
    Failed(String),
}

impl MetricsEndpoint {
    /// The gateway at `gateway` (e.g. `http://ukai-gateway`), with the
    /// relay's `token`.
    pub fn for_workspace(gateway: &str, token: &str) -> Result<Self, ResolveError> {
        let (channel, _) = crate::grpc::pod_channel(gateway, token, TIMEOUT)?;
        Ok(Self { channel })
    }

    pub async fn record(&self, turn: TurnRecord) -> Result<(), RecordError> {
        use ukai_proto::gateway::v1 as gw;
        use ukai_proto::gateway::v1::workspace_metrics_client::WorkspaceMetricsClient;
        let response = WorkspaceMetricsClient::new(self.channel.clone())
            .record(gw::WorkspaceRecordRequest {
                turn: Some(turn.into()),
            })
            .await
            .map_err(|status| match status.code() {
                tonic::Code::Unavailable => RecordError::Unavailable,
                _ => RecordError::Failed(status.message().to_owned()),
            })?
            .into_inner();
        if response.problems.is_empty() {
            Ok(())
        } else {
            Err(RecordError::Refused(response.problems))
        }
    }
}

const TIMEOUT: Duration = Duration::from_secs(10);

/// Reading the turns of a gateway agent's workspace, or of every workspace
/// of a gateway, as the app does.
#[derive(Debug, Clone)]
pub struct MetricsReader {
    channel: crate::grpc::Channel,
    /// The gateway's root, which tells gateways apart.
    root: String,
    agent: String,
    /// `None` for every workspace, of every agent.
    workspace: Option<ukai_proto::gateway::v1::Workspace>,
}

impl MetricsReader {
    /// The turns of `workspace` for `spec`, or `None` when the agent is not
    /// a gateway agent: only a gateway's relays measure turns.
    pub fn resolve(
        spec: &ukai_config::AgentSpec,
        lookup: impl Fn(ukai_config::Source, &str) -> Option<String> + Copy,
        workspace: Option<&crate::PaneWorkspace>,
    ) -> Result<Option<Self>, ResolveError> {
        let Some(agent) = crate::gateway::GatewayAgent::resolve(spec, lookup)? else {
            return Ok(None);
        };
        Ok(Some(Self {
            channel: agent.channel(TIMEOUT)?,
            workspace: Some(agent.gateway.workspace(workspace)),
            root: agent.root,
            agent: agent.id,
        }))
    }

    /// The turns of every workspace and agent of the gateway behind `spec`,
    /// or `None` when it is not a gateway agent.
    pub fn every_workspace(
        spec: &ukai_config::AgentSpec,
        lookup: impl Fn(ukai_config::Source, &str) -> Option<String> + Copy,
    ) -> Result<Option<Self>, ResolveError> {
        let Some(agent) = crate::gateway::GatewayAgent::resolve(spec, lookup)? else {
            return Ok(None);
        };
        Ok(Some(Self {
            channel: agent.channel(TIMEOUT)?,
            workspace: None,
            root: agent.root,
            agent: String::new(),
        }))
    }

    /// The gateway's root URL: readers of one gateway read the same turns.
    pub fn gateway(&self) -> &str {
        &self.root
    }

    /// The workspace's turns, or every workspace's (only `session`'s, if
    /// given), newest first, at most `limit` (0 for the store's limit).
    pub async fn list(
        &self,
        session: Option<&str>,
        limit: u32,
    ) -> Result<Vec<TurnRecord>, RecordError> {
        use ukai_proto::gateway::v1 as gw;
        use ukai_proto::gateway::v1::metrics_client::MetricsClient;
        let listed = MetricsClient::new(self.channel.clone())
            .list(gw::ListTurnsRequest {
                agent: self.agent.clone(),
                workspace: self.workspace.clone(),
                session: session.map(str::to_owned),
                limit,
                every_workspace: self.workspace.is_none(),
            })
            .await
            .map_err(|status| match status.code() {
                tonic::Code::Unavailable => RecordError::Unavailable,
                _ => RecordError::Failed(status.message().to_owned()),
            })?
            .into_inner();
        Ok(listed.turns.into_iter().map(Into::into).collect())
    }
}

impl MetricsReader {
    /// What the workspace's turns, or every workspace's, took, as the
    /// gateway's store sums them up: only turns since `since_ms` if given,
    /// with `recent` of the newest turns and slowest calls (0 for the
    /// store's default).
    pub async fn summarize(
        &self,
        since_ms: Option<u64>,
        recent: u32,
    ) -> Result<TurnsSummary, RecordError> {
        use ukai_proto::gateway::v1 as gw;
        use ukai_proto::gateway::v1::metrics_client::MetricsClient;
        let summary = MetricsClient::new(self.channel.clone())
            .summarize(gw::SummarizeTurnsRequest {
                agent: self.agent.clone(),
                workspace: self.workspace.clone(),
                every_workspace: self.workspace.is_none(),
                since_ms,
                recent,
            })
            .await
            .map_err(|status| match status.code() {
                tonic::Code::Unavailable => RecordError::Unavailable,
                _ => RecordError::Failed(status.message().to_owned()),
            })?
            .into_inner();
        Ok(summary.into())
    }
}

/// Count, total, mean, median, 90th percentile, and maximum of some
/// durations, in milliseconds; all 0 when there are none.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Spread {
    pub count: u64,
    pub total: u64,
    pub mean: u64,
    pub median: u64,
    pub p90: u64,
    pub max: u64,
}

impl Spread {
    pub fn of(values: impl IntoIterator<Item = u64>) -> Self {
        let mut values: Vec<u64> = values.into_iter().collect();
        if values.is_empty() {
            return Self::default();
        }
        values.sort_unstable();
        let count = values.len();
        let total: u64 = values.iter().sum();
        // Nearest rank: the smallest value at least `p` of them do not exceed.
        let rank = |p: usize| values[(count * p).div_ceil(100).saturating_sub(1)];
        Self {
            count: count as u64,
            total,
            mean: total / count as u64,
            median: rank(50),
            p90: rank(90),
            max: values[count - 1],
        }
    }
}

/// Where the turns' time went, summed over them, in milliseconds.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Split {
    /// Before the agent's first output.
    pub waiting: u64,
    pub thinking: u64,
    pub message: u64,
    /// The agent's own tool calls (a subagent's run inside the call that
    /// started it).
    pub tools: u64,
    pub permission: u64,
    /// The rest: between events, or tool calls running alongside each other
    /// counted once too often.
    pub other: u64,
}

impl Split {
    /// Each part, in the order they are shown.
    pub fn parts(&self) -> [(&'static str, u64); 6] {
        [
            ("waiting", self.waiting),
            ("thinking", self.thinking),
            ("reply", self.message),
            ("tools", self.tools),
            ("permission", self.permission),
            ("other", self.other),
        ]
    }

    pub fn total(&self) -> u64 {
        self.parts().iter().map(|(_, ms)| ms).sum()
    }
}

/// The tokens of the turns that say, summed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenTotals {
    /// How many turns said.
    pub turns: u64,
    pub input: u64,
    pub output: u64,
    pub thought: u64,
    pub cached_read: u64,
    pub cached_write: u64,
}

impl TokenTotals {
    /// The share of input read from the cache, if any input was counted.
    pub fn cache_share(&self) -> Option<f64> {
        (self.input > 0).then(|| self.cached_read as f64 / self.input as f64)
    }
}

/// Some turns, summed up.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Summary {
    pub turns: u64,
    pub sessions: u64,
    /// How many turns ended each way, most first.
    pub outcomes: Vec<(String, u64)>,
    pub duration: Spread,
    pub first_output: Spread,
    pub split: Split,
    pub tokens: TokenTotals,
    /// The latest cumulative cost of each session, summed, in the currency
    /// they report; `None` when none does, or they report more than one.
    pub cost: Option<(f64, String)>,
}

impl Summary {
    /// How many turns ended `outcome`.
    pub fn ended(&self, outcome: &str) -> u64 {
        self.outcomes
            .iter()
            .filter(|(name, _)| name == outcome)
            .map(|(_, count)| count)
            .sum()
    }
}

/// The turns that share a key, such as an agent or a model.
#[derive(Debug, Clone, PartialEq)]
pub struct Group {
    pub name: String,
    pub summary: Summary,
}

/// One kind of tool call across the turns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolKind {
    pub kind: String,
    pub calls: u64,
    pub failed: u64,
    /// Of the calls that finished.
    pub spread: Spread,
}

/// A tool call that took long, for the list of the slowest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlowCall {
    pub kind: String,
    pub title: String,
    pub duration_ms: u64,
    pub session: Option<String>,
}

/// What some turns took, as the store sums them up over all of them, with
/// the newest of them as they are.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TurnsSummary {
    pub summary: Summary,
    /// Most turns first.
    pub agents: Vec<Group>,
    pub models: Vec<Group>,
    /// Most time first.
    pub tools: Vec<ToolKind>,
    /// Slowest first.
    pub slowest: Vec<SlowCall>,
    /// Newest first.
    pub recent: Vec<TurnRecord>,
}

/// The wire form of a turn, as `ukai.metrics.v1` carries it.
pub mod wire {
    use ukai_proto::metrics::v1 as pb;

    use crate::mirror::mirror;

    use super::{
        Context, Group, SlowCall, Split, Spread, Summary, TokenTotals, Tokens, ToolKind,
        ToolRecord, TurnRecord, TurnsSummary,
    };

    impl From<TurnRecord> for pb::Turn {
        fn from(turn: TurnRecord) -> Self {
            Self {
                session: turn.session_id.unwrap_or_default(),
                agent: turn.agent,
                model: turn.model,
                effort: turn.effort,
                started_at_ms: turn.started_at_ms,
                outcome: turn.outcome,
                error: turn.error,
                duration_ms: turn.duration_ms,
                first_output_ms: turn.first_output_ms,
                thinking_ms: turn.thinking_ms,
                message_ms: turn.message_ms,
                permission_ms: turn.permission_ms,
                tools: turn
                    .tools
                    .into_iter()
                    .map(|tool| pb::ToolCall {
                        kind: tool.kind,
                        title: tool.title,
                        status: tool.status,
                        duration_ms: tool.duration_ms,
                        parent: tool.parent,
                    })
                    .collect(),
                tokens: turn.tokens.map(|tokens| pb::Tokens {
                    total: tokens.total,
                    input: tokens.input,
                    output: tokens.output,
                    thought: tokens.thought,
                    cached_read: tokens.cached_read,
                    cached_write: tokens.cached_write,
                }),
                context: turn.context.map(|context| pb::Context {
                    used: context.used,
                    size: context.size,
                    cost: context.cost,
                    currency: context.currency,
                }),
                joined: turn.joined,
                reconnected: turn.reconnected,
            }
        }
    }

    impl From<pb::Turn> for TurnRecord {
        fn from(turn: pb::Turn) -> Self {
            Self {
                session_id: Some(turn.session).filter(|session| !session.is_empty()),
                agent: turn.agent,
                model: turn.model,
                effort: turn.effort,
                started_at_ms: turn.started_at_ms,
                outcome: turn.outcome,
                error: turn.error,
                duration_ms: turn.duration_ms,
                first_output_ms: turn.first_output_ms,
                thinking_ms: turn.thinking_ms,
                message_ms: turn.message_ms,
                permission_ms: turn.permission_ms,
                tools: turn
                    .tools
                    .into_iter()
                    .map(|tool| ToolRecord {
                        kind: tool.kind,
                        title: tool.title,
                        status: tool.status,
                        duration_ms: tool.duration_ms,
                        parent: tool.parent,
                    })
                    .collect(),
                tokens: turn.tokens.map(|tokens| Tokens {
                    total: tokens.total,
                    input: tokens.input,
                    output: tokens.output,
                    thought: tokens.thought,
                    cached_read: tokens.cached_read,
                    cached_write: tokens.cached_write,
                }),
                context: turn.context.map(|context| Context {
                    used: context.used,
                    size: context.size,
                    cost: context.cost,
                    currency: context.currency,
                }),
                joined: turn.joined,
                reconnected: turn.reconnected,
            }
        }
    }

    mirror!(
        Spread,
        pb::Spread {
            count,
            total,
            mean,
            median,
            p90,
            max
        }
    );

    impl From<Summary> for pb::Summary {
        fn from(s: Summary) -> Self {
            Self {
                turns: s.turns,
                sessions: s.sessions,
                outcomes: s
                    .outcomes
                    .into_iter()
                    .map(|(outcome, count)| pb::Outcome { outcome, count })
                    .collect(),
                duration: Some(s.duration.into()),
                first_output: Some(s.first_output.into()),
                split: Some(pb::Split {
                    waiting: s.split.waiting,
                    thinking: s.split.thinking,
                    message: s.split.message,
                    tools: s.split.tools,
                    permission: s.split.permission,
                    other: s.split.other,
                }),
                tokens: Some(pb::TokenTotals {
                    turns: s.tokens.turns,
                    input: s.tokens.input,
                    output: s.tokens.output,
                    thought: s.tokens.thought,
                    cached_read: s.tokens.cached_read,
                    cached_write: s.tokens.cached_write,
                }),
                cost: s
                    .cost
                    .map(|(amount, currency)| pb::Cost { amount, currency }),
            }
        }
    }

    impl From<pb::Summary> for Summary {
        fn from(s: pb::Summary) -> Self {
            let split = s.split.unwrap_or_default();
            let tokens = s.tokens.unwrap_or_default();
            Self {
                turns: s.turns,
                sessions: s.sessions,
                outcomes: s
                    .outcomes
                    .into_iter()
                    .map(|o| (o.outcome, o.count))
                    .collect(),
                duration: s.duration.unwrap_or_default().into(),
                first_output: s.first_output.unwrap_or_default().into(),
                split: Split {
                    waiting: split.waiting,
                    thinking: split.thinking,
                    message: split.message,
                    tools: split.tools,
                    permission: split.permission,
                    other: split.other,
                },
                tokens: TokenTotals {
                    turns: tokens.turns,
                    input: tokens.input,
                    output: tokens.output,
                    thought: tokens.thought,
                    cached_read: tokens.cached_read,
                    cached_write: tokens.cached_write,
                },
                // An empty currency: more than one, so no sum.
                cost: s
                    .cost
                    .filter(|cost| !cost.currency.is_empty())
                    .map(|cost| (cost.amount, cost.currency)),
            }
        }
    }

    impl From<Group> for pb::Group {
        fn from(g: Group) -> Self {
            Self {
                name: g.name,
                summary: Some(g.summary.into()),
            }
        }
    }

    impl From<pb::Group> for Group {
        fn from(g: pb::Group) -> Self {
            Self {
                name: g.name,
                summary: g.summary.unwrap_or_default().into(),
            }
        }
    }

    impl From<TurnsSummary> for pb::SummarizeResponse {
        fn from(s: TurnsSummary) -> Self {
            Self {
                summary: Some(s.summary.into()),
                agents: s.agents.into_iter().map(Into::into).collect(),
                models: s.models.into_iter().map(Into::into).collect(),
                tools: s
                    .tools
                    .into_iter()
                    .map(|k| pb::ToolKind {
                        kind: k.kind,
                        calls: k.calls,
                        failed: k.failed,
                        spread: Some(k.spread.into()),
                    })
                    .collect(),
                slowest: s
                    .slowest
                    .into_iter()
                    .map(|c| pb::SlowCall {
                        kind: c.kind,
                        title: c.title,
                        duration_ms: c.duration_ms,
                        session: c.session.unwrap_or_default(),
                    })
                    .collect(),
                recent: s.recent.into_iter().map(Into::into).collect(),
            }
        }
    }

    impl From<pb::SummarizeResponse> for TurnsSummary {
        fn from(s: pb::SummarizeResponse) -> Self {
            Self {
                summary: s.summary.unwrap_or_default().into(),
                agents: s.agents.into_iter().map(Into::into).collect(),
                models: s.models.into_iter().map(Into::into).collect(),
                tools: s
                    .tools
                    .into_iter()
                    .map(|k| ToolKind {
                        kind: k.kind,
                        calls: k.calls,
                        failed: k.failed,
                        spread: k.spread.unwrap_or_default().into(),
                    })
                    .collect(),
                slowest: s
                    .slowest
                    .into_iter()
                    .map(|c| SlowCall {
                        kind: c.kind,
                        title: c.title,
                        duration_ms: c.duration_ms,
                        session: Some(c.session).filter(|s| !s.is_empty()),
                    })
                    .collect(),
                recent: s.recent.into_iter().map(Into::into).collect(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{PromptCaps, RestoreOutcome};
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case::none(&[], Spread::default())]
    #[case::one(&[5], Spread { count: 1, total: 5, mean: 5, median: 5, p90: 5, max: 5 })]
    #[case::ten(
        &[10, 1, 9, 2, 8, 3, 7, 4, 6, 5],
        Spread { count: 10, total: 55, mean: 5, median: 5, p90: 9, max: 10 },
    )]
    fn spreads_durations(#[case] values: &[u64], #[case] expected: Spread) {
        assert_eq!(Spread::of(values.iter().copied()), expected);
    }

    /// Feeds events to a recorder on a clock the test moves.
    struct Clock {
        recorder: Recorder,
        start: Instant,
        now: Instant,
    }

    impl Clock {
        fn new() -> Self {
            let start = Instant::now();
            Self {
                recorder: Recorder::new(),
                start,
                now: start,
            }
        }

        fn ready() -> Self {
            let mut clock = Self::new();
            clock.at(0, ready("s1", Vec::new()));
            clock
        }

        /// `event` arrives `ms` after the clock started.
        fn at(&mut self, ms: u64, event: SessionEvent) -> Option<TurnRecord> {
            self.now = self.start + Duration::from_millis(ms);
            self.recorder
                .observe(&event, self.now, UNIX_EPOCH + Duration::from_millis(ms))
        }
    }

    fn ready(session_id: &str, config: Vec<ConfigOption>) -> SessionEvent {
        SessionEvent::Ready {
            agent: Some("claude".into()),
            session_id: session_id.into(),
            restore: RestoreOutcome::Fresh,
            prompt: PromptCaps::default(),
            config,
            list_sessions: false,
        }
    }

    fn thought() -> SessionEvent {
        SessionEvent::AgentThought { text: "hm".into() }
    }

    fn message() -> SessionEvent {
        SessionEvent::AgentMessage { text: "ok".into() }
    }

    fn tool(id: &str, status: ToolStatus) -> SessionEvent {
        SessionEvent::ToolCall {
            id: id.into(),
            title: "cargo test".into(),
            kind: "execute".into(),
            status,
            diffs: Vec::new(),
        }
    }

    fn update(id: &str, status: ToolStatus) -> SessionEvent {
        SessionEvent::ToolCallUpdate {
            id: id.into(),
            title: None,
            status: Some(status),
            diffs: None,
        }
    }

    fn ended(stop_reason: &str, tokens: Option<TokenUsage>) -> SessionEvent {
        SessionEvent::TurnEnded {
            stop_reason: stop_reason.into(),
            tokens,
        }
    }

    fn select(category: &str, current: &str) -> ConfigOption {
        ConfigOption {
            id: category.into(),
            name: category.into(),
            description: None,
            category: Some(category.into()),
            value: ConfigValue::Select {
                current: current.into(),
                choices: Vec::new(),
            },
        }
    }

    #[test]
    fn measures_the_phases_of_a_turn() {
        let mut clock = Clock::ready();
        assert_eq!(clock.at(100, SessionEvent::TurnStarted), None);
        clock.at(400, thought());
        clock.at(600, thought());
        clock.at(900, tool("t1", ToolStatus::InProgress));
        clock.at(2900, update("t1", ToolStatus::Completed));
        clock.at(3000, message());
        let tokens = TokenUsage {
            total: 1500,
            input: 1000,
            output: 500,
            thought: Some(200),
            cached_read: Some(800),
            cached_write: None,
        };
        let record = clock
            .at(3500, ended("end_turn", Some(tokens)))
            .unwrap_or_else(|| panic!("no record"));
        assert_eq!(record.session_id.as_deref(), Some("s1"));
        assert_eq!(record.agent.as_deref(), Some("claude"));
        assert_eq!(record.started_at_ms, 100);
        assert_eq!(record.outcome, "end_turn");
        assert_eq!(record.duration_ms, 3400);
        assert_eq!(record.first_output_ms, Some(300));
        assert_eq!(record.thinking_ms, 500);
        assert_eq!(record.message_ms, 500);
        assert_eq!(record.permission_ms, 0);
        assert_eq!(
            record.tools,
            vec![ToolRecord {
                kind: "execute".into(),
                title: "cargo test".into(),
                status: "completed".into(),
                duration_ms: Some(2000),
                parent: None,
            }]
        );
        assert_eq!(record.tokens, Some(Tokens::from(tokens)));
    }

    #[test]
    fn measures_the_wait_for_permission() {
        let mut clock = Clock::ready();
        clock.at(0, SessionEvent::TurnStarted);
        clock.at(100, tool("t1", ToolStatus::Pending));
        clock.at(
            200,
            SessionEvent::PermissionRequested {
                id: 1,
                title: "run".into(),
                options: Vec::new(),
            },
        );
        clock.at(5200, update("t1", ToolStatus::InProgress));
        let record = clock
            .at(6000, ended("end_turn", None))
            .unwrap_or_else(|| panic!("no record"));
        assert_eq!(record.permission_ms, 5000);
        assert_eq!(record.tools[0].status, "in_progress");
        assert_eq!(record.tools[0].duration_ms, None);
    }

    #[test]
    fn records_subagent_tool_calls_with_their_parent() {
        let mut clock = Clock::ready();
        clock.at(0, SessionEvent::TurnStarted);
        clock.at(10, tool("task", ToolStatus::InProgress));
        let sub = |event| SessionEvent::Subagent {
            parent: "task".into(),
            event: Box::new(event),
        };
        clock.at(20, sub(thought()));
        clock.at(30, sub(tool("grep", ToolStatus::InProgress)));
        clock.at(80, sub(update("grep", ToolStatus::Failed)));
        clock.at(90, update("task", ToolStatus::Completed));
        let record = clock
            .at(100, ended("end_turn", None))
            .unwrap_or_else(|| panic!("no record"));
        assert_eq!(record.thinking_ms, 0);
        let grep = &record.tools[1];
        assert_eq!(grep.parent.as_deref(), Some("task"));
        assert_eq!(
            (grep.status.as_str(), grep.duration_ms),
            ("failed", Some(50))
        );
        assert_eq!(record.tools[0].duration_ms, Some(80));
    }

    #[test]
    fn ignores_the_history_a_restored_session_replays() {
        let mut clock = Clock::new();
        assert_eq!(clock.at(0, message()), None);
        assert_eq!(clock.at(1, ended("end_turn", None)), None);
        clock.at(2, ready("s1", Vec::new()));
        assert_eq!(clock.at(3, ended("end_turn", None)), None);
    }

    #[test]
    fn keeps_measuring_a_turn_across_a_reconnect() {
        let mut clock = Clock::ready();
        clock.at(0, SessionEvent::TurnStarted);
        clock.at(10, SessionEvent::Reconnecting { attempt: 1 });
        // The replay after reconnecting is history, not this turn.
        clock.at(20, tool("old", ToolStatus::Completed));
        clock.at(30, ready("s1", Vec::new()));
        clock.at(31, SessionEvent::TurnRunning);
        let record = clock
            .at(50, ended("end_turn", None))
            .unwrap_or_else(|| panic!("no record"));
        assert!(record.reconnected);
        assert!(!record.joined);
        assert_eq!(record.duration_ms, 50);
        assert!(record.tools.is_empty());
    }

    #[test]
    fn a_turn_running_before_the_pane_joined_is_marked() {
        let mut clock = Clock::ready();
        clock.at(10, SessionEvent::TurnRunning);
        let record = clock
            .at(30, ended("end_turn", None))
            .unwrap_or_else(|| panic!("no record"));
        assert!(record.joined);
        assert_eq!(record.duration_ms, 20);
    }

    #[rstest]
    #[case::failed(SessionEvent::PromptFailed { message: "no".into() }, "failed", Some("no"))]
    #[case::closed(SessionEvent::Closed { error: Some("gone".into()) }, "closed", Some("gone"))]
    #[case::superseded(SessionEvent::TurnStarted, "superseded", None)]
    fn a_turn_that_does_not_end_is_recorded_anyway(
        #[case] event: SessionEvent,
        #[case] outcome: &str,
        #[case] error: Option<&str>,
    ) {
        let mut clock = Clock::ready();
        clock.at(0, SessionEvent::TurnStarted);
        let record = clock.at(10, event).unwrap_or_else(|| panic!("no record"));
        assert_eq!(record.outcome, outcome);
        assert_eq!(record.error.as_deref(), error);
    }

    #[test]
    fn records_the_settings_and_usage_at_the_end() {
        let mut clock = Clock::new();
        clock.at(
            0,
            ready(
                "s1",
                vec![select("model", "opus"), select("thought_level", "high")],
            ),
        );
        clock.at(
            1,
            SessionEvent::ConfigChanged {
                options: vec![select("model", "sonnet")],
            },
        );
        clock.at(2, SessionEvent::TurnStarted);
        clock.at(
            3,
            SessionEvent::Usage(Usage {
                used: 53_000,
                size: 200_000,
                cost: Some(crate::Cost {
                    amount: 0.05,
                    currency: "USD".into(),
                }),
            }),
        );
        let record = clock
            .at(4, ended("end_turn", None))
            .unwrap_or_else(|| panic!("no record"));
        assert_eq!(record.model.as_deref(), Some("sonnet"));
        assert_eq!(record.effort.as_deref(), Some("high"));
        let context = record.context.unwrap_or_else(|| panic!("no record"));
        assert_eq!(
            (context.used, context.size, context.cost),
            (53_000, 200_000, Some(0.05))
        );
    }

    #[test]
    fn a_relay_measures_from_the_start() {
        let mut recorder = Recorder::of_session("s1");
        let start = Instant::now();
        recorder.observe(&SessionEvent::TurnStarted, start, UNIX_EPOCH);
        let record = recorder
            .observe(
                &ended("end_turn", None),
                start + Duration::from_millis(5),
                UNIX_EPOCH,
            )
            .unwrap_or_else(|| panic!("no record"));
        assert_eq!(record.session_id.as_deref(), Some("s1"));
        assert_eq!(record.duration_ms, 5);
    }

    #[test]
    fn a_turn_crosses_the_wire_unchanged() {
        let mut clock = Clock::ready();
        clock.at(0, SessionEvent::TurnStarted);
        clock.at(10, thought());
        clock.at(20, tool("t1", ToolStatus::InProgress));
        clock.at(30, update("t1", ToolStatus::Completed));
        let tokens = TokenUsage {
            total: 3,
            input: 2,
            output: 1,
            thought: None,
            cached_read: Some(1),
            cached_write: None,
        };
        let record = clock
            .at(40, ended("end_turn", Some(tokens)))
            .unwrap_or_else(|| panic!("no record"));
        let wire: ukai_proto::metrics::v1::Turn = record.clone().into();
        assert_eq!(TurnRecord::from(wire), record);
    }
}
