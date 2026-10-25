//! The gateway's gRPC services (`ukai.gateway.v1`) as its clients reach
//! them: gRPC-Web over the reqwest client each endpoint already has, so the
//! agent's headers, TLS, and timeouts apply as they do to its other
//! requests, and so does any HTTP/1.1 proxy in front of the gateway, such as
//! IAP, which might not pass gRPC's HTTP/2. Unary calls only.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use http_body_util::BodyExt;
use tonic::codegen::http;
use tonic_web::{GrpcWebCall, GrpcWebClientService};

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
}
