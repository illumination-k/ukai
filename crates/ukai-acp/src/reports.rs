//! Reports a gateway agent wrote in its workspace, read back over HTTP.
//!
//! An agent behind `ukai-gateway` works in a pod, and `ukai-relay`
//! advertises no `fs/*` capability to it: a turn running with no client
//! attached would block on a request nobody answers, and the agent's file
//! edits belong in the pod's checkout rather than on this machine. Its
//! reports therefore stay where it wrote them and are read back instead —
//! the relay serves its reports tree read-only next to `/acp`, the gateway
//! proxies that per workspace, and this is the client side.
//!
//! Listed paths are project-relative, as in `state.json`: a listing entry
//! and the pane that shows it name the same `reports/run.html`. What may be
//! read is the server's business (`ukai_core::report::ReportTree`, which the
//! relay serves from); nothing here decides it.

use std::time::Duration;

use reqwest::header::{CONTENT_TYPE, ETAG};
use serde::{Deserialize, Serialize};
use ukai_config::{AgentSpec, Source};

use crate::{PaneWorkspace, ResolveError, header_map, workspace_headers};

/// Suffix of an agent's ACP endpoint that lists its workspace's reports. One
/// file is `<it>/files<project-relative URL path>`.
pub const REPORTS_PATH: &str = "/reports";

/// Path under [`REPORTS_PATH`] holding the files themselves.
pub const FILES_PATH: &str = "/files";

/// How long a request may take. Generous: the hop is client → gateway → pod,
/// and a report can be a few megabytes of HTML.
const TIMEOUT: Duration = Duration::from_secs(30);

/// What a relay or gateway answers at [`REPORTS_PATH`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Listing {
    pub reports: Vec<Entry>,
}

/// One report in a [`Listing`]; newest first, as the relay lists them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// Relative to the agent's checkout, e.g. `reports/run.html`.
    pub path: String,
    /// When it last changed, in seconds since the epoch; absent when the
    /// server could not tell.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modified: Option<u64>,
}

/// One report file, as served.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct File {
    pub status: u16,
    /// What the server says it is; the client does not guess from the name.
    pub content_type: Option<String>,
    /// Changes when the file does, so a pane can tell it was rewritten.
    pub etag: Option<String>,
    pub body: Vec<u8>,
}

#[derive(Debug, thiserror::Error)]
pub enum ReportsError {
    #[error("cannot reach the gateway: {0}")]
    Unreachable(String),
    #[error("the agent's workspace is no longer there")]
    Gone,
    #[error("the gateway answered {0}")]
    Status(u16),
    #[error("the gateway sent a listing that cannot be read: {0}")]
    Listing(String),
}

/// Where one workspace's reports are read from: a gateway agent's endpoint
/// with the agent's headers and the pane's workspace among them.
#[derive(Debug, Clone)]
pub struct ReportsEndpoint {
    client: reqwest::Client,
    base: String,
}

impl PartialEq for ReportsEndpoint {
    /// Two endpoints are the same when they read the same workspace; the
    /// client behind them is interchangeable.
    fn eq(&self, other: &Self) -> bool {
        self.base == other.base
    }
}

impl ReportsEndpoint {
    /// The reports of `workspace` for `spec`, or `None` when the agent is
    /// not a gateway agent: no other transport serves reports.
    pub fn resolve(
        spec: &AgentSpec,
        lookup: impl Fn(Source, &str) -> Option<String> + Copy,
        workspace: Option<&PaneWorkspace>,
    ) -> Result<Option<Self>, ResolveError> {
        let AgentSpec::Gateway {
            url,
            headers,
            workspace: default_scope,
        } = spec
        else {
            return Ok(None);
        };
        let Some(base) = reports_base(&ukai_config::interpolate(url, lookup)?) else {
            return Ok(None);
        };
        let extra = workspace_headers(workspace, *default_scope);
        let client = reqwest::Client::builder()
            .default_headers(header_map(headers, &extra, lookup)?)
            .timeout(TIMEOUT)
            .build()
            .map_err(|error| ResolveError::Client(error.to_string()))?;
        Ok(Some(Self { client, base }))
    }

