//! The Job a Kubernetes agent runs in, built without touching the cluster.
//!
//! One pod per workspace, with containers sharing `/workspace`:
//!
//! - `relay-install`, an init container: copies `ukai-relay` out of the
//!   relay image, so the agent's image needs nothing from ukai.
//! - `workspace`, a native sidecar (`scripts/workspace.sh`): clones the
//!   repository, restores the last snapshot, and pushes snapshots of the
//!   agent's tree from a clone in `/pristine`, which the agent never mounts,
//!   so git never runs on the agent's own `.git` with the sidecar's token.
//! - `agent`: `ukai-relay` serving the agent at port [`RELAY_PORT`], with
//!   sessions that outlive client connections. It asks the sidecar for a
//!   snapshot after each turn and exits once idle for `idle_timeout`; the
//!   kubelet then stops the sidecar, whose TERM handler pushes a final
//!   snapshot. Its MCP server reaches the knowledge of the repository on
//!   the gateway at [`GATEWAY_URL_ENV`], with the relay's token.

use std::collections::BTreeMap;

use crate::config::{KubernetesAgent, parse_duration, parse_secret_ref};
use k8s_openapi::api::batch::v1::{Job, JobSpec};
use k8s_openapi::api::core::v1::{
    Capabilities, Container, ContainerPort, EmptyDirVolumeSource, EnvVar, EnvVarSource, ExecAction,
    HTTPGetAction, ObjectFieldSelector, PersistentVolumeClaimVolumeSource, PodSecurityContext,
    PodSpec, PodTemplateSpec, Probe, ResourceRequirements, SeccompProfile, SecretKeySelector,
    SecurityContext, Volume, VolumeMount,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use sha2::{Digest, Sha256};

pub(crate) const WORKSPACE_SCRIPT: &str = include_str!("../scripts/workspace.sh");

pub const MANAGED_BY: &str = "app.kubernetes.io/managed-by";
pub const WORKSPACE_LABEL: &str = "ukai.dev/workspace";

pub const AGENT_CONTAINER: &str = "agent";
pub const WORKSPACE_CONTAINER: &str = "workspace";

/// The checkout the agent works in: the session's `cwd`.
pub const REPO_DIR: &str = "/workspace/repo";

/// Where the relay serves ACP in the pod: `ws://<pod ip>:RELAY_PORT/acp`.
pub const RELAY_PORT: i32 = 7070;
/// Where it serves the reports of the checkout, read-only, on the same port
/// (`ukai_acp::reports`). The gateway proxies it per workspace.
pub const REPORTS_PATH: &str = "/reports";
/// The agent container's environment variable holding the relay's token.
pub const RELAY_TOKEN_ENV: &str = "UKAI_RELAY_TOKEN";
/// The agent container's environment variable holding where the gateway is
/// reached from inside the cluster.
pub const GATEWAY_URL_ENV: &str = "UKAI_GATEWAY_URL";
/// Annotation on the Job and its pod naming the repository.
pub const GIT_ANNOTATION: &str = "ukai.dev/git";
/// Annotation on the Job and its pod naming the knowledge scope, which the
/// gateway reads to tell whose knowledge a pod may use from its relay token.
pub const KNOWLEDGE_ANNOTATION: &str = "ukai.dev/knowledge";
/// The relay binary's path in the image named by [`job`]'s `relay_image`.
const RELAY_IN_IMAGE: &str = "/usr/local/bin/ukai-relay";
const RELAY_DIR: &str = "/ukai/bin";

/// Pods are capped at a day; the final snapshot still runs on the way out.
const ACTIVE_DEADLINE_SECONDS: i64 = 24 * 3600;
const FINISHED_TTL_SECONDS: i32 = 600;
const GRACE_PERIOD_SECONDS: i64 = 60;

/// Everything the Job depends on, resolved: the agent's declaration plus the
/// identity of the workspace.
#[derive(Debug, Clone)]
pub struct Plan {
    pub agent: KubernetesAgent,
    /// Short, stable id of `(user, namespace, repository, ref, agent command,
    /// pane key)`.
    pub workspace_id: String,
    pub user: String,
}

impl Plan {
    /// `pane` keys a workspace of one pane's own; without it, every
    /// connection to this agent shares one.
    pub fn new(agent: KubernetesAgent, user: &str, namespace: &str, pane: Option<&str>) -> Self {
        let mut parts = vec![
            user,
            namespace,
            agent.workspace.git.as_str(),
            agent.workspace.git_ref.as_deref().unwrap_or(""),
            agent.command.as_str(),
        ];
        // Only a pane's own workspace adds a part, so shared ones keep their id.
        parts.extend(pane);
        let workspace_id = workspace_id(&parts);
        Self {
            agent,
            workspace_id,
            user: user.to_owned(),
        }
    }

    /// What tells this workspace from every other: the hash in its Job's
    /// name, and what its reports are kept under.
    pub fn workspace_id(&self) -> &str {
        &self.workspace_id
    }

    /// The Job's name; deterministic, so two panes opening the same workspace
    /// meet at one pod.
    pub fn job_name(&self) -> String {
        format!("ukai-{}", self.workspace_id)
    }

    pub fn wip_ref(&self) -> String {
        self.agent
            .workspace
            .wip_ref
            .clone()
            .unwrap_or_else(|| format!("refs/ukai/wip/{}", self.workspace_id))
    }

    /// The labels of the Job and its pod: the agent's, then the gateway's.
    pub fn labels(&self) -> BTreeMap<String, String> {
        let mut labels = self.agent.labels.clone();
        labels.extend([
            (MANAGED_BY.to_owned(), "ukai".to_owned()),
            (WORKSPACE_LABEL.to_owned(), self.workspace_id.clone()),
        ]);
        labels
    }
}

/// What the gateway itself contributes to every Job.
#[derive(Debug, Clone)]
pub struct Runtime {
    /// The image whose `ukai-relay` the pod runs.
    pub relay_image: String,
    /// Where the pod reaches the gateway, e.g. `http://ukai-gateway`; set
    /// only when the gateway keeps knowledge, which is what pods reach it for.
    pub gateway_url: Option<String>,
}

/// A short hash of `parts`, 12 hex digits.
pub(crate) fn workspace_id(parts: &[&str]) -> String {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(part.as_bytes());
        // A separator that cannot occur in any part keeps ("ab", "c") and
        // ("a", "bc") apart.
        hasher.update([0]);
    }
    hasher
        .finalize()
        .iter()
        .take(6)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// The Job for `plan`, with `ukai-relay` taken from `runtime`'s image and
/// requiring `relay_token` from clients. Values were validated with the
/// config, so unparsable durations and secret references cannot reach here;
/// they fall back to defaults rather than panicking.
pub fn job(plan: &Plan, runtime: &Runtime, relay_token: &str) -> Job {
    let agent = &plan.agent;
    let seconds = |value: &str, default: u64| {
        parse_duration(value)
            .map_or(default, |duration| duration.as_secs())
            .to_string()
    };

    let mut workspace_env = vec![
        env("UKAI_GIT_URL", &agent.workspace.git),
        env("UKAI_WIP_REF", &plan.wip_ref()),
        env(
            "UKAI_SNAPSHOT_INTERVAL",
            &seconds(&agent.workspace.snapshot_interval, 300),
        ),
    ];
    if let Some(git_ref) = &agent.workspace.git_ref {
        workspace_env.push(env("UKAI_GIT_REF", git_ref));
    }
    let github_token = agent
        .workspace
        .github_token
        .as_ref()
        .and_then(|token| secret_ref("GH_TOKEN", token));
    workspace_env.extend(github_token.clone());

    let mut agent_env = vec![
        // First, so that the agent's `env` can refer to it.
        EnvVar {
            name: "POD_NAMESPACE".to_owned(),
            value: None,
            value_from: Some(EnvVarSource {
                field_ref: Some(ObjectFieldSelector {
                    field_path: "metadata.namespace".to_owned(),
                    api_version: None,
                }),
                ..EnvVarSource::default()
            }),
        },
        env("HOME", &agent.home),
        env("UKAI_IDLE_TIMEOUT", &seconds(&agent.idle_timeout, 1800)),
        env("UKAI_SNAPSHOT_TRIGGER", "/workspace/.ukai/snapshot"),
        env(RELAY_TOKEN_ENV, relay_token),
    ];
    agent_env.extend(
        runtime
            .gateway_url
            .as_deref()
            .map(|url| env(GATEWAY_URL_ENV, url)),
    );
    agent_env.extend(agent.env.iter().map(|(name, value)| env(name, value)));
    agent_env.extend(secret_env(&agent.secret_env));
    if let Some(token) = github_token {
        agent_env.push(token);
        // `git push` (and so `gh pr create`) authenticates through `gh`.
        agent_env.extend([
            env("GIT_CONFIG_COUNT", "1"),
            env("GIT_CONFIG_KEY_0", "credential.https://github.com.helper"),
            env("GIT_CONFIG_VALUE_0", "!gh auth git-credential"),
        ]);
    }
    if let Some(author) = &agent.workspace.author {
        agent_env.extend([
            env("GIT_AUTHOR_NAME", &author.name),
            env("GIT_AUTHOR_EMAIL", &author.email),
            env("GIT_COMMITTER_NAME", &author.name),
            env("GIT_COMMITTER_EMAIL", &author.email),
        ]);
    }

    // One volume per claim, however many paths mount it.
    let mut claims: Vec<&str> = agent.mounts.iter().map(|m| m.claim.as_str()).collect();
    claims.sort_unstable();
    claims.dedup();
    let claim_volume = |claim: &str| {
        let index = claims.iter().position(|c| *c == claim).unwrap_or_default();
        format!("claim-{index}")
    };

    let mut agent_mounts = vec![
        mount("workspace", "/workspace"),
        mount("home", &agent.home),
        mount("relay", RELAY_DIR),
    ];
    agent_mounts.extend(agent.mounts.iter().map(|m| VolumeMount {
        name: claim_volume(&m.claim),
        mount_path: expand_home(&m.path, &agent.home),
        sub_path: m.sub_path.clone(),
        read_only: m.read_only.then_some(true),
        ..VolumeMount::default()
    }));

    let mut volumes = vec![
        empty_dir("workspace"),
        empty_dir("pristine"),
        empty_dir("home"),
        empty_dir("relay"),
    ];
    volumes.extend(claims.iter().map(|claim| Volume {
        name: claim_volume(claim),
        persistent_volume_claim: Some(PersistentVolumeClaimVolumeSource {
            claim_name: (*claim).to_owned(),
            read_only: None,
        }),
        ..Volume::default()
    }));

    let install = Container {
        name: "relay-install".to_owned(),
        image: Some(runtime.relay_image.clone()),
        command: Some(vec![
            "cp".into(),
            RELAY_IN_IMAGE.into(),
            format!("{RELAY_DIR}/ukai-relay"),
        ]),
        resources: Some(resources("10m", "16Mi", "64Mi")),
        security_context: Some(container_security()),
        volume_mounts: Some(vec![mount("relay", RELAY_DIR)]),
        ..Container::default()
    };

    let sidecar = Container {
        name: WORKSPACE_CONTAINER.to_owned(),
        image: Some(agent.image.clone()),
        command: Some(vec!["bash".into(), "-c".into(), WORKSPACE_SCRIPT.into()]),
        env: Some(workspace_env),
        // A native sidecar: started before, and stopped after, the agent.
        restart_policy: Some("Always".to_owned()),
        // The agent starts once the checkout is in place.
        startup_probe: Some(Probe {
            exec: Some(ExecAction {
                command: Some(vec![
                    "test".into(),
                    "-e".into(),
                    "/workspace/.ukai/ready".into(),
                ]),
            }),
            period_seconds: Some(2),
            failure_threshold: Some(900),
            ..Probe::default()
        }),
        resources: Some(resources("50m", "128Mi", "512Mi")),
        security_context: Some(container_security()),
        volume_mounts: Some(vec![
            mount("workspace", "/workspace"),
            mount("pristine", "/pristine"),
        ]),
        ..Container::default()
    };

    let mut relay_command = vec![
        format!("{RELAY_DIR}/ukai-relay"),
        "--listen".to_owned(),
        format!("0.0.0.0:{RELAY_PORT}"),
        "--".to_owned(),
        agent.command.clone(),
    ];
    relay_command.extend(agent.args.iter().cloned());
    let main = Container {
        name: AGENT_CONTAINER.to_owned(),
        image: Some(agent.image.clone()),
        command: Some(relay_command),
        working_dir: Some(REPO_DIR.to_owned()),
        env: Some(agent_env),
        ports: Some(vec![ContainerPort {
            name: Some("acp".to_owned()),
            container_port: RELAY_PORT,
            ..ContainerPort::default()
        }]),
        readiness_probe: Some(Probe {
            http_get: Some(HTTPGetAction {
                path: Some("/health".to_owned()),
                port: IntOrString::Int(RELAY_PORT),
                ..HTTPGetAction::default()
            }),
            period_seconds: Some(2),
            ..Probe::default()
        }),
        resources: Some(resources(
            &agent.resources.cpu,
            &agent.resources.memory,
            agent
                .resources
                .memory_limit
                .as_deref()
                .unwrap_or(&agent.resources.memory),
        )),
        security_context: Some(container_security()),
        volume_mounts: Some(agent_mounts),
        ..Container::default()
    };

    let metadata = ObjectMeta {
        labels: Some(plan.labels()),
        annotations: Some(BTreeMap::from([
            (GIT_ANNOTATION.to_owned(), agent.workspace.git.clone()),
            (KNOWLEDGE_ANNOTATION.to_owned(), agent.knowledge_scope()),
        ])),
        ..ObjectMeta::default()
    };
    Job {
        metadata: ObjectMeta {
            name: Some(plan.job_name()),
            annotations: Some(BTreeMap::from([
                (GIT_ANNOTATION.to_owned(), agent.workspace.git.clone()),
                (KNOWLEDGE_ANNOTATION.to_owned(), agent.knowledge_scope()),
                ("ukai.dev/user".to_owned(), plan.user.clone()),
            ])),
            ..metadata.clone()
        },
        spec: Some(JobSpec {
            backoff_limit: Some(0),
            active_deadline_seconds: Some(ACTIVE_DEADLINE_SECONDS),
            ttl_seconds_after_finished: Some(FINISHED_TTL_SECONDS),
            template: PodTemplateSpec {
                metadata: Some(metadata),
                spec: Some(PodSpec {
                    restart_policy: Some("Never".to_owned()),
                    termination_grace_period_seconds: Some(GRACE_PERIOD_SECONDS),
                    security_context: Some(PodSecurityContext {
                        run_as_non_root: Some(true),
                        run_as_user: Some(agent.run_as_user),
                        run_as_group: Some(agent.run_as_user),
                        fs_group: Some(agent.run_as_user),
                        seccomp_profile: Some(SeccompProfile {
                            type_: "RuntimeDefault".to_owned(),
                            ..SeccompProfile::default()
                        }),
                        ..PodSecurityContext::default()
                    }),
                    init_containers: Some(vec![install, sidecar]),
                    containers: vec![main],
                    volumes: Some(volumes),
                    ..PodSpec::default()
                }),
            },
            ..JobSpec::default()
        }),
        status: None,
    }
}

fn expand_home(path: &str, home: &str) -> String {
    match path.strip_prefix("~/") {
        Some(rest) => format!("{}/{rest}", home.trim_end_matches('/')),
        None => path.to_owned(),
    }
}

fn env(name: &str, value: &str) -> EnvVar {
    EnvVar {
        name: name.to_owned(),
        value: Some(value.to_owned()),
        value_from: None,
    }
}

fn secret_env(refs: &BTreeMap<String, String>) -> impl Iterator<Item = EnvVar> + '_ {
    refs.iter()
        .filter_map(|(name, value)| secret_ref(name, value))
}

