//! The cluster side: find or create a workspace's Job and wait for its pod to
//! serve. Only namespaced, built-in resources are touched: `jobs` (get,
//! create, delete), `pods` (list, and `exec` for a schedule's command),
//! optionally `events` (list) for progress, and `configmaps` (list, patch,
//! delete) holding schedules.

use std::io::Read;
use std::time::Duration;

use k8s_openapi::api::authentication::v1::{SelfSubjectReview, SelfSubjectReviewStatus};
use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::{ConfigMap, ContainerStatus, Event, Pod};
use kube::api::{
    Api, AttachParams, AttachedProcess, DeleteParams, ListParams, Patch, PatchParams, PostParams,
};
use kube::config::InferConfigError;
use kube::{Client, Config};

use ukai_acp::checkout::{DIFF_PATH, DiffBase, FILES_PATH, RUN_PATH};
use ukai_acp::ports::{FORWARD_PATH, PORTS_PATH};

use crate::job::{
    AGENT_CONTAINER, GIT_ANNOTATION, KNOWLEDGE_ANNOTATION, MANAGED_BY, Plan, RELAY_PORT,
    RELAY_TOKEN_ENV, REPORTS_PATH, Runtime, WORKSPACE_CONTAINER, WORKSPACE_LABEL, job,
};

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

    /// The ConfigMaps labelled `label`, e.g. every saved schedule.
    pub async fn config_maps(&self, label: &str) -> Result<Vec<ConfigMap>> {
        let maps: Api<ConfigMap> = Api::namespaced(self.client.clone(), &self.namespace);
        Ok(maps.list(&ListParams::default().labels(label)).await?.items)
    }

    /// Create or replace `map` (server-side apply, so the gateway owns it).
    pub async fn apply_config_map(&self, map: &ConfigMap) -> Result<()> {
        let maps: Api<ConfigMap> = Api::namespaced(self.client.clone(), &self.namespace);
        let name = map.metadata.name.clone().unwrap_or_default();
        maps.patch(
            &name,
            &PatchParams::apply("ukai-gateway").force(),
            &Patch::Apply(map),
        )
        .await?;
        Ok(())
    }

    /// Delete a ConfigMap; one already gone is not an error.
    pub async fn delete_config_map(&self, name: &str) -> Result<()> {
        let maps: Api<ConfigMap> = Api::namespaced(self.client.clone(), &self.namespace);
        match maps.delete(name, &DeleteParams::default()).await {
            Ok(_) => Ok(()),
            Err(kube::Error::Api(status)) if status.code == 404 => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    /// Run `command` in `pod`'s agent container, reading its stdout and stderr.
    pub async fn exec_in_agent(&self, pod: &str, command: Vec<String>) -> Result<AttachedProcess> {
        let pods: Api<Pod> = Api::namespaced(self.client.clone(), &self.namespace);
        let params = AttachParams::default()
            .container(AGENT_CONTAINER)
            .stdin(false)
            .stdout(true)
            .stderr(true);
        Ok(pods.exec(pod, command, &params).await?)
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
    /// relay from `runtime`) if there is none and replacing it if it has
    /// finished. `progress` hears each change in what is being waited for.
    pub async fn ensure_pod(
        &self,
        plan: &Plan,
        runtime: &Runtime,
        progress: impl Fn(String),
    ) -> Result<Pod> {
        let name = plan.job_name();
        let mut last = String::new();
        let waiting = async {
            loop {
                match self.step(plan, runtime).await? {
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

    /// The workspace's serving pod, if it is already there. Nothing is
    /// created or replaced: a report request must not start a pod, it only
    /// reads from one that is running.
    pub async fn serving_pod(&self, plan: &Plan) -> Result<Option<Pod>> {
        let jobs: Api<Job> = Api::namespaced(self.client.clone(), &self.namespace);
        let name = plan.job_name();
        let Some(job) = jobs.get_opt(&name).await? else {
            return Ok(None);
        };
        let pods: Api<Pod> = Api::namespaced(self.client.clone(), &self.namespace);
        let found = pods
            .list(&ListParams::default().labels(&format!("job-name={name}")))
            .await?
            .items;
        Ok(serving(&job, found))
    }

    /// The relay of every agent pod that is ready, with the workspace it
    /// works in: what the reports archive copies from.
    pub async fn ready_relays(&self) -> Result<Vec<(String, RelayEndpoint)>> {
        let pods: Api<Pod> = Api::namespaced(self.client.clone(), &self.namespace);
        let found = pods
            .list(&ListParams::default().labels(&format!("{MANAGED_BY}=ukai")))
            .await?
            .items;
        Ok(ready_relays(&found))
    }

    /// The addresses of every agent pod, running or not: what
    /// `access::guard` keeps off the person's side.
    pub async fn agent_pod_addresses(&self) -> Result<std::collections::HashSet<std::net::IpAddr>> {
        let pods: Api<Pod> = Api::namespaced(self.client.clone(), &self.namespace);
        let found = pods
            .list(&ListParams::default().labels(&format!("{MANAGED_BY}=ukai")))
            .await?
            .items;
        Ok(pod_addresses(&found))
    }

    /// The agent pod whose relay wants `token`: its knowledge scope, and its
    /// workspace. This is how the gateway tells whose knowledge a pod's MCP
    /// server reads and writes, without trusting anything the pod says.
    pub async fn pod_of(&self, token: &str) -> Result<Option<PodIdentity>> {
        let pods: Api<Pod> = Api::namespaced(self.client.clone(), &self.namespace);
        let found = pods
            .list(&ListParams::default().labels(&format!("{MANAGED_BY}=ukai")))
            .await?
            .items;
        Ok(found
            .iter()
            .find(|pod| {
                relay_token(pod).is_some_and(|wanted| ukai_core::secret::same_secret(wanted, token))
            })
            .and_then(|pod| {
                let annotations = pod.metadata.annotations.as_ref()?;
                // A pod from before scopes were annotated has its repository.
                let scope = match annotations.get(KNOWLEDGE_ANNOTATION) {
                    Some(scope) => scope.clone(),
                    None => ukai_core::knowledge::repository_key(annotations.get(GIT_ANNOTATION)?),
                };
                Some(PodIdentity {
                    scope,
                    workspace: pod.metadata.labels.as_ref()?.get(WORKSPACE_LABEL)?.clone(),
                })
            }))
    }

    /// Stop the workspace's Job, if there is one, so the next connection
    /// starts a fresh pod: a restart that picks up a newer image or a newer
    /// release the agent's command fetches. The workspace sidecar snapshots
    /// on its way out, as when the pod idles out.
    pub async fn stop(&self, plan: &Plan) -> Result<()> {
        let jobs: Api<Job> = Api::namespaced(self.client.clone(), &self.namespace);
        match jobs
            .delete(&plan.job_name(), &DeleteParams::foreground())
            .await
        {
            Ok(_) => Ok(()),
            Err(kube::Error::Api(status)) if status.is_not_found() => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    /// Look at the workspace's Job once and move it along: create it, replace
    /// it if it has finished, or report what its pod is waiting for.
    async fn step(&self, plan: &Plan, runtime: &Runtime) -> Result<Step> {
        let jobs: Api<Job> = Api::namespaced(self.client.clone(), &self.namespace);
        let name = plan.job_name();
        let Some(existing) = jobs.get_opt(&name).await? else {
            let job = job(plan, runtime, &token()?);
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

/// The pod of `job` that can be served from: ready, not on its way out, and
/// not left over from a Job that has finished. Pure, so the gateway's
/// routing can be tested without a cluster.
fn serving(job: &Job, pods: Vec<Pod>) -> Option<Pod> {
    if job.metadata.deletion_timestamp.is_some() || finished(job) {
        return None;
    }
    pods.into_iter()
        .find(|pod| pod.metadata.deletion_timestamp.is_none() && pod_state(pod) == PodState::Ready)
}

/// The ready pods among `pods`, by workspace, with where their relays are.
fn ready_relays(pods: &[Pod]) -> Vec<(String, RelayEndpoint)> {
    pods.iter()
        .filter(|pod| {
            pod.metadata.deletion_timestamp.is_none() && pod_state(pod) == PodState::Ready
        })
        .filter_map(|pod| {
            let workspace = pod.metadata.labels.as_ref()?.get(WORKSPACE_LABEL)?.clone();
            Some((workspace, RelayEndpoint::of(pod)?))
        })
        .collect()
}

/// Which agent pod a request came from, as the gateway found it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PodIdentity {
    /// What its knowledge is kept under.
    pub scope: String,
    /// Its workspace id.
    pub workspace: String,
}

/// Where `pod`'s relay serves, and the token it wants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayEndpoint {
    /// `<pod ip>:<relay port>`.
    pub address: String,
    pub token: String,
}

impl RelayEndpoint {
    pub fn of(pod: &Pod) -> Option<Self> {
        let ip = pod.status.as_ref()?.pod_ip.as_ref()?;
        Some(Self {
            address: format!("{ip}:{RELAY_PORT}"),
            token: relay_token(pod)?.to_owned(),
        })
    }

    /// The ACP endpoint client frames are relayed to.
    pub fn acp_url(&self) -> String {
        format!("ws://{}/acp", self.address)
    }

    /// The reports endpoint, with `suffix` appended as it arrived: either
    /// nothing (the listing) or `/files/<path>`, still percent-encoded.
    pub fn reports_url(&self, suffix: &str) -> String {
        format!("http://{}{REPORTS_PATH}{suffix}", self.address)
    }

    /// What listens in the pod, and the previews its agent asked for.
    pub fn ports_url(&self) -> String {
        format!("http://{}{PORTS_PATH}", self.address)
    }

    /// The files of the pod's checkout.
    pub fn files_url(&self) -> String {
        format!("http://{}{FILES_PATH}", self.address)
    }

    /// Where a command is run in the pod's checkout.
    pub fn run_url(&self) -> String {
        format!("http://{}{RUN_PATH}", self.address)
    }

    /// The changes of the pod's checkout since `base`.
    pub fn diff_url(&self, base: DiffBase) -> String {
        let base = match base {
            DiffBase::Branch => "branch",
            DiffBase::Uncommitted => "uncommitted",
        };
        format!("http://{}{DIFF_PATH}?base={base}", self.address)
    }

    /// The WebSocket carrying one connection to `port` in the pod.
    pub fn forward_url(&self, port: u16) -> String {
        format!("ws://{}{FORWARD_PATH}/{port}", self.address)
    }
}

/// The token `pod`'s relay wants from its clients.
fn relay_token(pod: &Pod) -> Option<&str> {
    pod.spec
        .as_ref()?
        .containers
        .iter()
        .find(|container| container.name == AGENT_CONTAINER)?
        .env
        .iter()
        .flatten()
        .find(|env| env.name == RELAY_TOKEN_ENV)?
        .value
        .as_deref()
}

/// 128 random bits, hex: the relay's token for one Job.
pub fn token() -> Result<String> {
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

/// Every address `pods` have: what a request from one of them comes from.
fn pod_addresses(pods: &[Pod]) -> std::collections::HashSet<std::net::IpAddr> {
    pods.iter()
        .filter_map(|pod| pod.status.as_ref())
        .flat_map(|status| {
            status
                .pod_ips
                .iter()
                .flatten()
                .map(|ip| ip.ip.clone())
                .chain(status.pod_ip.clone())
        })
        .filter_map(|ip| ip.parse::<std::net::IpAddr>().ok())
        .map(|ip| ip.to_canonical())
        .collect()
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
    fn copies_reports_only_from_ready_pods() {
        let agent_pod = |name: &str, workspace: Option<&str>, phase: &str, deleting: bool| {
            let mut metadata = json!({ "name": name });
            if let Some(workspace) = workspace {
                metadata["labels"] = json!({ WORKSPACE_LABEL: workspace });
            }
            if deleting {
                metadata["deletionTimestamp"] = json!("2026-01-01T00:00:00Z");
            }
            serde_json::from_value::<Pod>(json!({
                "metadata": metadata,
                "spec": { "containers": [
                    { "name": "agent", "env": [{ "name": "UKAI_RELAY_TOKEN", "value": "t" }] }
                ] },
                "status": {
                    "phase": phase,
                    "podIP": "10.0.0.7",
                    "conditions": [{ "type": "Ready", "status": "True" }]
                }
            }))
            .unwrap_or_default()
        };
        let pods = [
            agent_pod("ready", Some("w1"), "Running", false),
            agent_pod("starting", Some("w2"), "Pending", false),
            agent_pod("stopping", Some("w3"), "Running", true),
            agent_pod("unlabelled", None, "Running", false),
        ];
        let found: Vec<_> = ready_relays(&pods)
            .into_iter()
            .map(|(workspace, endpoint)| (workspace, endpoint.address))
            .collect();
        assert_eq!(found, [("w1".to_owned(), "10.0.0.7:7070".to_owned())]);
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
                address: "10.0.0.7:7070".into(),
                token: "t".into()
            })
        );
        assert_eq!(RelayEndpoint::of(&Pod::default()), None);
        let endpoint = RelayEndpoint::of(&pod).unwrap_or_else(|| panic!("the pod has an address"));
        assert_eq!(endpoint.acp_url(), "ws://10.0.0.7:7070/acp");
        assert_eq!(endpoint.reports_url(""), "http://10.0.0.7:7070/reports");
        assert_eq!(endpoint.ports_url(), "http://10.0.0.7:7070/ports");
        assert_eq!(
            endpoint.forward_url(5173),
            "ws://10.0.0.7:7070/forward/5173"
        );
        assert_eq!(
            endpoint.reports_url("/files/reports/run%201.html"),
            "http://10.0.0.7:7070/reports/files/reports/run%201.html"
        );
    }

    #[rstest]
    #[case::ready(json!({}), json!([
        { "metadata": { "name": "p" },
          "status": { "phase": "Running", "conditions": [{ "type": "Ready", "status": "True" }] } }
    ]), Some("p"))]
    #[case::no_pods(json!({}), json!([]), None)]
    #[case::starting(json!({}), json!([
        { "metadata": { "name": "p" }, "status": { "phase": "Pending" } }
    ]), None)]
    #[case::terminating(json!({}), json!([
        { "metadata": { "name": "p", "deletionTimestamp": "2026-01-01T00:00:00Z" },
          "status": { "phase": "Running", "conditions": [{ "type": "Ready", "status": "True" }] } }
    ]), None)]
    #[case::finished_job(
        json!({ "conditions": [{ "type": "Complete", "status": "True" }] }),
        json!([
            { "metadata": { "name": "p" },
              "status": { "phase": "Running", "conditions": [{ "type": "Ready", "status": "True" }] } }
        ]),
        None
    )]
    fn serves_reports_only_from_a_running_pod(
        #[case] job_status: serde_json::Value,
        #[case] pods: serde_json::Value,
        #[case] expected: Option<&str>,
    ) {
        let job: Job = serde_json::from_value(json!({ "status": job_status }))
            .unwrap_or_else(|e| panic!("{e}"));
        let pods: Vec<Pod> = serde_json::from_value(pods).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(
            serving(&job, pods)
                .and_then(|pod| pod.metadata.name)
                .as_deref(),
            expected
        );
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
