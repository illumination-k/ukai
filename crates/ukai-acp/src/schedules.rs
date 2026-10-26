//! Schedules a gateway runs for one of its agents: the types, and the
//! client side of the gateway's `ukai.gateway.v1.Schedules` (gRPC-Web, see
//! `crate::grpc`; [`wire`] for the conversions).
//!
//! A schedule runs a shell command in the agent's workspace, or one prompt
//! turn of the agent, on a cron pattern. The gateway keeps the schedules and
//! fires them, so they run while no client is connected; ukai only edits
//! them and reads their runs. When a schedule is due, and what is wrong with
//! one, is `ukai_core::schedule`'s business, shared by both ends.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use ukai_config::{AgentSpec, Source};
use ukai_proto::gateway::v1 as gw;
use ukai_proto::gateway::v1::schedule_change::Outcome as Change;
use ukai_proto::gateway::v1::schedules_client::SchedulesClient;

use crate::ResolveError;
use crate::gateway::GatewayAgent;
use crate::grpc::{self, Channel};

pub mod wire;

const TIMEOUT: Duration = Duration::from_secs(20);

/// What a schedule runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Task {
    /// `sh -c <command>` in the agent's checkout.
    Command { command: String },
    /// One prompt turn of the agent, in a session of its own.
    Prompt { prompt: String },
}

/// The part of a schedule its author sets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScheduleSpec {
    pub name: String,
    /// Five fields (minute hour day-of-month month day-of-week) or an alias
    /// such as `@daily`, read at [`Self::utc_offset_minutes`].
    pub cron: String,
    /// The author's offset from UTC when they saved it, e.g. 540 for JST, so
    /// `0 9 * * *` means their nine o'clock. A fixed offset: it does not
    /// follow daylight saving time.
    #[serde(default)]
    pub utc_offset_minutes: i32,
    pub task: Task,
    pub enabled: bool,
}

/// A schedule as the gateway keeps it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Schedule {
    pub id: String,
    /// The gateway agent it runs.
    pub agent: String,
    #[serde(flatten)]
    pub spec: ScheduleSpec,
    /// Every run of the schedule asks for this workspace (`pod` scope), so
    /// runs share a pod and a client can open a run's session there.
    pub workspace_key: String,
    /// When the schedule was last due, fired or not, in Unix seconds. Saving
    /// the schedule moves it to that moment, so an edit never fires for a
    /// time already past.
    pub last_due: i64,
}

/// Why a run started.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Trigger {
    Schedule,
    /// "Run now".
    Manual,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Outcome {
    Running,
    Succeeded,
    Failed {
        message: String,
    },
    /// Stopped by a user.
    Stopped,
    /// The gateway restarted while it ran.
    Interrupted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Run {
    pub trigger: Trigger,
    pub started: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished: Option<i64>,
    pub outcome: Outcome,
    /// The end of the command's stdout and stderr, or of the agent's messages.
    #[serde(default)]
    pub output: String,
    /// Whether `output` lost its beginning.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
    /// The ACP session of a prompt run, to open in a pane afterwards.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
}

/// A schedule as listed: with when it fires next and how it last went.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScheduleInfo {
    #[serde(flatten)]
    pub schedule: Schedule,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_due: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_run: Option<Run>,
}

#[derive(Debug, thiserror::Error)]
pub enum SchedulesError {
    #[error("cannot reach the gateway: {0}")]
    Unreachable(String),
    #[error("{}", .0.join("; "))]
    Refused(Vec<String>),
    /// No such schedule, or the gateway does not know the agent.
    #[error("no such schedule")]
    NotFound,
    #[error("the gateway refused: {0}")]
    Denied(String),
    #[error("the gateway failed: {0}")]
    Failed(String),
    #[error("the gateway sent something that cannot be read: {0}")]
    Malformed(String),
}

impl From<tonic::Status> for SchedulesError {
    fn from(status: tonic::Status) -> Self {
        match grpc::Failure::from(status) {
            grpc::Failure::Unreachable(why) => Self::Unreachable(why),
            grpc::Failure::NotFound => Self::NotFound,
            grpc::Failure::Denied(why) => Self::Denied(why),
            grpc::Failure::TooMany => Self::Failed("too many requests".to_owned()),
            grpc::Failure::NotRunning => Self::Failed("the workspace is not running".to_owned()),
            grpc::Failure::Failed(why) => Self::Failed(why),
        }
    }
}

impl From<wire::Malformed> for SchedulesError {
    fn from(error: wire::Malformed) -> Self {
        Self::Malformed(error.to_string())
    }
}

/// One gateway agent's schedules, with the agent's headers.
#[derive(Debug, Clone)]
pub struct SchedulesEndpoint {
    channel: Channel,
    root: String,
    agent: String,
}

impl PartialEq for SchedulesEndpoint {
    fn eq(&self, other: &Self) -> bool {
        self.root == other.root && self.agent == other.agent
    }
}

impl SchedulesEndpoint {
    /// The schedules of `spec`'s agent, or `None` when it is not a gateway
    /// agent: nothing else keeps schedules.
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

