//! Which session published which report of a gateway agent's workspace (see
//! "セッションのレポート" in `docs/design.md`): [`Reports`] keeps it in
//! SurrealDB, and [`Service`] answers `ukai.reports.v1.ReportStore` to the
//! gateway.
//!
//! The reports themselves stay in the workspace's checkout and the
//! gateway's copy of it; this keeps only who published them, so it outlives
//! the pod and the copy alike. Table `publication` holds one record per
//! workspace, session, and path (the record id), so publishing again only
//! moves its time; a report the user deletes is forgotten for every session.
//! The gateway names the workspace from the pod's Job and this service
//! believes it; what a publication may be is `ukai_core::publication`'s.

use std::sync::Arc;

use serde::Deserialize;
use serde_json::json;
use surrealdb::Surreal;
use surrealdb::engine::any::Any;
use tokio::sync::OnceCell;
use tonic::{Request, Response, Status};
use ukai_core::publication::{self, Publication, Published, WORKSPACE_MAX};
use ukai_proto::reports::v1 as pb;

use crate::db::StoreConfig;
use crate::request::{reached, workspace};

pub const DATABASE: &str = "reports";

/// Applied on every connection; each statement leaves what exists alone.
const SCHEMA: &str = "
DEFINE TABLE IF NOT EXISTS publication SCHEMAFULL;
DEFINE FIELD IF NOT EXISTS workspace ON publication TYPE string;
DEFINE FIELD IF NOT EXISTS session ON publication TYPE string;
DEFINE FIELD IF NOT EXISTS path ON publication TYPE string;
DEFINE FIELD IF NOT EXISTS title ON publication TYPE option<string>;
DEFINE FIELD IF NOT EXISTS published ON publication TYPE int;
DEFINE INDEX IF NOT EXISTS publication_workspace ON publication FIELDS workspace;
";

/// Record `$sid` publishing `$path` in `$workspace`, unless that would
/// be a new record past `$max` of the workspace's. A publication with no
/// title keeps the one given before.
const PUBLISH: &str = "
BEGIN TRANSACTION;
LET $id = type::record('publication', [$workspace, $sid, $path]);
LET $existing = (SELECT * FROM ONLY $id);
IF $existing = NONE AND count(SELECT id FROM publication WHERE workspace = $workspace) >= $max {
    THROW 'ukai:full'
};
LET $title = IF $title = NONE OR $title = NULL { $existing.title } ELSE { $title };
UPSERT $id CONTENT {
    workspace: $workspace,
    session: $sid,
    path: $path,
    title: $title,
    published: $published,
} RETURN NONE;
COMMIT TRANSACTION;
";

/// What a transaction throws when the workspace keeps all it may.
const FULL: &str = "ukai:full";

