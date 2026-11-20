//! Workflows a gateway agent's workspace runs: [ayaito] flows, whose agent
//! nodes are sessions of the workspace's agent.
//!
//! The relay in the pod runs them (the flows the checkout's `ukai.toml`
//! declares, started by the user, and the scripts the agent writes and
//! starts through the relay's MCP server), and the gateway passes requests
//! through per workspace, as it does the checkout's files. These are the
//! types both ends speak and the client side ([`WorkflowsEndpoint`], the
//! gateway's `ukai.gateway.v1.Workflows`, gRPC-Web; see `crate::grpc`).
//!
//! [ayaito]: https://github.com/illumination-k/ayaito

use std::time::Duration;

use serde::{Deserialize, Serialize};
use ukai_config::{AgentSpec, Source};
use ukai_proto::gateway::v1 as gw;
use ukai_proto::gateway::v1::workflows_client::WorkflowsClient;

use crate::gateway::GatewayAgent;
use crate::grpc::{self, Channel};
use crate::{PaneWorkspace, ResolveError};

/// Where a relay lists the declared flows and its runs (JSON, for the
/// gateway). A run is `<RUNS_PATH>/<id>`, stopped at `<RUNS_PATH>/<id>/stop`,
/// and started by a POST to [`RUNS_PATH`].
pub const WORKFLOWS_PATH: &str = "/workflows";
pub const RUNS_PATH: &str = "/workflows/runs";
/// Where a run's agent nodes ask the relay for a turn; for the run alone.
pub const AGENT_PATH: &str = "/workflows/agent";

/// How long a call may take; none of them waits for a run.
const TIMEOUT: Duration = Duration::from_secs(20);

/// A flow the checkout's `ukai.toml` declares.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeclaredFlow {
    pub name: String,
    /// Relative to the checkout.
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// Who started a run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Launcher {
    #[default]
    User,
    Agent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    Running,
    Done,
    /// A node ended the run early, as a success.
    Stopped,
    Failed,
    Cancelled,
    /// The relay restarted while it ran.
    Interrupted,
}

impl RunState {
    pub fn is_running(self) -> bool {
        self == Self::Running
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunSummary {
    pub id: String,
    /// The declared flow's name, or the script's path.
    pub flow: String,
    #[serde(default)]
    pub by: Launcher,
    /// Milliseconds since the epoch by the pod's clock.
    pub started: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished: Option<u64>,
    pub state: RunState,
    /// Why it stopped or failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// What a relay answers at [`WORKFLOWS_PATH`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowListing {
    pub flows: Vec<DeclaredFlow>,
    /// Newest first.
    pub runs: Vec<RunSummary>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeKind {
    /// Code.
    #[default]
    Fn,
    Agent,
    Flow,
    Parallel,
    Until,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeState {
    #[default]
    Pending,
    Running,
    Done,
    /// A checkpoint returned what an earlier run saved.
    Skipped,
    /// It ended the run early.
    Stopped,
    Failed,
}

/// A node of a run's flow, as it is drawn, with what the run did to it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Node {
    pub name: String,
    #[serde(default)]
    pub kind: NodeKind,
    /// How deep under the flow it is drawn; the flow itself is 0.
    #[serde(default)]
    pub depth: u32,
    /// What the edge into it does (`map`, its middleware), if anything.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edge: Option<String>,
    #[serde(default)]
    pub state: NodeState,
    /// How many times it started.
    #[serde(default)]
    pub runs: u32,
    #[serde(default)]
    pub retries: u32,
    /// How long its last finished run took.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ms: Option<u64>,
    /// Its error, or why it was skipped or stopped the run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// The sessions its agent calls ran in, oldest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sessions: Vec<String>,
}

/// A run with its nodes: what a relay answers at `<RUNS_PATH>/<id>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Run {
    #[serde(flatten)]
    pub summary: RunSummary,
    /// As JSON.
    pub input: String,
    pub nodes: Vec<Node>,
    /// As JSON; possibly cut short.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    /// The end of what the run printed.
    #[serde(default)]
    pub log: String,
}

/// What starts a run: exactly one of `flow`, `path`, and `script`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StartRequest {
    /// A flow the checkout declares.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flow: Option<String>,
    /// A script in the checkout, relative to it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// A script's source, which the relay keeps.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub script: Option<String>,
    /// The flow's input, as JSON; empty for none.
    #[serde(default)]
    pub input: String,
    /// A run to resume under its id, instead of a new one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<String>,
    #[serde(default)]
    pub by: Launcher,
}

/// How far an agent node may touch the checkout, as ayaito has it. A flow
/// runs unattended, so a permission request is answered from this.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Access {
    #[default]
    ReadOnly,
    Write,
}

