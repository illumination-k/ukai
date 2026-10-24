//! The gateway's control plane besides knowledge, as gRPC and gRPC-Web
//! (`ukai.gateway.v1`): `Agents` (the agents, and restarting a workspace),
//! `Ports` (what listens in a workspace), and `Schedules`. All take the
//! gateway's token. What is not request and answer stays HTTP: ACP's
//! `/acp`, the tunnels at `/forward`, and the reports a web view loads.

use std::sync::Arc;

use tonic::{Request, Response, Status};
use ukai_acp::ports::PortListing;
use ukai_core::schedule::{ScheduleInfo, ScheduleSpec, Trigger};
use ukai_proto::gateway::v1 as gw;
use ukai_proto::gateway::v1::schedule_change::Outcome as Change;

use crate::archive::RelaySource;
use crate::cluster::RelayEndpoint;
use crate::config::KubernetesAgent;
use crate::job::Plan;
use crate::{Gateway, schedules};

/// How long the relay may take to list its pod's ports.
const LIST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

#[derive(Clone)]
pub struct Api {
    pub gateway: Arc<Gateway>,
}

impl Api {
    /// Refuse a request without the gateway's token.
    fn authorize<T>(&self, request: &Request<T>) -> Result<(), Status> {
        let headers = request.metadata().clone().into_headers();
        if crate::authorized(&self.gateway, &headers) {
            Ok(())
        } else {
            Err(Status::unauthenticated(
                "the gateway's token is missing or wrong",
            ))
        }
    }

    fn agent(&self, id: &str) -> Result<&KubernetesAgent, Status> {
        self.gateway
            .agents
            .get(id)
            .ok_or_else(|| Status::not_found(format!("the gateway has no agent `{id}`")))
    }

    /// The workspace of `agent` a request names, as for its `/acp`.
    fn plan(&self, agent: &str, workspace: Option<gw::Workspace>) -> Result<Plan, Status> {
        let agent = self.agent(agent)?.clone();
        let pane = pane(workspace.unwrap_or_default())?;
        Ok(Plan::new(
            agent,
            &self.gateway.user,
            self.gateway.cluster.namespace(),
            pane.as_deref(),
        ))
    }

    /// The relay of the workspace's running pod; FAILED_PRECONDITION when
    /// it has none, since nothing here starts one.
    async fn running_relay(&self, plan: &Plan) -> Result<RelayEndpoint, Status> {
        let pod = self
            .gateway
            .cluster
            .serving_pod(plan)
            .await
            .map_err(|error| {
                tracing::warn!(%error, "cannot look up the workspace's pod");
                Status::unavailable("cannot look up the workspace's pod")
            })?;
        pod.as_ref().and_then(RelayEndpoint::of).ok_or_else(|| {
            Status::failed_precondition(
                "the agent's workspace is not running; open the agent to start it",
            )
        })
    }
}

/// The pane key of a `pod` workspace; `None` for the shared one.
fn pane(workspace: gw::Workspace) -> Result<Option<String>, Status> {
    match workspace.scope() {
        gw::WorkspaceScope::Shared => Ok(None),
        gw::WorkspaceScope::Pod if crate::valid_pane_key(&workspace.key) => Ok(Some(workspace.key)),
        gw::WorkspaceScope::Pod => Err(Status::invalid_argument(
            "a pod workspace's key is 1-64 letters, digits, `-`, or `_`",
        )),
    }
}

#[tonic::async_trait]
impl gw::agents_server::Agents for Api {
    async fn list(
        &self,
        request: Request<gw::ListAgentsRequest>,
    ) -> Result<Response<gw::ListAgentsResponse>, Status> {
        self.authorize(&request)?;
        Ok(Response::new(gw::ListAgentsResponse {
            agents: self.gateway.agents.keys().cloned().collect(),
        }))
    }

