//! What each turn of a gateway agent's sessions took (see "ターンのメトリクス"
//! in `docs/design.md`): [`Metrics`] keeps the turns the relays measured in
//! SurrealDB, and [`Service`] answers `ukai.metrics.v1.MetricsStore` to the
//! gateway.
//!
//! Table `turn` holds one record per turn, as the relay measured it plus
//! the workspace and agent the gateway found from the pod. It is
//! schemaless, so what a turn holds can grow without migrating the table,
//! and can be queried for whatever needs improving (`SELECT
//! math::mean(duration_ms) FROM turn GROUP BY agent`, tools by kind, …).
//!
//! [`Metrics::summarize`] sums up every turn a filter lets through, however
//! many: SurrealQL picks out each turn's numbers (not its tool titles) and
//! the tool calls' kinds and times, and the store groups them and takes the
//! percentiles, which SurrealQL's `math::percentile` cannot within a
//! `GROUP BY`.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde::Deserialize;
use serde_json::json;
use surrealdb::Surreal;
use surrealdb::engine::any::Any;
use tokio::sync::OnceCell;
use tonic::{Request, Response, Status};
use ukai_acp::metrics::{
    Group, SlowCall, Split, Spread, Summary, TokenTotals, Tokens, ToolKind, ToolRecord, TurnRecord,
    TurnsSummary,
};
use ukai_proto::metrics::v1 as pb;

use crate::db::StoreConfig;
use crate::request::{reached, workspace};

pub const DATABASE: &str = "metrics";

/// Applied on every connection; each statement leaves what exists alone.
const SCHEMA: &str = "
DEFINE TABLE IF NOT EXISTS turn SCHEMALESS;
DEFINE INDEX IF NOT EXISTS turn_workspace ON turn FIELDS workspace, started_at_ms;
DEFINE INDEX IF NOT EXISTS turn_session ON turn FIELDS session_id;
";

/// The most turns [`Metrics::list`] returns, and what a request for 0 gets.
pub const LIST_MAX: u32 = 1000;
/// The newest turns [`Metrics::summarize`] returns as they are, and what a
/// request for 0 gets.
pub const RECENT_MAX: u32 = 200;
pub const RECENT_DEFAULT: u32 = 50;
/// How many of the slowest tool calls [`Metrics::summarize`] returns.
pub const SLOWEST: usize = 10;
/// A turn with more tool calls than this is refused.
pub const TOOLS_MAX: usize = 10_000;
/// The longest tool title kept, in characters; longer ones are cut.
pub const TITLE_MAX: usize = 1000;