/// One turn an agent node asks for, at [`AGENT_PATH`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentCall {
    pub run: String,
    /// The node asking, when the run could tell.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
    pub prompt: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default)]
    pub access: Access,
    /// The session of an earlier call to go on in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
}

/// The turn's answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentAnswer {
    /// What the agent said after its last tool call.
    pub text: String,
    pub session: String,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WorkflowsError {
    #[error("cannot reach the gateway: {0}")]
    Unreachable(String),
    #[error("the agent's workspace is not running")]
    NotRunning,
    #[error("{0}")]
    Refused(String),
    #[error("the gateway refused: {0}")]
    Failed(String),
    #[error("the gateway sent a workflow that cannot be read: {0}")]
    Malformed(String),
}

impl From<tonic::Status> for WorkflowsError {
    fn from(status: tonic::Status) -> Self {
        if status.code() == tonic::Code::Aborted {
            return Self::Refused(status.message().to_owned());
        }
        match grpc::Failure::from(status) {
            grpc::Failure::Unreachable(why) => Self::Unreachable(why),
            grpc::Failure::NotRunning => Self::NotRunning,
            grpc::Failure::NotFound => Self::Failed("no such agent or run".to_owned()),
            grpc::Failure::Denied(why) | grpc::Failure::Failed(why) => Self::Failed(why),
            grpc::Failure::TooMany => Self::Failed("too many requests".to_owned()),
        }
    }
}

impl From<Malformed> for WorkflowsError {
    fn from(error: Malformed) -> Self {
        Self::Malformed(error.to_string())
    }
}

/// The workflows of one workspace of a gateway agent, with the agent's
/// headers.
#[derive(Debug, Clone)]
pub struct WorkflowsEndpoint {
    channel: Channel,
    root: String,
    agent: String,
    workspace: gw::Workspace,
}

impl PartialEq for WorkflowsEndpoint {
    fn eq(&self, other: &Self) -> bool {
        self.root == other.root && self.agent == other.agent && self.workspace == other.workspace
    }
}

impl WorkflowsEndpoint {
    /// The workflows of `pane`'s workspace of `spec`'s agent, or `None` when
    /// it is not a gateway agent: nothing else runs them.
    pub fn resolve(
        spec: &AgentSpec,
        lookup: impl Fn(Source, &str) -> Option<String> + Copy,
        pane: Option<&PaneWorkspace>,
    ) -> Result<Option<Self>, ResolveError> {
        let Some(agent) = GatewayAgent::resolve(spec, lookup)? else {
            return Ok(None);
        };
        Ok(Some(Self {
            channel: agent.channel(TIMEOUT)?,
            workspace: agent.gateway.workspace(pane),
            root: agent.root,
            agent: agent.id,
        }))
    }

    fn client(&self) -> WorkflowsClient<Channel> {
        WorkflowsClient::new(self.channel.clone())
    }

    fn one(&self, run: &str) -> gw::WorkflowRunRequest {
        gw::WorkflowRunRequest {
            agent: self.agent.clone(),
            workspace: Some(self.workspace.clone()),
            run: run.to_owned(),
        }
    }

    pub async fn list(&self) -> Result<WorkflowListing, WorkflowsError> {
        let request = gw::ListWorkflowsRequest {
            agent: self.agent.clone(),
            workspace: Some(self.workspace.clone()),
        };
        Ok(self.client().list(request).await?.into_inner().into())
    }

    pub async fn get(&self, run: &str) -> Result<Run, WorkflowsError> {
        let response = self.client().get(self.one(run)).await?;
        Ok(response.into_inner().try_into()?)
    }

