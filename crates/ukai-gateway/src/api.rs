//! The gateway's control plane besides knowledge, as gRPC and gRPC-Web
//! (`ukai.gateway.v1`): `Agents` (the agents, and restarting a workspace),
//! `Ports` (what listens in a workspace), `Files` and `Commands` (the files
//! of a workspace's checkout, and the user's commands run in it), and
//! `Schedules`. All take the
//! gateway's token. What is not request and answer stays HTTP: ACP's
//! `/acp`, the tunnels at `/forward`, and the reports a web view loads.

use std::sync::Arc;

use tonic::{Request, Response, Status};
use ukai_acp::checkout::{
    CheckoutDiff, CommandOutput, DiffBase, FileListing, RUN_TIMEOUT, RunRequest,
};
use ukai_acp::ports::PortListing;
use ukai_core::schedule::{ScheduleInfo, ScheduleSpec, Trigger};
use ukai_proto::gateway::v1 as gw;
use ukai_proto::gateway::v1::schedule_change::Outcome as Change;

use crate::archive::RelaySource;
use crate::cluster::RelayEndpoint;
use crate::job::Plan;
use crate::{Gateway, access, schedules};

/// How long the relay may take to list its pod's ports or files.
const LIST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

#[derive(Clone)]
pub struct Api {
    pub gateway: Arc<Gateway>,
}

impl Api {
    /// Refuse a request without the gateway's token.
    fn authorize<T>(&self, request: &Request<T>) -> Result<(), Status> {
        let headers = request.metadata().clone().into_headers();
        Ok(access::authorize(&self.gateway, &headers)?)
    }

    /// The workspace of `agent` a request names, as for its `/acp`.
    fn plan(&self, agent: &str, workspace: Option<gw::Workspace>) -> Result<Plan, Status> {
        let pane = access::pane_of(workspace)?;
        Ok(access::plan(&self.gateway, agent, pane.as_deref())?)
    }

    /// The relay of the workspace's running pod; FAILED_PRECONDITION when
    /// it has none, since nothing here starts one.
    async fn running_relay(&self, plan: &Plan) -> Result<RelayEndpoint, Status> {
        access::running_relay(&self.gateway, plan)
            .await?
            .ok_or_else(|| {
                Status::failed_precondition(
                    "the agent's workspace is not running; open the agent to start it",
                )
            })
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
            let source = RelaySource {
                client: &gateway.http,
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
            self.gateway
                .http
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

#[tonic::async_trait]
impl gw::files_server::Files for Api {
    async fn list(
        &self,
        request: Request<gw::ListFilesRequest>,
    ) -> Result<Response<gw::FileListing>, Status> {
        self.authorize(&request)?;
        let request = request.into_inner();
        let plan = self.plan(&request.agent, request.workspace)?;
        let relay = self.running_relay(&plan).await?;
        let listing = async {
            self.gateway
                .http
                .get(relay.files_url())
                .bearer_auth(&relay.token)
                .timeout(LIST_TIMEOUT)
                .send()
                .await?
                .error_for_status()?
                .json::<FileListing>()
                .await
        }
        .await
        .map_err(|error| {
            tracing::warn!(agent = %request.agent, %error, "cannot list the workspace's files");
            Status::unavailable("cannot list the workspace's files")
        })?;
        Ok(Response::new(listing.into()))
    }

    async fn diff(
        &self,
        request: Request<gw::DiffRequest>,
    ) -> Result<Response<gw::CheckoutDiff>, Status> {
        self.authorize(&request)?;
        let request = request.into_inner();
        let base: DiffBase = request.base().into();
        let plan = self.plan(&request.agent, request.workspace)?;
        let relay = self.running_relay(&plan).await?;
        let unavailable = |error: reqwest::Error| {
            tracing::warn!(agent = %request.agent, %error, "cannot diff the workspace's checkout");
            Status::unavailable("cannot diff the workspace's checkout")
        };
        let response = self
            .gateway
            .http
            .get(relay.diff_url(base))
            .bearer_auth(&relay.token)
            // A few git commands, which a large checkout makes slow.
            .timeout(RUN_TIMEOUT)
            .send()
            .await
            .map_err(unavailable)?;
        // What the relay says is wrong with the checkout goes back as it is.
        if response.status() == reqwest::StatusCode::UNPROCESSABLE_ENTITY {
            let problem = response.text().await.map_err(unavailable)?;
            return Err(Status::aborted(problem));
        }
        let diff = response
            .error_for_status()
            .map_err(unavailable)?
            .json::<CheckoutDiff>()
            .await
            .map_err(unavailable)?;
        Ok(Response::new(diff.into()))
    }
}

#[tonic::async_trait]
impl gw::commands_server::Commands for Api {
    async fn run(
        &self,
        request: Request<gw::RunCommandRequest>,
    ) -> Result<Response<gw::CommandOutput>, Status> {
        self.authorize(&request)?;
        let request = request.into_inner();
        if request.command.trim().is_empty() {
            return Err(Status::invalid_argument("the command is empty"));
        }
        let plan = self.plan(&request.agent, request.workspace)?;
        let relay = self.running_relay(&plan).await?;
        tracing::info!(agent = %request.agent, job = %plan.job_name(), "running a command for the user");
        let output = async {
            self.gateway
                .http
                .post(relay.run_url())
                .bearer_auth(&relay.token)
                .json(&RunRequest {
                    command: request.command,
                })
                // The relay stops the command itself; this is for a relay
                // that does not answer at all.
                .timeout(RUN_TIMEOUT + LIST_TIMEOUT)
                .send()
                .await?
                .error_for_status()?
                .json::<CommandOutput>()
                .await
        }
        .await
        .map_err(|error| {
            tracing::warn!(agent = %request.agent, %error, "cannot run a command in the workspace");
            Status::unavailable("cannot run the command in the workspace")
        })?;
        Ok(Response::new(output.into()))
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
        access::agent(&self.gateway, agent)?;
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
    use tonic::Code;

    use super::*;

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