/// `name` from the Secret key `value` names as `<secret>/<key>`.
fn secret_ref(name: &str, value: &str) -> Option<EnvVar> {
    let (secret, key) = parse_secret_ref(value)?;
    Some(EnvVar {
        name: name.to_owned(),
        value: None,
        value_from: Some(EnvVarSource {
            secret_key_ref: Some(SecretKeySelector {
                name: secret.to_owned(),
                key: key.to_owned(),
                optional: None,
            }),
            ..EnvVarSource::default()
        }),
    })
}

fn mount(name: &str, path: &str) -> VolumeMount {
    VolumeMount {
        name: name.to_owned(),
        mount_path: path.to_owned(),
        ..VolumeMount::default()
    }
}

fn empty_dir(name: &str) -> Volume {
    Volume {
        name: name.to_owned(),
        empty_dir: Some(EmptyDirVolumeSource::default()),
        ..Volume::default()
    }
}

fn resources(cpu: &str, memory: &str, memory_limit: &str) -> ResourceRequirements {
    ResourceRequirements {
        requests: Some(BTreeMap::from([
            ("cpu".to_owned(), Quantity(cpu.to_owned())),
            ("memory".to_owned(), Quantity(memory.to_owned())),
        ])),
        limits: Some(BTreeMap::from([(
            "memory".to_owned(),
            Quantity(memory_limit.to_owned()),
        )])),
        ..ResourceRequirements::default()
    }
}

/// What Pod Security Admission's `restricted` level asks of a container.
fn container_security() -> SecurityContext {
    SecurityContext {
        allow_privilege_escalation: Some(false),
        capabilities: Some(Capabilities {
            drop: Some(vec!["ALL".to_owned()]),
            add: None,
        }),
        ..SecurityContext::default()
    }
}

#[cfg(test)]
mod tests;
