//! Metrics per pull request (see "PR ごとのメトリクス" in `docs/design.md`):
//! the types of what each prompt turn of a gateway agent's sessions took,
//! and of what the gateway answers per branch, as they cross the wire
//! (`ukai.metrics.v1`, `ukai.gateway.v1.Metrics`); [`TurnsEndpoint`], the
//! relay's client of the gateway's `WorkspaceMetrics`; and
//! [`MetricsEndpoint`], the app's client of its `Metrics`.
//!
//! What a turn is made of, and how turns add up to a branch, is
//! `ukai_core::metrics`.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use ukai_config::{AgentSpec, Source};
use ukai_proto::gateway::v1 as gw;
use ukai_proto::metrics::v1 as pb;

use crate::ResolveError;
use crate::gateway::GatewayAgent;
use crate::grpc::Channel;
pub use crate::session::Cost;

/// How long a request may take: the gateway may ask GitHub about every
/// branch.
const TIMEOUT: Duration = Duration::from_secs(60);

/// Tokens a turn used, as the agent reported them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Tokens {
    pub total: u64,
    pub input: u64,
    pub output: u64,
    pub thought: Option<u64>,
    pub cached_read: Option<u64>,
    pub cached_write: Option<u64>,
}

/// The tool calls (or CI runs) of one name.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ToolStat {
    pub name: String,
    pub count: u32,
    pub failed: u32,
    pub millis: u64,
}

/// One prompt turn of a session. Times are Unix milliseconds.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Turn {
    pub session: String,
    pub branch: Option<String>,
    pub started: i64,
    pub ended: i64,
    pub stop_reason: Option<String>,
    pub tokens: Option<Tokens>,
    pub cost: Option<Cost>,
    pub context_used: u64,
    pub context_size: u64,
    pub tools: Vec<ToolStat>,
}

/// A pull request as GitHub has it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PullRequest {
    pub number: u64,
    pub title: String,
    pub url: String,
    /// `open`, `closed`, or `merged`.
    pub state: String,
    pub created: i64,
    pub merged: Option<i64>,
    pub closed: Option<i64>,
}

/// GitHub Actions runs on a branch.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Ci {
    pub runs: u32,
    pub failed: u32,
    pub millis: u64,
    pub workflows: Vec<ToolStat>,
}

/// What an agent's sessions spent on one branch.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Branch {
    /// Empty for turns that ended off any branch.
    pub branch: String,
    pub repo: Option<String>,
    pub pull_request: Option<PullRequest>,
    pub ci: Option<Ci>,
    pub github_problem: Option<String>,
    pub sessions: u32,
    pub turns: u32,
    pub agent_millis: u64,
    pub tokens: Option<Tokens>,
    pub cost: Vec<Cost>,
    pub context_peak: u64,
    pub tools: Vec<ToolStat>,
    pub first: i64,
    pub last: i64,
}

impl From<pb::Tokens> for Tokens {
    fn from(t: pb::Tokens) -> Self {
        Self {
            total: t.total,
            input: t.input,
            output: t.output,
            thought: t.thought,
            cached_read: t.cached_read,
            cached_write: t.cached_write,
        }
    }
}

impl From<Tokens> for pb::Tokens {
    fn from(t: Tokens) -> Self {
        Self {
            total: t.total,
            input: t.input,
            output: t.output,
            thought: t.thought,
            cached_read: t.cached_read,
            cached_write: t.cached_write,
        }
    }
}

impl From<pb::Cost> for Cost {
    fn from(c: pb::Cost) -> Self {
        Self {
            amount: c.amount,
            currency: c.currency,
        }
    }
}

impl From<Cost> for pb::Cost {
    fn from(c: Cost) -> Self {
        Self {
            amount: c.amount,
            currency: c.currency,
        }
    }
}

impl From<pb::ToolStat> for ToolStat {
    fn from(t: pb::ToolStat) -> Self {
        Self {
            name: t.name,
            count: t.count,
            failed: t.failed,
            millis: t.millis,
        }
    }
}

impl From<ToolStat> for pb::ToolStat {
    fn from(t: ToolStat) -> Self {
        Self {
            name: t.name,
            count: t.count,
            failed: t.failed,
            millis: t.millis,
        }
    }
}

