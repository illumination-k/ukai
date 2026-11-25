//! What each turn of the workspaces' sessions took, as the gateway takes it
//! (see "ターンのメトリクス" in `docs/design.md`): `ukai.gateway.v1.WorkspaceMetrics`
//! to its agent pods, whose relays measure their sessions' turns, and
//! `ukai-store` keeps them (`ukai.metrics.v1.MetricsStore`), under the
//! workspace and agent the gateway finds from the pod's Job, never from the
//! request.

use std::sync::Arc;

use tonic::metadata::{Ascii, MetadataValue};
use tonic::transport::Channel;
use tonic::{Request, Response, Status};
use ukai_proto::gateway::v1 as gw;
use ukai_proto::metrics::v1 as pb;
use ukai_proto::metrics::v1::metrics_store_client::MetricsStoreClient;

use crate::Gateway;
use crate::knowledge::{answer, request};

/// The metrics side of `ukai-store`, at the address the gateway was given.
#[derive(Clone)]
pub struct MetricsClient {
    client: MetricsStoreClient<Channel>,
    /// `Bearer <token>`, marked sensitive so it is never logged.
    token: MetadataValue<Ascii>,
}

impl MetricsClient {
    /// A client of the store at `url`; see [`crate::knowledge::connect`].
    pub fn new(url: &str, token: &str) -> Result<Self, String> {
        let (channel, token) = crate::knowledge::connect(url, token)?;
        Ok(Self {
            client: MetricsStoreClient::new(channel),
            token,
        })
    }

    pub async fn record(
        &self,
        workspace: &str,
        turn: pb::Turn,
    ) -> Result<pb::RecordResponse, Status> {
        let message = request(
            &self.token,
            pb::RecordRequest {
                workspace: workspace.to_owned(),
                turn: Some(turn),
            },
        );
        answer(self.client.clone().record(message).await)
    }
}

/// `ukai.gateway.v1.WorkspaceMetrics`: an agent pod, known by its relay's
/// token.
pub struct PodMetrics {
    pub gateway: Arc<Gateway>,
}

#[tonic::async_trait]
impl gw::workspace_metrics_server::WorkspaceMetrics for PodMetrics {
    async fn record(
        &self,
        request: Request<gw::WorkspaceRecordRequest>,
    ) -> Result<Response<pb::RecordResponse>, Status> {
        let store = self.gateway.metrics.as_ref().ok_or_else(|| {
            Status::unavailable(
                "this gateway keeps no store: set UKAI_STORE_URL and UKAI_STORE_TOKEN",
            )
        })?;
        let pod = crate::knowledge::pod_asking(&self.gateway, &request).await?;
        let mut turn = request
            .into_inner()
            .turn
            .ok_or_else(|| Status::invalid_argument("the request records no turn"))?;
        turn.agent = pod.agent;
        Ok(Response::new(store.record(&pod.workspace, turn).await?))
    }
}
