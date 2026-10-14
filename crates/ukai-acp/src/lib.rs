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
    PermissionChoice, PermissionId, PlanItem, RestoreOutcome, SessionEvent, SessionEvents,
    SessionHandle, SessionOptions, ToolStatus, spawn_session,
};

use agent_client_protocol::{AcpAgent, AcpAgentConfig};
use agent_client_protocol_http::{HttpClient, HttpClientError};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use ukai_config::{AgentSpec, InterpolateError, interpolate};

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
pub fn resolve(
    spec: &AgentSpec,
    lookup: impl Fn(&str) -> Option<String> + Copy,
) -> Result<Transport, ResolveError> {
    match spec {
        AgentSpec::Http { url, headers } => remote("http", url, headers, lookup),
        AgentSpec::Websocket { url, headers } => remote("websocket", url, headers, lookup),
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

fn remote(
    transport: &'static str,
    url: &str,
    headers: &std::collections::BTreeMap<String, String>,
    lookup: impl Fn(&str) -> Option<String> + Copy,
) -> Result<Transport, ResolveError> {
    let url = interpolate(url, lookup)?;
    // `HttpClient` picks HTTP/SSE or WebSocket from the scheme alone, so a
    // `transport` that disagrees with the URL would silently be ignored.
    let (expected, schemes) = match transport {
        "websocket" => ("a ws:// or wss://", ["ws://", "wss://"]),
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
        assert!(matches!(resolve(&spec, lookup), Ok(Transport::Remote(_))));
    }

    #[test]
    fn resolves_stdio_with_expanded_env() {
        let spec = AgentSpec::Stdio {
            command: "agent".into(),
            args: vec!["--token=${env:TOKEN}".into()],
            env: BTreeMap::from([("KEY".into(), "${env:TOKEN}".into())]),
        };
        let Ok(Transport::Local(agent)) = resolve(&spec, lookup) else {
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
            resolve(&spec, lookup),
            Err(ResolveError::SchemeMismatch { .. })
        ));
    }

    #[test]
    fn rejects_missing_env() {
        let spec = http("https://example.com", &[("Authorization", "${env:NOPE}")]);
        assert!(matches!(
            resolve(&spec, lookup),
            Err(ResolveError::Interpolate(InterpolateError::MissingEnv(name))) if name == "NOPE"
        ));
    }

    #[test]
    fn rejects_invalid_header_name() {
        let spec = http("https://example.com", &[("bad header", "x")]);
        assert!(matches!(
            resolve(&spec, lookup),
            Err(ResolveError::InvalidHeader(name)) if name == "bad header"
        ));
    }
}
