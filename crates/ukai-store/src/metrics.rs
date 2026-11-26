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

use std::sync::Arc;

use serde_json::json;
use surrealdb::Surreal;
use surrealdb::engine::any::Any;
use tokio::sync::OnceCell;
use tonic::{Request, Response, Status};
use ukai_acp::metrics::TurnRecord;
use ukai_proto::metrics::v1 as pb;

use crate::db::StoreConfig;

pub const DATABASE: &str = "metrics";

/// Applied on every connection; each statement leaves what exists alone.
const SCHEMA: &str = "
DEFINE TABLE IF NOT EXISTS turn SCHEMALESS;
DEFINE INDEX IF NOT EXISTS turn_workspace ON turn FIELDS workspace, started_at_ms;
DEFINE INDEX IF NOT EXISTS turn_session ON turn FIELDS session_id;
";

/// The most turns [`Metrics::list`] returns, and what a request for 0 gets.
pub const LIST_MAX: u32 = 1000;
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
        let conditions: Vec<&str> = [
            filter.workspace.map(|_| "workspace = $workspace"),
            filter.session.map(|_| "session_id = $sid"),
            filter.agent.map(|_| "agent = $agent"),
        ]
        .into_iter()
        .flatten()
        .collect();
        let filtered = if conditions.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", conditions.join(" AND "))
        };
        let rows: Vec<serde_json::Value> = self
            .db
            .query(format!(
                "SELECT * OMIT id, workspace FROM turn {filtered} \
                 ORDER BY started_at_ms DESC LIMIT $limit"
            ))
            .bind(json!({
                "workspace": filter.workspace,
                "sid": filter.session,
                "agent": filter.agent,
                "limit": limit,
            }))
            .await?
            .check()?
            .take(0)?;
        rows.into_iter()
            .map(|row| Ok(serde_json::from_value(row)?))
            .collect()
    }
}

/// Which turns [`Metrics::list`] returns: those of the workspace, session,
/// and agent given, every one's of what is not.
#[derive(Debug, Clone, Copy, Default)]
pub struct Filter<'a> {
    pub workspace: Option<&'a str>,
    pub session: Option<&'a str>,
    pub agent: Option<&'a str>,
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
        self.store
            .get()
            .ok_or_else(|| Status::unavailable("the metrics store is not reached yet"))
    }
}

fn workspace(workspace: &str) -> Result<&str, Status> {
    if workspace.is_empty() {
        Err(Status::invalid_argument("the request names no workspace"))
    } else {
        Ok(workspace)
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
}

#[cfg(test)]
mod tests {
    use pb::metrics_store_server::MetricsStore;
    use rstest::rstest;
    use tonic::Code;
    use ukai_acp::metrics::{Context, Tokens, ToolRecord};

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
