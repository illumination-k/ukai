//! Whether an agent is behind its latest release, and restarting it to
//! catch up.
//!
//! An agent names itself in its `initialize` response (`agentInfo`). When
//! that name is an npm package, as for `@agentclientprotocol/claude-agent-acp`,
//! the registry says what its latest version is; [`crate::SessionOptions`]
//! takes the lookup as an [`UpdateCheck`], and a session that finds itself
//! behind asks before starting (see [`crate::SessionEvent::UpdateAvailable`]).
//!
//! Catching up means restarting the agent so that its command fetches the
//! release again, e.g. `npx -y <package>@latest`. A local agent restarts
//! with its session; a gateway agent's pod is replaced through
//! [`RestartEndpoint`].

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use ukai_config::{AgentSpec, Source};

use ukai_proto::gateway::v1 as gw;
use ukai_proto::gateway::v1::agents_client::AgentsClient;

use crate::gateway::GatewayAgent;
use crate::grpc::{self, Channel};
use crate::{PaneWorkspace, ResolveError};

/// An agent running an older release than its latest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Update {
    /// The package, from the agent's `agentInfo.name`.
    pub package: String,
    pub current: String,
    pub latest: String,
}

type Lookup = dyn Fn(String) -> Pin<Box<dyn Future<Output = Option<String>> + Send>> + Send + Sync;

/// Finds the latest version of a package; `None` when it cannot tell.
#[derive(Clone)]
pub struct UpdateCheck {
    lookup: Arc<Lookup>,
    /// The release a restart was meant to bring.
    expected: Option<String>,
}

impl std::fmt::Debug for UpdateCheck {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("UpdateCheck")
    }
}

/// How long the registry may take; a session waits on it before starting.
const NPM_TIMEOUT: Duration = Duration::from_secs(5);

impl UpdateCheck {
    pub fn new<F, Fut>(lookup: F) -> Self
    where
        F: Fn(String) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Option<String>> + Send + 'static,
    {
        Self {
            lookup: Arc::new(move |package| Box::pin(lookup(package))),
            expected: None,
        }
    }

    /// The check after restarting the agent to reach `latest`: still being
    /// behind it is reported rather than asked about again, since another
    /// restart would not change it.
    #[must_use]
    pub fn expecting(mut self, latest: impl Into<String>) -> Self {
        self.expected = Some(latest.into());
        self
    }

    /// Whether `update` is the one a restart already failed to bring.
    pub(crate) fn missed(&self, update: &Update) -> bool {
        self.expected.as_deref() == Some(update.latest.as_str())
    }

    /// The `latest` dist-tag on registry.npmjs.org.
    pub fn npm() -> Self {
        let client = reqwest::Client::builder().timeout(NPM_TIMEOUT).build().ok();
        Self::new(move |package: String| {
            let client = client.clone();
            async move {
                let url = format!("https://registry.npmjs.org/{package}/latest");
                let response = client?.get(url).send().await.ok()?;
                if !response.status().is_success() {
                    return None;
                }
                let body: serde_json::Value = response.json().await.ok()?;
                Some(body.get("version")?.as_str()?.to_owned())
            }
        })
    }

    /// The [`Update`] for an agent that calls itself `package` at `current`,
    /// if the latest release is newer.
    pub(crate) async fn check(&self, package: &str, current: &str) -> Option<Update> {
        if !is_npm_package(package) {
            return None;
        }
        let latest = (self.lookup)(package.to_owned()).await?;
        is_newer(current, &latest).then(|| Update {
            package: package.to_owned(),
            current: current.to_owned(),
            latest,
        })
    }
}

/// Whether `name` can be an npm package, so that asking the registry about
/// it is meaningful and the name is safe to put in a URL path.
pub fn is_npm_package(name: &str) -> bool {
    let part = |part: &str| {
        !part.is_empty()
            && !part.starts_with('.')
            && part.bytes().all(|b| {
                b.is_ascii_lowercase()
                    || b.is_ascii_digit()
                    || matches!(b, b'-' | b'.' | b'_' | b'~')
            })
    };
    match name.strip_prefix('@') {
        Some(scoped) => scoped
            .split_once('/')
            .is_some_and(|(scope, name)| part(scope) && part(name)),
        None => part(name),
    }
}

/// Whether `latest` is a later release than `current`. Versions compare by
/// their numeric `major.minor.patch`; a pre-release is behind its release.
/// Anything that does not parse is never newer, so a strange version shows
/// no prompt.
pub fn is_newer(current: &str, latest: &str) -> bool {
    match (parse(current), parse(latest)) {
        (Some((current, current_pre)), Some((latest, latest_pre))) => {
            latest > current || (latest == current && current_pre && !latest_pre)
        }
        _ => false,
    }
}

/// `(major, minor, patch)` and whether it is a pre-release.
fn parse(version: &str) -> Option<([u64; 3], bool)> {
    let version = version.strip_prefix('v').unwrap_or(version);
    let version = version.split('+').next()?;
    let (release, pre) = match version.split_once('-') {
        Some((release, _)) => (release, true),
        None => (version, false),
    };
    let mut parts = release.split('.').map(|part| part.parse::<u64>().ok());
    let numbers = [parts.next()??, parts.next()??, parts.next()??];
    parts.next().is_none().then_some((numbers, pre))
}

const RESTART_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, thiserror::Error)]
pub enum RestartError {
    #[error("cannot reach the gateway: {0}")]
    Unreachable(String),
    #[error("{0}")]
    Failed(String),
}

