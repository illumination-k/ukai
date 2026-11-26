//! The gateway's GitHub App (see "GitHub App" in `docs/design.md`): it signs
//! a JWT with the App's private key, finds the App's installation on an
//! agent's repository, and mints installation tokens limited to that one
//! repository and the agent's `workspace.github_permissions`. Agent pods ask
//! for theirs at `ukai.gateway.v1.WorkspaceGitHub` with their relay's token,
//! whenever git or `gh` needs one, so a workspace never holds a credential
//! longer-lived than an hour, nor one for any other repository.
//!
//! Tokens are kept until [`REFRESH_MARGIN`] before they expire, per
//! repository and permissions, so the pods of one repository share one.
//! Neither the key nor a token is ever logged.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use jsonwebtoken::{Algorithm, EncodingKey, Header};
use serde::{Deserialize, Serialize};
use tonic::{Request, Response, Status};
use ukai_proto::gateway::v1 as gw;

use crate::Gateway;
use crate::config::GitHubRepository;

/// GitHub's API, unless `UKAI_GITHUB_API_URL` names a GitHub Enterprise
/// Server's (`https://<host>/api/v3`).
pub const DEFAULT_API: &str = "https://api.github.com";

/// A token this close to expiring is minted again rather than handed out,
/// so whoever asks has it for at least this long.
pub const REFRESH_MARGIN: Duration = Duration::from_secs(10 * 60);

/// GitHub refuses App JWTs that live longer than ten minutes, and allows a
/// minute of clock drift; the JWT is backdated by that.
const JWT_LIFETIME: Duration = Duration::from_secs(9 * 60);
const CLOCK_DRIFT: Duration = Duration::from_secs(60);

const TIMEOUT: Duration = Duration::from_secs(20);

/// An installation token and when it expires, in seconds since the Unix
/// epoch.
#[derive(Clone, PartialEq, Eq)]
pub struct Minted {
    pub token: String,
    pub expires_at: i64,
}

impl std::fmt::Debug for Minted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Minted")
            .field("token", &"<redacted>")
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