#[derive(Debug, thiserror::Error)]
pub enum ReportsError {
    #[error("{}", .0.join("; "))]
    Refused(Vec<String>),
    #[error("the reports store failed: {0}")]
    Store(#[from] surrealdb::Error),
    #[error("the reports store returned something that cannot be read: {0}")]
    Unreadable(#[from] serde_json::Error),
}

#[derive(Deserialize)]
struct Row {
    session: String,
    path: String,
    title: Option<String>,
    published: u64,
}

/// The publications, by workspace.
pub struct Reports {
    db: Surreal<Any>,
}

impl Reports {
    pub async fn connect(config: &StoreConfig) -> Result<Self, ReportsError> {
        let db = crate::db::connect(config, DATABASE, SCHEMA).await?;
        Ok(Self { db })
    }

    /// Record `publication` in `workspace`, refusing what is wrong with it.
    /// Its time is now, whatever it says.
    pub async fn publish(
        &self,
        workspace: &str,
        publication: Publication,
    ) -> Result<(), ReportsError> {
        let Publication { path, published } = publication;
        let problems = publication::problems(&published.session, &path, published.title.as_deref());
        if !problems.is_empty() {
            return Err(ReportsError::Refused(
                problems.iter().map(ToString::to_string).collect(),
            ));
        }
        let mut response = self
            .db
            .query(PUBLISH)
            .bind(json!({
                "workspace": workspace,
                "sid": published.session,
                "path": path,
                "title": publication::title(published.title.as_deref()),
                "published": time::OffsetDateTime::now_utc().unix_timestamp(),
                "max": WORKSPACE_MAX,
            }))
            .await?;
        let errors = response.take_errors();
        if errors
            .values()
            .any(|error| error.to_string().contains(FULL))
        {
            return Err(ReportsError::Refused(vec![format!(
                "this workspace has published {WORKSPACE_MAX} reports; delete some first"
            )]));
        }
        match errors.into_iter().min_by_key(|(index, _)| *index) {
            Some((_, error)) => Err(error.into()),
            None => Ok(()),
        }
    }

    /// Every publication in `workspace`, newest first.
    pub async fn list(&self, workspace: &str) -> Result<Vec<Publication>, ReportsError> {
        let rows: Vec<serde_json::Value> = self
            .db
            .query(
                "SELECT session, path, title, published FROM publication \
                 WHERE workspace = $workspace ORDER BY published DESC, path",
            )
            .bind(json!({ "workspace": workspace }))
            .await?
            .check()?
            .take(0)?;
        rows.into_iter()
            .map(|row| {
                let row: Row = serde_json::from_value(row)?;
                Ok(Publication {
                    path: row.path,
                    published: Published {
                        session: row.session,
                        title: row.title,
                        at: row.published,
                    },
                })
            })
            .collect()
    }

    /// Forget every session's publication of `path` in `workspace`.
    pub async fn forget(&self, workspace: &str, path: &str) -> Result<(), ReportsError> {
        self.db
            .query("DELETE publication WHERE workspace = $workspace AND path = $path")
            .bind(json!({ "workspace": workspace, "path": path }))
            .await?
            .check()?;
        Ok(())
    }
}

/// `ukai.reports.v1.ReportStore`, behind the token only the gateway holds
/// (the interceptor is [`crate::knowledge::service::authorize`]).
pub struct Service {
    /// Set once the store is reached; until then every call is UNAVAILABLE.
    store: Arc<OnceCell<Reports>>,
}

impl Service {
    pub fn new(store: Arc<OnceCell<Reports>>) -> Self {
        Self { store }
    }

    fn store(&self) -> Result<&Reports, Status> {
        reached(&self.store, "reports")
    }
}

fn failed(error: ReportsError) -> Status {
    tracing::warn!(%error, "reports request failed");
    Status::internal("the reports store failed")
}

#[tonic::async_trait]
impl pb::report_store_server::ReportStore for Service {
    async fn publish(
        &self,
        request: Request<pb::PublishRequest>,
    ) -> Result<Response<pb::PublishResponse>, Status> {
        let request = request.into_inner();
        let workspace = workspace(&request.workspace)?;
        let publication = request
            .publication
            .ok_or_else(|| Status::invalid_argument("the request publishes nothing"))?;
        let problems = match self.store()?.publish(workspace, publication.into()).await {
            Ok(()) => Vec::new(),
            Err(ReportsError::Refused(problems)) => problems,
            Err(error) => return Err(failed(error)),
        };
        Ok(Response::new(pb::PublishResponse { problems }))
    }

    async fn list(
        &self,
        request: Request<pb::ListRequest>,
    ) -> Result<Response<pb::ListResponse>, Status> {
        let request = request.into_inner();
        let publications = self
            .store()?
            .list(workspace(&request.workspace)?)
            .await
            .map_err(failed)?;
        Ok(Response::new(pb::ListResponse {
            publications: publications.into_iter().map(Into::into).collect(),
        }))
    }

    async fn forget(
        &self,
        request: Request<pb::ForgetRequest>,
    ) -> Result<Response<pb::ForgetResponse>, Status> {
        let request = request.into_inner();
        self.store()?
            .forget(workspace(&request.workspace)?, &request.path)
            .await
            .map_err(failed)?;
        Ok(Response::new(pb::ForgetResponse {}))
    }
}

#[cfg(test)]
mod tests {
    use pb::report_store_server::ReportStore;
    use tonic::Code;

    use super::*;

    async fn store() -> Reports {
        Reports::connect(&StoreConfig {
            url: "mem://".into(),
            credentials: None,
        })
        .await
        .unwrap_or_else(|e| panic!("connect: {e}"))
    }

    fn publication(session: &str, path: &str, title: Option<&str>) -> Publication {
        Publication {
            path: path.into(),
            published: Published {
                session: session.into(),
                title: title.map(Into::into),
                at: 0,
            },
        }
    }

    fn sessions_of(publications: &[Publication], path: &str) -> Vec<String> {
        let mut sessions: Vec<String> = publications
            .iter()
            .filter(|p| p.path == path)
            .map(|p| p.published.session.clone())
            .collect();
        sessions.sort();
        sessions
    }

    #[tokio::test]
    async fn keeps_who_published_what_per_workspace() -> Result<(), ReportsError> {
        let store = store().await;
        store
            .publish("w1", publication("s1", "reports/a.html", Some("A")))
            .await?;
        store
            .publish("w1", publication("s2", "reports/a.html", None))
            .await?;
        store
            .publish("w1", publication("s2", "reports/b.html", None))
            .await?;
        store
            .publish("w2", publication("s3", "reports/a.html", None))
            .await?;
        let w1 = store.list("w1").await?;
        assert_eq!(w1.len(), 3);
        assert_eq!(sessions_of(&w1, "reports/a.html"), ["s1", "s2"]);
        assert_eq!(sessions_of(&w1, "reports/b.html"), ["s2"]);
        assert!(w1.iter().all(|p| p.published.at > 0));
        assert_eq!(
            sessions_of(&store.list("w2").await?, "reports/a.html"),
            ["s3"]
        );
        assert!(store.list("w3").await?.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn publishing_again_keeps_one_record_and_its_title() -> Result<(), ReportsError> {
        let store = store().await;
        store
            .publish("w", publication("s", "reports/a.html", Some(" Run ")))
            .await?;
        store
            .publish("w", publication("s", "reports/a.html", None))
            .await?;
        let listed = store.list("w").await?;
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].published.title.as_deref(), Some("Run"));
        store
            .publish("w", publication("s", "reports/a.html", Some("Run 2")))
            .await?;
        assert_eq!(
            store.list("w").await?[0].published.title.as_deref(),
            Some("Run 2")
        );
        Ok(())
    }

    #[tokio::test]
    async fn a_deleted_report_is_forgotten_for_every_session() -> Result<(), ReportsError> {
        let store = store().await;
        for session in ["s1", "s2"] {
            store
                .publish("w", publication(session, "reports/a.html", None))
                .await?;
        }
        store
            .publish("w", publication("s1", "reports/b.html", None))
            .await?;
        store
            .publish("other", publication("s1", "reports/a.html", None))
            .await?;
        store.forget("w", "reports/a.html").await?;
        let listed = store.list("w").await?;
        assert_eq!(
            listed.iter().map(|p| p.path.as_str()).collect::<Vec<_>>(),
            ["reports/b.html"]
        );
        assert_eq!(store.list("other").await?.len(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn refuses_every_problem_with_a_publication() {
        let store = store().await;
        let refused = store
            .publish("w", publication("", "../a.html", Some("a\nb")))
            .await;
        let Err(ReportsError::Refused(problems)) = refused else {
            panic!("{refused:?}");
        };
        assert_eq!(problems.len(), 3, "{problems:?}");
    }

    #[tokio::test]
    async fn the_schema_can_be_applied_again() -> Result<(), ReportsError> {
        let store = store().await;
        store.db.query(SCHEMA).await?.check()?;
        Ok(())
    }

    fn service(store: Reports) -> Service {
        Service::new(Arc::new(OnceCell::new_with(Some(store))))
    }

    #[tokio::test]
    async fn answers_the_gateway() -> Result<(), Status> {
        let service = service(store().await);
        let published = service
            .publish(Request::new(pb::PublishRequest {
                workspace: "w".into(),
                publication: Some(publication("s", "reports/a.html", Some("A")).into()),
            }))
            .await?
            .into_inner();
        assert!(published.problems.is_empty(), "{published:?}");
        let refused = service
            .publish(Request::new(pb::PublishRequest {
                workspace: "w".into(),
                publication: Some(publication("s", "/etc/passwd", None).into()),
            }))
            .await?
            .into_inner();
        assert_eq!(refused.problems.len(), 1);
        let listed = service
            .list(Request::new(pb::ListRequest {
                workspace: "w".into(),
            }))
            .await?
            .into_inner();
        assert_eq!(listed.publications.len(), 1);
        assert_eq!(listed.publications[0].title.as_deref(), Some("A"));
        service
            .forget(Request::new(pb::ForgetRequest {
                workspace: "w".into(),
                path: "reports/a.html".into(),
            }))
            .await?;
        let listed = service
            .list(Request::new(pb::ListRequest {
                workspace: "w".into(),
            }))
            .await?
            .into_inner();
        assert!(listed.publications.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn a_request_must_name_its_workspace() {
        let service = service(store().await);
        let listed = service
            .list(Request::new(pb::ListRequest {
                workspace: String::new(),
            }))
            .await;
        assert_eq!(
            listed.map(|_| ()).map_err(|s| s.code()),
            Err(Code::InvalidArgument)
        );
    }

    #[tokio::test]
    async fn an_unreached_store_is_unavailable() {
        let service = Service::new(Arc::new(OnceCell::new()));
        let listed = service
            .list(Request::new(pb::ListRequest {
                workspace: "w".into(),
            }))
            .await;
        assert_eq!(
            listed.map(|_| ()).map_err(|s| s.code()),
            Err(Code::Unavailable)
        );
    }
}
