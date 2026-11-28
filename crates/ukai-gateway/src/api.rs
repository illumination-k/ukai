//! The gateway's control plane besides knowledge, as gRPC and gRPC-Web
//! (`ukai.gateway.v1`): `Agents` (the agents, and restarting a workspace),
//! `Ports` (what listens in a workspace), `Flows` (the runs of the ayaito
//! flows started in a workspace), `Files` and `Commands` (the files
//! of a workspace's checkout, and the user's commands run in it), and
//! `Schedules`. All take the
//! gateway's token. What is not request and answer stays HTTP: ACP's
//! `/acp`, the tunnels at `/forward`, and the reports a web view loads.

use std::sync::Arc;

use tonic::{Request, Response, Status};
use ukai_acp::checkout::{
    CheckoutDiff, CommandOutput, DiffBase, FileListing, MAX_UPLOAD_BYTES, RUN_TIMEOUT, RunRequest,
    Uploaded,
};
use ukai_acp::flows::{FlowListing, Order, Ordered, RunChunk};
use ukai_acp::ports::PortListing;
use ukai_acp::review::{AnnotateRequest, Review};
use ukai_core::schedule::{ScheduleInfo, ScheduleSpec, Trigger};
use ukai_proto::gateway::v1 as gw;
use ukai_proto::gateway::v1::schedule_change::Outcome as Change;

use crate::archive::RelaySource;
use crate::cluster::RelayEndpoint;
use crate::job::Plan;
use crate::{Gateway, access, schedules, secretary};

/// How long the relay may take to list its pod's ports or files.
const LIST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

#[derive(Clone)]
pub struct Api {
    pub gateway: Arc<Gateway>,
}

impl Api {
    /// Refuse a request without the gateway's token.
    pub(crate) fn authorize<T>(&self, request: &Request<T>) -> Result<(), Status> {
        let headers = request.metadata().clone().into_headers();
        Ok(access::authorize(&self.gateway, &headers)?)
    }

    /// The workspace of `agent` a request names, as for its `/acp`.
    pub(crate) fn plan(
        &self,
        agent: &str,
        workspace: Option<gw::Workspace>,
    ) -> Result<Plan, Status> {
        let pane = access::pane_of(workspace)?;
        Ok(access::plan(&self.gateway, agent, pane.as_deref())?)
    }

    /// The relay of the running workspace of `agent` a request names;
    /// FAILED_PRECONDITION when it has none, since nothing here starts one.
    async fn relay<'a>(
        &'a self,
        agent: &'a str,
        workspace: Option<gw::Workspace>,
    ) -> Result<Relay<'a>, Status> {
        let plan = self.plan(agent, workspace)?;
        let endpoint = access::running_relay(&self.gateway, &plan)
            .await?
            .ok_or_else(|| {
                Status::failed_precondition(
                    "the agent's workspace is not running; open the agent to start it",
                )
            })?;
        Ok(Relay {
            http: &self.gateway.http,
            agent,
            plan,
            endpoint,
        })
    }
}

/// The relay of a running workspace, asked for `agent`.
struct Relay<'a> {
    http: &'a reqwest::Client,
    agent: &'a str,
    plan: Plan,
    endpoint: RelayEndpoint,
}