#[derive(Debug, thiserror::Error)]
pub enum MetricsError {
    #[error("{}", .0.join("; "))]
    Refused(Vec<String>),
    #[error("the metrics store failed: {0}")]
    Store(#[from] surrealdb::Error),
    #[error("the metrics store returned something that cannot be read: {0}")]
    Unreadable(#[from] serde_json::Error),
}

/// Every reason `turn` cannot be kept.
pub fn problems(turn: &TurnRecord) -> Vec<String> {
    let mut problems = Vec::new();
    if turn.session_id.as_deref().is_none_or(str::is_empty) {
        problems.push("the turn names no session".to_owned());
    }
    if turn.outcome.is_empty() {
        problems.push("the turn has no outcome".to_owned());
    }
    if turn.tools.len() > TOOLS_MAX {
        problems.push(format!(
            "the turn has {} tool calls, more than {TOOLS_MAX}",
            turn.tools.len()
        ));
    }
    problems
}

/// The turns, by workspace.
pub struct Metrics {
    db: Surreal<Any>,
}

impl Metrics {
    pub async fn connect(config: &StoreConfig) -> Result<Self, MetricsError> {
        let db = crate::db::connect(config, DATABASE, SCHEMA).await?;
        Ok(Self { db })
    }

    /// Keep `turn` of `workspace`, refusing what is wrong with it.
    pub async fn record(&self, workspace: &str, mut turn: TurnRecord) -> Result<(), MetricsError> {
        let problems = problems(&turn);
        if !problems.is_empty() {
            return Err(MetricsError::Refused(problems));
        }
        for tool in &mut turn.tools {
            if tool.title.chars().count() > TITLE_MAX {
                tool.title = tool.title.chars().take(TITLE_MAX).collect();
            }
        }
        let mut content = serde_json::to_value(&turn)?;
        content["workspace"] = json!(workspace);
        self.db
            .query("CREATE turn CONTENT $content RETURN NONE")
            .bind(json!({ "content": content }))
            .await?
            .check()?;
        Ok(())
    }

    /// The turns `filter` lets through, newest first, at most `limit` of
    /// them ([`LIST_MAX`] for 0 or more).
    pub async fn list(
        &self,
        filter: &Filter<'_>,
        limit: u32,
    ) -> Result<Vec<TurnRecord>, MetricsError> {
        let limit = if limit == 0 {
            LIST_MAX
        } else {
            limit.min(LIST_MAX)
        };
        let rows: Vec<serde_json::Value> = self
            .db
            .query(format!(
                "SELECT * OMIT id, workspace FROM turn {} \
                 ORDER BY started_at_ms DESC LIMIT $limit",
                filter.clause()
            ))
            .bind(filter.bindings(json!({ "limit": limit })))
            .await?
            .check()?
            .take(0)?;
        rows.into_iter()
            .map(|row| Ok(serde_json::from_value(row)?))
            .collect()
    }

    /// Every turn `filter` lets through, summed up, with the `recent`
    /// newest ([`RECENT_DEFAULT`] for 0, at most [`RECENT_MAX`]) as they
    /// are and the [`SLOWEST`] tool calls.
    pub async fn summarize(
        &self,
        filter: &Filter<'_>,
        recent: u32,
    ) -> Result<TurnsSummary, MetricsError> {
        let recent = match recent {
            0 => RECENT_DEFAULT,
            n => n.min(RECENT_MAX),
        };
        let filtered = filter.clause();
        let mut response = self
            .db
            .query(format!(
                "SELECT agent, model, session_id, outcome, duration_ms, first_output_ms, \
                 thinking_ms, message_ms, permission_ms, \
                 math::sum(tools[WHERE parent = NONE AND duration_ms != NONE].duration_ms) \
                 AS own_tools_ms, tokens, context.cost AS cost, context.currency AS currency \
                 FROM turn {filtered} ORDER BY started_at_ms DESC;
                 SELECT kind, status, duration_ms FROM array::flatten(\
                 (SELECT VALUE tools.{{kind, status, duration_ms}} FROM turn {filtered}));
                 SELECT session_id, tools, \
                 math::max(tools[WHERE duration_ms != NONE].duration_ms) AS longest \
                 FROM turn {filtered} ORDER BY longest DESC LIMIT $slowest;
                 SELECT * OMIT id, workspace FROM turn {filtered} \
                 ORDER BY started_at_ms DESC LIMIT $recent;"
            ))
            .bind(filter.bindings(json!({ "slowest": SLOWEST, "recent": recent })))
            .await?
            .check()?;
        let turns: Vec<TurnRow> = read(response.take(0)?)?;
        let calls: Vec<CallRow> = read(response.take(1)?)?;
        let slow: Vec<SlowTurnRow> = read(response.take(2)?)?;
        let recent: Vec<TurnRecord> = read(response.take(3)?)?;
        Ok(TurnsSummary {
            summary: summary_of(&turns.iter().collect::<Vec<_>>()),
            agents: groups(&turns, |row| row.agent.as_deref()),
            models: groups(&turns, |row| row.model.as_deref()),
            tools: tool_kinds(calls),
            slowest: slowest(slow),
            recent,
        })
    }
}

/// Which turns [`Metrics::list`] and [`Metrics::summarize`] take: those of
/// the workspace, session, and agent given, every one's of what is not, and
/// only those since `since_ms` if given.
#[derive(Debug, Clone, Copy, Default)]
pub struct Filter<'a> {
    pub workspace: Option<&'a str>,
    pub session: Option<&'a str>,
    pub agent: Option<&'a str>,
    pub since_ms: Option<u64>,
}

impl Filter<'_> {
    /// The `WHERE` clause, empty when it lets every turn through.
    fn clause(&self) -> String {
        let conditions: Vec<&str> = [
            self.workspace.map(|_| "workspace = $workspace"),
            self.session.map(|_| "session_id = $sid"),
            self.agent.map(|_| "agent = $agent"),
            self.since_ms.map(|_| "started_at_ms >= $since"),
        ]
        .into_iter()
        .flatten()
        .collect();
        if conditions.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", conditions.join(" AND "))
        }
    }

    /// `more` with the values [`Filter::clause`] names.
    fn bindings(&self, mut more: serde_json::Value) -> serde_json::Value {
        more["workspace"] = json!(self.workspace);
        more["sid"] = json!(self.session);
        more["agent"] = json!(self.agent);
        more["since"] = json!(self.since_ms);
        more
    }
}

