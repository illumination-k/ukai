//! What each prompt turn of a gateway agent's sessions took (see "PR ごとの
//! メトリクス" in `docs/design.md`): [`Metrics`] keeps the turns in
//! SurrealDB, and [`Service`] answers `ukai.metrics.v1.MetricsStore` to the
//! gateway.
//!
//! Table `turn` holds one record per workspace, session, and start (the
//! record id), so recording a turn again replaces it. The turn itself is
//! kept as JSON in `data`; `agent` and `ended` are there to choose turns by.
//! The gateway names the workspace and agent from the pod and this service
//! believes it; what a turn may be is `ukai_core::metrics`'s.

use std::sync::Arc;

use serde::Deserialize;
use serde_json::json;
use surrealdb::Surreal;
use surrealdb::engine::any::Any;
use tokio::sync::OnceCell;
use tonic::{Request, Response, Status};
use ukai_core::metrics::{self, Turn};
use ukai_proto::metrics::v1 as pb;

use crate::db::StoreConfig;

pub const DATABASE: &str = "metrics";

/// Most turns one listing returns, the newest.
pub const LIST_MAX: usize = 20_000;

/// Applied on every connection; each statement leaves what exists alone.
const SCHEMA: &str = "
DEFINE TABLE IF NOT EXISTS turn SCHEMAFULL;
DEFINE FIELD IF NOT EXISTS workspace ON turn TYPE string;
DEFINE FIELD IF NOT EXISTS agent ON turn TYPE string;
DEFINE FIELD IF NOT EXISTS ended ON turn TYPE int;
DEFINE FIELD IF NOT EXISTS data ON turn TYPE string;
DEFINE INDEX IF NOT EXISTS turn_agent ON turn FIELDS agent, ended;
";