    /// The listing URL, which also identifies the workspace being read.
    pub fn base(&self) -> &str {
        &self.base
    }

    /// Every report of the workspace, newest first.
    pub async fn list(&self) -> Result<Vec<Entry>, ReportsError> {
        let response = self.send(self.client.get(&self.base)).await?;
        let status = response.status().as_u16();
        if status != 200 {
            return Err(failure(status));
        }
        response
            .json::<Listing>()
            .await
            .map(|listing| listing.reports)
            .map_err(|error| ReportsError::Listing(error.to_string()))
    }

    /// One report, or an asset it loads, at `url_path`: a `/`-prefixed,
    /// percent-encoded project-relative path, as a report's web view asks
    /// for it. A status the server chose (403, 404, 410) comes back in the
    /// [`File`] so the pane can show it; only failing to ask is an error.
    pub async fn fetch(&self, url_path: &str) -> Result<File, ReportsError> {
        let response = self.send(self.client.get(self.file_url(url_path))).await?;
        let status = response.status().as_u16();
        let header = |name: reqwest::header::HeaderName| {
            response
                .headers()
                .get(name)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned)
        };
        let content_type = header(CONTENT_TYPE);
        let etag = header(ETAG);
        let body = response
            .bytes()
            .await
            .map_err(|error| ReportsError::Unreachable(error.to_string()))?;
        Ok(File {
            status,
            content_type,
            etag,
            body: body.into(),
        })
    }

    /// The validator of one report, to notice that the agent rewrote it
    /// without fetching it again. `None` when the server sent none.
    pub async fn stamp(&self, url_path: &str) -> Result<Option<String>, ReportsError> {
        let response = self.send(self.client.head(self.file_url(url_path))).await?;
        let status = response.status().as_u16();
        if status != 200 {
            return Err(failure(status));
        }
        Ok(response
            .headers()
            .get(ETAG)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned))
    }

    fn file_url(&self, url_path: &str) -> String {
        format!("{}{FILES_PATH}{url_path}", self.base)
    }

    async fn send(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<reqwest::Response, ReportsError> {
        request
            .send()
            .await
            .map_err(|error| ReportsError::Unreachable(error.to_string()))
    }
}

/// Where the reports of a gateway agent at `url` are served: the same
/// endpoint over HTTP, with `/acp` replaced. `None` for a URL whose scheme
/// is not one a gateway speaks.
pub fn reports_base(url: &str) -> Option<String> {
    endpoint_base(url, REPORTS_PATH)
}

/// A gateway agent's HTTP endpoint next to its `/acp` at `url`: `suffix` in
/// place of `/acp`.
pub(crate) fn endpoint_base(url: &str, suffix: &str) -> Option<String> {
    let (scheme, rest) = match url.split_once("://")? {
        ("ws", rest) => ("http", rest),
        ("wss", rest) => ("https", rest),
        // A gateway URL is validated to be ws:// or wss://; an http one
        // names the same endpoint.
        ("http", rest) => ("http", rest),
        ("https", rest) => ("https", rest),
        _ => return None,
    };
    let rest = rest.split(['?', '#']).next().unwrap_or(rest);
    let rest = rest.strip_suffix("/acp").unwrap_or(rest);
    let rest = rest.trim_end_matches('/');
    (!rest.is_empty()).then(|| format!("{scheme}://{rest}{suffix}"))
}