impl From<pb::Turn> for Turn {
    fn from(t: pb::Turn) -> Self {
        Self {
            session: t.session,
            branch: t.branch,
            started: t.started,
            ended: t.ended,
            stop_reason: t.stop_reason,
            tokens: t.tokens.map(Into::into),
            cost: t.cost.map(Into::into),
            context_used: t.context_used,
            context_size: t.context_size,
            tools: t.tools.into_iter().map(Into::into).collect(),
        }
    }
}

impl From<Turn> for pb::Turn {
    fn from(t: Turn) -> Self {
        Self {
            session: t.session,
            branch: t.branch,
            started: t.started,
            ended: t.ended,
            stop_reason: t.stop_reason,
            tokens: t.tokens.map(Into::into),
            cost: t.cost.map(Into::into),
            context_used: t.context_used,
            context_size: t.context_size,
            tools: t.tools.into_iter().map(Into::into).collect(),
        }
    }
}

impl From<gw::PullRequest> for PullRequest {
    fn from(p: gw::PullRequest) -> Self {
        Self {
            number: p.number,
            title: p.title,
            url: p.url,
            state: p.state,
            created: p.created,
            merged: p.merged,
            closed: p.closed,
        }
    }
}

impl From<PullRequest> for gw::PullRequest {
    fn from(p: PullRequest) -> Self {
        Self {
            number: p.number,
            title: p.title,
            url: p.url,
            state: p.state,
            created: p.created,
            merged: p.merged,
            closed: p.closed,
        }
    }
}

impl From<gw::Ci> for Ci {
    fn from(c: gw::Ci) -> Self {
        Self {
            runs: c.runs,
            failed: c.failed,
            millis: c.millis,
            workflows: c.workflows.into_iter().map(Into::into).collect(),
        }
    }
}

impl From<Ci> for gw::Ci {
    fn from(c: Ci) -> Self {
        Self {
            runs: c.runs,
            failed: c.failed,
            millis: c.millis,
            workflows: c.workflows.into_iter().map(Into::into).collect(),
        }
    }
}

impl From<gw::Branch> for Branch {
    fn from(b: gw::Branch) -> Self {
        Self {
            branch: b.branch,
            repo: b.repo,
            pull_request: b.pull_request.map(Into::into),
            ci: b.ci.map(Into::into),
            github_problem: b.github_problem,
            sessions: b.sessions,
            turns: b.turns,
            agent_millis: b.agent_millis,
            tokens: b.tokens.map(Into::into),
            cost: b.cost.into_iter().map(Into::into).collect(),
            context_peak: b.context_peak,
            tools: b.tools.into_iter().map(Into::into).collect(),
            first: b.first,
            last: b.last,
        }
    }
}

