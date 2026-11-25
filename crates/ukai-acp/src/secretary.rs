//! A secretary agent's view of the other workspaces (see "秘書" in
//! `docs/design.md`): what a relay answers about its sessions at
//! [`SESSIONS_PATH`], which the gateway reads, and [`SecretaryEndpoint`],
//! the secretary pod's client of the gateway's `WorkspaceSecretary`, and
//! [`OverviewEndpoint`], the app's view of the same through `Agents`, for
//! the secretary pane. [`marked`] is how a secretary's prompt reads in the
//! session it goes to, and [`unmarked`] how the app tells it apart.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use ukai_proto::gateway::v1 as gw;

use crate::ResolveError;
use crate::gateway::GatewayAgent;
use ukai_config::{AgentSpec, Source};

/// Where a relay lists its sessions (`GET`) and starts one (`POST`, with a
/// [`PromptBody`]). One session is `<it>/<id>`: `GET <it>/<id>/transcript`
/// reads its end (`?limit=`), and `POST <it>/<id>/prompt` sends it a
/// [`PromptBody`].
pub const SESSIONS_PATH: &str = "/sessions";

/// How many transcript entries a relay returns when asked for none, and at
/// most.
pub const TRANSCRIPT_DEFAULT: usize = 20;
pub const TRANSCRIPT_MAX: usize = 200;

/// How much of a prompt or reply a summary keeps, and of one transcript
/// entry, in characters.
pub const SUMMARY_CHARS: usize = 300;
pub const ENTRY_CHARS: usize = 4000;

/// How long a start may take: the gateway may have to start the pod, which
/// can mean pulling its image and cloning.
const START_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// One session as a relay lists it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionSummary {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub running: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    /// The agent asks something no one has answered.
    pub waiting: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_reply: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    User,
    Agent,
    Tool,
}

/// One turn's message or tool call, chunks joined.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub kind: EntryKind,
    pub text: String,
}

/// What `POST` to [`SESSIONS_PATH`] and to a session's `prompt` take.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptBody {
    pub text: String,
}

/// The query of a session's `transcript`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranscriptQuery {
    pub limit: Option<usize>,
}

/// What `POST` to [`SESSIONS_PATH`] answers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Started {
    pub session: String,
}

/// The start of the line [`marked`] puts first.
const MARK_START: &str = "[ukai] This prompt is from `";
const MARK_END: &str = "`, the user's secretary agent, not from the user.\n\n";

/// `prompt` as the secretary `agent` sends it: marked, so the agent
/// receiving it and the user reading the session both see whose it is.
pub fn marked(agent: &str, prompt: &str) -> String {
    format!("{MARK_START}{agent}{MARK_END}{prompt}")
}

/// The secretary and the prompt of a text [`marked`] made; `None` for any
/// other text, such as the user's own.
pub fn unmarked(text: &str) -> Option<(&str, &str)> {
    let rest = text.strip_prefix(MARK_START)?;
    let (agent, prompt) = rest.split_once(MARK_END)?;
    (!agent.is_empty() && !agent.contains('`')).then_some((agent, prompt))
}

/// `text` cut to `max` characters, marked when cut.
pub fn clip(text: &str, max: usize) -> String {
    let text = text.trim();
    match text.char_indices().nth(max) {
        Some((end, _)) => format!("{}…", &text[..end]),
        None => text.to_owned(),
    }
}

impl From<SessionSummary> for gw::SessionSummary {
    fn from(summary: SessionSummary) -> Self {
        Self {
            id: summary.id,
            title: summary.title,
            running: summary.running,
            stop_reason: summary.stop_reason,
            waiting: summary.waiting,
            last_prompt: summary.last_prompt,
            last_reply: summary.last_reply,
        }
    }
}

impl From<gw::SessionSummary> for SessionSummary {
    fn from(summary: gw::SessionSummary) -> Self {
        Self {
            id: summary.id,
            title: summary.title,
            running: summary.running,
            stop_reason: summary.stop_reason,
            waiting: summary.waiting,
            last_prompt: summary.last_prompt,
            last_reply: summary.last_reply,
        }
    }
}

