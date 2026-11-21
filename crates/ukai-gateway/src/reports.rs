//! Which session published which report, as the gateway serves it (see
//! "セッションのレポート" in `docs/design.md`): `ukai.gateway.v1.WorkspaceReports`
//! to its agent pods, whose relays' MCP servers publish the reports their
//! sessions write, and the sessions added to each report of a workspace's
//! listing (`/agents/<id>/reports`) for its clients. `ukai-store` keeps it
//! (`ukai.reports.v1.ReportStore`), by the workspace the gateway finds from
//! the pod's Job, never from the request.

use std::sync::Arc;

use tonic::metadata::{Ascii, MetadataValue};
use tonic::transport::Channel;
use tonic::{Request, Response, Status};
use ukai_acp::reports::{Listing, Publication};
use ukai_proto::gateway::v1 as gw;
use ukai_proto::reports::v1 as pb;
use ukai_proto::reports::v1::report_store_client::ReportStoreClient;

use crate::Gateway;
use crate::knowledge::{answer, request};

/// The reports side of `ukai-store`, at the address the gateway was given.
#[derive(Clone)]
pub struct ReportsClient {
    client: ReportStoreClient<Channel>,
    /// `Bearer <token>`, marked sensitive so it is never logged.
    token: MetadataValue<Ascii>,
}

impl ReportsClient {
    /// A client of the store at `url`; see [`crate::knowledge::connect`].
    pub fn new(url: &str, token: &str) -> Result<Self, String> {
        let (channel, token) = crate::knowledge::connect(url, token)?;
        Ok(Self {
            client: ReportStoreClient::new(channel),
            token,
        })
    }

    pub async fn publish(
        &self,
        workspace: &str,
        publication: pb::Publication,
    ) -> Result<pb::PublishResponse, Status> {
        let message = request(
            &self.token,
            pb::PublishRequest {
                workspace: workspace.to_owned(),
                publication: Some(publication),
            },
        );
        answer(self.client.clone().publish(message).await)
    }

    pub async fn list(&self, workspace: &str) -> Result<Vec<Publication>, Status> {
        let message = request(
            &self.token,
            pb::ListRequest {
                workspace: workspace.to_owned(),
            },
        );
        let listed = answer(self.client.clone().list(message).await)?;
        Ok(listed.publications.into_iter().map(Into::into).collect())
    }

    pub async fn forget(&self, workspace: &str, path: &str) -> Result<(), Status> {
        let message = request(
            &self.token,
            pb::ForgetRequest {
                workspace: workspace.to_owned(),
                path: path.to_owned(),
            },
        );
        answer(self.client.clone().forget(message).await).map(|_| ())
    }
}

/// `listing` of `workspace` with the sessions that published each report.
/// Without a store, or when it cannot be reached, the listing goes out as it
/// is: a report is still there to open, whoever published it.
pub async fn annotate(gateway: &Gateway, workspace: &str, mut listing: Listing) -> Listing {
    let Some(store) = &gateway.reports else {
        return listing;
    };
    match store.list(workspace).await {
        Ok(publications) => listing.annotate(publications),
        Err(status) => {
            tracing::warn!(%workspace, code = ?status.code(), message = status.message(), "cannot read who published the workspace's reports");
        }
    }
    listing
}

/// Forget who published a report the user deleted from `workspace`.
pub async fn forget(gateway: &Gateway, workspace: &str, path: &str) {
    let Some(store) = &gateway.reports else {
        return;
    };
    // The listing leaves out what is no longer there, so a publication
    // left behind only costs room.
    if let Err(status) = store.forget(workspace, path).await {
        tracing::warn!(%workspace, code = ?status.code(), message = status.message(), "cannot forget who published a deleted report");
    }
}

/// `ukai.gateway.v1.WorkspaceReports`: an agent pod, known by its relay's
/// token.
pub struct PodReports {
    pub gateway: Arc<Gateway>,
}

#[tonic::async_trait]
impl gw::workspace_reports_server::WorkspaceReports for PodReports {
    async fn publish(
        &self,
        request: Request<gw::WorkspacePublishRequest>,
    ) -> Result<Response<pb::PublishResponse>, Status> {
        let store = self.gateway.reports.as_ref().ok_or_else(|| {
            Status::unavailable(
                "this gateway keeps no store: set UKAI_STORE_URL and UKAI_STORE_TOKEN",
            )
        })?;
        let pod = crate::knowledge::pod_asking(&self.gateway, &request).await?;
        let request = request.into_inner();
        let publication = pb::Publication {
            session: request.session,
            path: request.path,
            title: request.title,
            published: 0,
        };
        let answered = store.publish(&pod.workspace, publication).await?;
        if answered.problems.is_empty() {
            tracing::info!(workspace = %pod.workspace, "a session published a report");
        }
        Ok(Response::new(answered))
    }
}