impl Relay<'_> {
    /// A GET of `url`, with the relay's token.
    fn get(&self, url: String) -> reqwest::RequestBuilder {
        self.http.get(url).bearer_auth(&self.endpoint.token)
    }

    /// A POST to `url`, with the relay's token.
    fn post(&self, url: String) -> reqwest::RequestBuilder {
        self.http.post(url).bearer_auth(&self.endpoint.token)
    }

    /// Send `request` and read the relay's JSON answer. With `problem`,
    /// what the relay says is wrong (422) goes back through it as it is;
    /// any other failure is logged as `failure` and is UNAVAILABLE.
    async fn ask<R: serde::de::DeserializeOwned>(
        &self,
        request: reqwest::RequestBuilder,
        failure: &'static str,
        problem: Option<fn(String) -> Status>,
    ) -> Result<R, Status> {
        let agent = self.agent;
        let unavailable = |error: reqwest::Error| {
            tracing::warn!(%agent, %error, "{failure}");
            Status::unavailable(failure)
        };
        let response = request.send().await.map_err(unavailable)?;
        if let Some(problem) = problem
            && response.status() == reqwest::StatusCode::UNPROCESSABLE_ENTITY
        {
            let text = response.text().await.map_err(unavailable)?;
            return Err(problem(text));
        }
        response
            .error_for_status()
            .map_err(unavailable)?
            .json::<R>()
            .await
            .map_err(unavailable)
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
            agents: self
                .gateway
                .agents
                .iter()
                .map(|(id, agent)| gw::Agent {
                    id: id.clone(),
                    command: agent.command.clone(),
                    args: agent.args.clone(),
                    secretary: agent.secretary,
                })
                .collect(),
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

    async fn workspaces(
        &self,
        request: Request<gw::ListWorkspacesRequest>,
    ) -> Result<Response<gw::ListWorkspacesResponse>, Status> {
        self.authorize(&request)?;
        Ok(Response::new(gw::ListWorkspacesResponse {
            workspaces: secretary::workspaces(&self.gateway, |_| true).await?,
        }))
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
        let relay = self.relay(&request.agent, request.workspace).await?;
        let listing: PortListing = relay
            .ask(
                relay.get(relay.endpoint.ports_url()).timeout(LIST_TIMEOUT),
                "cannot list the workspace's ports",
                None,
            )
            .await?;
        Ok(Response::new(listing.into()))
    }
}

/// `/<run>` under the relay's flows, or INVALID_ARGUMENT when `run` is no
/// run id: an id is the name of a folder, and goes into a URL as it is.
fn run_suffix(run: &str) -> Result<String, Status> {
    ukai_acp::flows::run_id(run)
        .map(|run| format!("/{run}"))
        .ok_or_else(|| Status::invalid_argument(format!("`{run}` is not a run id")))
}

#[tonic::async_trait]
impl gw::flows_server::Flows for Api {
    async fn list(
        &self,
        request: Request<gw::ListFlowsRequest>,
    ) -> Result<Response<gw::FlowListing>, Status> {
        self.authorize(&request)?;
        let request = request.into_inner();
        let relay = self.relay(&request.agent, request.workspace).await?;
        let listing: FlowListing = relay
            .ask(
                relay
                    .get(relay.endpoint.flows_url(""))
                    .timeout(LIST_TIMEOUT),
                "cannot list the workspace's flows",
                None,
            )
            .await?;
        Ok(Response::new(listing.into()))
    }

    async fn read(
        &self,
        request: Request<gw::ReadFlowRequest>,
    ) -> Result<Response<gw::FlowRun>, Status> {
        self.authorize(&request)?;
        let request = request.into_inner();
        let suffix = run_suffix(&request.run)?;
        let relay = self.relay(&request.agent, request.workspace).await?;
        let run: RunChunk = relay
            .ask(
                relay
                    .get(format!(
                        "{}?after={}",
                        relay.endpoint.flows_url(&suffix),
                        request.after
                    ))
                    .timeout(LIST_TIMEOUT),
                "cannot read the workspace's flow",
                // What is wrong with the run asked for.
                Some(Status::aborted),
            )
            .await?;
        Ok(Response::new(run.into()))
    }

    async fn order(
        &self,
        request: Request<gw::OrderFlowRequest>,
    ) -> Result<Response<gw::FlowOrdered>, Status> {
        self.authorize(&request)?;
        let request = request.into_inner();
        let suffix = run_suffix(&request.run)?;
        let relay = self.relay(&request.agent, request.workspace).await?;
        let ordered: Ordered = relay
            .ask(
                relay
                    .post(relay.endpoint.flows_url(&suffix))
                    .json(&Order {
                        node: request.node,
                        message: request.message,
                    })
                    // ayaito's command line, which answers within the relay's
                    // own limit.
                    .timeout(RUN_TIMEOUT),
                "cannot reach the workspace's flow",
                // Why ayaito refused, word for word.
                Some(Status::aborted),
            )
            .await?;
        Ok(Response::new(gw::FlowOrdered { said: ordered.said }))
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
        let relay = self.relay(&request.agent, request.workspace).await?;
        let listing: FileListing = relay
            .ask(
                relay.get(relay.endpoint.files_url()).timeout(LIST_TIMEOUT),
                "cannot list the workspace's files",
                None,
            )
            .await?;
        Ok(Response::new(listing.into()))
    }