fn read<T: serde::de::DeserializeOwned>(
    rows: Vec<serde_json::Value>,
) -> Result<Vec<T>, MetricsError> {
    rows.into_iter()
        .map(|row| Ok(serde_json::from_value(row)?))
        .collect()
}

/// A turn's numbers, as [`Metrics::summarize`] reads them.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct TurnRow {
    agent: Option<String>,
    model: Option<String>,
    session_id: Option<String>,
    outcome: String,
    duration_ms: u64,
    first_output_ms: Option<u64>,
    thinking_ms: u64,
    message_ms: u64,
    permission_ms: u64,
    /// The agent's own finished tool calls, summed.
    own_tools_ms: u64,
    tokens: Option<Tokens>,
    cost: Option<f64>,
    currency: Option<String>,
}

/// A tool call's kind and time.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct CallRow {
    kind: String,
    status: String,
    duration_ms: Option<u64>,
}

/// A turn whose longest tool call may be among the slowest.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct SlowTurnRow {
    session_id: Option<String>,
    tools: Vec<ToolRecord>,
}

/// `rows`, newest first, summed up.
fn summary_of(rows: &[&TurnRow]) -> Summary {
    let mut outcomes: BTreeMap<&str, u64> = BTreeMap::new();
    let mut split = Split::default();
    let mut tokens = TokenTotals::default();
    // Newest first, so the first cost seen of a session is its latest.
    let mut costs: BTreeMap<Option<&str>, (f64, &str)> = BTreeMap::new();
    for row in rows {
        *outcomes.entry(&row.outcome).or_default() += 1;
        let waiting = row.first_output_ms.unwrap_or(0);
        let counted =
            waiting + row.thinking_ms + row.message_ms + row.own_tools_ms + row.permission_ms;
        split.waiting += waiting;
        split.thinking += row.thinking_ms;
        split.message += row.message_ms;
        split.tools += row.own_tools_ms;
        split.permission += row.permission_ms;
        split.other += row.duration_ms.saturating_sub(counted);
        if let Some(used) = row.tokens {
            tokens.turns += 1;
            tokens.input += used.input;
            tokens.output += used.output;
            tokens.thought += used.thought.unwrap_or(0);
            tokens.cached_read += used.cached_read.unwrap_or(0);
            tokens.cached_write += used.cached_write.unwrap_or(0);
        }
        if let (Some(cost), Some(currency)) = (row.cost, &row.currency) {
            costs
                .entry(row.session_id.as_deref())
                .or_insert((cost, currency));
        }
    }
    let mut outcomes: Vec<(String, u64)> = outcomes
        .into_iter()
        .map(|(outcome, count)| (outcome.to_owned(), count))
        .collect();
    outcomes.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let currencies: Vec<&str> = costs.values().map(|(_, currency)| *currency).collect();
    let cost = match currencies.first() {
        Some(first) if currencies.iter().all(|c| c == first) => Some((
            costs.values().map(|(cost, _)| cost).sum(),
            (*first).to_owned(),
        )),
        _ => None,
    };
    let mut sessions: Vec<Option<&str>> = rows.iter().map(|r| r.session_id.as_deref()).collect();
    sessions.sort_unstable();
    sessions.dedup();
    Summary {
        turns: rows.len() as u64,
        sessions: sessions.len() as u64,
        outcomes,
        duration: Spread::of(rows.iter().map(|r| r.duration_ms)),
        first_output: Spread::of(rows.iter().filter_map(|r| r.first_output_ms)),
        split,
        tokens,
        cost,
    }
}

