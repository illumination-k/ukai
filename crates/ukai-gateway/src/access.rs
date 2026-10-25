//! Who may ask the gateway for what, and which workspace they mean: shared
//! by the HTTP routes and the gRPC services, which only say it differently.
//!
//! A person's requests carry the gateway's token, when it has one. Agent
//! pods reach the gateway too (for their own knowledge), and a pod does not
//! hold that token; but a gateway with no token of its own (one behind IAP,
//! whose `Authorization` is IAP's) would take a pod's request for a
//! person's. So the person's side also refuses any request whose peer is an
//! agent pod ([`AgentPods`], [`guard`]), token or not.

use std::collections::HashSet;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use ukai_proto::gateway::v1 as gw;

use crate::Gateway;
use crate::cluster::RelayEndpoint;
use crate::config::KubernetesAgent;
use crate::job::Plan;

/// Why a request is not answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Denial {
    /// The gateway's token is missing or wrong.
    Unauthorized,
    /// From an agent pod, which may not act as a person.
    AgentPod,
    NoAgent(String),
    BadWorkspace(&'static str),
    /// The cluster cannot be asked what the request needs.
    Unavailable(&'static str),
}

impl IntoResponse for Denial {
    fn into_response(self) -> Response {
        match self {
            Self::Unauthorized => StatusCode::UNAUTHORIZED.into_response(),
            Self::AgentPod => (
                StatusCode::FORBIDDEN,
                "an agent pod may not ask what a person asks",
            )
                .into_response(),
            Self::NoAgent(_) => StatusCode::NOT_FOUND.into_response(),
            Self::BadWorkspace(problem) => (StatusCode::BAD_REQUEST, problem).into_response(),
            Self::Unavailable(why) => (StatusCode::BAD_GATEWAY, why).into_response(),
        }
    }
}

impl From<Denial> for tonic::Status {
    fn from(denial: Denial) -> Self {
        match denial {
            Denial::Unauthorized => {
                Self::unauthenticated("the gateway's token is missing or wrong")
            }
            Denial::AgentPod => {
                Self::permission_denied("an agent pod may not ask what a person asks")
            }
            Denial::NoAgent(id) => Self::not_found(format!("the gateway has no agent `{id}`")),
            Denial::BadWorkspace(problem) => Self::invalid_argument(problem),
            Denial::Unavailable(why) => Self::unavailable(why),
        }
    }
}

/// Whether `headers` carry the gateway's token, when it has one.
pub fn authorized(gateway: &Gateway, headers: &HeaderMap) -> bool {
    let Some(token) = &gateway.token else {
        return true;
    };
    headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .is_some_and(|presented| ukai_core::secret::same_secret(token, presented))
}

/// [`authorized`], or the denial.
pub fn authorize(gateway: &Gateway, headers: &HeaderMap) -> Result<(), Denial> {
    authorized(gateway, headers)
        .then_some(())
        .ok_or(Denial::Unauthorized)
}

pub fn agent<'g>(gateway: &'g Gateway, id: &str) -> Result<&'g KubernetesAgent, Denial> {
    gateway
        .agents
        .get(id)
        .ok_or_else(|| Denial::NoAgent(id.to_owned()))
}

/// The workspace of agent `id` with the pane key `pane` (`None` for the
/// shared one), as `/acp` places a connection.
pub fn plan(gateway: &Gateway, id: &str, pane: Option<&str>) -> Result<Plan, Denial> {
    Ok(Plan::new(
        agent(gateway, id)?.clone(),
        &gateway.user,
        gateway.cluster.namespace(),
        pane,
    ))
}