    async fn restart(
        &self,
        request: Request<gw::RestartRequest>,
    ) -> Result<Response<gw::RestartResponse>, Status> {
        self.authorize(&request)?;
        let request = request.into_inner();
        let plan = self.plan(&request.agent, request.workspace)?;
        let gateway = &self.gateway;
        // What the pod holds last goes into the copy before the pod goes.
        if let Some(archive) = &gateway.archive
            && let Ok(Some(pod)) = gateway.cluster.serving_pod(&plan).await
            && let Some(endpoint) = RelayEndpoint::of(&pod)
        {
            let client = reqwest::Client::new();
            let source = RelaySource {
                client: &client,
                endpoint: &endpoint,
            };
            if let Err(error) = archive.sync(plan.workspace_id(), &source).await {
                tracing::warn!(agent = %request.agent, %error, "cannot copy the reports before the restart");
            }
        }
        match gateway.cluster.stop(&plan).await {
            Ok(()) => {
                tracing::info!(agent = %request.agent, job = %plan.job_name(), "restarting the agent's workspace");
                Ok(Response::new(gw::RestartResponse {}))
            }
            Err(error) => {
                tracing::warn!(agent = %request.agent, %error, "cannot stop the workspace's pod");
                Err(Status::unavailable("cannot stop the workspace's pod"))
            }
        }
    }
}

#[tonic::async_trait]
impl gw::ports_server::Ports for Api {
    async fn list(
        &self,
        request: Request<gw::ListPortsRequest>,
    ) -> Result<Response<gw::PortListing>, Status> {
        self.authorize(&request)?;
        let request = request.into_inner();
        let plan = self.plan(&request.agent, request.workspace)?;
        let relay = self.running_relay(&plan).await?;
        let listing = async {
            reqwest::Client::new()
                .get(relay.ports_url())
                .bearer_auth(&relay.token)
                .timeout(LIST_TIMEOUT)
                .send()
                .await?
                .error_for_status()?
                .json::<PortListing>()
                .await
        }
        .await
        .map_err(|error| {
            tracing::warn!(agent = %request.agent, %error, "cannot list the workspace's ports");
            Status::unavailable("cannot list the workspace's ports")
        })?;
        Ok(Response::new(listing.into()))
    }
}

/// A spec from the wire, or why it is not one.
fn spec(spec: Option<gw::ScheduleSpec>) -> Result<ScheduleSpec, Status> {
    spec.ok_or_else(|| Status::invalid_argument("the request has no schedule"))?
        .try_into()
        .map_err(|error: ukai_acp::schedules::wire::Malformed| {
            Status::invalid_argument(error.to_string())
        })
}

/// A schedule the gateway has, or NOT_FOUND.
fn found(info: Option<ScheduleInfo>) -> Result<Response<gw::ScheduleInfo>, Status> {
    info.map(|info| Response::new(info.into()))
        .ok_or_else(|| Status::not_found("no such schedule"))
}

/// What a change came to.
fn change(
    result: Result<Option<ScheduleInfo>, Vec<String>>,
) -> Result<Response<gw::ScheduleChange>, Status> {
    let outcome = match result {
        Ok(Some(info)) => Change::Saved(info.into()),
        Ok(None) => return Err(Status::not_found("no such schedule")),
        Err(problems) => Change::Refused(gw::Refused { problems }),
    };
    Ok(Response::new(gw::ScheduleChange {
        outcome: Some(outcome),
    }))
}

impl Api {
    /// The scheduler, for a request about `agent`'s schedules.
    fn schedules<T>(
        &self,
        request: &Request<T>,
        agent: &str,
    ) -> Result<&schedules::Scheduler, Status> {
        self.authorize(request)?;
        self.agent(agent)?;
        Ok(&self.gateway.schedules)
    }
}

#[tonic::async_trait]
impl gw::schedules_server::Schedules for Api {
    async fn list(
        &self,
        request: Request<gw::AgentRequest>,
    ) -> Result<Response<gw::ListSchedulesResponse>, Status> {
        let agent = &request.get_ref().agent;
        let schedules = self.schedules(&request, agent)?.list(agent);
        Ok(Response::new(gw::ListSchedulesResponse {
            schedules: schedules.into_iter().map(Into::into).collect(),
        }))
    }

