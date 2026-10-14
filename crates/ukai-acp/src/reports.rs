//! Reports written in a gateway agent's workspace, read over HTTP.
//!
//! An agent behind `ukai-relay` writes its reports into the pod's checkout:
//! the relay has no `fs/*` to send them to the client with, and forwarding
//! `fs/*` would send the agent's code edits to the client's machine and block
//! turns that run with no client attached. So the relay serves the files it
//! has read-only next to `/acp`, `ukai-gateway` proxies that per workspace,
//! and a pane reads them through [`ReportsEndpoint`] instead of the project
//! root.
//!
//! The listing types are the wire format of those endpoints, shared with the
//! relay (which serves them) through `ukai_core::report`.

use std::path::{Path, PathBuf};

use reqwest::header::HeaderMap;
use serde::{Deserialize, Serialize};
use ukai_config::{AgentSpec, Reports};

use crate::{PaneWorkspace, ResolveError, header_map, workspace_headers};

/// One report a workspace holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReportEntry {
    /// Relative to the workspace root (the relay's working directory).
    pub path: PathBuf,
    /// When it last changed, in seconds since the Unix epoch; `None` when
    /// unreadable. A pane follows it to reload what it shows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modified: Option<u64>,
}

/// What the relay answers a listing request with.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReportListing {
    pub files: Vec<ReportEntry>,
}

/// Bytes the relay answered with, and the status it answered: a 404 is the
/// view's to render, not an error here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fetched {
    pub status: u16,
    pub body: Vec<u8>,
}

/// A body larger than this is refused rather than held in memory.
const MAX_BODY: u64 = 64 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum ReportsError {
    #[error("cannot reach the workspace: {0}")]
    Request(String),
    #[error("the workspace answered {status}{}", message_suffix(.message))]
    Status { status: u16, message: String },
    #[error("the workspace sent an unreadable listing: {0}")]
    Listing(String),
    #[error("{} is larger than {MAX_BODY} bytes", .0.display())]
    TooLarge(PathBuf),
}

fn message_suffix(message: &str) -> String {
    if message.is_empty() {
        String::new()
    } else {
        format!(": {message}")
    }
}

/// How many characters of an error body are kept for the message.
const MAX_MESSAGE: usize = 200;

/// Where one workspace's reports are, with what every request sends: the
/// agent's headers (authentication) and the pane's workspace.
#[derive(Debug, Clone)]
pub struct ReportsEndpoint {
    client: reqwest::Client,
    /// `https://<gateway>/agents/<id>/reports`.
    url: String,
    headers: HeaderMap,
}

/// The reports endpoint of `spec`, or `None` for an agent that has none
/// (anything but a gateway agent, or a URL that does not end in `/acp`).
/// `workspace` is the pane's, as for [`crate::resolve`]: the reports of a
/// pane with a pod of its own are that pod's.
pub fn resolve_reports(
    spec: &AgentSpec,
    lookup: impl Fn(&str) -> Option<String> + Copy,
    workspace: Option<&PaneWorkspace>,
) -> Result<Option<ReportsEndpoint>, ResolveError> {
    let AgentSpec::Gateway {
        url,
        headers,
        workspace: default_scope,
    } = spec
    else {
        return Ok(None);
    };
    let url = ukai_config::interpolate(url, lookup)?;
    let Some(url) = reports_url(&url) else {
        return Ok(None);
    };
    let headers = header_map(
        headers,
        &workspace_headers(workspace, *default_scope),
        lookup,
    )?;
    Ok(Some(ReportsEndpoint {
        client: reqwest::Client::new(),
        url,
        headers,
    }))
}

/// `ws://host/agents/a/acp` → `http://host/agents/a/reports`, the endpoint
/// next to it. `None` when the URL is not an ACP endpoint of that shape.
fn reports_url(acp_url: &str) -> Option<String> {
    let (scheme, rest) = acp_url
        .strip_prefix("ws://")
        .map(|rest| ("http://", rest))
        .or_else(|| {
            acp_url
                .strip_prefix("wss://")
                .map(|rest| ("https://", rest))
        })?;
    let rest = rest.trim_end_matches('/').strip_suffix("/acp")?;
    Some(format!("{scheme}{rest}/reports"))
}