    fn client(&self) -> SchedulesClient<Channel> {
        SchedulesClient::new(self.channel.clone())
    }

    fn one(&self, id: &str) -> gw::ScheduleRequest {
        gw::ScheduleRequest {
            agent: self.agent.clone(),
            id: id.to_owned(),
        }
    }

    pub async fn list(&self) -> Result<Vec<ScheduleInfo>, SchedulesError> {
        let request = gw::AgentRequest {
            agent: self.agent.clone(),
        };
        let response = self.client().list(request).await?.into_inner();
        Ok(response
            .schedules
            .into_iter()
            .map(ScheduleInfo::try_from)
            .collect::<Result<_, _>>()?)
    }

    pub async fn create(&self, spec: &ScheduleSpec) -> Result<ScheduleInfo, SchedulesError> {
        let request = gw::CreateScheduleRequest {
            agent: self.agent.clone(),
            spec: Some(spec.clone().into()),
        };
        saved(self.client().create(request).await?.into_inner())
    }

    pub async fn get(&self, id: &str) -> Result<ScheduleInfo, SchedulesError> {
        let response = self.client().get(self.one(id)).await?;
        Ok(response.into_inner().try_into()?)
    }

    pub async fn update(
        &self,
        id: &str,
        spec: &ScheduleSpec,
    ) -> Result<ScheduleInfo, SchedulesError> {
        let request = gw::UpdateScheduleRequest {
            agent: self.agent.clone(),
            id: id.to_owned(),
            spec: Some(spec.clone().into()),
        };
        saved(self.client().update(request).await?.into_inner())
    }

    pub async fn delete(&self, id: &str) -> Result<(), SchedulesError> {
        self.client().delete(self.one(id)).await?;
        Ok(())
    }

    /// Start a run now; the schedule as it is afterwards.
    pub async fn run(&self, id: &str) -> Result<ScheduleInfo, SchedulesError> {
        let response = self.client().run(self.one(id)).await?;
        Ok(response.into_inner().try_into()?)
    }

    pub async fn stop(&self, id: &str) -> Result<ScheduleInfo, SchedulesError> {
        let response = self.client().stop(self.one(id)).await?;
        Ok(response.into_inner().try_into()?)
    }

    /// The schedule's recent runs, newest first.
    pub async fn runs(&self, id: &str) -> Result<Vec<Run>, SchedulesError> {
        let response = self.client().runs(self.one(id)).await?;
        Ok(response
            .into_inner()
            .runs
            .into_iter()
            .map(Run::try_from)
            .collect::<Result<_, _>>()?)
    }
}

/// The schedule a change saved, or every reason it was refused.
fn saved(change: gw::ScheduleChange) -> Result<ScheduleInfo, SchedulesError> {
    match change.outcome {
        Some(Change::Saved(info)) => Ok(info.try_into()?),
        Some(Change::Refused(refused)) => Err(SchedulesError::Refused(refused.problems)),
        None => Err(wire::Malformed("outcome").into()),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use rstest::rstest;
    use ukai_config::WorkspaceScope;

    use super::*;

    #[test]
    fn resolves_a_gateway_agents_schedules() {
        let spec = AgentSpec::Gateway {
            url: "wss://gw.example.com/agents/claude/acp".into(),
            headers: BTreeMap::from([("Authorization".into(), "Bearer t".into())]),
            workspace: WorkspaceScope::Pod,
        };
        let endpoint = SchedulesEndpoint::resolve(&spec, |_, _| None)
            .unwrap_or_else(|e| panic!("{e}"))
            .unwrap_or_else(|| panic!("a gateway agent has schedules"));
        assert_eq!(
            (endpoint.root.as_str(), endpoint.agent.as_str()),
            ("https://gw.example.com", "claude")
        );
    }

    #[rstest]
    #[case::stdio(AgentSpec::Stdio { command: "agent".into(), args: Vec::new(), env: BTreeMap::new() })]
    #[case::websocket(AgentSpec::Websocket { url: "wss://example.com/acp".into(), headers: BTreeMap::new() })]
    fn other_transports_have_no_schedules(#[case] spec: AgentSpec) {
        assert!(matches!(
            SchedulesEndpoint::resolve(&spec, |_, _| None),
            Ok(None)
        ));
    }

    #[test]
    fn a_listed_schedule_reads_back_flat() {
        let info = ScheduleInfo {
            schedule: Schedule {
                id: "a1".into(),
                agent: "claude".into(),
                spec: ScheduleSpec {
                    name: "Nightly".into(),
                    cron: "0 3 * * *".into(),
                    utc_offset_minutes: 540,
                    task: Task::Prompt {
                        prompt: "triage".into(),
                    },
                    enabled: true,
                },
                workspace_key: "s-a1".into(),
                last_due: 1,
            },
            next_due: Some(2),
            last_run: None,
        };
        let json = serde_json::to_value(&info).unwrap_or_default();
        assert_eq!(json["cron"], "0 3 * * *");
        assert_eq!(json["task"]["kind"], "prompt");
        assert_eq!(
            serde_json::from_value::<ScheduleInfo>(json).ok(),
            Some(info)
        );
    }
}