/// What a status that is not 200 means: a workspace whose pod has gone is a
/// case of its own, and the gateway's 404 means the same to a client.
fn failure(status: u16) -> ReportsError {
    match status {
        404 | 410 => ReportsError::Gone,
        other => ReportsError::Status(other),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use rstest::rstest;
    use ukai_config::WorkspaceScope;

    use super::*;

    fn gateway(url: &str) -> AgentSpec {
        AgentSpec::Gateway {
            url: url.to_owned(),
            headers: BTreeMap::from([("Authorization".to_owned(), "Bearer t".to_owned())]),
            workspace: WorkspaceScope::Pod,
        }
    }

    #[rstest]
    #[case::ws(
        "ws://localhost:8080/agents/a/acp",
        Some("http://localhost:8080/agents/a/reports")
    )]
    #[case::wss(
        "wss://gw.example.com/agents/a/acp",
        Some("https://gw.example.com/agents/a/reports")
    )]
    #[case::trailing_slash("ws://gw/agents/a/", Some("http://gw/agents/a/reports"))]
    #[case::no_acp_suffix("ws://gw/agents/a", Some("http://gw/agents/a/reports"))]
    #[case::query("wss://gw/agents/a/acp?x=1", Some("https://gw/agents/a/reports"))]
    #[case::host_only("ws://gw", Some("http://gw/reports"))]
    #[case::not_a_url("agents/a/acp", None)]
    #[case::unsupported_scheme("file:///tmp/acp", None)]
    fn derives_the_reports_endpoint(#[case] url: &str, #[case] expected: Option<&str>) {
        assert_eq!(reports_base(url).as_deref(), expected);
    }

    #[test]
    fn resolves_a_gateway_agents_reports_for_the_panes_workspace() {
        let workspace = PaneWorkspace {
            key: "k1".into(),
            scope: WorkspaceScope::Pod,
        };
        let endpoint = ReportsEndpoint::resolve(
            &gateway("ws://localhost:8080/agents/a/acp"),
            |_, _| None,
            Some(&workspace),
        )
        .unwrap_or_else(|e| panic!("resolve: {e}"))
        .unwrap_or_else(|| panic!("a gateway agent serves reports"));
        assert_eq!(endpoint.base(), "http://localhost:8080/agents/a/reports");
        assert_eq!(
            endpoint.file_url("/reports/2026/run%201.html"),
            "http://localhost:8080/agents/a/reports/files/reports/2026/run%201.html"
        );
    }

    #[rstest]
    #[case::stdio(AgentSpec::Stdio { command: "agent".into(), args: Vec::new(), env: BTreeMap::new() })]
    #[case::http(AgentSpec::Http { url: "https://example.com/acp".into(), headers: BTreeMap::new() })]
    #[case::websocket(AgentSpec::Websocket { url: "wss://example.com/acp".into(), headers: BTreeMap::new() })]
    fn other_transports_serve_no_reports(#[case] spec: AgentSpec) {
        assert!(matches!(
            ReportsEndpoint::resolve(&spec, |_, _| None, None),
            Ok(None)
        ));
    }

    #[test]
    fn refuses_to_resolve_without_the_agents_credentials() {
        let spec = AgentSpec::Gateway {
            url: "ws://gw/agents/a/acp".into(),
            headers: BTreeMap::from([("Authorization".into(), "Bearer ${env:NOPE}".into())]),
            workspace: WorkspaceScope::Shared,
        };
        assert!(ReportsEndpoint::resolve(&spec, |_, _| None, None).is_err());
    }

    #[test]
    fn a_vanished_workspace_is_told_apart_from_other_failures() {
        assert!(matches!(failure(410), ReportsError::Gone));
        assert!(matches!(failure(404), ReportsError::Gone));
        assert!(matches!(failure(401), ReportsError::Status(401)));
    }

    #[test]
    fn listings_round_trip() {
        let listing = Listing {
            reports: vec![
                Entry {
                    path: "reports/run.html".into(),
                    modified: Some(1_700_000_000),
                },
                Entry {
                    path: "reports/img/plot.png".into(),
                    modified: None,
                },
            ],
        };
        let json = serde_json::to_string(&listing).unwrap_or_default();
        assert_eq!(serde_json::from_str::<Listing>(&json).ok(), Some(listing));
        assert!(!json.contains("null"), "{json}");
        // A listing from a relay that could not stat a file.
        assert_eq!(
            serde_json::from_str::<Listing>(r#"{"reports":[{"path":"reports/run.html"}]}"#)
                .ok()
                .and_then(|listing| listing.reports.first().cloned()),
            Some(Entry {
                path: "reports/run.html".into(),
                modified: None
            })
        );
    }
}
