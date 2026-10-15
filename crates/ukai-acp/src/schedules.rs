//! Schedules a gateway runs for one of its agents: the wire types, and the
//! client side of `/agents/<id>/schedules`.
//!
//! A schedule runs a shell command in the agent's workspace, or one prompt
//! turn of the agent, on a cron pattern. The gateway keeps the schedules and
//! fires them, so they run while no client is connected; ukai only edits
//! them and reads their runs. When a schedule is due, and what is wrong with
//! one, is `ukai_core::schedule`'s business, shared by both ends.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use ukai_config::{AgentSpec, Source};

use crate::{ResolveError, header_map};

/// Suffix of an agent's ACP endpoint that lists its schedules. One schedule
/// is `<it>/<id>`, with `/runs`, `/run`, and `/stop` below that.
pub const SCHEDULES_PATH: &str = "/schedules";

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

/// The body of a 422: every reason a schedule was refused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Refusal {
    pub problems: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum SchedulesError {
    #[error("cannot reach the gateway: {0}")]
    Unreachable(String),
    #[error("{}", .0.join("; "))]
    Refused(Vec<String>),
    #[error("no such schedule")]
    NotFound,
    #[error("the gateway answered {0}")]
    Status(u16),
    #[error("the gateway sent something that cannot be read: {0}")]
    Body(String),
}

/// One gateway agent's schedules, with the agent's headers.
#[derive(Debug, Clone)]
pub struct SchedulesEndpoint {
    client: reqwest::Client,
    base: String,
}

impl PartialEq for SchedulesEndpoint {
    fn eq(&self, other: &Self) -> bool {
        self.base == other.base
    }
}

impl SchedulesEndpoint {
    /// The schedules of `spec`'s agent, or `None` when it is not a gateway
    /// agent: nothing else keeps schedules.
    pub fn resolve(
        spec: &AgentSpec,
        lookup: impl Fn(Source, &str) -> Option<String> + Copy,
    ) -> Result<Option<Self>, ResolveError> {
        let AgentSpec::Gateway { url, headers, .. } = spec else {
            return Ok(None);
        };
        let Some(base) =
            crate::reports::endpoint_base(&ukai_config::interpolate(url, lookup)?, SCHEDULES_PATH)
        else {
            return Ok(None);
        };
        let client = reqwest::Client::builder()
            .default_headers(header_map(headers, &[], lookup)?)
            .timeout(TIMEOUT)
            .build()
            .map_err(|error| ResolveError::Client(error.to_string()))?;
        Ok(Some(Self { client, base }))
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    pub async fn list(&self) -> Result<Vec<ScheduleInfo>, SchedulesError> {
        self.call(self.client.get(&self.base)).await
    }

    pub async fn create(&self, spec: &ScheduleSpec) -> Result<ScheduleInfo, SchedulesError> {
        self.call(self.client.post(&self.base).json(spec)).await
    }

    pub async fn get(&self, id: &str) -> Result<ScheduleInfo, SchedulesError> {
        self.call(self.client.get(self.url(id, ""))).await
    }

    pub async fn update(
        &self,
        id: &str,
        spec: &ScheduleSpec,
    ) -> Result<ScheduleInfo, SchedulesError> {
        self.call(self.client.put(self.url(id, "")).json(spec))
            .await
    }

    pub async fn delete(&self, id: &str) -> Result<(), SchedulesError> {
        self.call::<serde_json::Value>(self.client.delete(self.url(id, "")))
            .await
            .map(drop)
    }

    /// Start a run now; the schedule as it is afterwards.
    pub async fn run(&self, id: &str) -> Result<ScheduleInfo, SchedulesError> {
        self.call(self.client.post(self.url(id, "/run"))).await
    }

    pub async fn stop(&self, id: &str) -> Result<ScheduleInfo, SchedulesError> {
        self.call(self.client.post(self.url(id, "/stop"))).await
    }

    /// The schedule's recent runs, newest first.
    pub async fn runs(&self, id: &str) -> Result<Vec<Run>, SchedulesError> {
        self.call(self.client.get(self.url(id, "/runs"))).await
    }

    fn url(&self, id: &str, suffix: &str) -> String {
        let id: String = percent_encode(id);
        format!("{}/{id}{suffix}", self.base)
    }

    async fn call<T: serde::de::DeserializeOwned>(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<T, SchedulesError> {
        let response = request
            .send()
            .await
            .map_err(|error| SchedulesError::Unreachable(error.to_string()))?;
        match response.status().as_u16() {
            200 | 201 => response
                .json::<T>()
                .await
                .map_err(|error| SchedulesError::Body(error.to_string())),
            404 => Err(SchedulesError::NotFound),
            422 => Err(match response.json::<Refusal>().await {
                Ok(refusal) => SchedulesError::Refused(refusal.problems),
                Err(error) => SchedulesError::Body(error.to_string()),
            }),
            status => Err(SchedulesError::Status(status)),
        }
    }
}

/// Ids are made by the gateway from letters and digits; anything else is
/// escaped rather than trusted.
fn percent_encode(id: &str) -> String {
    id.bytes()
        .map(|byte| match byte {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' => (byte as char).to_string(),
            other => format!("%{other:02X}"),
        })
        .collect()
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
            endpoint.base(),
            "https://gw.example.com/agents/claude/schedules"
        );
        assert_eq!(
            endpoint.url("a1/../b", "/runs"),
            "https://gw.example.com/agents/claude/schedules/a1%2F%2E%2E%2Fb/runs"
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
