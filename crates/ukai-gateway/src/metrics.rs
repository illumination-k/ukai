//! What each turn of the workspaces' sessions took, as the gateway takes it
//! (see "ターンのメトリクス" in `docs/design.md`): `ukai.gateway.v1.WorkspaceMetrics`
//! to its agent pods, whose relays measure their sessions' turns, and
//! `ukai-store` keeps them (`ukai.metrics.v1.MetricsStore`), under the
//! workspace and agent the gateway finds from the pod's Job, never from the
//! request. Clients read a workspace's turns at `ukai.gateway.v1.Metrics`
//! ([`crate::api`]), which never starts a pod.

use std::sync::Arc;

use tonic::metadata::{Ascii, MetadataValue};
use tonic::transport::Channel;
use tonic::{Request, Response, Status};
use ukai_proto::gateway::v1 as gw;
use ukai_proto::metrics::v1 as pb;
use ukai_proto::metrics::v1::metrics_store_client::MetricsStoreClient;

use crate::Gateway;
use crate::api::Api;
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

    /// The turns of `workspace`, or with `None` of every workspace, of
    /// `agent`'s if given.
    pub async fn list(
        &self,
        workspace: Option<&str>,
        agent: Option<String>,
        session: Option<String>,
        limit: u32,
    ) -> Result<pb::ListResponse, Status> {
        let message = request(
            &self.token,
            pb::ListRequest {
                workspace: workspace.unwrap_or_default().to_owned(),
                session,
                limit,
                agent,
            },
        );
        answer(self.client.clone().list(message).await)
    }
}

/// The store, or UNAVAILABLE when the gateway keeps none.
fn store(gateway: &Gateway) -> Result<&MetricsClient, Status> {
    gateway.metrics.as_ref().ok_or_else(|| {
        Status::unavailable("this gateway keeps no store: set UKAI_STORE_URL and UKAI_STORE_TOKEN")
    })
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
        let store = store(&self.gateway)?;
        let pod = crate::knowledge::pod_asking(&self.gateway, &request).await?;
        let mut turn = request
            .into_inner()
            .turn
            .ok_or_else(|| Status::invalid_argument("the request records no turn"))?;
        turn.agent = pod.agent;
        Ok(Response::new(store.record(&pod.workspace, turn).await?))
    }
}

#[tonic::async_trait]
impl gw::metrics_server::Metrics for Api {
    async fn list(
        &self,
        request: Request<gw::ListTurnsRequest>,
    ) -> Result<Response<pb::ListResponse>, Status> {
        self.authorize(&request)?;
        let request = request.into_inner();
        if request.every_workspace {
            // The store holds this gateway's turns only, so every workspace
            // of it is every one the gateway's token may read.
            let agent = Some(request.agent).filter(|agent| !agent.is_empty());
            if let Some(agent) = &agent {
                crate::access::agent(&self.gateway, agent)?;
            }
            let store = store(&self.gateway)?;
            return Ok(Response::new(
                store
                    .list(None, agent, request.session, request.limit)
                    .await?,
            ));
        }
        let plan = self.plan(&request.agent, request.workspace)?;
        let store = store(&self.gateway)?;
        Ok(Response::new(
            store
                .list(
                    Some(plan.workspace_id()),
                    None,
                    request.session,
                    request.limit,
                )
                .await?,
        ))
    }
}