impl From<Branch> for gw::Branch {
    fn from(b: Branch) -> Self {
        Self {
            branch: b.branch,
            repo: b.repo,
            pull_request: b.pull_request.map(Into::into),
            ci: b.ci.map(Into::into),
            github_problem: b.github_problem,
            sessions: b.sessions,
            turns: b.turns,
            agent_millis: b.agent_millis,
            tokens: b.tokens.map(Into::into),
            cost: b.cost.into_iter().map(Into::into).collect(),
            context_peak: b.context_peak,
            tools: b.tools.into_iter().map(Into::into).collect(),
            first: b.first,
            last: b.last,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum MetricsError {
    #[error("{}", .0.join("; "))]
    Refused(Vec<String>),
    /// The gateway keeps no store, or predates metrics.
    #[error("the gateway keeps no metrics: {0}")]
    Unavailable(String),
    #[error("cannot reach the gateway: {0}")]
    Failed(String),
}

impl From<tonic::Status> for MetricsError {
    fn from(status: tonic::Status) -> Self {
        let message = status.message().to_owned();
        match status.code() {
            tonic::Code::Unavailable | tonic::Code::Unimplemented => Self::Unavailable(message),
            _ => Self::Failed(message),
        }
    }
}

/// The gateway's `WorkspaceMetrics`, from an agent's pod.
#[derive(Debug, Clone)]
pub struct TurnsEndpoint {
    channel: Channel,
}

impl TurnsEndpoint {
    /// The gateway at `gateway` (e.g. `http://ukai-gateway`), with the
    /// relay's `token`.
    pub fn for_workspace(gateway: &str, token: &str) -> Result<Self, ResolveError> {
        let (channel, _) = crate::grpc::pod_channel(gateway, token, TIMEOUT)?;
        Ok(Self { channel })
    }

    pub async fn record(&self, turn: Turn) -> Result<(), MetricsError> {
        use gw::workspace_metrics_client::WorkspaceMetricsClient;
        let response = WorkspaceMetricsClient::new(self.channel.clone())
            .record(pb::Turn::from(turn))
            .await?
            .into_inner();
        if response.problems.is_empty() {
            Ok(())
        } else {
            Err(MetricsError::Refused(response.problems))
        }
    }
}

/// One gateway agent's metrics per branch, with the agent's headers.
#[derive(Debug, Clone)]
pub struct MetricsEndpoint {
    channel: Channel,
    root: String,
    agent: String,
}

impl PartialEq for MetricsEndpoint {
    fn eq(&self, other: &Self) -> bool {
        self.root == other.root && self.agent == other.agent
    }
}

impl MetricsEndpoint {
    /// The metrics of `spec`'s agent, or `None` when it is not a gateway
    /// agent: nothing else records them.
    pub fn resolve(
        spec: &AgentSpec,
        lookup: impl Fn(Source, &str) -> Option<String> + Copy,
    ) -> Result<Option<Self>, ResolveError> {
        let Some(agent) = GatewayAgent::resolve(spec, lookup)? else {
            return Ok(None);
        };
        Ok(Some(Self {
            channel: agent.channel(TIMEOUT)?,
            root: agent.root,
            agent: agent.id,
        }))
    }

    /// The agent's branches over the last `days` (the gateway's default
    /// for 0), newest activity first.
    pub async fn branches(&self, days: u32) -> Result<Vec<Branch>, MetricsError> {
        use gw::metrics_client::MetricsClient;
        let response = MetricsClient::new(self.channel.clone())
            .branches(gw::BranchesRequest {
                agent: self.agent.clone(),
                days,
            })
            .await?
            .into_inner();
        if let Some(problem) = response.problem {
            return Err(MetricsError::Unavailable(problem));
        }
        Ok(response.branches.into_iter().map(Into::into).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_turn_survives_the_wire() {
        let turn = Turn {
            session: "s".into(),
            branch: Some("feat/x".into()),
            started: 1,
            ended: 2,
            stop_reason: Some("end_turn".into()),
            tokens: Some(Tokens {
                total: 3,
                input: 2,
                output: 1,
                thought: None,
                cached_read: Some(5),
                cached_write: None,
            }),
            cost: Some(Cost {
                amount: 0.5,
                currency: "USD".into(),
            }),
            context_used: 10,
            context_size: 20,
            tools: vec![ToolStat {
                name: "$ cargo test".into(),
                count: 2,
                failed: 1,
                millis: 30,
            }],
        };
        assert_eq!(Turn::from(pb::Turn::from(turn.clone())), turn);
    }

    #[test]
    fn a_branch_survives_the_wire() {
        let branch = Branch {
            branch: "b".into(),
            repo: Some("o/r".into()),
            pull_request: Some(PullRequest {
                number: 7,
                title: "t".into(),
                url: "https://github.com/o/r/pull/7".into(),
                state: "merged".into(),
                created: 1,
                merged: Some(2),
                closed: Some(2),
            }),
            ci: Some(Ci {
                runs: 2,
                failed: 1,
                millis: 9,
                workflows: vec![ToolStat::default()],
            }),
            github_problem: None,
            sessions: 1,
            turns: 2,
            agent_millis: 3,
            tokens: Some(Tokens::default()),
            cost: vec![Cost {
                amount: 1.0,
                currency: "USD".into(),
            }],
            context_peak: 4,
            tools: Vec::new(),
            first: 5,
            last: 6,
        };
        assert_eq!(Branch::from(gw::Branch::from(branch.clone())), branch);
    }
}