/// `rows` grouped by `key` (`None` named `unknown`), most turns first.
fn groups(rows: &[TurnRow], key: impl Fn(&TurnRow) -> Option<&str>) -> Vec<Group> {
    let mut by: BTreeMap<&str, Vec<&TurnRow>> = BTreeMap::new();
    for row in rows {
        by.entry(key(row).unwrap_or("unknown"))
            .or_default()
            .push(row);
    }
    let mut groups: Vec<Group> = by
        .into_iter()
        .map(|(name, rows)| Group {
            name: name.to_owned(),
            summary: summary_of(&rows),
        })
        .collect();
    groups.sort_by(|a, b| {
        b.summary
            .turns
            .cmp(&a.summary.turns)
            .then_with(|| a.name.cmp(&b.name))
    });
    groups
}

/// The tool calls by kind, most time first.
fn tool_kinds(calls: Vec<CallRow>) -> Vec<ToolKind> {
    let mut kinds: BTreeMap<String, (u64, u64, Vec<u64>)> = BTreeMap::new();
    for call in calls {
        let entry = kinds.entry(call.kind).or_default();
        entry.0 += 1;
        if call.status == "failed" {
            entry.1 += 1;
        }
        entry.2.extend(call.duration_ms);
    }
    let mut kinds: Vec<ToolKind> = kinds
        .into_iter()
        .map(|(kind, (calls, failed, durations))| ToolKind {
            kind,
            calls,
            failed,
            spread: Spread::of(durations),
        })
        .collect();
    kinds.sort_by(|a, b| {
        b.spread
            .total
            .cmp(&a.spread.total)
            .then_with(|| a.kind.cmp(&b.kind))
    });
    kinds
}

/// The [`SLOWEST`] finished tool calls of `turns`, slowest first.
fn slowest(turns: Vec<SlowTurnRow>) -> Vec<SlowCall> {
    let mut calls: Vec<SlowCall> = turns
        .into_iter()
        .flat_map(|turn| {
            let session = turn.session_id;
            turn.tools.into_iter().filter_map(move |tool| {
                Some(SlowCall {
                    duration_ms: tool.duration_ms?,
                    kind: tool.kind,
                    title: tool.title,
                    session: session.clone(),
                })
            })
        })
        .collect();
    calls.sort_by_key(|call| std::cmp::Reverse(call.duration_ms));
    calls.truncate(SLOWEST);
    calls
}

/// `ukai.metrics.v1.MetricsStore`, behind the token only the gateway holds
/// (the interceptor is [`crate::knowledge::service::authorize`]).
pub struct Service {
    /// Set once the store is reached; until then every call is UNAVAILABLE.
    store: Arc<OnceCell<Metrics>>,
}

impl Service {
    pub fn new(store: Arc<OnceCell<Metrics>>) -> Self {
        Self { store }
    }

    fn store(&self) -> Result<&Metrics, Status> {
        reached(&self.store, "metrics")
    }
}

fn failed(error: MetricsError) -> Status {
    tracing::warn!(%error, "metrics request failed");
    Status::internal("the metrics store failed")
}

#[tonic::async_trait]
impl pb::metrics_store_server::MetricsStore for Service {
    async fn record(
        &self,
        request: Request<pb::RecordRequest>,
    ) -> Result<Response<pb::RecordResponse>, Status> {
        let request = request.into_inner();
        let workspace = workspace(&request.workspace)?;
        let turn = request
            .turn
            .ok_or_else(|| Status::invalid_argument("the request records no turn"))?;
        let problems = match self.store()?.record(workspace, turn.into()).await {
            Ok(()) => Vec::new(),
            Err(MetricsError::Refused(problems)) => problems,
            Err(error) => return Err(failed(error)),
        };
        Ok(Response::new(pb::RecordResponse { problems }))
    }