    async fn diff(
        &self,
        request: Request<gw::DiffRequest>,
    ) -> Result<Response<gw::CheckoutDiff>, Status> {
        self.authorize(&request)?;
        let request = request.into_inner();
        let base: DiffBase = request.base().into();
        let relay = self.relay(&request.agent, request.workspace).await?;
        let diff: CheckoutDiff = relay
            .ask(
                relay
                    .get(relay.endpoint.diff_url(base))
                    // A few git commands, which a large checkout makes slow.
                    .timeout(RUN_TIMEOUT),
                "cannot diff the workspace's checkout",
                // What is wrong with the checkout.
                Some(Status::aborted),
            )
            .await?;
        Ok(Response::new(diff.into()))
    }

    async fn annotate(
        &self,
        request: Request<gw::AnnotateRequest>,
    ) -> Result<Response<gw::Review>, Status> {
        self.authorize(&request)?;
        let request = request.into_inner();
        let relay = self.relay(&request.agent, request.workspace).await?;
        let review: Review = relay
            .ask(
                relay
                    .post(relay.endpoint.review_url())
                    .json(&AnnotateRequest {
                        id: request.id,
                        reply: request.reply,
                        resolved: request.resolved,
                    })
                    .timeout(LIST_TIMEOUT),
                "cannot answer the agent's review",
                Some(Status::aborted),
            )
            .await?;
        Ok(Response::new(review.into()))
    }

    async fn upload(
        &self,
        request: Request<gw::UploadRequest>,
    ) -> Result<Response<gw::Uploaded>, Status> {
        self.authorize(&request)?;
        let request = request.into_inner();
        if request.content.len() > MAX_UPLOAD_BYTES {
            return Err(Status::invalid_argument(format!(
                "files over {} MiB are not uploaded",
                MAX_UPLOAD_BYTES / 1024 / 1024
            )));
        }
        let relay = self.relay(&request.agent, request.workspace).await?;
        let uploaded: Uploaded = relay
            .ask(
                relay
                    .post(relay.endpoint.upload_url(&request.name))
                    .body(request.content)
                    .timeout(RUN_TIMEOUT),
                "cannot upload the file to the workspace",
                // What is wrong with the file.
                Some(Status::invalid_argument),
            )
            .await?;
        Ok(Response::new(gw::Uploaded {
            path: uploaded.path,
        }))
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
        let relay = self.relay(&request.agent, request.workspace).await?;
        tracing::info!(agent = %request.agent, job = %relay.plan.job_name(), "running a command for the user");
        let output: CommandOutput = relay
            .ask(
                relay
                    .post(relay.endpoint.run_url())
                    .json(&RunRequest {
                        command: request.command,
                    })
                    // The relay stops the command itself; this is for a relay
                    // that does not answer at all.
                    .timeout(RUN_TIMEOUT + LIST_TIMEOUT),
                "cannot run the command in the workspace",
                None,
            )
            .await?;
        Ok(Response::new(output.into()))
    }
}

/// A spec from the wire, or why it is not one.
pub(crate) fn spec(spec: Option<gw::ScheduleSpec>) -> Result<ScheduleSpec, Status> {
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
pub(crate) fn change(
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
    fn a_run_is_named_by_an_id_that_goes_into_a_url() {
        assert_eq!(
            run_suffix("weekly-1a2b").ok().as_deref(),
            Some("/weekly-1a2b")
        );
        for run in ["", "../x", "a/b", "a b", "a?b", ".hidden"] {
            assert_eq!(
                run_suffix(run).map_err(|status| status.code()),
                Err(Code::InvalidArgument),
                "{run}"
            );
        }
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
