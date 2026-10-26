//! The gateway's gRPC services (`ukai.gateway.v1`) as its clients reach
//! them: gRPC-Web over the reqwest client each endpoint already has, so the
//! agent's headers, TLS, and timeouts apply as they do to its other
//! requests, and so does any HTTP/1.1 proxy in front of the gateway, such as
//! IAP, which might not pass gRPC's HTTP/2. Unary calls only.

use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use http_body_util::BodyExt;
use tonic::Code;
use tonic::codegen::{Bytes, http};
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
            // So is its answer, with the trailers after it.
            let (parts, body) = http::Response::from(response).into_parts();
            let body = body.collect().await?.to_bytes();
            Ok(http::Response::from_parts(parts, apart(body)))
        })
    }
}

/// A gRPC-Web answer with its trailers as a chunk of their own. tonic-web
/// 0.14 drops trailers that arrive in the same chunk as a message, and tonic
/// then fails the call for want of a status, as it would a truncated one; a
/// proxy may well pass a small answer in one chunk.
fn apart(body: Bytes) -> reqwest::Body {
    match trailers_at(&body) {
        Some(at) if at > 0 => {
            let chunks = [Ok::<_, Infallible>(body.slice(..at)), Ok(body.slice(at..))];
            reqwest::Body::wrap_stream(futures::stream::iter(chunks))
        }
        _ => reqwest::Body::from(body),
    }
}

/// Where the trailers frame of a gRPC-Web body starts, past its messages,
/// each a flag byte, a big-endian `u32` length, and that many bytes.
fn trailers_at(body: &[u8]) -> Option<usize> {
    const TRAILERS: u8 = 0x80;
    let mut at = 0;
    while let Some(flag) = body.get(at) {
        if flag & TRAILERS != 0 {
            return Some(at);
        }
        let len = body.get(at + 1..at + 5)?.try_into().ok()?;
        at = at
            .checked_add(5)?
            .checked_add(u32::from_be_bytes(len) as usize)?;
    }
    None
}

/// A gateway agent's `/acp` URL as the gateway's root and the agent's id:
/// `wss://gw.example.com/agents/claude/acp` is `https://gw.example.com` and
/// `claude`, where the gRPC services are `<root>/<service>/<method>`. The
/// URL is read as for the agent's other endpoints
/// (`crate::reports::endpoint_base`), and the id decoded as the gateway
/// decodes it in a path.
pub fn agent_root(url: &str) -> Option<(String, String)> {
    let agent = crate::reports::endpoint_base(url, "")?;
    let (root, id) = agent.rsplit_once("/agents/")?;
    let id = percent_encoding::percent_decode_str(id)
        .decode_utf8()
        .ok()?;
    let has_root = root
        .split_once("://")
        .is_some_and(|(_, host)| !host.is_empty());
    (has_root && !id.is_empty() && !id.contains('/')).then(|| (root.to_owned(), id.into_owned()))
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
    use tonic_web::GrpcWebClientService;
    use ukai_proto::gateway::v1::ports_client::PortsClient;

    use super::*;

    const EMPTY_MESSAGE: &[u8] = b"\0\0\0\0\0";
    const OK: &[u8] = b"\x80\0\0\0\x0fgrpc-status:0\r\n";

    #[rstest]
    #[case::message_then_trailers(&[EMPTY_MESSAGE, OK], Some(5))]
    #[case::two_messages(&[&b"\0\0\0\0\x01x"[..], EMPTY_MESSAGE, OK], Some(11))]
    #[case::trailers_only(&[OK], Some(0))]
    #[case::no_trailers(&[EMPTY_MESSAGE], None)]
    #[case::cut_short(&[&b"\0\0\0\0\x09abc"[..]], None)]
    #[case::empty(&[], None)]
    fn finds_the_trailers(#[case] frames: &[&[u8]], #[case] expected: Option<usize>) {
        assert_eq!(trailers_at(&frames.concat()), expected);
    }

    /// What a proxy may do with a small answer: send the message and its
    /// trailers in one chunk.
    #[tokio::test]
    async fn reads_a_message_and_its_trailers_sent_together() {
        let gateway = tower::service_fn(|_: http::Request<_>| async {
            let body = Bytes::from([EMPTY_MESSAGE, OK].concat());
            Ok::<_, Infallible>(
                http::Response::builder()
                    .header("content-type", "application/grpc-web+proto")
                    .body(apart(body))
                    .unwrap_or_else(|e| panic!("{e}")),
            )
        });
        let listing = PortsClient::new(GrpcWebClientService::new(gateway))
            .list(gw::ListPortsRequest::default())
            .await
            .map(tonic::Response::into_inner)
            .map_err(|status| status.to_string());
        assert_eq!(listing, Ok(gw::PortListing::default()));
    }

    #[rstest]
    #[case::wss("wss://gw.example.com/agents/claude/acp", Some(("https://gw.example.com", "claude")))]
    #[case::ws_port("ws://localhost:8080/agents/a/acp", Some(("http://localhost:8080", "a")))]
    #[case::prefix("https://example.com/ukai/agents/a/acp/", Some(("https://example.com/ukai", "a")))]
    #[case::query("wss://gw/agents/a/acp?x=1", Some(("https://gw", "a")))]
    #[case::not_acp("wss://gw/agents/a/other", None)]
    #[case::encoded("wss://gw/agents/my%20agent/acp", Some(("https://gw", "my agent")))]
    #[case::encoded_slash("wss://gw/agents/a%2Fb/acp", None)]
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
