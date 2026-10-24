//! The gateway's gRPC services (`ukai.gateway.v1`) as its clients reach
//! them: gRPC-Web over the reqwest client each endpoint already has, so the
//! agent's headers, TLS, and timeouts apply as they do to its other
//! requests, and so does any HTTP/1.1 proxy in front of the gateway, such as
//! IAP, which might not pass gRPC's HTTP/2. Unary calls only.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use http_body_util::BodyExt;
use tonic::Code;
use tonic::codegen::http;
use tonic_web::{GrpcWebCall, GrpcWebClientService};
use ukai_config::WorkspaceScope;
use ukai_proto::gateway::v1 as gw;

use crate::PaneWorkspace;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// One gateway, as the generated clients call it.
pub type Channel = GrpcWebClientService<Web>;

/// The gateway at `root` (`https://gw.example.com`), through `client`.
pub fn channel(client: reqwest::Client, root: String) -> Channel {
    GrpcWebClientService::new(Web { client, root })
}

/// Sends a gRPC-Web request with reqwest.
#[derive(Debug, Clone)]
pub struct Web {
    client: reqwest::Client,
    root: String,
}

impl tower::Service<http::Request<GrpcWebCall<tonic::body::Body>>> for Web {
    type Response = http::Response<reqwest::Body>;
    type Error = BoxError;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, BoxError>> + Send>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), BoxError>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: http::Request<GrpcWebCall<tonic::body::Body>>) -> Self::Future {
        let client = self.client.clone();
        let url = format!("{}{}", self.root, request.uri().path());
        Box::pin(async move {
            let (parts, body) = request.into_parts();
            // A unary call's request is one message, so it is sent whole.
            let body = body.collect().await?.to_bytes();
            let response = client
                .post(url)
                .headers(parts.headers)
                .body(body)
                .send()
                .await?;
            Ok(http::Response::from(response))
        })
    }
}

/// A gateway agent's `/acp` URL as the gateway's root and the agent's id:
/// `wss://gw.example.com/agents/claude/acp` is `https://gw.example.com` and
/// `claude`, where the gRPC services are `<root>/<service>/<method>`.
pub fn agent_root(url: &str) -> Option<(String, String)> {
    let (scheme, rest) = match url.split_once("://")? {
        ("ws" | "http", rest) => ("http", rest),
        ("wss" | "https", rest) => ("https", rest),
        _ => return None,
    };
    let rest = rest.split(['?', '#']).next().unwrap_or(rest);
    let rest = rest.trim_end_matches('/');
    let rest = rest.strip_suffix("/acp")?;
    let (root, agent) = rest.rsplit_once("/agents/")?;
    (!root.is_empty() && !agent.is_empty() && !agent.contains('/'))
        .then(|| (format!("{scheme}://{root}"), agent.to_owned()))
}

/// The workspace a pane's request is about, as for its `/acp`: the pane's
/// own scope, or else the agent's `default` one.
pub fn workspace(workspace: Option<&PaneWorkspace>, default: WorkspaceScope) -> gw::Workspace {
    let scope = workspace.map_or(default, |workspace| workspace.scope);
    let (scope, key) = match scope {
        WorkspaceScope::Shared => (gw::WorkspaceScope::Shared, String::new()),
        WorkspaceScope::Pod => (
            gw::WorkspaceScope::Pod,
            workspace.map(|w| w.key.clone()).unwrap_or_default(),
        ),
    };
    gw::Workspace {
        scope: scope.into(),
        key,
    }
}

/// What went wrong with a call, as every client of the gateway tells it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
    /// The gateway, or what it asks, cannot be reached now.
    Unreachable(String),
    NotFound,
    /// The token is missing or wrong, or the caller may not do this.
    Denied(String),
    TooMany,
    /// The workspace is not running, and the call does not start it.
    NotRunning,
    Failed(String),
}

impl From<tonic::Status> for Failure {
    fn from(status: tonic::Status) -> Self {
        let message = status.message().to_owned();
        match status.code() {
            Code::NotFound => Self::NotFound,
            Code::ResourceExhausted => Self::TooMany,
            Code::FailedPrecondition => Self::NotRunning,
            Code::Unauthenticated | Code::PermissionDenied => Self::Denied(message),
            Code::Unavailable | Code::DeadlineExceeded | Code::Cancelled => {
                Self::Unreachable(message)
            }
            // What the transport failed with, such as a connection refused.
            Code::Unknown if std::error::Error::source(&status).is_some() => {
                Self::Unreachable(message)
            }
            Code::Unimplemented => Self::Failed(
                "the gateway does not offer this over gRPC; it may need upgrading".to_owned(),
            ),
            code => Self::Failed(format!("{code:?}: {message}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case::wss("wss://gw.example.com/agents/claude/acp", Some(("https://gw.example.com", "claude")))]
    #[case::ws_port("ws://localhost:8080/agents/a/acp", Some(("http://localhost:8080", "a")))]
    #[case::prefix("https://example.com/ukai/agents/a/acp/", Some(("https://example.com/ukai", "a")))]
    #[case::query("wss://gw/agents/a/acp?x=1", Some(("https://gw", "a")))]
    #[case::not_acp("wss://gw/agents/a/other", None)]
    #[case::no_agent("wss://gw/acp", None)]
    #[case::nested("wss://gw/agents/a/b/acp", None)]
    #[case::scheme("ftp://gw/agents/a/acp", None)]
    fn finds_the_gateway_and_agent(#[case] url: &str, #[case] expected: Option<(&str, &str)>) {
        assert_eq!(
            agent_root(url),
            expected.map(|(root, agent)| (root.to_owned(), agent.to_owned()))
        );
    }

    #[rstest]
    #[case::agents_default(None, WorkspaceScope::Shared, gw::WorkspaceScope::Shared, "")]
    #[case::agents_pod_default(None, WorkspaceScope::Pod, gw::WorkspaceScope::Pod, "")]
    #[case::panes_own(
        Some(WorkspaceScope::Pod),
        WorkspaceScope::Shared,
        gw::WorkspaceScope::Pod,
        "k1"
    )]
    #[case::pane_shares(
        Some(WorkspaceScope::Shared),
        WorkspaceScope::Pod,
        gw::WorkspaceScope::Shared,
        ""
    )]
    fn a_pane_names_its_workspace(
        #[case] pane: Option<WorkspaceScope>,
        #[case] default: WorkspaceScope,
        #[case] scope: gw::WorkspaceScope,
        #[case] key: &str,
    ) {
        let pane = pane.map(|scope| PaneWorkspace {
            key: "k1".into(),
            scope,
        });
        let workspace = workspace(pane.as_ref(), default);
        assert_eq!((workspace.scope(), workspace.key.as_str()), (scope, key));
    }
}
