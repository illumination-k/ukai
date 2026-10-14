//! Connections from ukai to ACP agents.
//!
//! [`resolve`] turns a declared [`AgentSpec`] into a ready-to-connect
//! [`Transport`] without touching the network: references are expanded,
//! URLs and headers are checked, and the remote/local split is decided here
//! so the UI never sees transport details. [`spawn_session`] then runs one
//! ACP session over it.

mod fs;
mod session;

pub use fs::{FsError, FsRoot};
pub use session::{
    AgentCommand, AgentMode, Attachment, FileDiff, ModeState, PROGRESS_METHOD, PermissionChoice,
    PermissionId, PlanItem, Prompt, PromptCaps, RestoreOutcome, SessionEvent, SessionEvents,
    SessionHandle, SessionOptions, TURN_STATE_METHOD, ToolStatus, spawn_session,
};

use agent_client_protocol::{AcpAgent, AcpAgentConfig};
use agent_client_protocol_http::{HttpClient, HttpClientError};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde::{Deserialize, Serialize};
pub use ukai_config::WorkspaceScope;
use ukai_config::{AgentSpec, InterpolateError, interpolate};

/// The workspace a pane asks a gateway agent for, sent with the connection:
/// a [`WorkspaceScope::Pod`] pane gets a pod of its own, found again by `key`
/// when the pane reconnects.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneWorkspace {
    pub key: String,
    pub scope: WorkspaceScope,
}

/// Headers `ukai-gateway` reads to place a connection.
pub const WORKSPACE_HEADER: &str = "ukai-workspace";
pub const WORKSPACE_SCOPE_HEADER: &str = "ukai-workspace-scope";

/// A resolved way to reach one agent. Both variants implement
/// `agent_client_protocol::ConnectTo<Client>`.
#[derive(Debug)]
pub enum Transport {
    /// HTTP/SSE or WebSocket, chosen by the URL scheme.
    Remote(HttpClient),
    /// A local subprocess over stdio.
    Local(AcpAgent),
}