impl ReportsEndpoint {
    /// The files the workspace holds under `reports.dirs` matching
    /// `reports.globs`. The settings travel with the request: the relay has
    /// no configuration of its own, and the sidebar lists what the project
    /// asked for.
    pub async fn list(&self, reports: &Reports) -> Result<Vec<ReportEntry>, ReportsError> {
        let mut url = self.parse(&self.url)?;
        {
            let mut query = url.query_pairs_mut();
            for dir in &reports.dirs {
                query.append_pair("dir", &dir.to_string_lossy());
            }
            for glob in &reports.globs {
                query.append_pair("glob", glob);
            }
        }
        let response = self
            .client
            .get(url)
            .headers(self.headers.clone())
            .send()
            .await
            .map_err(request_error)?;
        let status = response.status();
        let body = response.bytes().await.map_err(request_error)?;
        if !status.is_success() {
            return Err(ReportsError::Status {
                status: status.as_u16(),
                message: message(&body),
            });
        }
        let listing: ReportListing =
            serde_json::from_slice(&body).map_err(|error| ReportsError::Listing(error.to_string()))?;
        Ok(listing.files)
    }

    /// One file of the workspace: `path` (a URL path) under `base`, itself
    /// relative to the workspace root. The relay confines the result to the
    /// base, so what a report can reach does not depend on this side.
    pub async fn fetch(&self, base: &Path, path: &str) -> Result<Fetched, ReportsError> {
        let mut url = self.parse(&format!("{}/file{path}", self.url))?;
        url.query_pairs_mut()
            .append_pair("base", &base.to_string_lossy());
        let response = self
            .client
            .get(url)
            .headers(self.headers.clone())
            .send()
            .await
            .map_err(request_error)?;
        let status = response.status().as_u16();
        if response.content_length().is_some_and(|size| size > MAX_BODY) {
            return Err(ReportsError::TooLarge(PathBuf::from(path)));
        }
        let body = response.bytes().await.map_err(request_error)?;
        Ok(Fetched {
            status,
            body: body.to_vec(),
        })
    }
}

impl ReportsEndpoint {
    fn parse(&self, url: &str) -> Result<reqwest::Url, ReportsError> {
        reqwest::Url::parse(url).map_err(|error| ReportsError::Request(error.to_string()))
    }
}

/// Errors keep the message but not the URL: a misconfigured endpoint can
/// carry a credential in it.
fn request_error(error: reqwest::Error) -> ReportsError {
    ReportsError::Request(error.without_url().to_string())
}

fn message(body: &[u8]) -> String {
    String::from_utf8_lossy(body)
        .chars()
        .take(MAX_MESSAGE)
        .collect::<String>()
        .trim()
        .to_owned()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use rstest::rstest;

    use super::*;
    use crate::WorkspaceScope;

    #[rstest]
    #[case::ws("ws://gw:8080/agents/claude/acp", Some("http://gw:8080/agents/claude/reports"))]
    #[case::wss("wss://gw/agents/a/acp", Some("https://gw/agents/a/reports"))]
    #[case::trailing_slash("ws://gw/agents/a/acp/", Some("http://gw/agents/a/reports"))]
    #[case::not_acp("ws://gw/agents/a/socket", None)]
    #[case::not_websocket("https://gw/agents/a/acp", None)]
    fn the_endpoint_sits_next_to_acp(#[case] url: &str, #[case] expected: Option<&str>) {
        assert_eq!(reports_url(url).as_deref(), expected);
    }

    #[test]
    fn only_gateway_agents_have_reports() {
        let lookup = |_: &str| None;
        let stdio = AgentSpec::Stdio {
            command: "agent".into(),
            args: Vec::new(),
            env: BTreeMap::new(),
        };
        assert!(matches!(resolve_reports(&stdio, lookup, None), Ok(None)));

        let gateway = AgentSpec::Gateway {
            url: "ws://gw:8080/agents/claude/acp".into(),
            headers: BTreeMap::from([("Authorization".into(), "Bearer t".into())]),
            workspace: WorkspaceScope::Shared,
        };
        let workspace = PaneWorkspace {
            key: "k1".into(),
            scope: WorkspaceScope::Pod,
        };
        let endpoint = resolve_reports(&gateway, lookup, Some(&workspace))
            .unwrap_or_else(|e| panic!("resolve: {e}"))
            .unwrap_or_else(|| panic!("a gateway agent has reports"));
        assert_eq!(endpoint.url, "http://gw:8080/agents/claude/reports");
        assert_eq!(
            endpoint.headers.get(crate::WORKSPACE_SCOPE_HEADER),
            Some(&reqwest::header::HeaderValue::from_static("pod"))
        );
        assert!(endpoint.headers.contains_key(reqwest::header::AUTHORIZATION));
    }
}