impl Minted {
    /// Whether it is still good for [`REFRESH_MARGIN`] at `now`.
    pub fn fresh(&self, now: i64) -> bool {
        self.expires_at.saturating_sub(now) > secs(REFRESH_MARGIN)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GitHubError {
    #[error("the GitHub App is not installed on {0}")]
    NotInstalled(String),
    #[error("GitHub refused the App ({status}): {message}")]
    Refused { status: u16, message: String },
    #[error("cannot reach GitHub: {0}")]
    Unreachable(String),
    #[error("cannot sign the App's JWT: {0}")]
    Key(String),
}

impl From<GitHubError> for Status {
    fn from(error: GitHubError) -> Self {
        match &error {
            GitHubError::NotInstalled(_) | GitHubError::Refused { .. } => {
                Self::failed_precondition(error.to_string())
            }
            GitHubError::Unreachable(_) => Self::unavailable(error.to_string()),
            GitHubError::Key(_) => Self::internal(error.to_string()),
        }
    }
}

/// What a token is minted for: one repository, with these permissions.
type Wanted = (GitHubRepository, BTreeMap<String, String>);

/// The App, as `UKAI_GITHUB_APP_ID` and `UKAI_GITHUB_APP_PRIVATE_KEY` give it.
pub struct GitHubApp {
    app_id: String,
    key: EncodingKey,
    /// The API's root, without a trailing `/`.
    api: String,
    http: reqwest::Client,
    /// Held while minting, so two pods asking at once mint one token.
    tokens: tokio::sync::Mutex<HashMap<Wanted, Minted>>,
}

/// The claims of an App's JWT.
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Claims {
    pub iat: i64,
    pub exp: i64,
    pub iss: String,
}

/// The claims of a JWT for `app_id` signed at `now`.
pub fn claims(app_id: &str, now: i64) -> Claims {
    Claims {
        iat: now - secs(CLOCK_DRIFT),
        exp: now + secs(JWT_LIFETIME),
        iss: app_id.to_owned(),
    }
}

#[derive(Deserialize)]
struct Installation {
    id: u64,
}

#[derive(Serialize)]
struct AccessTokenRequest<'a> {
    repositories: [&'a str; 1],
    permissions: &'a BTreeMap<String, String>,
}

#[derive(Deserialize)]
struct AccessToken {
    token: String,
    expires_at: String,
}

#[derive(Deserialize)]
struct ErrorBody {
    message: String,
}

impl GitHubApp {
    /// The App `app_id` with the PEM `private_key`, at `api` (the root of
    /// GitHub's REST API).
    pub fn new(app_id: &str, private_key: &str, api: &str) -> Result<Self, String> {
        if app_id.trim().is_empty() {
            return Err("the App id is empty".to_owned());
        }
        let key = EncodingKey::from_rsa_pem(private_key.as_bytes())
            .map_err(|_| "the App's private key is not an RSA key in PEM".to_owned())?;
        let http = reqwest::Client::builder()
            .timeout(TIMEOUT)
            .user_agent(concat!("ukai-gateway/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|error| error.to_string())?;
        Ok(Self {
            app_id: app_id.trim().to_owned(),
            key,
            api: api.trim_end_matches('/').to_owned(),
            http,
            tokens: tokio::sync::Mutex::default(),
        })
    }

    fn jwt(&self, now: i64) -> Result<String, GitHubError> {
        jsonwebtoken::encode(
            &Header::new(Algorithm::RS256),
            &claims(&self.app_id, now),
            &self.key,
        )
        .map_err(|error| GitHubError::Key(error.to_string()))
    }

    /// A token for `repository` with `permissions`, the one kept if it is
    /// still [`Minted::fresh`].
    pub async fn token(
        &self,
        repository: &GitHubRepository,
        permissions: &BTreeMap<String, String>,
    ) -> Result<Minted, GitHubError> {
        let wanted = (repository.clone(), permissions.clone());
        let mut tokens = self.tokens.lock().await;
        let now = unix_now();
        if let Some(minted) = tokens.get(&wanted).filter(|minted| minted.fresh(now)) {
            return Ok(minted.clone());
        }
        let minted = self.mint(repository, permissions, now).await?;
        tokens.retain(|_, kept| kept.fresh(now));
        tokens.insert(wanted, minted.clone());
        tracing::info!(
            repository = %repository.full_name(),
            expires_at = minted.expires_at,
            "minted a GitHub installation token"
        );
        Ok(minted)
    }

    async fn mint(
        &self,
        repository: &GitHubRepository,
        permissions: &BTreeMap<String, String>,
        now: i64,
    ) -> Result<Minted, GitHubError> {
        let jwt = self.jwt(now)?;
        let installation = self
            .http
            .get(format!(
                "{}/repos/{}/{}/installation",
                self.api, repository.owner, repository.name
            ))
            .bearer_auth(&jwt)
            .headers(api_headers())
            .send()
            .await
            .map_err(unreachable)?;
        if installation.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(GitHubError::NotInstalled(repository.full_name()));
        }
        let Installation { id } = answer(installation).await?;
        let response = self
            .http
            .post(format!("{}/app/installations/{id}/access_tokens", self.api))
            .bearer_auth(&jwt)
            .headers(api_headers())
            .json(&AccessTokenRequest {
                repositories: [&repository.name],
                permissions,
            })
            .send()
            .await
            .map_err(unreachable)?;
        let AccessToken { token, expires_at } = answer(response).await?;
        let expires_at = time::OffsetDateTime::parse(
            &expires_at,
            &time::format_description::well_known::Rfc3339,
        )
        .map_err(|_| GitHubError::Refused {
            status: 200,
            message: format!("a token expiring at `{expires_at}`"),
        })?
        .unix_timestamp();
        Ok(Minted { token, expires_at })
    }
}

fn api_headers() -> reqwest::header::HeaderMap {
    use reqwest::header::{ACCEPT, HeaderMap, HeaderValue};
    let mut headers = HeaderMap::new();
    headers.insert(
        ACCEPT,
        HeaderValue::from_static("application/vnd.github+json"),
    );
    headers.insert(
        "x-github-api-version",
        HeaderValue::from_static("2022-11-28"),
    );
    headers
}

fn unreachable(error: reqwest::Error) -> GitHubError {
    // Without the URL, which is ours, and with nothing of the request.
    GitHubError::Unreachable(error.without_url().to_string())
}

/// The body of a successful `response`, or GitHub's refusal.
async fn answer<T: serde::de::DeserializeOwned>(
    response: reqwest::Response,
) -> Result<T, GitHubError> {
    let status = response.status();
    if status.is_success() {
        return response.json().await.map_err(|error| GitHubError::Refused {
            status: status.as_u16(),
            message: format!("an answer that is not GitHub's: {}", error.without_url()),
        });
    }
    let message = response
        .json::<ErrorBody>()
        .await
        .map_or_else(|_| status.to_string(), |body| body.message);
    Err(GitHubError::Refused {
        status: status.as_u16(),
        message,
    })
}

fn secs(duration: Duration) -> i64 {
    i64::try_from(duration.as_secs()).unwrap_or(i64::MAX)
}

fn unix_now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, secs)
}

/// `ukai.gateway.v1.WorkspaceGitHub`: an agent pod, known by its relay's
/// token, asking for its own repository.
pub struct PodGitHub {
    pub gateway: Arc<Gateway>,
}

#[tonic::async_trait]
impl gw::workspace_git_hub_server::WorkspaceGitHub for PodGitHub {
    async fn token(
        &self,
        request: Request<gw::WorkspaceTokenRequest>,
    ) -> Result<Response<gw::GitHubToken>, Status> {
        let app = self.gateway.github.as_ref().ok_or_else(|| {
            Status::unavailable(
                "this gateway has no GitHub App: set UKAI_GITHUB_APP_ID and UKAI_GITHUB_APP_PRIVATE_KEY",
            )
        })?;
        let pod = crate::knowledge::pod_asking(&self.gateway, &request).await?;
        let agent = pod
            .agent
            .as_deref()
            .and_then(|id| self.gateway.agents.get(id))
            .ok_or_else(|| {
                Status::failed_precondition("the pod's agent is not in the gateway's config")
            })?;
        let repository = agent
            .workspace
            .github_repository()
            .filter(|_| agent.workspace.github_app)
            .ok_or_else(|| {
                Status::failed_precondition("the pod's agent does not use the GitHub App")
            })?;
        let minted = app
            .token(&repository, &agent.workspace.github_permissions())
            .await
            .inspect_err(|error| {
                tracing::warn!(repository = %repository.full_name(), %error, "cannot mint a GitHub token");
            })?;
        Ok(Response::new(gw::GitHubToken {
            token: minted.token,
            expires_at: minted.expires_at,
            host: repository.host.clone(),
            repository: repository.full_name(),
        }))
    }
}

#[cfg(test)]
mod tests;
