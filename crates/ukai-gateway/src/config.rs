//! `gateway.toml`: the agents the gateway serves at `/agents/<id>/acp`.
//!
//! The file lives with the gateway in the cluster, so unlike `ukai.toml` it
//! is not layered with anything and nothing in it is `${env:...}`
//! interpolated: its values end up in pod specs, and secrets are referenced
//! as Kubernetes Secrets instead. [`parse`] collects every problem rather
//! than stopping at the first.

use std::collections::BTreeMap;

use serde::Deserialize;

/// The whole `gateway.toml` document.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GatewayConfig {
    #[serde(default)]
    pub agents: BTreeMap<String, KubernetesAgent>,
}

/// An agent the gateway runs in a pod per workspace, in its own namespace.
/// Values end up in a pod spec that anyone with read access to the namespace
/// can see, so nothing here is `${env:...}` interpolated; secrets are
/// referenced as Kubernetes Secrets instead.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KubernetesAgent {
    /// Image for both the agent and the workspace sidecar. It needs bash and
    /// git, and `gh` for `workspace.github_token` to work.
    pub image: String,
    /// The ACP agent, started in the repository checkout for each session.
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// Plain environment of the agent container. Values may refer to
    /// `$(POD_NAMESPACE)`, which the gateway sets first.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Agent environment from Secrets, as `<secret>/<key>`.
    #[serde(default)]
    pub secret_env: BTreeMap<String, String>,
    /// `HOME` of the agent: an emptyDir that `mounts` may overlay.
    #[serde(default = "KubernetesAgent::default_home")]
    pub home: String,
    /// uid and gid of both containers.
    #[serde(default = "KubernetesAgent::default_user")]
    pub run_as_user: i64,
    pub workspace: Workspace,
    #[serde(default)]
    pub mounts: Vec<Mount>,
    #[serde(default)]
    pub resources: Resources,
    /// Labels of the pod and its Job, on top of the gateway's own, e.g. one
    /// an admission webhook injects credentials by.
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    /// How long the pod outlives its last agent process, e.g. `"30m"`.
    #[serde(default = "KubernetesAgent::default_idle_timeout")]
    pub idle_timeout: String,
    /// The project whose knowledge the agent reads and writes, shared with
    /// every agent naming the same one, whatever repository it works on;
    /// the agent's repository when absent.
    pub knowledge: Option<String>,
}

impl KubernetesAgent {
    fn default_home() -> String {
        "/home/ukai".to_owned()
    }

    fn default_user() -> i64 {
        65534
    }

    fn default_idle_timeout() -> String {
        "30m".to_owned()
    }

    /// What the agent's knowledge is kept under: its project, or else its
    /// repository.
    pub fn knowledge_scope(&self) -> String {
        self.knowledge
            .clone()
            .unwrap_or_else(|| ukai_core::knowledge::repository_key(&self.workspace.git))
    }
}

/// The repository the pod works on. It is cloned fresh into an emptyDir, and
/// the agent's working tree is pushed to `wip_ref` periodically, after each
/// turn, and when the pod stops, then restored from there by the next pod.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Workspace {
    pub git: String,
    /// Branch to check out; the remote's default when absent.
    #[serde(rename = "ref")]
    pub git_ref: Option<String>,
    /// A GitHub token Secret, as `<secret>/<key>`: `GH_TOKEN` of both the
    /// sidecar (to push snapshots) and the agent (to push and open pull
    /// requests), with git's credential helper pointed at `gh`.
    pub github_token: Option<String>,
    /// The agent's commit identity: `GIT_AUTHOR_*` and `GIT_COMMITTER_*`.
    pub author: Option<Author>,
    /// Ref snapshots go to; `refs/ukai/wip/<workspace id>` when absent.
    pub wip_ref: Option<String>,
    /// Time between periodic snapshots, e.g. `"5m"`.
    #[serde(default = "Workspace::default_snapshot_interval")]
    pub snapshot_interval: String,
}

impl Workspace {
    fn default_snapshot_interval() -> String {
        "5m".to_owned()
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Author {
    pub name: String,
    pub email: String,
}

/// An existing PersistentVolumeClaim mounted into the agent container. ukai
/// never creates or deletes claims.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Mount {
    pub claim: String,
    /// Absolute, or `~/...` relative to `home`.
    pub path: String,
    pub sub_path: Option<String>,
    #[serde(default)]
    pub read_only: bool,
}

/// Requests of the agent container, and its memory limit.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Resources {
    #[serde(default = "Resources::default_cpu")]
    pub cpu: String,
    #[serde(default = "Resources::default_memory")]
    pub memory: String,
    /// The memory limit; the memory request when absent.
    pub memory_limit: Option<String>,
}

impl Resources {
    fn default_cpu() -> String {
        "250m".to_owned()
    }

