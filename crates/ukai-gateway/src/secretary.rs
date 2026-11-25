//! `ukai.gateway.v1.WorkspaceSecretary` (see "秘書" in `docs/design.md`):
//! a secretary agent's pod reading the other workspaces' sessions and
//! prompting them, through each workspace's relay (`/sessions`). The pod is
//! known by its relay's token, and only a pod whose agent `gateway.toml`
//! marks `secretary = true` is answered. A prompt goes to the relay with no
//! client of its own, so the user's pane keeps the session.

use std::sync::Arc;
use std::time::Duration;

use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use reqwest::StatusCode;
use tonic::{Request, Response, Status};
use ukai_acp::secretary::{Entry, PromptBody, SessionSummary, Started, marked};
use ukai_proto::gateway::v1 as gw;

use crate::Gateway;
use crate::access;
use crate::cluster::{RelayEndpoint, RunningPod};
use crate::job::Plan;

/// How long listing waits on each relay.
const LIST_TIMEOUT: Duration = Duration::from_secs(5);

/// The longest prompt a secretary may send.
const PROMPT_MAX_BYTES: usize = 64 * 1024;

pub struct PodSecretary {
    pub gateway: Arc<Gateway>,
}

/// The pod asking, once it is known to be a secretary's.
struct Asking {
    agent: String,
    workspace: String,
}

impl PodSecretary {
    async fn asking<T>(&self, request: &Request<T>) -> Result<Asking, Status> {
        let pod = crate::knowledge::pod_asking(&self.gateway, request).await?;
        let agent = pod
            .agent
            .filter(|agent| {
                self.gateway
                    .agents
                    .get(agent)
                    .is_some_and(|declared| declared.secretary)
            })
            .ok_or_else(|| Status::permission_denied("this pod's agent is not a secretary"))?;
        Ok(Asking {
            agent,
            workspace: pod.workspace,
        })
    }

    /// The workspace a secretary names, never its own.
    fn target(
        &self,
        asking: &Asking,
        agent: &str,
        workspace: Option<gw::Workspace>,
    ) -> Result<Plan, Status> {
        let pane = access::pane_of(workspace)?;
        let plan = access::plan(&self.gateway, agent, pane.as_deref())?;
        if plan.workspace_id() == asking.workspace {
            return Err(Status::permission_denied(
                "a secretary does not reach its own workspace",
            ));
        }
        Ok(plan)
    }

    async fn running(&self, plan: &Plan) -> Result<RelayEndpoint, Status> {
        access::running_relay(&self.gateway, plan)
            .await?
            .ok_or_else(|| Status::failed_precondition("the workspace is not running"))
    }
}

fn address(session: Option<gw::SessionAddress>) -> Result<gw::SessionAddress, Status> {
    session
        .filter(|session| !session.session.is_empty())
        .ok_or_else(|| Status::invalid_argument("name the session"))
}

fn prompt_problem(prompt: &str) -> Result<(), Status> {
    if prompt.trim().is_empty() {
        return Err(Status::invalid_argument("the prompt is empty"));
    }
    if prompt.len() > PROMPT_MAX_BYTES {
        return Err(Status::invalid_argument(format!(
            "a prompt is at most {PROMPT_MAX_BYTES} bytes"
        )));
    }
    Ok(())
}

fn session_path(session: &str, then: &str) -> String {
    format!("/{}{then}", utf8_percent_encode(session, NON_ALPHANUMERIC))
}

/// What the relay's answer means, as a status: its body is its reason.
async fn refused(response: reqwest::Response) -> Status {
    let code = response.status();
    let why = response.text().await.unwrap_or_default();
    match code {
        StatusCode::NOT_FOUND => Status::not_found(why),
        StatusCode::CONFLICT => Status::failed_precondition(why),
        StatusCode::UNPROCESSABLE_ENTITY => Status::invalid_argument(why),
        _ => {
            tracing::warn!(%code, "a relay refused a secretary's request");
            Status::unavailable(format!("the workspace's relay answered {code}"))
        }
    }
}

fn unreachable(error: &reqwest::Error) -> Status {
    tracing::warn!(%error, "cannot reach a workspace's relay for a secretary");
    Status::unavailable("cannot reach the workspace's relay")
}

fn workspace_of(pane: Option<String>) -> gw::Workspace {
    match pane {
        Some(key) => gw::Workspace {
            scope: gw::WorkspaceScope::Pod.into(),
            key,
        },
        None => gw::Workspace::default(),
    }
}

/// The running workspaces `keep` lets through, each with its sessions as
/// its relay lists them (or why it did not), asked all at once.
pub async fn workspaces(
    gateway: &Gateway,
    keep: impl Fn(&RunningPod) -> bool,
) -> Result<Vec<gw::RunningWorkspace>, Status> {
    let pods = gateway.cluster.running_pods().await.map_err(|error| {
        tracing::warn!(%error, "cannot list the running workspaces");
        Status::unavailable("cannot list the workspaces")
    })?;
    let listed = pods
        .into_iter()
        .filter(|pod| keep(pod))
        .map(|pod| sessions(gateway, pod));
    Ok(futures::future::join_all(listed).await)
}