    /// Start the declared `flow` on `input` (JSON, or empty), resuming `run`
    /// when given.
    pub async fn start(
        &self,
        flow: &str,
        input: &str,
        run: Option<&str>,
    ) -> Result<RunSummary, WorkflowsError> {
        let request = gw::StartWorkflowRequest {
            agent: self.agent.clone(),
            workspace: Some(self.workspace.clone()),
            flow: flow.to_owned(),
            input: input.to_owned(),
            run: run.map(str::to_owned),
        };
        Ok(self.client().start(request).await?.into_inner().into())
    }

    pub async fn stop(&self, run: &str) -> Result<RunSummary, WorkflowsError> {
        Ok(self.client().stop(self.one(run)).await?.into_inner().into())
    }
}

/// What a relay's workflows answered, to the gateway and to the relay's own
/// MCP server.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RelayError {
    #[error("cannot reach the relay: {0}")]
    Unreachable(String),
    #[error("no such run")]
    NotFound,
    /// What the relay refused, in its words: a problem a line.
    #[error("{0}")]
    Refused(String),
}

/// A relay's workflows over HTTP, from inside the cluster or the pod.
#[derive(Debug, Clone)]
pub struct RelayWorkflows {
    http: reqwest::Client,
    /// `http://<host>:<port>`.
    base: String,
    token: Option<String>,
}

impl RelayWorkflows {
    pub fn new(http: reqwest::Client, base: impl Into<String>, token: Option<String>) -> Self {
        Self {
            http,
            base: base.into(),
            token,
        }
    }

    /// The workflows of the relay at `base`, with a client of its own.
    pub fn at(base: impl Into<String>, token: Option<String>) -> Self {
        Self::new(reqwest::Client::new(), base, token)
    }