impl From<tonic::Status> for RestartError {
    fn from(status: tonic::Status) -> Self {
        match grpc::Failure::from(status) {
            grpc::Failure::Unreachable(why) => Self::Unreachable(why),
            grpc::Failure::NotFound => Self::Failed("the gateway has no such agent".to_owned()),
            grpc::Failure::Denied(why) => Self::Failed(format!("the gateway refused: {why}")),
            grpc::Failure::TooMany => Self::Failed("too many requests".to_owned()),
            grpc::Failure::NotRunning => Self::Failed("the workspace is not running".to_owned()),
            grpc::Failure::Failed(why) => Self::Failed(why),
        }
    }
}

/// Where a gateway agent's workspace pod is restarted (the gateway's
/// `ukai.gateway.v1.Agents.Restart`): it stops the pod, and the next
/// connection starts a fresh one.
#[derive(Debug, Clone)]
pub struct RestartEndpoint {
    channel: Channel,
    agent: String,
    workspace: gw::Workspace,
}

impl RestartEndpoint {
    /// The restart endpoint of `workspace` for `spec`, or `None` when the
    /// agent is not a gateway agent.
    pub fn resolve(
        spec: &AgentSpec,
        lookup: impl Fn(Source, &str) -> Option<String> + Copy,
        workspace: Option<&PaneWorkspace>,
    ) -> Result<Option<Self>, ResolveError> {
        let Some(agent) = GatewayAgent::resolve(spec, lookup)? else {
            return Ok(None);
        };
        Ok(Some(Self {
            channel: agent.channel(RESTART_TIMEOUT)?,
            workspace: agent.gateway.workspace(workspace),
            agent: agent.id,
        }))
    }

    pub async fn restart(&self) -> Result<(), RestartError> {
        let request = gw::RestartRequest {
            agent: self.agent.clone(),
            workspace: Some(self.workspace.clone()),
        };
        AgentsClient::new(self.channel.clone())
            .restart(request)
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use rstest::rstest;
    use ukai_config::WorkspaceScope;

    use super::*;

    #[rstest]
    #[case::minor("0.70.0", "0.91.0", true)]
    #[case::major("1.9.9", "2.0.0", true)]
    #[case::numeric_not_lexical("0.9.0", "0.10.0", true)]
    #[case::same("0.91.0", "0.91.0", false)]
    #[case::ahead("0.92.0", "0.91.0", false)]
    #[case::pre_release_behind("1.0.0-rc.1", "1.0.0", true)]
    #[case::pre_release_latest("1.0.0", "1.1.0-beta", true)]
    #[case::pre_release_of_same("1.0.0", "1.0.0-beta", false)]
    #[case::v_prefix("v0.70.0", "0.91.0", true)]
    #[case::build_metadata("0.70.0+abc", "0.70.0", false)]
    #[case::unparsable_current("dev", "0.91.0", false)]
    #[case::unparsable_latest("0.70.0", "next", false)]
    #[case::too_few_parts("0.70", "0.91.0", false)]
    #[case::too_many_parts("0.70.0.1", "0.91.0", false)]
    fn compares_versions(#[case] current: &str, #[case] latest: &str, #[case] newer: bool) {
        assert_eq!(is_newer(current, latest), newer);
    }

    #[rstest]
    #[case::scoped("@agentclientprotocol/claude-agent-acp", true)]
    #[case::plain("codex-acp", true)]
    #[case::dotted("some.agent_x~1", true)]
    #[case::display_name("Claude Agent", false)]
    #[case::uppercase("Agent", false)]
    #[case::empty("", false)]
    #[case::scope_only("@scope", false)]
    #[case::empty_scope("@/name", false)]
    #[case::nested("@a/b/c", false)]
    #[case::path("../etc", false)]
    #[case::query("a?b", false)]
    fn recognises_npm_packages(#[case] name: &str, #[case] valid: bool) {
        assert_eq!(is_npm_package(name), valid);
    }

    #[tokio::test]
    async fn checks_only_npm_names_and_newer_releases() {
        let check = UpdateCheck::new(|package: String| async move {
            (package == "@scope/agent").then(|| "2.0.0".to_owned())
        });
        assert_eq!(
            check.check("@scope/agent", "1.0.0").await,
            Some(Update {
                package: "@scope/agent".into(),
                current: "1.0.0".into(),
                latest: "2.0.0".into(),
            })
        );
        assert_eq!(check.check("@scope/agent", "2.0.0").await, None);
        assert_eq!(check.check("Other Agent", "1.0.0").await, None);
        assert_eq!(check.check("unknown", "1.0.0").await, None);
    }

    #[test]
    fn restarts_next_to_the_gateway_acp_endpoint() {
        let spec = AgentSpec::Gateway {
            url: "wss://gw.example.com/agents/a/acp".into(),
            headers: BTreeMap::new(),
            workspace: WorkspaceScope::Shared,
        };
        let endpoint = RestartEndpoint::resolve(&spec, |_, _| None, None)
            .ok()
            .flatten()
            .unwrap_or_else(|| panic!("a gateway agent restarts"));
        assert_eq!(endpoint.agent, "a");
        assert_eq!(endpoint.workspace.scope(), gw::WorkspaceScope::Shared);
    }

    #[test]
    fn only_gateway_agents_restart_through_an_endpoint() {
        let spec = AgentSpec::Stdio {
            command: "agent".into(),
            args: Vec::new(),
            env: BTreeMap::new(),
        };
        assert!(matches!(
            RestartEndpoint::resolve(&spec, |_, _| None, None),
            Ok(None)
        ));
    }
}