#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
    #[error(transparent)]
    Interpolate(#[from] InterpolateError),
    #[error("transport `{transport}` expects {expected} URL, got `{url}`")]
    SchemeMismatch {
        transport: &'static str,
        expected: &'static str,
        url: String,
    },
    #[error("invalid header `{0}`")]
    InvalidHeader(String),
    #[error(transparent)]
    Http(#[from] HttpClientError),
}

/// Resolve `spec`, reading `${env:NAME}` references through `lookup`.
/// `workspace` is the pane's, for a gateway agent; other agents ignore it.
pub fn resolve(
    spec: &AgentSpec,
    lookup: impl Fn(&str) -> Option<String> + Copy,
    workspace: Option<&PaneWorkspace>,
) -> Result<Transport, ResolveError> {
    match spec {
        AgentSpec::Http { url, headers } => remote("http", url, headers, &[], lookup),
        AgentSpec::Websocket { url, headers } => remote("websocket", url, headers, &[], lookup),
        AgentSpec::Gateway {
            url,
            headers,
            workspace: default_scope,
        } => remote(
            "gateway",
            url,
            headers,
            &workspace_headers(workspace, *default_scope),
            lookup,
        ),
        AgentSpec::Stdio { command, args, env } => {
            let args = args
                .iter()
                .map(|arg| interpolate(arg, lookup))
                .collect::<Result<Vec<_>, _>>()?;
            let env = env
                .iter()
                .map(|(name, value)| Ok((name.clone(), interpolate(value, lookup)?)))
                .collect::<Result<Vec<_>, InterpolateError>>()?;
            let config = AcpAgentConfig::new(interpolate(command, lookup)?)
                .args(args)
                .envs(env);
            Ok(Transport::Local(AcpAgent::new(config)))
        }
    }
}

/// What a gateway connection says about its pane: the scope (the pane's own,
/// else the agent's default) and, when the pane has one, its key.
fn workspace_headers(
    workspace: Option<&PaneWorkspace>,
    default_scope: WorkspaceScope,
) -> Vec<(&'static str, String)> {
    let scope = workspace.map_or(default_scope, |workspace| workspace.scope);
    let scope = match scope {
        WorkspaceScope::Shared => "shared",
        WorkspaceScope::Pod => "pod",
    };
    let mut headers = vec![(WORKSPACE_SCOPE_HEADER, scope.to_owned())];
    if let Some(workspace) = workspace {
        headers.push((WORKSPACE_HEADER, workspace.key.clone()));
    }
    headers
}

fn remote(
    transport: &'static str,
    url: &str,
    headers: &std::collections::BTreeMap<String, String>,
    extra: &[(&'static str, String)],
    lookup: impl Fn(&str) -> Option<String> + Copy,
) -> Result<Transport, ResolveError> {
    let url = interpolate(url, lookup)?;
    // `HttpClient` picks HTTP/SSE or WebSocket from the scheme alone, so a
    // `transport` that disagrees with the URL would silently be ignored.
    let (expected, schemes) = match transport {
        "websocket" | "gateway" => ("a ws:// or wss://", ["ws://", "wss://"]),
        _ => ("an http:// or https://", ["http://", "https://"]),
    };
    if !schemes.iter().any(|scheme| url.starts_with(scheme)) {
        return Err(ResolveError::SchemeMismatch {
            transport,
            expected,
            url,
        });
    }

    let mut map = HeaderMap::new();
    for (name, value) in headers {
        let invalid = || ResolveError::InvalidHeader(name.clone());
        let header = HeaderName::from_bytes(name.as_bytes()).map_err(|_| invalid())?;
        let mut value =
            HeaderValue::from_str(&interpolate(value, lookup)?).map_err(|_| invalid())?;
        // Header values are typically credentials: keep them out of Debug output.
        value.set_sensitive(true);
        map.insert(header, value);
    }
    for (name, value) in extra {
        let value = HeaderValue::from_str(value)
            .map_err(|_| ResolveError::InvalidHeader((*name).to_owned()))?;
        map.insert(*name, value);
    }

    let client = HttpClient::builder(&url)
        .configure_http(|http| http.default_headers(map))
        .build()?;
    Ok(Transport::Remote(client))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use rstest::rstest;

    use super::*;

    fn lookup(name: &str) -> Option<String> {
        (name == "TOKEN").then(|| "s3cret".to_owned())
    }

    fn http(url: &str, headers: &[(&str, &str)]) -> AgentSpec {
        AgentSpec::Http {
            url: url.to_owned(),
            headers: headers
                .iter()
                .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
                .collect(),
        }
    }

    #[rstest]
    #[case::https(http("https://agents.example.com", &[("Authorization", "Bearer ${env:TOKEN}")]))]
    #[case::websocket(AgentSpec::Websocket { url: "wss://agents.example.com/acp".into(), headers: BTreeMap::new() })]
    fn resolves_remote(#[case] spec: AgentSpec) {
        assert!(matches!(
            resolve(&spec, lookup, None),
            Ok(Transport::Remote(_))
        ));
    }

    #[test]
    fn resolves_stdio_with_expanded_env() {
        let spec = AgentSpec::Stdio {
            command: "agent".into(),
            args: vec!["--token=${env:TOKEN}".into()],
            env: BTreeMap::from([("KEY".into(), "${env:TOKEN}".into())]),
        };
        let Ok(Transport::Local(agent)) = resolve(&spec, lookup, None) else {
            panic!("expected a local transport");
        };
        assert_eq!(agent.config().arguments(), ["--token=s3cret"]);
        assert_eq!(
            agent.config().environment().get("KEY").map(String::as_str),
            Some("s3cret")
        );
    }

    #[rstest]
    #[case::ws_url_on_http(http("wss://example.com", &[]))]
    #[case::http_url_on_websocket(AgentSpec::Websocket { url: "https://example.com".into(), headers: BTreeMap::new() })]
    fn rejects_scheme_mismatch(#[case] spec: AgentSpec) {
        assert!(matches!(
            resolve(&spec, lookup, None),
            Err(ResolveError::SchemeMismatch { .. })
        ));
    }

    #[rstest]
    #[case::pane_scope(Some(WorkspaceScope::Pod), WorkspaceScope::Shared, "pod", Some("k1"))]
    #[case::default_scope(None, WorkspaceScope::Pod, "pod", None)]
    #[case::shared(
        Some(WorkspaceScope::Shared),
        WorkspaceScope::Pod,
        "shared",
        Some("k1")
    )]
    fn gateway_connections_carry_the_panes_workspace(
        #[case] pane_scope: Option<WorkspaceScope>,
        #[case] default_scope: WorkspaceScope,
        #[case] scope: &str,
        #[case] key: Option<&str>,
    ) {
        let workspace = pane_scope.map(|scope| PaneWorkspace {
            key: "k1".into(),
            scope,
        });
        let headers = workspace_headers(workspace.as_ref(), default_scope);
        assert!(headers.contains(&(WORKSPACE_SCOPE_HEADER, scope.to_owned())));
        assert_eq!(
            headers
                .iter()
                .find(|(name, _)| *name == WORKSPACE_HEADER)
                .map(|(_, value)| value.as_str()),
            key
        );
        let spec = AgentSpec::Gateway {
            url: "ws://localhost:8080/agents/a/acp".into(),
            headers: BTreeMap::new(),
            workspace: default_scope,
        };
        assert!(matches!(
            resolve(&spec, lookup, workspace.as_ref()),
            Ok(Transport::Remote(_))
        ));
    }

    #[test]
    fn rejects_missing_env() {
        let spec = http("https://example.com", &[("Authorization", "${env:NOPE}")]);
        assert!(matches!(
            resolve(&spec, lookup, None),
            Err(ResolveError::Interpolate(InterpolateError::MissingEnv(name))) if name == "NOPE"
        ));
    }

    #[test]
    fn rejects_invalid_header_name() {
        let spec = http("https://example.com", &[("bad header", "x")]);
        assert!(matches!(
            resolve(&spec, lookup, None),
            Err(ResolveError::InvalidHeader(name)) if name == "bad header"
        ));
    }
}