async fn sessions(gateway: &Gateway, pod: RunningPod) -> gw::RunningWorkspace {
    let asked = gateway
        .http
        .get(pod.relay.sessions_url(""))
        .bearer_auth(&pod.relay.token)
        .timeout(LIST_TIMEOUT)
        .send()
        .await
        .and_then(reqwest::Response::error_for_status);
    let listed = match asked {
        Ok(response) => response
            .json::<Vec<SessionSummary>>()
            .await
            .map_err(|error| error.to_string()),
        Err(error) => Err(error.to_string()),
    };
    let (sessions, unreachable) = match listed {
        Ok(sessions) => (sessions.into_iter().map(Into::into).collect(), None),
        Err(error) => {
            tracing::warn!(workspace = %pod.workspace, %error, "cannot list a workspace's sessions");
            (Vec::new(), Some("its relay did not answer".to_owned()))
        }
    };
    gw::RunningWorkspace {
        agent: pod.agent,
        workspace: Some(workspace_of(pod.pane)),
        sessions,
        unreachable,
    }
}

#[tonic::async_trait]
impl gw::workspace_secretary_server::WorkspaceSecretary for PodSecretary {
    async fn list_workspaces(
        &self,
        request: Request<gw::ListWorkspacesRequest>,
    ) -> Result<Response<gw::ListWorkspacesResponse>, Status> {
        let asking = self.asking(&request).await?;
        Ok(Response::new(gw::ListWorkspacesResponse {
            workspaces: workspaces(&self.gateway, |pod| pod.workspace != asking.workspace).await?,
        }))
    }

    async fn read_session(
        &self,
        request: Request<gw::ReadSessionRequest>,
    ) -> Result<Response<gw::ReadSessionResponse>, Status> {
        let asking = self.asking(&request).await?;
        let request = request.into_inner();
        let session = address(request.session)?;
        let plan = self.target(&asking, &session.agent, session.workspace)?;
        let relay = self.running(&plan).await?;
        let mut path = session_path(&session.session, "/transcript");
        if request.limit > 0 {
            path.push_str(&format!("?limit={}", request.limit));
        }
        let response = self
            .gateway
            .http
            .get(relay.sessions_url(&path))
            .bearer_auth(&relay.token)
            .send()
            .await
            .map_err(|error| unreachable(&error))?;
        if !response.status().is_success() {
            return Err(refused(response).await);
        }
        let entries = response
            .json::<Vec<Entry>>()
            .await
            .map_err(|error| unreachable(&error))?;
        Ok(Response::new(gw::ReadSessionResponse {
            entries: entries.into_iter().map(Into::into).collect(),
        }))
    }

    async fn prompt_session(
        &self,
        request: Request<gw::PromptSessionRequest>,
    ) -> Result<Response<gw::PromptSessionResponse>, Status> {
        let asking = self.asking(&request).await?;
        let request = request.into_inner();
        prompt_problem(&request.prompt)?;
        let session = address(request.session)?;
        let plan = self.target(&asking, &session.agent, session.workspace)?;
        let relay = self.running(&plan).await?;
        let response = self
            .gateway
            .http
            .post(relay.sessions_url(&session_path(&session.session, "/prompt")))
            .bearer_auth(&relay.token)
            .json(&PromptBody {
                text: marked(&asking.agent, &request.prompt),
            })
            .send()
            .await
            .map_err(|error| unreachable(&error))?;
        if !response.status().is_success() {
            return Err(refused(response).await);
        }
        tracing::info!(secretary = %asking.agent, agent = %session.agent, "a secretary prompted a session");
        Ok(Response::new(gw::PromptSessionResponse {}))
    }

    async fn start_session(
        &self,
        request: Request<gw::StartSessionRequest>,
    ) -> Result<Response<gw::StartSessionResponse>, Status> {
        let asking = self.asking(&request).await?;
        let request = request.into_inner();
        prompt_problem(&request.prompt)?;
        let plan = self.target(&asking, &request.agent, request.workspace)?;
        let pod = self
            .gateway
            .cluster
            .ensure_pod(&plan, &self.gateway.runtime, |message| {
                tracing::debug!(%message, "starting a workspace for a secretary");
            })
            .await
            .map_err(|error| {
                Status::unavailable(format!("the workspace did not start: {error}"))
            })?;
        let relay = RelayEndpoint::of(&pod)
            .ok_or_else(|| Status::unavailable("the workspace's pod has no address"))?;
        let response = self
            .gateway
            .http
            .post(relay.sessions_url(""))
            .bearer_auth(&relay.token)
            .json(&PromptBody {
                text: marked(&asking.agent, &request.prompt),
            })
            .send()
            .await
            .map_err(|error| unreachable(&error))?;
        if !response.status().is_success() {
            return Err(refused(response).await);
        }
        let started = response
            .json::<Started>()
            .await
            .map_err(|error| unreachable(&error))?;
        tracing::info!(secretary = %asking.agent, agent = %request.agent, "a secretary started a session");
        Ok(Response::new(gw::StartSessionResponse {
            session: started.session,
        }))
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case::fine("rebase", true)]
    #[case::empty("  \n", false)]
    #[case::too_long(&"x".repeat(PROMPT_MAX_BYTES + 1), false)]
    fn checks_a_prompt(#[case] prompt: &str, #[case] fine: bool) {
        assert_eq!(prompt_problem(prompt).is_ok(), fine);
    }

    #[test]
    fn a_session_id_is_encoded_into_its_path() {
        assert_eq!(session_path("a/b c", "/prompt"), "/a%2Fb%20c/prompt");
    }

    #[test]
    fn a_pane_key_is_a_workspace_of_its_own() {
        assert_eq!(workspace_of(None).scope(), gw::WorkspaceScope::Shared);
        let own = workspace_of(Some("k1".into()));
        assert_eq!(
            (own.scope(), own.key.as_str()),
            (gw::WorkspaceScope::Pod, "k1")
        );
    }
}
