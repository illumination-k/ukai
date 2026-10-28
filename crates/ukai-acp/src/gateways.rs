//! What a gateway offers to open: the agents of its `gateway.toml`
//! (`ukai.gateway.v1.Agents.List`), which a client lists under the gateway
//! as `<gateway>/<agent>` (see [`ukai_config::Config::list_gateway`]).

use std::time::Duration;

use ukai_config::{Gateway, Source};
use ukai_proto::gateway::v1 as gw;
use ukai_proto::gateway::v1::agents_client::AgentsClient;

use crate::grpc::{self, Channel};
use crate::{ResolveError, header_map};

/// How long a listing may take: the picker waits on it.
const LIST_TIMEOUT: Duration = Duration::from_secs(15);

/// One agent a gateway serves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GatewayAgent {
    pub id: String,
    /// What its pod runs, e.g. `npx -y opencode-ai acp`.
    pub command: String,
    pub args: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum ListError {
    #[error("cannot reach the gateway: {0}")]
    Unreachable(String),
    #[error("{0}")]
    Failed(String),
}

impl From<tonic::Status> for ListError {
    fn from(status: tonic::Status) -> Self {
        match grpc::Failure::from(status) {
            grpc::Failure::Unreachable(why) => Self::Unreachable(why),
            grpc::Failure::Denied(why) => Self::Failed(format!("the gateway refused: {why}")),
            grpc::Failure::TooMany => Self::Failed("too many requests".to_owned()),
            grpc::Failure::NotFound | grpc::Failure::NotRunning => {
                Self::Failed("the gateway lists no agents".to_owned())
            }
            grpc::Failure::Failed(why) => Self::Failed(why),
        }
    }
}

/// Where a gateway's agents are listed.
#[derive(Debug, Clone)]
pub struct GatewayEndpoint {
    channel: Channel,
}

impl GatewayEndpoint {
    pub fn resolve(
        gateway: &Gateway,
        lookup: impl Fn(Source, &str) -> Option<String> + Copy,
    ) -> Result<Self, ResolveError> {
        let url = ukai_config::interpolate(&gateway.url, lookup)?;
        let root = crate::reports::endpoint_base(&url, "")
            .ok_or_else(|| ResolveError::Client(format!("`{url}` is not a gateway URL")))?;
        let client = reqwest::Client::builder()
            .default_headers(header_map(&gateway.headers, &[], lookup)?)
            .timeout(LIST_TIMEOUT)
            .build()
            .map_err(|error| ResolveError::Client(error.to_string()))?;
        Ok(Self {
            channel: grpc::channel(client, root),
        })
    }

    /// The gateway's agents, by id.
    pub async fn agents(&self) -> Result<Vec<GatewayAgent>, ListError> {
        let listing = AgentsClient::new(self.channel.clone())
            .list(gw::ListAgentsRequest {})
            .await?
            .into_inner();
        Ok(listing
            .agents
            .into_iter()
            .map(|agent| GatewayAgent {
                id: agent.id,
                command: agent.command,
                args: agent.args,
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use tonic::{Request, Response, Status};
    use tonic_web::GrpcWebLayer;
    use tower::Layer;
    use ukai_config::WorkspaceScope;
    use ukai_proto::gateway::v1::agents_server::{Agents, AgentsServer};

    use super::*;

    struct Fake;

    #[tonic::async_trait]
    impl Agents for Fake {
        async fn list(
            &self,
            request: Request<gw::ListAgentsRequest>,
        ) -> Result<Response<gw::ListAgentsResponse>, Status> {
            let token = request
                .metadata()
                .get("authorization")
                .and_then(|value| value.to_str().ok());
            if token != Some("Bearer t") {
                return Err(Status::unauthenticated("no token"));
            }
            Ok(Response::new(gw::ListAgentsResponse {
                agents: vec![gw::Agent {
                    id: "opencode".into(),
                    command: "opencode".into(),
                    args: vec!["acp".into()],
                }],
            }))
        }

        async fn restart(
            &self,
            _: Request<gw::RestartRequest>,
        ) -> Result<Response<gw::RestartResponse>, Status> {
            Err(Status::unimplemented("not here"))
        }
    }

    /// Serve the fake as the gateway does, returning its root URL.
    async fn serve() -> String {
        let router =
            tonic::service::Routes::new(GrpcWebLayer::new().layer(AgentsServer::new(Fake)))
                .prepare()
                .into_axum_router();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap_or_else(|e| panic!("bind: {e}"));
        let address = listener
            .local_addr()
            .unwrap_or_else(|e| panic!("address: {e}"));
        tokio::spawn(async move { axum::serve(listener, router).await });
        format!("http://{address}")
    }

    fn gateway(url: String, token: &str) -> GatewayEndpoint {
        let gateway = Gateway {
            url,
            headers: BTreeMap::from([("Authorization".into(), format!("Bearer {token}"))]),
            workspace: WorkspaceScope::Shared,
        };
        GatewayEndpoint::resolve(&gateway, |_, _| None).unwrap_or_else(|e| panic!("{e}"))
    }

    #[tokio::test]
    async fn lists_the_agents_with_the_gateways_headers() {
        let root = serve().await;
        // The root as a WebSocket URL names the same gateway.
        let agents = gateway(root.replacen("http", "ws", 1), "t").agents().await;
        assert_eq!(
            agents.ok(),
            Some(vec![GatewayAgent {
                id: "opencode".into(),
                command: "opencode".into(),
                args: vec!["acp".into()],
            }])
        );
        let refused = gateway(root, "wrong").agents().await;
        assert!(
            matches!(&refused, Err(ListError::Failed(why)) if why.contains("refused")),
            "{refused:?}"
        );
    }
}