/// A workspace with a running pod, as the app's secretary pane lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunningWorkspace {
    /// The agent's id in `gateway.toml`.
    pub agent: String,
    /// The pane key of a workspace of its own; `None` for the shared one.
    pub pane: Option<String>,
    pub sessions: Vec<SessionSummary>,
    /// Why its relay could not list its sessions.
    pub unreachable: Option<String>,
}

impl From<gw::RunningWorkspace> for RunningWorkspace {
    fn from(workspace: gw::RunningWorkspace) -> Self {
        let pane = workspace
            .workspace
            .filter(|workspace| workspace.scope() == gw::WorkspaceScope::Pod)
            .map(|workspace| workspace.key);
        Self {
            agent: workspace.agent,
            pane,
            sessions: workspace.sessions.into_iter().map(Into::into).collect(),
            unreachable: workspace.unreachable,
        }
    }
}

impl From<Entry> for gw::TranscriptEntry {
    fn from(entry: Entry) -> Self {
        let kind = match entry.kind {
            EntryKind::User => gw::TranscriptKind::User,
            EntryKind::Agent => gw::TranscriptKind::Agent,
            EntryKind::Tool => gw::TranscriptKind::Tool,
        };
        Self {
            kind: kind.into(),
            text: entry.text,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SecretaryError {
    /// The gateway or relay does not have what was named.
    #[error("{0}")]
    NotFound(String),
    /// Not now: the workspace is not running, or the session's turn is.
    #[error("{0}")]
    NotNow(String),
    #[error("{0}")]
    Refused(String),
    #[error("cannot reach the gateway: {0}")]
    Failed(String),
}

impl From<tonic::Status> for SecretaryError {
    fn from(status: tonic::Status) -> Self {
        let message = status.message().to_owned();
        match status.code() {
            tonic::Code::NotFound => Self::NotFound(message),
            tonic::Code::FailedPrecondition => Self::NotNow(message),
            tonic::Code::PermissionDenied | tonic::Code::InvalidArgument => Self::Refused(message),
            _ => Self::Failed(message),
        }
    }
}

/// The gateway's `WorkspaceSecretary`, from a secretary's pod.
#[derive(Debug, Clone)]
pub struct SecretaryEndpoint {
    channel: crate::grpc::Channel,
}

type Client = gw::workspace_secretary_client::WorkspaceSecretaryClient<crate::grpc::Channel>;

impl SecretaryEndpoint {
    /// The gateway at `gateway` (e.g. `http://ukai-gateway`), with the
    /// relay's `token`.
    pub fn for_workspace(gateway: &str, token: &str) -> Result<Self, ResolveError> {
        let (channel, _) = crate::grpc::pod_channel(gateway, token, START_TIMEOUT)?;
        Ok(Self { channel })
    }

    fn client(&self) -> Client {
        Client::new(self.channel.clone())
    }

    pub async fn list(&self) -> Result<Vec<gw::RunningWorkspace>, SecretaryError> {
        Ok(self
            .client()
            .list_workspaces(gw::ListWorkspacesRequest {})
            .await?
            .into_inner()
            .workspaces)
    }

    pub async fn read(
        &self,
        session: gw::SessionAddress,
        limit: u32,
    ) -> Result<Vec<gw::TranscriptEntry>, SecretaryError> {
        Ok(self
            .client()
            .read_session(gw::ReadSessionRequest {
                session: Some(session),
                limit,
            })
            .await?
            .into_inner()
            .entries)
    }

    pub async fn prompt(
        &self,
        session: gw::SessionAddress,
        prompt: &str,
    ) -> Result<(), SecretaryError> {
        self.client()
            .prompt_session(gw::PromptSessionRequest {
                session: Some(session),
                prompt: prompt.to_owned(),
            })
            .await?;
        Ok(())
    }

    /// The new session's id.
    pub async fn start(
        &self,
        agent: &str,
        workspace: gw::Workspace,
        prompt: &str,
    ) -> Result<String, SecretaryError> {
        Ok(self
            .client()
            .start_session(gw::StartSessionRequest {
                agent: agent.to_owned(),
                workspace: Some(workspace),
                prompt: prompt.to_owned(),
            })
            .await?
            .into_inner()
            .session)
    }
}

/// How long the overview may take: the gateway asks every relay, each
/// within 5 seconds, at once.
const OVERVIEW_TIMEOUT: Duration = Duration::from_secs(15);

/// The running workspaces of a gateway agent's gateway, for the app
/// (`Agents.Workspaces`, with the gateway's token).
#[derive(Debug, Clone)]
pub struct OverviewEndpoint {
    channel: crate::grpc::Channel,
}

impl OverviewEndpoint {
    /// The gateway of `spec`, or `None` when it is not a gateway agent.
    pub fn resolve(
        spec: &AgentSpec,
        lookup: impl Fn(Source, &str) -> Option<String> + Copy,
    ) -> Result<Option<Self>, ResolveError> {
        let Some(agent) = GatewayAgent::resolve(spec, lookup)? else {
            return Ok(None);
        };
        Ok(Some(Self {
            channel: agent.channel(OVERVIEW_TIMEOUT)?,
        }))
    }

    pub async fn list(&self) -> Result<Vec<RunningWorkspace>, SecretaryError> {
        Ok(gw::agents_client::AgentsClient::new(self.channel.clone())
            .workspaces(gw::ListWorkspacesRequest {})
            .await?
            .into_inner()
            .workspaces
            .into_iter()
            .map(Into::into)
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case::short("hi", 5, "hi")]
    #[case::exact("hello", 5, "hello")]
    #[case::cut("hello world", 5, "hello…")]
    #[case::characters_not_bytes("あいうえおか", 3, "あいう…")]
    #[case::trimmed("  hi \n", 5, "hi")]
    fn clips(#[case] text: &str, #[case] max: usize, #[case] expected: &str) {
        assert_eq!(clip(text, max), expected);
    }

    #[test]
    fn a_marked_prompt_says_whose_it_is() {
        let text = marked("aide", "rebase on main");
        assert!(
            text.starts_with("[ukai] This prompt is from `aide`"),
            "{text}"
        );
        assert!(text.ends_with("\n\nrebase on main"), "{text}");
        assert_eq!(unmarked(&text), Some(("aide", "rebase on main")));
    }

    #[rstest]
    #[case::the_users("rebase on main")]
    #[case::only_the_start("[ukai] This prompt is from `aide`")]
    #[case::no_agent(
        "[ukai] This prompt is from ``, the user's secretary agent, not from the user.\n\nx"
    )]
    #[case::quoted_further_in(
        "see: [ukai] This prompt is from `aide`, the user's secretary agent, not from the user.\n\nx"
    )]
    fn other_text_is_not_marked(#[case] text: &str) {
        assert_eq!(unmarked(text), None);
    }