    async fn list(
        &self,
        request: Request<pb::ListRequest>,
    ) -> Result<Response<pb::ListResponse>, Status> {
        let request = request.into_inner();
        // An empty workspace asks for every workspace's turns.
        let filter = Filter {
            workspace: Some(request.workspace.as_str()).filter(|w| !w.is_empty()),
            session: request.session.as_deref(),
            agent: request.agent.as_deref(),
            since_ms: None,
        };
        let turns = self
            .store()?
            .list(&filter, request.limit)
            .await
            .map_err(failed)?;
        Ok(Response::new(pb::ListResponse {
            turns: turns.into_iter().map(Into::into).collect(),
        }))
    }

    async fn summarize(
        &self,
        request: Request<pb::SummarizeRequest>,
    ) -> Result<Response<pb::SummarizeResponse>, Status> {
        let request = request.into_inner();
        // An empty workspace asks for every workspace's turns.
        let filter = Filter {
            workspace: Some(request.workspace.as_str()).filter(|w| !w.is_empty()),
            session: None,
            agent: request.agent.as_deref(),
            since_ms: request.since_ms,
        };
        let summary = self
            .store()?
            .summarize(&filter, request.recent)
            .await
            .map_err(failed)?;
        Ok(Response::new(summary.into()))
    }
}

#[cfg(test)]
mod tests {
    use pb::metrics_store_server::MetricsStore;
    use rstest::rstest;
    use tonic::Code;
    use ukai_acp::metrics::Context;

    use super::*;

    async fn store() -> Metrics {
        Metrics::connect(&StoreConfig {
            url: "mem://".into(),
            credentials: None,
        })
        .await
        .unwrap_or_else(|e| panic!("connect: {e}"))
    }

    fn turn(session: &str, started_at_ms: u64) -> TurnRecord {
        TurnRecord {
            session_id: Some(session.into()),
            agent: Some("claude".into()),
            model: Some("opus".into()),
            effort: None,
            started_at_ms,
            outcome: "end_turn".into(),
            error: None,
            duration_ms: 3400,
            first_output_ms: Some(300),
            thinking_ms: 500,
            message_ms: 500,
            permission_ms: 0,
            tools: vec![ToolRecord {
                kind: "execute".into(),
                title: "cargo test".into(),
                status: "completed".into(),
                duration_ms: Some(2000),
                parent: None,
            }],
            tokens: Some(Tokens {
                total: 1500,
                input: 1000,
                output: 500,
                thought: Some(200),
                cached_read: None,
                cached_write: None,
            }),
            context: Some(Context {
                used: 53_000,
                size: 200_000,
                cost: Some(0.05),
                currency: Some("USD".into()),
            }),
            joined: false,
            reconnected: false,
        }
    }