/// Whether `key` can name a pane's own workspace: 1-64 letters, digits,
/// `-`, or `_`.
fn valid_pane_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 64
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// The pane key the headers `Ukai-Workspace-Scope: pod` and
/// `Ukai-Workspace: <key>` ask for; `None` for the shared workspace.
pub fn pane_of_headers(headers: &HeaderMap) -> Result<Option<String>, Denial> {
    let header = |name| headers.get(name).and_then(|value| value.to_str().ok());
    match header("ukai-workspace-scope") {
        None | Some("shared") => Ok(None),
        Some("pod") => {
            let key = header("ukai-workspace").ok_or(Denial::BadWorkspace(
                "scope `pod` needs a Ukai-Workspace key",
            ))?;
            valid_pane_key(key)
                .then(|| Some(key.to_owned()))
                .ok_or(Denial::BadWorkspace(
                    "Ukai-Workspace must be 1-64 letters, digits, `-`, or `_`",
                ))
        }
        Some(_) => Err(Denial::BadWorkspace(
            "Ukai-Workspace-Scope must be `shared` or `pod`",
        )),
    }
}

/// The pane key a gRPC request's `Workspace` asks for, likewise. A request
/// that does not say, or names a scope this gateway does not know, is
/// refused rather than taken for the shared workspace.
pub fn pane_of(workspace: Option<gw::Workspace>) -> Result<Option<String>, Denial> {
    let workspace = workspace.unwrap_or_default();
    match gw::WorkspaceScope::try_from(workspace.scope) {
        Ok(gw::WorkspaceScope::Unspecified) => Err(Denial::BadWorkspace(
            "the request does not say which workspace",
        )),
        Ok(gw::WorkspaceScope::Shared) => Ok(None),
        Ok(gw::WorkspaceScope::Pod) if valid_pane_key(&workspace.key) => Ok(Some(workspace.key)),
        Ok(gw::WorkspaceScope::Pod) => Err(Denial::BadWorkspace(
            "a pod workspace's key is 1-64 letters, digits, `-`, or `_`",
        )),
        Err(_) => Err(Denial::BadWorkspace(
            "a workspace scope this gateway does not know",
        )),
    }
}

/// The relay of the workspace's running pod; `None` when it has none. It
/// never starts a pod: what asks this way reads work that is already there.
pub async fn running_relay(
    gateway: &Gateway,
    plan: &Plan,
) -> Result<Option<RelayEndpoint>, Denial> {
    match gateway.cluster.serving_pod(plan).await {
        Ok(pod) => Ok(pod.as_ref().and_then(RelayEndpoint::of)),
        Err(error) => {
            tracing::warn!(job = %plan.job_name(), %error, "cannot look up the workspace's pod");
            Err(Denial::Unavailable("cannot look up the workspace's pod"))
        }
    }
}

/// How old the list of agent pod addresses may be before an address not in
/// it is looked up again. A pod's address is in the list once the kubelet
/// reports it, a moment after the pod's sandbox starts and long before its
/// agent can send anything (the relay is copied in and the checkout cloned
/// first), so a list this fresh has it.
const AGENT_PODS_FRESH: Duration = Duration::from_secs(1);

/// The addresses of the agent pods, listed again when one that is not among
/// them asks and the list is older than [`AGENT_PODS_FRESH`].
#[derive(Default)]
pub struct AgentPods {
    known: tokio::sync::Mutex<(Option<Instant>, HashSet<IpAddr>)>,
}

impl AgentPods {
    /// Whether `peer` is an agent pod, with `list` listing their addresses
    /// when the known ones are stale. A failed listing is a denial: the
    /// request cannot be told apart from a pod's.
    pub async fn contains<F, Fut, E>(&self, peer: IpAddr, list: F) -> Result<bool, Denial>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<HashSet<IpAddr>, E>>,
        E: std::fmt::Display,
    {
        let mut known = self.known.lock().await;
        if known.1.contains(&peer) {
            return Ok(true);
        }
        let fresh = known.0.is_some_and(|at| at.elapsed() < AGENT_PODS_FRESH);
        if fresh {
            return Ok(false);
        }
        let addresses = list().await.map_err(|error| {
            tracing::warn!(%error, "cannot list the agent pods");
            Denial::Unavailable("cannot tell whether the request is an agent pod's")
        })?;
        *known = (Some(Instant::now()), addresses);
        Ok(known.1.contains(&peer))
    }
}

/// Paths an agent pod may ask: its own knowledge, which it asks with its
/// relay's token, and the health check.
fn for_pods(path: &str) -> bool {
    path == "/health" || path.starts_with("/ukai.gateway.v1.WorkspaceKnowledge/")
}