    async fn send<T: serde::de::DeserializeOwned>(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<T, RelayError> {
        let request = match &self.token {
            Some(token) => request.bearer_auth(token),
            None => request,
        };
        let unreachable = |error: reqwest::Error| RelayError::Unreachable(error.to_string());
        let response = request.timeout(TIMEOUT).send().await.map_err(unreachable)?;
        match response.status() {
            reqwest::StatusCode::NOT_FOUND => Err(RelayError::NotFound),
            reqwest::StatusCode::UNPROCESSABLE_ENTITY => Err(RelayError::Refused(
                response.text().await.map_err(unreachable)?,
            )),
            _ => response
                .error_for_status()
                .map_err(unreachable)?
                .json()
                .await
                .map_err(unreachable),
        }
    }

    pub async fn list(&self) -> Result<WorkflowListing, RelayError> {
        let url = format!("{}{WORKFLOWS_PATH}", self.base);
        self.send(self.http.get(url)).await
    }

    pub async fn get(&self, run: &str) -> Result<Run, RelayError> {
        self.send(self.http.get(self.run_url(run, ""))).await
    }

    pub async fn start(&self, request: &StartRequest) -> Result<RunSummary, RelayError> {
        let url = format!("{}{RUNS_PATH}", self.base);
        self.send(self.http.post(url).json(request)).await
    }

    pub async fn stop(&self, run: &str) -> Result<RunSummary, RelayError> {
        self.send(self.http.post(self.run_url(run, "/stop"))).await
    }

    fn run_url(&self, run: &str, suffix: &str) -> String {
        let run = percent_encoding::utf8_percent_encode(run, percent_encoding::NON_ALPHANUMERIC);
        format!("{}{RUNS_PATH}/{run}{suffix}", self.base)
    }
}

/// A message without a part it must have.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("a workflow message has no {0}")]
pub struct Malformed(pub &'static str);

impl From<Launcher> for gw::WorkflowLauncher {
    fn from(by: Launcher) -> Self {
        match by {
            Launcher::User => Self::User,
            Launcher::Agent => Self::Agent,
        }
    }
}

impl From<gw::WorkflowLauncher> for Launcher {
    fn from(by: gw::WorkflowLauncher) -> Self {
        match by {
            gw::WorkflowLauncher::User => Self::User,
            gw::WorkflowLauncher::Agent => Self::Agent,
        }
    }
}

impl From<RunState> for gw::WorkflowRunState {
    fn from(state: RunState) -> Self {
        match state {
            RunState::Running => Self::Running,
            RunState::Done => Self::Done,
            RunState::Stopped => Self::Stopped,
            RunState::Failed => Self::Failed,
            RunState::Cancelled => Self::Cancelled,
            RunState::Interrupted => Self::Interrupted,
        }
    }
}

impl From<gw::WorkflowRunState> for RunState {
    fn from(state: gw::WorkflowRunState) -> Self {
        match state {
            gw::WorkflowRunState::Running => Self::Running,
            gw::WorkflowRunState::Done => Self::Done,
            gw::WorkflowRunState::Stopped => Self::Stopped,
            gw::WorkflowRunState::Failed => Self::Failed,
            gw::WorkflowRunState::Cancelled => Self::Cancelled,
            gw::WorkflowRunState::Interrupted => Self::Interrupted,
        }
    }
}

impl From<NodeKind> for gw::WorkflowNodeKind {
    fn from(kind: NodeKind) -> Self {
        match kind {
            NodeKind::Fn => Self::Fn,
            NodeKind::Agent => Self::Agent,
            NodeKind::Flow => Self::Flow,
            NodeKind::Parallel => Self::Parallel,
            NodeKind::Until => Self::Until,
        }
    }
}

impl From<gw::WorkflowNodeKind> for NodeKind {
    fn from(kind: gw::WorkflowNodeKind) -> Self {
        match kind {
            gw::WorkflowNodeKind::Fn => Self::Fn,
            gw::WorkflowNodeKind::Agent => Self::Agent,
            gw::WorkflowNodeKind::Flow => Self::Flow,
            gw::WorkflowNodeKind::Parallel => Self::Parallel,
            gw::WorkflowNodeKind::Until => Self::Until,
        }
    }
}

impl From<NodeState> for gw::WorkflowNodeState {
    fn from(state: NodeState) -> Self {
        match state {
            NodeState::Pending => Self::Pending,
            NodeState::Running => Self::Running,
            NodeState::Done => Self::Done,
            NodeState::Skipped => Self::Skipped,
            NodeState::Stopped => Self::Stopped,
            NodeState::Failed => Self::Failed,
        }
    }
}

impl From<gw::WorkflowNodeState> for NodeState {
    fn from(state: gw::WorkflowNodeState) -> Self {
        match state {
            gw::WorkflowNodeState::Pending => Self::Pending,
            gw::WorkflowNodeState::Running => Self::Running,
            gw::WorkflowNodeState::Done => Self::Done,
            gw::WorkflowNodeState::Skipped => Self::Skipped,
            gw::WorkflowNodeState::Stopped => Self::Stopped,
            gw::WorkflowNodeState::Failed => Self::Failed,
        }
    }
}

impl From<RunSummary> for gw::WorkflowRunSummary {
    fn from(run: RunSummary) -> Self {
        Self {
            id: run.id,
            flow: run.flow,
            by: gw::WorkflowLauncher::from(run.by).into(),
            started: run.started,
            finished: run.finished,
            state: gw::WorkflowRunState::from(run.state).into(),
            detail: run.detail,
        }
    }
}

impl From<gw::WorkflowRunSummary> for RunSummary {
    fn from(run: gw::WorkflowRunSummary) -> Self {
        Self {
            by: run.by().into(),
            state: run.state().into(),
            id: run.id,
            flow: run.flow,
            started: run.started,
            finished: run.finished,
            detail: run.detail,
        }
    }
}

impl From<WorkflowListing> for gw::WorkflowListing {
    fn from(listing: WorkflowListing) -> Self {
        Self {
            flows: listing
                .flows
                .into_iter()
                .map(|flow| gw::DeclaredWorkflow {
                    name: flow.name,
                    path: flow.path,
                    description: flow.description,
                })
                .collect(),
            runs: listing.runs.into_iter().map(Into::into).collect(),
        }
    }
}

impl From<gw::WorkflowListing> for WorkflowListing {
    fn from(listing: gw::WorkflowListing) -> Self {
        Self {
            flows: listing
                .flows
                .into_iter()
                .map(|flow| DeclaredFlow {
                    name: flow.name,
                    path: flow.path,
                    description: flow.description,
                })
                .collect(),
            runs: listing.runs.into_iter().map(Into::into).collect(),
        }
    }
}

impl From<Node> for gw::WorkflowNode {
    fn from(node: Node) -> Self {
        Self {
            kind: gw::WorkflowNodeKind::from(node.kind).into(),
            state: gw::WorkflowNodeState::from(node.state).into(),
            name: node.name,
            depth: node.depth,
            edge: node.edge,
            runs: node.runs,
            retries: node.retries,
            ms: node.ms,
            detail: node.detail,
            sessions: node.sessions,
        }
    }
}

impl From<gw::WorkflowNode> for Node {
    fn from(node: gw::WorkflowNode) -> Self {
        Self {
            kind: node.kind().into(),
            state: node.state().into(),
            name: node.name,
            depth: node.depth,
            edge: node.edge,
            runs: node.runs,
            retries: node.retries,
            ms: node.ms,
            detail: node.detail,
            sessions: node.sessions,
        }
    }
}

impl From<Run> for gw::WorkflowRun {
    fn from(run: Run) -> Self {
        Self {
            summary: Some(run.summary.into()),
            input: run.input,
            nodes: run.nodes.into_iter().map(Into::into).collect(),
            output: run.output,
            log: run.log,
        }
    }
}

impl TryFrom<gw::WorkflowRun> for Run {
    type Error = Malformed;

    fn try_from(run: gw::WorkflowRun) -> Result<Self, Malformed> {
        Ok(Self {
            summary: run.summary.ok_or(Malformed("summary"))?.into(),
            input: run.input,
            nodes: run.nodes.into_iter().map(Into::into).collect(),
            output: run.output,
            log: run.log,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run() -> Run {
        Run {
            summary: RunSummary {
                id: "r1".to_owned(),
                flow: "weekly".to_owned(),
                by: Launcher::Agent,
                started: 10,
                finished: Some(20),
                state: RunState::Stopped,
                detail: Some("nothing to do".to_owned()),
            },
            input: "{}".to_owned(),
            nodes: vec![Node {
                name: "select".to_owned(),
                kind: NodeKind::Agent,
                depth: 1,
                edge: Some("map, checkpoint".to_owned()),
                state: NodeState::Failed,
                runs: 2,
                retries: 1,
                ms: Some(1500),
                detail: Some("boom".to_owned()),
                sessions: vec!["s1".to_owned()],
            }],
            output: Some("{\"a\":1}".to_owned()),
            log: "hello".to_owned(),
        }
    }

    #[test]
    fn a_run_survives_the_wire() {
        let wire = gw::WorkflowRun::from(run());
        assert_eq!(Run::try_from(wire), Ok(run()));
    }

    #[test]
    fn a_run_survives_json() {
        let json = serde_json::to_string(&run()).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(
            serde_json::from_str::<Run>(&json).unwrap_or_else(|e| panic!("{e}")),
            run()
        );
    }

    #[test]
    fn a_run_without_a_summary_is_malformed() {
        let wire = gw::WorkflowRun {
            summary: None,
            ..gw::WorkflowRun::from(run())
        };
        assert_eq!(Run::try_from(wire), Err(Malformed("summary")));
    }

    #[test]
    fn a_listing_survives_the_wire() {
        let listing = WorkflowListing {
            flows: vec![DeclaredFlow {
                name: "weekly".to_owned(),
                path: "flows/weekly.ts".to_owned(),
                description: Some("Writes the weekly article".to_owned()),
            }],
            runs: vec![run().summary],
        };
        let wire = gw::WorkflowListing::from(listing.clone());
        assert_eq!(WorkflowListing::from(wire), listing);
    }

    #[test]
    fn a_refusal_keeps_its_reason() {
        let status = tonic::Status::aborted("no flow named x");
        assert_eq!(
            WorkflowsError::from(status),
            WorkflowsError::Refused("no flow named x".to_owned())
        );
    }
}