    fn default_memory() -> String {
        "1Gi".to_owned()
    }
}

impl Default for Resources {
    fn default() -> Self {
        Self {
            cpu: Self::default_cpu(),
            memory: Self::default_memory(),
            memory_limit: None,
        }
    }
}

/// A duration like `"45s"`, `"30m"`, or `"2h"`.
pub fn parse_duration(value: &str) -> Option<std::time::Duration> {
    let split = value.find(|c: char| !c.is_ascii_digit())?;
    let (number, unit) = value.split_at(split);
    let number: u64 = number.parse().ok()?;
    let seconds = match unit {
        "s" => number,
        "m" => number.checked_mul(60)?,
        "h" => number.checked_mul(3600)?,
        _ => return None,
    };
    Some(std::time::Duration::from_secs(seconds))
}

/// A `<secret>/<key>` reference, split.
pub fn parse_secret_ref(value: &str) -> Option<(&str, &str)> {
    let (secret, key) = value.split_once('/')?;
    (!secret.is_empty() && !key.is_empty() && !key.contains('/')).then_some((secret, key))
}

/// One problem in a `gateway.toml`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Problem {
    #[error("{context} must not be empty")]
    Empty { context: String },
    #[error("{context}: `{value}` is not a duration like \"45s\", \"30m\", or \"2h\"")]
    InvalidDuration { context: String, value: String },
    #[error("{context}: `{value}` is not a `<secret>/<key>` reference")]
    InvalidSecretRef { context: String, value: String },
    #[error("{context}: `{path}` must be absolute or start with `~/`")]
    InvalidMountPath { context: String, path: String },
    #[error(
        "{context}: `{value}` is not a project name: lowercase letters, digits, and `-`, `_`, `.`, `/`"
    )]
    InvalidKnowledgeScope { context: String, value: String },
    #[error("{context}: label `{key}` is the gateway's own")]
    ReservedLabel { context: String, key: String },
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error(transparent)]
    Toml(#[from] toml::de::Error),
    #[error("invalid configuration:\n{}", list(.0))]
    Invalid(Vec<Problem>),
}

fn list(problems: &[Problem]) -> String {
    problems
        .iter()
        .map(|problem| format!("  - {problem}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Parse and validate a `gateway.toml` document.
pub fn parse(src: &str) -> Result<GatewayConfig, ConfigError> {
    let config: GatewayConfig = toml::from_str(src)?;
    let mut problems = Vec::new();
    for (id, agent) in &config.agents {
        validate(&format!("agents.{id}"), agent, &mut problems);
    }
    if problems.is_empty() {
        Ok(config)
    } else {
        Err(ConfigError::Invalid(problems))
    }
}

fn validate(context: &str, agent: &KubernetesAgent, problems: &mut Vec<Problem>) {
    for (field, value) in [
        ("image", &agent.image),
        ("command", &agent.command),
        ("workspace.git", &agent.workspace.git),
    ] {
        if value.trim().is_empty() {
            problems.push(Problem::Empty {
                context: format!("{context}.{field}"),
            });
        }
    }
    for (field, value) in [
        ("idle_timeout", &agent.idle_timeout),
        (
            "workspace.snapshot_interval",
            &agent.workspace.snapshot_interval,
        ),
    ] {
        if parse_duration(value).is_none_or(|duration| duration.is_zero()) {
            problems.push(Problem::InvalidDuration {
                context: format!("{context}.{field}"),
                value: value.clone(),
            });
        }
    }
    let secrets = agent
        .secret_env
        .iter()
        .map(|(name, value)| (format!("{context}.secret_env.{name}"), value))
        .chain(
            agent
                .workspace
                .github_token
                .iter()
                .map(|value| (format!("{context}.workspace.github_token"), value)),
        );
    for (context, value) in secrets {
        if parse_secret_ref(value).is_none() {
            problems.push(Problem::InvalidSecretRef {
                context,
                value: value.clone(),
            });
        }
    }
    if let Some(scope) = &agent.knowledge
        && !ukai_core::knowledge::valid_scope(scope)
    {
        problems.push(Problem::InvalidKnowledgeScope {
            context: format!("{context}.knowledge"),
            value: scope.clone(),
        });
    }
    for key in agent.labels.keys() {
        if key == crate::job::MANAGED_BY || key.starts_with("ukai.dev/") {
            problems.push(Problem::ReservedLabel {
                context: format!("{context}.labels"),
                key: key.clone(),
            });
        }
    }
    for (index, mount) in agent.mounts.iter().enumerate() {
        if !(mount.path.starts_with('/') || mount.path.starts_with("~/")) {
            problems.push(Problem::InvalidMountPath {
                context: format!("{context}.mounts[{index}]"),
                path: mount.path.clone(),
            });
        }
    }
}

#[cfg(test)]
mod tests;