#[derive(Debug, thiserror::Error)]
pub enum MetricsError {
    #[error("{}", .0.join("; "))]
    Refused(Vec<String>),
    #[error("the metrics store failed: {0}")]
    Store(#[from] surrealdb::Error),
    #[error("the metrics store returned something that cannot be read: {0}")]
    Unreadable(#[from] serde_json::Error),
}

#[derive(Deserialize)]
struct Row {
    workspace: String,
    data: String,
}

/// The turns, by agent.
pub struct Metrics {
    db: Surreal<Any>,
}

impl Metrics {
    pub async fn connect(config: &StoreConfig) -> Result<Self, MetricsError> {
        let db = crate::db::connect(config, DATABASE, SCHEMA).await?;
        Ok(Self { db })
    }

    /// Record `turn` of `agent`'s `workspace`, refusing what is wrong with
    /// it.
    pub async fn record(
        &self,
        workspace: &str,
        agent: &str,
        turn: &Turn,
    ) -> Result<(), MetricsError> {
        let problems = metrics::problems(turn);
        if !problems.is_empty() {
            return Err(MetricsError::Refused(
                problems.iter().map(ToString::to_string).collect(),
            ));
        }
        self.db
            .query(
                "UPSERT type::record('turn', [$workspace, $sid, $started]) CONTENT {
                    workspace: $workspace, agent: $agent, ended: $ended, data: $data,
                } RETURN NONE",
            )
            .bind(json!({
                "workspace": workspace,
                "agent": agent,
                "sid": turn.session,
                "started": turn.started,
                "ended": turn.ended,
                "data": serde_json::to_string(turn)?,
            }))
            .await?
            .check()?;
        Ok(())
    }

    /// `agent`'s turns that ended at or after `since`, each with its
    /// workspace, newest first, at most [`LIST_MAX`].
    pub async fn list(&self, agent: &str, since: i64) -> Result<Vec<(String, Turn)>, MetricsError> {
        let rows: Vec<serde_json::Value> = self
            .db
            .query(
                "SELECT workspace, data, ended FROM turn \
                 WHERE agent = $agent AND ended >= $since ORDER BY ended DESC LIMIT $max",
            )
            .bind(json!({ "agent": agent, "since": since, "max": LIST_MAX }))
            .await?
            .check()?
            .take(0)?;
        rows.into_iter()
            .map(|row| {
                let row: Row = serde_json::from_value(row)?;
                Ok((row.workspace, serde_json::from_str(&row.data)?))
            })
            .collect()
    }
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

fn named<'a>(value: &'a str, what: &str) -> Result<&'a str, Status> {
    if value.is_empty() {
        Err(Status::invalid_argument(format!(
            "the request names no {what}"
        )))
    } else {
        Ok(value)
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
        let workspace = named(&request.workspace, "workspace")?;
        let agent = named(&request.agent, "agent")?;
        let turn: Turn = request
            .turn
            .ok_or_else(|| Status::invalid_argument("the request records no turn"))?
            .into();
        let problems = match self.store()?.record(workspace, agent, &turn).await {
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
        let turns = self
            .store()?
            .list(named(&request.agent, "agent")?, request.since)
            .await
            .map_err(failed)?;
        Ok(Response::new(pb::ListResponse {
            turns: turns
                .into_iter()
                .map(|(workspace, turn)| pb::RecordedTurn {
                    workspace,
                    turn: Some(turn.into()),
                })
                .collect(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use pb::metrics_store_server::MetricsStore;
    use tonic::Code;
    use ukai_core::metrics::{Cost, ToolStat};

    use super::*;

    async fn store() -> Metrics {
        Metrics::connect(&StoreConfig {
            url: "mem://".into(),
            credentials: None,
        })
        .await
        .unwrap_or_else(|e| panic!("connect: {e}"))
    }

    fn turn(session: &str, started: i64, ended: i64) -> Turn {
        Turn {
            session: session.into(),
            branch: Some("feat/x".into()),
            started,
            ended,
            cost: Some(Cost {
                amount: 0.5,
                currency: "USD".into(),
            }),
            tools: vec![ToolStat {
                name: "$ cargo test".into(),
                count: 1,
                failed: 0,
                millis: 9,
            }],
            ..Turn::default()
        }
    }

    #[tokio::test]
    async fn keeps_an_agents_turns_newest_first() -> Result<(), MetricsError> {
        let store = store().await;
        store.record("w1", "a", &turn("s1", 0, 10)).await?;
        store.record("w1", "a", &turn("s1", 20, 30)).await?;
        store.record("w2", "a", &turn("s2", 5, 40)).await?;
        store.record("w3", "b", &turn("s3", 0, 50)).await?;
        let listed = store.list("a", 0).await?;
        assert_eq!(
            listed
                .iter()
                .map(|(w, t)| (w.as_str(), t.ended))
                .collect::<Vec<_>>(),
            [("w2", 40), ("w1", 30), ("w1", 10)]
        );
        assert_eq!(listed[1].1, turn("s1", 20, 30));
        assert_eq!(store.list("a", 30).await?.len(), 2);
        assert!(store.list("c", 0).await?.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn recording_a_turn_again_replaces_it() -> Result<(), MetricsError> {
        let store = store().await;
        store.record("w", "a", &turn("s", 0, 10)).await?;
        store.record("w", "a", &turn("s", 0, 12)).await?;
        let listed = store.list("a", 0).await?;
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].1.ended, 12);
        Ok(())
    }

    #[tokio::test]
    async fn refuses_every_problem_with_a_turn() {
        let store = store().await;
        let refused = store
            .record(
                "w",
                "a",
                &Turn {
                    session: String::new(),
                    started: 5,
                    ended: 1,
                    ..Turn::default()
                },
            )
            .await;
        let Err(MetricsError::Refused(problems)) = refused else {
            panic!("{refused:?}");
        };
        assert_eq!(problems.len(), 2, "{problems:?}");
    }

    #[tokio::test]
    async fn the_schema_can_be_applied_again() -> Result<(), MetricsError> {
        let store = store().await;
        store.db.query(SCHEMA).await?.check()?;
        Ok(())
    }

    fn service(store: Metrics) -> Service {
        Service::new(Arc::new(OnceCell::new_with(Some(store))))
    }

    #[tokio::test]
    async fn answers_the_gateway() -> Result<(), Status> {
        let service = service(store().await);
        let recorded = service
            .record(Request::new(pb::RecordRequest {
                workspace: "w".into(),
                agent: "a".into(),
                turn: Some(turn("s", 0, 10).into()),
            }))
            .await?
            .into_inner();
        assert!(recorded.problems.is_empty(), "{recorded:?}");
        let refused = service
            .record(Request::new(pb::RecordRequest {
                workspace: "w".into(),
                agent: "a".into(),
                turn: Some(turn("", 0, 10).into()),
            }))
            .await?
            .into_inner();
        assert_eq!(refused.problems.len(), 1);
        let listed = service
            .list(Request::new(pb::ListRequest {
                agent: "a".into(),
                since: 0,
            }))
            .await?
            .into_inner();
        assert_eq!(listed.turns.len(), 1);
        assert_eq!(listed.turns[0].workspace, "w");
        Ok(())
    }

    #[rstest::rstest]
    #[case::workspace("", "a")]
    #[case::agent("w", "")]
    #[tokio::test]
    async fn a_record_must_name_its_workspace_and_agent(
        #[case] workspace: &str,
        #[case] agent: &str,
    ) {
        let service = service(store().await);
        let recorded = service
            .record(Request::new(pb::RecordRequest {
                workspace: workspace.into(),
                agent: agent.into(),
                turn: Some(turn("s", 0, 1).into()),
            }))
            .await;
        assert_eq!(
            recorded.map(|_| ()).map_err(|s| s.code()),
            Err(Code::InvalidArgument)
        );
    }

    #[tokio::test]
    async fn an_unreached_store_is_unavailable() {
        let service = Service::new(Arc::new(OnceCell::new()));
        let listed = service
            .list(Request::new(pb::ListRequest {
                agent: "a".into(),
                since: 0,
            }))
            .await;
        assert_eq!(
            listed.map(|_| ()).map_err(|s| s.code()),
            Err(Code::Unavailable)
        );
    }
}