    #[tokio::test]
    async fn keeps_turns_per_workspace_newest_first() -> Result<(), MetricsError> {
        let store = store().await;
        store.record("w1", turn("s1", 100)).await?;
        store.record("w1", turn("s2", 300)).await?;
        store.record("w1", turn("s1", 200)).await?;
        store.record("w2", turn("s3", 100)).await?;
        let of = |workspace| Filter {
            workspace: Some(workspace),
            ..Filter::default()
        };
        let w1 = store.list(&of("w1"), 0).await?;
        assert_eq!(
            w1.iter().map(|t| t.started_at_ms).collect::<Vec<_>>(),
            [300, 200, 100]
        );
        assert_eq!(w1[2], turn("s1", 100));
        let s1 = Filter {
            session: Some("s1"),
            ..of("w1")
        };
        assert_eq!(store.list(&s1, 0).await?.len(), 2);
        assert_eq!(store.list(&of("w1"), 1).await?.len(), 1);
        assert!(store.list(&of("w3"), 0).await?.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn lists_every_workspace_by_agent() -> Result<(), MetricsError> {
        let store = store().await;
        store.record("w1", turn("s1", 100)).await?;
        store.record("w2", turn("s2", 300)).await?;
        let codex = TurnRecord {
            agent: Some("codex".into()),
            ..turn("s3", 200)
        };
        store.record("w2", codex).await?;
        let every = store.list(&Filter::default(), 0).await?;
        assert_eq!(
            every.iter().map(|t| t.started_at_ms).collect::<Vec<_>>(),
            [300, 200, 100]
        );
        let claude = Filter {
            agent: Some("claude"),
            ..Filter::default()
        };
        assert_eq!(
            store
                .list(&claude, 0)
                .await?
                .iter()
                .map(|t| t.started_at_ms)
                .collect::<Vec<_>>(),
            [300, 100]
        );
        Ok(())
    }

    fn tool(
        kind: &str,
        status: &str,
        duration_ms: Option<u64>,
        parent: Option<&str>,
    ) -> ToolRecord {
        ToolRecord {
            kind: kind.into(),
            title: format!("{kind} {duration_ms:?}"),
            status: status.into(),
            duration_ms,
            parent: parent.map(Into::into),
        }
    }

    /// A turn of `session` with only a duration.
    fn bare(session: &str, started_at_ms: u64, duration_ms: u64) -> TurnRecord {
        TurnRecord {
            duration_ms,
            first_output_ms: None,
            thinking_ms: 0,
            message_ms: 0,
            tools: Vec::new(),
            tokens: None,
            context: None,
            ..turn(session, started_at_ms)
        }
    }

    async fn summarize(turns: Vec<TurnRecord>) -> Result<TurnsSummary, MetricsError> {
        let store = store().await;
        for turn in turns {
            store.record("w", turn).await?;
        }
        store.summarize(&Filter::default(), 0).await
    }

    #[tokio::test]
    async fn splits_where_the_time_went() -> Result<(), MetricsError> {
        let mut busy = bare("s1", 1, 10_000);
        busy.first_output_ms = Some(1_000);
        busy.thinking_ms = 2_000;
        busy.message_ms = 1_000;
        busy.permission_ms = 500;
        busy.tools = vec![
            tool("execute", "completed", Some(4_000), None),
            // Inside the call that started its subagent: not counted again.
            tool("read", "completed", Some(3_000), Some("task")),
            tool("edit", "pending", None, None),
        ];
        let summary = summarize(vec![busy]).await?.summary;
        assert_eq!(
            summary.split,
            Split {
                waiting: 1_000,
                thinking: 2_000,
                message: 1_000,
                tools: 4_000,
                permission: 500,
                other: 1_500,
            }
        );
        assert_eq!(summary.split.total(), 10_000);
        Ok(())
    }

    #[tokio::test]
    async fn parallel_tool_calls_leave_nothing_negative() -> Result<(), MetricsError> {
        let mut parallel = bare("s1", 1, 1_000);
        parallel.tools = vec![
            tool("read", "completed", Some(900), None),
            tool("read", "completed", Some(900), None),
        ];
        assert_eq!(summarize(vec![parallel]).await?.summary.split.other, 0);
        Ok(())
    }

    #[tokio::test]
    async fn ranks_tool_kinds_by_time_and_counts_failures() -> Result<(), MetricsError> {
        let mut first = bare("s1", 1, 0);
        first.tools = vec![
            tool("execute", "completed", Some(5_000), None),
            tool("execute", "failed", Some(1_000), None),
            tool("read", "completed", Some(100), None),
        ];
        let mut second = bare("s2", 2, 0);
        second.tools = vec![tool("read", "completed", Some(200), None)];
        let summary = summarize(vec![first, second]).await?;
        let kinds: Vec<(&str, u64, u64, u64)> = summary
            .tools
            .iter()
            .map(|k| (k.kind.as_str(), k.calls, k.failed, k.spread.total))
            .collect();
        assert_eq!(kinds, [("execute", 2, 1, 6_000), ("read", 2, 0, 300)]);
        assert_eq!(summary.slowest[0].duration_ms, 5_000);
        assert_eq!(summary.slowest[0].session.as_deref(), Some("s1"));
        assert_eq!(summary.slowest.len(), 4);
        assert_eq!(summary.summary.sessions, 2);
        Ok(())
    }

    #[tokio::test]
    async fn keeps_only_the_slowest_calls() -> Result<(), MetricsError> {
        let turns = (0..15)
            .map(|i| TurnRecord {
                tools: vec![tool("execute", "completed", Some(i * 100), None)],
                ..bare("s", i, 0)
            })
            .collect();
        let slowest = summarize(turns).await?.slowest;
        assert_eq!(slowest.len(), SLOWEST);
        assert_eq!(slowest[0].duration_ms, 1_400);
        assert_eq!(slowest[SLOWEST - 1].duration_ms, 500);
        Ok(())
    }

    #[tokio::test]
    async fn sums_tokens_and_each_sessions_latest_cost() -> Result<(), MetricsError> {
        let with = |session: &str, started_at_ms: u64, cost: f64| TurnRecord {
            tokens: Some(Tokens {
                total: 150,
                input: 100,
                output: 50,
                thought: Some(10),
                cached_read: Some(60),
                cached_write: None,
            }),
            context: Some(Context {
                used: 0,
                size: 0,
                cost: Some(cost),
                currency: Some("USD".into()),
            }),
            ..bare(session, started_at_ms, 0)
        };
        // s1's latest cost is 0.30.
        let summary = summarize(vec![
            with("s1", 4, 0.30),
            with("s2", 3, 0.05),
            with("s1", 2, 0.10),
            bare("s3", 1, 0),
        ])
        .await?
        .summary;
        assert_eq!(summary.tokens.turns, 3);
        assert_eq!(
            (
                summary.tokens.input,
                summary.tokens.output,
                summary.tokens.thought
            ),
            (300, 150, 30)
        );
        assert_eq!(summary.tokens.cache_share(), Some(0.6));
        let (cost, currency) = summary.cost.unwrap_or_default();
        assert!((cost - 0.35).abs() < 1e-9, "{cost}");
        assert_eq!(currency, "USD");
        Ok(())
    }

    #[tokio::test]
    async fn mixed_currencies_give_no_cost() -> Result<(), MetricsError> {
        let priced = |session: &str, currency: &str| TurnRecord {
            context: Some(Context {
                used: 0,
                size: 0,
                cost: Some(1.0),
                currency: Some(currency.into()),
            }),
            ..bare(session, 1, 0)
        };
        let summary = summarize(vec![priced("a", "USD"), priced("b", "EUR")]).await?;
        assert_eq!(summary.summary.cost, None);
        Ok(())
    }

    #[tokio::test]
    async fn counts_outcomes_most_first() -> Result<(), MetricsError> {
        let failed = TurnRecord {
            outcome: "failed".into(),
            ..bare("s", 2, 0)
        };
        let summary = summarize(vec![bare("s", 1, 0), failed, bare("s", 3, 0)])
            .await?
            .summary;
        assert_eq!(
            summary.outcomes,
            [("end_turn".to_owned(), 2), ("failed".to_owned(), 1)]
        );
        assert_eq!(summary.ended("failed"), 1);
        Ok(())
    }

    #[tokio::test]
    async fn groups_by_agent_and_model_most_turns_first() -> Result<(), MetricsError> {
        let of = |agent: Option<&str>, model: Option<&str>, session: &str| TurnRecord {
            agent: agent.map(Into::into),
            model: model.map(Into::into),
            ..bare(session, 1, 1_000)
        };
        let summary = summarize(vec![
            of(Some("codex"), Some("gpt"), "a"),
            of(Some("claude"), Some("opus"), "b"),
            of(Some("claude"), None, "c"),
            of(None, Some("opus"), "d"),
        ])
        .await?;
        let named = |groups: &[Group]| -> Vec<(String, u64)> {
            groups
                .iter()
                .map(|g| (g.name.clone(), g.summary.turns))
                .collect()
        };
        assert_eq!(
            named(&summary.agents),
            [
                ("claude".to_owned(), 2),
                ("codex".to_owned(), 1),
                ("unknown".to_owned(), 1)
            ]
        );
        assert_eq!(
            named(&summary.models),
            [
                ("opus".to_owned(), 2),
                ("gpt".to_owned(), 1),
                ("unknown".to_owned(), 1)
            ]
        );
        assert_eq!(summary.agents[0].summary.duration.median, 1_000);
        Ok(())
    }

    #[tokio::test]
    async fn summarizes_every_turn_the_filter_lets_through() -> Result<(), MetricsError> {
        let store = store().await;
        // More than a list returns, all of them counted.
        for i in 0..(u64::from(LIST_MAX) + 5) {
            store.record("w1", bare("s", i, 10)).await?;
        }
        store.record("w2", bare("t", 5_000, 10)).await?;
        let every = store.summarize(&Filter::default(), 3).await?;
        assert_eq!(every.summary.turns, u64::from(LIST_MAX) + 6);
        assert_eq!(
            every
                .recent
                .iter()
                .map(|t| t.started_at_ms)
                .collect::<Vec<_>>(),
            [5_000, 1_004, 1_003]
        );
        let since = Filter {
            workspace: Some("w1"),
            since_ms: Some(1_000),
            ..Filter::default()
        };
        assert_eq!(store.summarize(&since, 0).await?.summary.turns, 5);
        let none = store
            .summarize(
                &Filter {
                    agent: Some("codex"),
                    ..Filter::default()
                },
                0,
            )
            .await?;
        assert_eq!(none, TurnsSummary::default());
        Ok(())
    }

    #[tokio::test]
    async fn cuts_long_tool_titles() -> Result<(), MetricsError> {
        let store = store().await;
        let mut long = turn("s", 1);
        long.tools[0].title = "x".repeat(TITLE_MAX + 5);
        store.record("w", long).await?;
        let listed = store.list(&Filter::default(), 0).await?;
        assert_eq!(listed[0].tools[0].title.chars().count(), TITLE_MAX);
        Ok(())
    }

    #[rstest]
    #[case::no_session(TurnRecord { session_id: None, ..turn("s", 0) }, "names no session")]
    #[case::empty_session(TurnRecord { session_id: Some(String::new()), ..turn("s", 0) }, "names no session")]
    #[case::no_outcome(TurnRecord { outcome: String::new(), ..turn("s", 0) }, "no outcome")]
    #[case::too_many_tools(
        TurnRecord { tools: vec![turn("s", 0).tools[0].clone(); TOOLS_MAX + 1], ..turn("s", 0) },
        "tool calls",
    )]
    fn refuses_what_is_wrong(#[case] turn: TurnRecord, #[case] problem: &str) {
        let problems = problems(&turn);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains(problem), "{problems:?}");
    }

    #[test]
    fn collects_every_problem() {
        let turn = TurnRecord {
            session_id: None,
            outcome: String::new(),
            ..turn("s", 0)
        };
        assert_eq!(problems(&turn).len(), 2);
    }

    #[tokio::test]
    async fn the_service_answers_refusals_as_problems() {
        let cell = Arc::new(OnceCell::new());
        let service = Service::new(cell.clone());
        let unreached = service
            .list(Request::new(pb::ListRequest {
                workspace: "w".into(),
                session: None,
                limit: 0,
                agent: None,
            }))
            .await;
        assert_eq!(unreached.err().map(|s| s.code()), Some(Code::Unavailable));
        let _ = cell.set(store().await);
        let refused = service
            .record(Request::new(pb::RecordRequest {
                workspace: "w".into(),
                turn: Some(
                    TurnRecord {
                        outcome: String::new(),
                        ..turn("s", 0)
                    }
                    .into(),
                ),
            }))
            .await
            .unwrap_or_else(|s| panic!("{s}"))
            .into_inner();
        assert_eq!(refused.problems.len(), 1);
        let nameless = service
            .record(Request::new(pb::RecordRequest {
                workspace: String::new(),
                turn: Some(turn("s", 0).into()),
            }))
            .await;
        assert_eq!(
            nameless.err().map(|s| s.code()),
            Some(Code::InvalidArgument)
        );
    }
}
