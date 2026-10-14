//! The cluster side: find or create a workspace's Job and wait for its pod to
//! serve. Only namespaced, built-in resources are touched: `jobs` (get,
//! create, delete), `pods` (list), and optionally `events` (list) for
//! progress.

use std::io::Read;
use std::time::Duration;

use k8s_openapi::api::authentication::v1::{SelfSubjectReview, SelfSubjectReviewStatus};
use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::{ContainerStatus, Event, Pod};
use kube::api::{Api, DeleteParams, ListParams, PostParams};
use kube::config::InferConfigError;
use kube::{Client, Config};

use crate::job::{AGENT_CONTAINER, Plan, RELAY_PORT, RELAY_TOKEN_ENV, WORKSPACE_CONTAINER, job};

/// Long enough for a cold node to pull a large image and clone the repository.
const POD_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Waiting reasons that will not resolve on their own.
const FATAL_REASONS: &[&str] = &[
    "ImagePullBackOff",
    "InvalidImageName",
    "CreateContainerConfigError",
    "CreateContainerError",
];

#[derive(Debug, thiserror::Error)]
pub enum ClusterError {
    #[error(transparent)]
    InferConfig(#[from] InferConfigError),
    #[error(transparent)]
    Kube(#[from] kube::Error),
    #[error("the agent pod cannot start: {0}")]
    PodFailed(String),
    #[error("timed out after {}s waiting for the agent pod", .0.as_secs())]
    Timeout(Duration),
    #[error("could not generate a relay token: {0}")]
    Token(#[source] std::io::Error),
}

type Result<T> = std::result::Result<T, ClusterError>;

/// A client for the gateway's namespace.
#[derive(Clone)]
pub struct Cluster {
    client: Client,
    namespace: String,
}

impl Cluster {
    /// From the pod's service account when in a cluster, else the kubeconfig.
    pub async fn infer() -> Result<Self> {
        let config = Config::infer().await?;
        let namespace = config.default_namespace.clone();
        Ok(Self {
            client: Client::try_from(config)?,
            namespace,
        })
    }

    pub fn namespace(&self) -> &str {
        &self.namespace
    }

    /// The image of `pod`'s first container.
    pub async fn container_image(&self, pod: &str) -> Result<Option<String>> {
        let pods: Api<Pod> = Api::namespaced(self.client.clone(), &self.namespace);
        let pod = pods.get(pod).await?;
        Ok(pod
            .spec
            .and_then(|spec| spec.containers.into_iter().next())
            .and_then(|container| container.image))
    }

    /// The authenticated user's name, which keeps workspaces apart in a
    /// shared namespace.
    pub async fn whoami(&self) -> Result<String> {
        let review: Api<SelfSubjectReview> = Api::all(self.client.clone());
        let review = review
            .create(&PostParams::default(), &SelfSubjectReview::default())
            .await?;
        Ok(review
            .status
            .and_then(|SelfSubjectReviewStatus { user_info }| user_info?.username)
            .unwrap_or_default())
    }

    /// Return a ready pod for `plan`'s workspace, creating its Job (with the
    /// relay from `relay_image`) if there is none and replacing it if it has
    /// finished. `progress` hears each change in what is being waited for.
    pub async fn ensure_pod(
        &self,
        plan: &Plan,
        relay_image: &str,
        progress: impl Fn(String),
    ) -> Result<Pod> {
        let name = plan.job_name();
        let mut last = String::new();
        let waiting = async {
            loop {
                match self.step(plan, relay_image).await? {
                    Step::Ready(pod) => return Ok(*pod),
                    Step::Waiting(message) => {
                        if message != last {
                            tracing::debug!(job = %name, %message, "agent pod");
                            progress(message.clone());
                            last = message;
                        }
                    }
                }
                tokio::time::sleep(POLL_INTERVAL).await;
            }
        };
        tokio::time::timeout(POD_TIMEOUT, waiting)
            .await
            .unwrap_or(Err(ClusterError::Timeout(POD_TIMEOUT)))
    }

    /// Look at the workspace's Job once and move it along: create it, replace
    /// it if it has finished, or report what its pod is waiting for.
    async fn step(&self, plan: &Plan, relay_image: &str) -> Result<Step> {
        let jobs: Api<Job> = Api::namespaced(self.client.clone(), &self.namespace);
        let name = plan.job_name();
        let Some(existing) = jobs.get_opt(&name).await? else {
            let job = job(plan, relay_image, &token()?);
            match jobs.create(&PostParams::default(), &job).await {
                Ok(_) => {}
                // Another pane got there first; use its Job.
                Err(kube::Error::Api(status)) if status.is_conflict() => {}
                Err(error) => return Err(error.into()),
            }
            return Ok(Step::Waiting("Creating the agent pod".to_owned()));
        };
        if existing.metadata.deletion_timestamp.is_some() {
            return Ok(Step::Waiting(
                "Waiting for the previous pod to stop".to_owned(),
            ));
        }
        if finished(&existing) {
            match jobs.delete(&name, &DeleteParams::foreground()).await {
                Ok(_) => {}
                Err(kube::Error::Api(status)) if status.is_not_found() => {}
                Err(error) => return Err(error.into()),
            }
            return Ok(Step::Waiting("Replacing the stopped pod".to_owned()));
        }
        let pods: Api<Pod> = Api::namespaced(self.client.clone(), &self.namespace);
        let selector = format!("job-name={name}");
        let pod = pods
            .list(&ListParams::default().labels(&selector))
            .await?
            .items
            .into_iter()
            .find(|pod| pod.metadata.deletion_timestamp.is_none());
        let Some(pod) = pod else {
            return Ok(Step::Waiting(
                "Waiting for the pod to be created".to_owned(),
            ));
        };
        match pod_state(&pod) {
            PodState::Ready => Ok(Step::Ready(Box::new(pod))),
            PodState::Failed(reason) => Err(ClusterError::PodFailed(reason)),
            PodState::Waiting(message) => Ok(Step::Waiting(match self.latest_event(&pod).await {
                Some(event) => format!("{message} ({event})"),
                None => message,
            })),
        }
    }

    /// The newest event about `pod`, e.g. `Pulling image "..."`. Events are
    /// only decoration, so failing to read them is not an error.
    async fn latest_event(&self, pod: &Pod) -> Option<String> {
        let name = pod.metadata.name.as_deref()?;
        let events: Api<Event> = Api::namespaced(self.client.clone(), &self.namespace);
        let selector = format!("involvedObject.kind=Pod,involvedObject.name={name}");
        let list = events
            .list(&ListParams::default().fields(&selector))
            .await
            .ok()?;
        list.items
            .into_iter()
            .max_by_key(|event| {
                event
                    .last_timestamp
                    .as_ref()
                    .map(|time| time.0)
                    .or_else(|| event.event_time.as_ref().map(|time| time.0))
            })
            .and_then(|event| event.message)
    }
}

/// Where `pod`'s relay serves, and the token it wants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayEndpoint {
    pub url: String,
    pub token: String,
}

impl RelayEndpoint {
    pub fn of(pod: &Pod) -> Option<Self> {
        let ip = pod.status.as_ref()?.pod_ip.as_ref()?;
        let agent = pod
            .spec
            .as_ref()?
            .containers
            .iter()
            .find(|container| container.name == AGENT_CONTAINER)?;
        let token = agent
            .env
            .iter()
            .flatten()
            .find(|env| env.name == RELAY_TOKEN_ENV)?
            .value
            .clone()?;
        Some(Self {
            url: format!("ws://{ip}:{RELAY_PORT}/acp"),
            token,
        })
    }
}

/// 128 random bits, hex: the relay's token for one Job.
fn token() -> Result<String> {
    let mut bytes = [0u8; 16];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut random| random.read_exact(&mut bytes))
        .map_err(ClusterError::Token)?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn finished(job: &Job) -> bool {
    job.status
        .as_ref()
        .and_then(|status| status.conditions.as_ref())
        .is_some_and(|conditions| {
            conditions.iter().any(|condition| {
                matches!(condition.type_.as_str(), "Complete" | "Failed")
                    && condition.status == "True"
            })
        })
}

enum Step {
    Ready(Box<Pod>),
    Waiting(String),
}

#[derive(Debug, PartialEq, Eq)]
enum PodState {
    Ready,
    Waiting(String),
    Failed(String),
}

fn pod_state(pod: &Pod) -> PodState {
    let Some(status) = &pod.status else {
        return PodState::Waiting("Waiting for the pod to be scheduled".to_owned());
    };
    let ready = status
        .conditions
        .iter()
        .flatten()
        .any(|condition| condition.type_ == "Ready" && condition.status == "True");
    if ready && status.phase.as_deref() == Some("Running") {
        return PodState::Ready;
    }
    if matches!(status.phase.as_deref(), Some("Succeeded" | "Failed")) {
        return PodState::Waiting("Waiting for the stopped pod to be replaced".to_owned());
    }
    let containers = status
        .init_container_statuses
        .iter()
        .flatten()
        .chain(status.container_statuses.iter().flatten());
    for container in containers.clone() {
        let Some(waiting) = container.state.as_ref().and_then(|s| s.waiting.as_ref()) else {
            continue;
        };
        let reason = waiting.reason.as_deref().unwrap_or_default();
        if FATAL_REASONS.contains(&reason) {
            let message = waiting.message.as_deref().unwrap_or_default();
            return PodState::Failed(format!("{}: {reason}: {message}", container.name));
        }
    }
    if let Some(unschedulable) = status.conditions.iter().flatten().find(|condition| {
        condition.type_ == "PodScheduled" && condition.reason.as_deref() == Some("Unschedulable")
    }) {
        let message = unschedulable.message.as_deref().unwrap_or("no node fits");
        return PodState::Waiting(format!("Waiting for a node: {message}"));
    }
    let sidecar = containers
        .clone()
        .find(|container| container.name == WORKSPACE_CONTAINER);
    if sidecar.is_some_and(is_running_unstarted) {
        return PodState::Waiting("Cloning the repository".to_owned());
    }
    PodState::Waiting("Starting containers".to_owned())
}

fn is_running_unstarted(container: &ContainerStatus) -> bool {
    container
        .state
        .as_ref()
        .is_some_and(|s| s.running.is_some())
        && container.started != Some(true)
}

#[cfg(test)]
mod tests {
    use rstest::rstest;
    use serde_json::json;

    use super::*;

    fn pod(status: serde_json::Value) -> Pod {
        serde_json::from_value(json!({ "metadata": { "name": "p" }, "status": status }))
            .unwrap_or_default()
    }

    #[rstest]
    #[case::ready(
        json!({ "phase": "Running", "conditions": [{ "type": "Ready", "status": "True" }] }),
        PodState::Ready
    )]
    #[case::pulling(
        json!({ "phase": "Pending", "initContainerStatuses": [
            { "name": "workspace", "image": "i", "imageID": "", "ready": false, "restartCount": 0,
              "state": { "waiting": { "reason": "PodInitializing" } } }
        ] }),
        PodState::Waiting("Starting containers".into())
    )]
    #[case::cloning(
        json!({ "phase": "Pending", "initContainerStatuses": [
            { "name": "workspace", "image": "i", "imageID": "", "ready": false, "restartCount": 0,
              "started": false, "state": { "running": {} } }
        ] }),
        PodState::Waiting("Cloning the repository".into())
    )]
    #[case::bad_image(
        json!({ "phase": "Pending", "initContainerStatuses": [
            { "name": "workspace", "image": "i", "imageID": "", "ready": false, "restartCount": 0,
              "state": { "waiting": { "reason": "ImagePullBackOff", "message": "not found" } } }
        ] }),
        PodState::Failed("workspace: ImagePullBackOff: not found".into())
    )]
    #[case::missing_secret(
        json!({ "phase": "Pending", "containerStatuses": [
            { "name": "agent", "image": "i", "imageID": "", "ready": false, "restartCount": 0,
              "state": { "waiting": { "reason": "CreateContainerConfigError", "message": "secret \"gh\" not found" } } }
        ] }),
        PodState::Failed("agent: CreateContainerConfigError: secret \"gh\" not found".into())
    )]
    #[case::unschedulable(
        json!({ "phase": "Pending", "conditions": [
            { "type": "PodScheduled", "status": "False", "reason": "Unschedulable", "message": "0/3 nodes" }
        ] }),
        PodState::Waiting("Waiting for a node: 0/3 nodes".into())
    )]
    #[case::stopped(json!({ "phase": "Succeeded" }), PodState::Waiting("Waiting for the stopped pod to be replaced".into()))]
    fn reads_pod_state(#[case] status: serde_json::Value, #[case] expected: PodState) {
        assert_eq!(pod_state(&pod(status)), expected);
    }

    #[test]
    fn reads_the_relay_endpoint() {
        let pod: Pod = serde_json::from_value(json!({
            "spec": { "containers": [
                { "name": "agent", "env": [{ "name": "UKAI_RELAY_TOKEN", "value": "t" }] }
            ] },
            "status": { "podIP": "10.0.0.7" }
        }))
        .unwrap_or_default();
        assert_eq!(
            RelayEndpoint::of(&pod),
            Some(RelayEndpoint {
                url: "ws://10.0.0.7:7070/acp".into(),
                token: "t".into()
            })
        );
        assert_eq!(RelayEndpoint::of(&Pod::default()), None);
    }

    #[test]
    fn tokens_are_random_hex() {
        let (a, b) = (token().unwrap_or_default(), token().unwrap_or_default());
        assert_eq!(a.len(), 32);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }

    #[rstest]
    #[case::running(json!({}), false)]
    #[case::complete(json!({ "conditions": [{ "type": "Complete", "status": "True" }] }), true)]
    #[case::failed(json!({ "conditions": [{ "type": "Failed", "status": "True" }] }), true)]
    #[case::suspended(json!({ "conditions": [{ "type": "Suspended", "status": "True" }] }), false)]
    fn detects_finished_jobs(#[case] status: serde_json::Value, #[case] expected: bool) {
        let job: Job = serde_json::from_value(json!({ "status": status })).unwrap_or_default();
        assert_eq!(finished(&job), expected);
    }
}
