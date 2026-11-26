//! An agent pod's client of the gateway's GitHub App (see "GitHub App" in
//! `docs/design.md`): [`TokenEndpoint`] asks `ukai.gateway.v1.WorkspaceGitHub`
//! for an installation token for the pod's own repository, with the relay's
//! token. The relay answers git's credential helper with it and keeps `gh`'s
//! configuration holding a fresh one.

use std::time::Duration;

use crate::ResolveError;

/// An installation token for the pod's repository.
#[derive(Clone, PartialEq, Eq)]
pub struct GitHubToken {
    pub token: String,
    /// In seconds since the Unix epoch.
    pub expires_at: i64,
    /// Where git reaches the repository, e.g. `github.com`.
    pub host: String,
    /// `owner/name`.
    pub repository: String,
}

impl std::fmt::Debug for GitHubToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GitHubToken")
            .field("token", &"<redacted>")
            .field("expires_at", &self.expires_at)
            .field("host", &self.host)
            .field("repository", &self.repository)
            .finish()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TokenError {
    /// The gateway has no App, or the pod's agent does not use it.
    #[error("{0}")]
    NotOffered(String),
    #[error("the gateway's GitHub App failed: {0}")]
    Failed(String),
}

/// The gateway's `WorkspaceGitHub`, as an agent pod asks it.
#[derive(Debug, Clone)]
pub struct TokenEndpoint {
    channel: crate::grpc::Channel,
}

const TIMEOUT: Duration = Duration::from_secs(30);

impl TokenEndpoint {
    /// The gateway at `gateway` (e.g. `http://ukai-gateway`), with the
    /// relay's `token`.
    pub fn for_workspace(gateway: &str, token: &str) -> Result<Self, ResolveError> {
        let (channel, _) = crate::grpc::pod_channel(gateway, token, TIMEOUT)?;
        Ok(Self { channel })
    }

    pub async fn token(&self) -> Result<GitHubToken, TokenError> {
        use ukai_proto::gateway::v1 as gw;
        use ukai_proto::gateway::v1::workspace_git_hub_client::WorkspaceGitHubClient;
        let token = WorkspaceGitHubClient::new(self.channel.clone())
            .token(gw::WorkspaceTokenRequest {})
            .await
            .map_err(|status| match status.code() {
                tonic::Code::FailedPrecondition | tonic::Code::Unimplemented => {
                    TokenError::NotOffered(status.message().to_owned())
                }
                _ => TokenError::Failed(status.message().to_owned()),
            })?
            .into_inner();
        Ok(GitHubToken {
            token: token.token,
            expires_at: token.expires_at,
            host: token.host,
            repository: token.repository,
        })
    }
}