    async fn create(
        &self,
        request: Request<gw::CreateScheduleRequest>,
    ) -> Result<Response<gw::ScheduleChange>, Status> {
        let schedules = self.schedules(&request, &request.get_ref().agent)?;
        let request = request.into_inner();
        change(
            schedules
                .create(&request.agent, spec(request.spec)?)
                .map(Some),
        )
    }

    async fn get(
        &self,
        request: Request<gw::ScheduleRequest>,
    ) -> Result<Response<gw::ScheduleInfo>, Status> {
        let schedules = self.schedules(&request, &request.get_ref().agent)?;
        let request = request.get_ref();
        found(schedules.info(&request.agent, &request.id))
    }

    async fn update(
        &self,
        request: Request<gw::UpdateScheduleRequest>,
    ) -> Result<Response<gw::ScheduleChange>, Status> {
        let schedules = self.schedules(&request, &request.get_ref().agent)?;
        let request = request.into_inner();
        change(schedules.update(&request.agent, &request.id, spec(request.spec)?))
    }

    async fn delete(
        &self,
        request: Request<gw::ScheduleRequest>,
    ) -> Result<Response<gw::DeleteScheduleResponse>, Status> {
        let schedules = self.schedules(&request, &request.get_ref().agent)?;
        let request = request.get_ref();
        if schedules.delete(&request.agent, &request.id) {
            Ok(Response::new(gw::DeleteScheduleResponse {}))
        } else {
            Err(Status::not_found("no such schedule"))
        }
    }

    async fn runs(
        &self,
        request: Request<gw::ScheduleRequest>,
    ) -> Result<Response<gw::RunsResponse>, Status> {
        let schedules = self.schedules(&request, &request.get_ref().agent)?;
        let request = request.get_ref();
        let runs = schedules
            .runs(&request.agent, &request.id)
            .ok_or_else(|| Status::not_found("no such schedule"))?;
        Ok(Response::new(gw::RunsResponse {
            runs: runs.into_iter().map(Into::into).collect(),
        }))
    }

    async fn run(
        &self,
        request: Request<gw::ScheduleRequest>,
    ) -> Result<Response<gw::ScheduleInfo>, Status> {
        let schedules = self.schedules(&request, &request.get_ref().agent)?;
        let request = request.get_ref();
        if schedules.info(&request.agent, &request.id).is_none() {
            return Err(Status::not_found("no such schedule"));
        }
        schedules::fire(&self.gateway, &request.id, Trigger::Manual);
        found(self.gateway.schedules.info(&request.agent, &request.id))
    }

    async fn stop(
        &self,
        request: Request<gw::ScheduleRequest>,
    ) -> Result<Response<gw::ScheduleInfo>, Status> {
        let schedules = self.schedules(&request, &request.get_ref().agent)?;
        let request = request.get_ref();
        found(schedules.stop(&request.agent, &request.id))
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;
    use tonic::Code;

    use super::*;

    #[rstest]
    #[case::shared(gw::WorkspaceScope::Shared, "ignored", Ok(None))]
    #[case::pod(gw::WorkspaceScope::Pod, "pane-1_a", Ok(Some("pane-1_a")))]
    #[case::pod_without_key(gw::WorkspaceScope::Pod, "", Err(Code::InvalidArgument))]
    #[case::pod_bad_key(gw::WorkspaceScope::Pod, "a/b", Err(Code::InvalidArgument))]
    fn a_request_names_its_workspace(
        #[case] scope: gw::WorkspaceScope,
        #[case] key: &str,
        #[case] expected: Result<Option<&str>, Code>,
    ) {
        let workspace = gw::Workspace {
            scope: scope.into(),
            key: key.into(),
        };
        assert_eq!(
            pane(workspace).map_err(|s| s.code()),
            expected.map(|key| key.map(str::to_owned))
        );
    }

    #[test]
    fn a_change_is_saved_refused_or_not_found() {
        assert!(matches!(
            change(Err(vec!["no cron".into()])).map(|r| r.into_inner().outcome),
            Ok(Some(Change::Refused(gw::Refused { problems }))) if problems == ["no cron"]
        ));
        assert_eq!(
            change(Ok(None)).map(drop).map_err(|s| s.code()),
            Err(Code::NotFound)
        );
    }
}