    #[test]
    fn a_workspace_of_its_own_keeps_its_key() {
        let listed = |scope: gw::WorkspaceScope| {
            RunningWorkspace::from(gw::RunningWorkspace {
                agent: "cloud".into(),
                workspace: Some(gw::Workspace {
                    scope: scope.into(),
                    key: "k1".into(),
                }),
                sessions: vec![gw::SessionSummary {
                    id: "s1".into(),
                    waiting: true,
                    ..gw::SessionSummary::default()
                }],
                unreachable: None,
            })
        };
        let own = listed(gw::WorkspaceScope::Pod);
        assert_eq!(own.pane.as_deref(), Some("k1"));
        assert!(own.sessions[0].waiting);
        assert_eq!(listed(gw::WorkspaceScope::Shared).pane, None);
    }

    #[test]
    fn a_summary_reads_as_the_relay_writes_it() {
        let summary = SessionSummary {
            id: "s1".into(),
            running: true,
            last_prompt: Some("fix it".into()),
            ..SessionSummary::default()
        };
        let text = serde_json::to_string(&summary).unwrap_or_default();
        assert!(text.contains("\"lastPrompt\""), "{text}");
        assert_eq!(
            serde_json::from_str::<SessionSummary>(&text).ok(),
            Some(summary)
        );
    }
}