/// Refuse what an agent pod asks of a person's side.
pub async fn guard(
    State(gateway): State<Arc<Gateway>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    request: Request,
    next: Next,
) -> Response {
    if for_pods(request.uri().path()) {
        return next.run(request).await;
    }
    let peer = peer.ip().to_canonical();
    let cluster = &gateway.cluster;
    let agent_pod = gateway
        .agent_pods
        .contains(peer, || cluster.agent_pod_addresses())
        .await;
    tracing::debug!(%peer, path = request.uri().path(), ?agent_pod, "who asks");
    match agent_pod {
        Ok(false) => next.run(request).await,
        Ok(true) => {
            tracing::warn!(%peer, path = request.uri().path(), "refused an agent pod asking as a person");
            Denial::AgentPod.into_response()
        }
        Err(denial) => denial.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use rstest::rstest;

    use super::*;

    fn ip(text: &str) -> IpAddr {
        text.parse().unwrap_or_else(|e| panic!("{e}"))
    }

    #[tokio::test]
    async fn knows_agent_pods_and_lists_them_again_only_when_stale() {
        let pods = AgentPods::default();
        let listed = AtomicUsize::new(0);
        let list = || async {
            listed.fetch_add(1, Ordering::SeqCst);
            Ok::<_, String>(HashSet::from([ip("10.0.0.7")]))
        };
        assert_eq!(pods.contains(ip("10.0.0.7"), list).await, Ok(true));
        // A known pod, and an address listed a moment ago, ask nothing.
        assert_eq!(pods.contains(ip("10.0.0.7"), list).await, Ok(true));
        assert_eq!(pods.contains(ip("10.0.9.9"), list).await, Ok(false));
        assert_eq!(listed.load(Ordering::SeqCst), 1);
        pods.known.lock().await.0 = Instant::now().checked_sub(AGENT_PODS_FRESH * 2);
        assert_eq!(pods.contains(ip("10.0.9.9"), list).await, Ok(false));
        assert_eq!(listed.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_failed_listing_is_a_denial() {
        let pods = AgentPods::default();
        let failed = pods
            .contains(ip("10.0.0.7"), || async {
                Err::<HashSet<IpAddr>, _>("down")
            })
            .await;
        assert!(matches!(failed, Err(Denial::Unavailable(_))));
    }

    #[rstest]
    #[case::health("/health", true)]
    #[case::own_knowledge("/ukai.gateway.v1.WorkspaceKnowledge/Put", true)]
    #[case::persons_knowledge("/ukai.gateway.v1.Knowledge/Approve", false)]
    #[case::schedules("/ukai.gateway.v1.Schedules/Create", false)]
    #[case::acp("/agents/a/acp", false)]
    #[case::lookalike("/ukai.gateway.v1.WorkspaceKnowledgeX/Put", false)]
    fn only_a_pods_own_paths_are_open_to_pods(#[case] path: &str, #[case] open: bool) {
        assert_eq!(for_pods(path), open);
    }

    #[rstest]
    #[case::shared(gw::WorkspaceScope::Shared.into(), "ignored", Ok(None))]
    #[case::pod(gw::WorkspaceScope::Pod.into(), "pane-1_a", Ok(Some("pane-1_a")))]
    #[case::pod_without_key(gw::WorkspaceScope::Pod.into(), "", Err(()))]
    #[case::pod_bad_key(gw::WorkspaceScope::Pod.into(), "a/b", Err(()))]
    #[case::unknown_scope(7, "k", Err(()))]
    #[case::unspecified(gw::WorkspaceScope::Unspecified.into(), "", Err(()))]
    fn a_request_names_its_workspace(
        #[case] scope: i32,
        #[case] key: &str,
        #[case] expected: Result<Option<&str>, ()>,
    ) {
        let workspace = gw::Workspace {
            scope,
            key: key.into(),
        };
        assert_eq!(
            pane_of(Some(workspace)).map_err(drop),
            expected.map(|key| key.map(str::to_owned))
        );
    }
}
