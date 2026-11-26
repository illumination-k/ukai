use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::Json;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use base64::Engine;
use jsonwebtoken::{DecodingKey, Validation};
use serde_json::{Value, json};

use super::*;

const APP_ID: &str = "123456";

/// A fresh RSA key: the App's private key in PEM, and the public key GitHub
/// checks the JWT with.
fn key() -> (String, DecodingKey) {
    use aws_lc_rs::encoding::AsDer;
    use aws_lc_rs::rsa::{KeyPair, KeySize};
    use aws_lc_rs::signature::KeyPair as _;
    let pair = KeyPair::generate(KeySize::Rsa2048).unwrap_or_else(|e| panic!("{e}"));
    let der = pair.as_der().unwrap_or_else(|e| panic!("{e}"));
    let body = base64::engine::general_purpose::STANDARD.encode(der.as_ref());
    let lines: Vec<&str> = body
        .as_bytes()
        .chunks(64)
        .map(|line| std::str::from_utf8(line).unwrap_or_default())
        .collect();
    let pem = format!(
        "-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----\n",
        lines.join("\n")
    );
    (pem, DecodingKey::from_rsa_der(pair.public_key().as_ref()))
}

fn repository() -> GitHubRepository {
    GitHubRepository {
        host: "github.com".into(),
        owner: "o".into(),
        name: "r".into(),
    }
}

fn permissions() -> BTreeMap<String, String> {
    BTreeMap::from([("contents".to_owned(), "write".to_owned())])
}

/// GitHub as the App sees it, checking each JWT.
struct Fake {
    key: DecodingKey,
    installed: bool,
    expires_at: &'static str,
    minted: AtomicUsize,
    asked: Mutex<Vec<Value>>,
}

impl Fake {
    /// The refusal of a request whose JWT is not the App's.
    fn refuse(&self, headers: &HeaderMap) -> Option<Response> {
        let Some(jwt) = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
        else {
            return Some(StatusCode::UNAUTHORIZED.into_response());
        };
        let mut validation = Validation::new(Algorithm::RS256);
        validation.set_issuer(&[APP_ID]);
        validation.set_required_spec_claims(&["exp", "iat", "iss"]);
        jsonwebtoken::decode::<Claims>(jwt, &self.key, &validation)
            .err()
            .map(|_| {
                (
                    StatusCode::UNAUTHORIZED,
                    Json(json!({ "message": "A JSON web token could not be decoded" })),
                )
                    .into_response()
            })
    }
}

async fn installation(
    State(fake): State<Arc<Fake>>,
    Path((owner, name)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    if let Some(refused) = fake.refuse(&headers) {
        return refused;
    }
    if !fake.installed || (owner, name) != ("o".to_owned(), "r".to_owned()) {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "message": "Not Found" })),
        )
            .into_response();
    }
    Json(json!({ "id": 42 })).into_response()
}

async fn access_tokens(
    State(fake): State<Arc<Fake>>,
    Path(id): Path<u64>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    if let Some(refused) = fake.refuse(&headers) {
        return refused;
    }
    assert_eq!(id, 42);
    fake.asked
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(body);
    let n = fake.minted.fetch_add(1, Ordering::SeqCst);
    (
        StatusCode::CREATED,
        Json(json!({ "token": format!("ghs_{n}"), "expires_at": fake.expires_at })),
    )
        .into_response()
}

/// The App, signing with a fresh key, against a fake GitHub.
async fn app(installed: bool, expires_at: &'static str) -> (GitHubApp, Arc<Fake>) {
    let (pem, key) = key();
    let fake = Arc::new(Fake {
        key,
        installed,
        expires_at,
        minted: AtomicUsize::new(0),
        asked: Mutex::default(),
    });
    let router = axum::Router::new()
        .route("/repos/{owner}/{name}/installation", get(installation))
        .route("/app/installations/{id}/access_tokens", post(access_tokens))
        .with_state(fake.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    let address = listener.local_addr().unwrap_or_else(|e| panic!("{e}"));
    tokio::spawn(async move { axum::serve(listener, router).await });
    let app = GitHubApp::new(APP_ID, &pem, &format!("http://{address}/"))
        .unwrap_or_else(|e| panic!("{e}"));
    (app, fake)
}

#[test]
fn the_jwt_is_backdated_and_lives_under_ten_minutes() {
    let claims = claims(APP_ID, 1_000_000);
    assert_eq!(claims.iss, APP_ID);
    assert!(claims.iat < 1_000_000);
    assert!(claims.exp - claims.iat <= 10 * 60);
}

#[test]
fn a_token_is_kept_until_shortly_before_it_expires() {
    let minted = |expires_at| Minted {
        token: "t".into(),
        expires_at,
    };
    let margin = secs(REFRESH_MARGIN);
    assert!(minted(1000 + margin + 1).fresh(1000));
    assert!(!minted(1000 + margin).fresh(1000));
    assert!(!minted(0).fresh(1000));
}

#[test]
fn a_token_is_never_printed() {
    let minted = Minted {
        token: "ghs_secret".into(),
        expires_at: 1,
    };
    assert!(!format!("{minted:?}").contains("ghs_secret"));
}

#[test]
fn refuses_a_key_that_is_not_rsa_pem() {
    assert!(GitHubApp::new(APP_ID, "not a key", DEFAULT_API).is_err());
    assert!(GitHubApp::new(" ", &key().0, DEFAULT_API).is_err());
}

#[tokio::test]
async fn mints_one_token_for_the_repository_and_keeps_it() {
    let (app, fake) = app(true, "2999-01-01T00:00:00Z").await;
    let first = app.token(&repository(), &permissions()).await;
    let second = app.token(&repository(), &permissions()).await;
    assert_eq!(
        first,
        Ok(Minted {
            token: "ghs_0".into(),
            expires_at: 32_472_144_000,
        })
    );
    assert_eq!(second, first);
    assert_eq!(fake.minted.load(Ordering::SeqCst), 1);
    // Only this repository, with only the agent's permissions.
    let asked = fake.asked.lock().unwrap_or_else(|e| e.into_inner()).clone();
    assert_eq!(
        asked,
        [json!({ "repositories": ["r"], "permissions": { "contents": "write" } })]
    );
}

#[tokio::test]
async fn mints_again_once_the_token_is_about_to_expire() {
    let (app, fake) = app(true, "2000-01-01T00:00:00Z").await;
    let _ = app.token(&repository(), &permissions()).await;
    let again = app.token(&repository(), &permissions()).await;
    assert_eq!(again.map(|minted| minted.token), Ok("ghs_1".to_owned()));
    assert_eq!(fake.minted.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn other_permissions_get_a_token_of_their_own() {
    let (app, fake) = app(true, "2999-01-01T00:00:00Z").await;
    let _ = app.token(&repository(), &permissions()).await;
    let read = BTreeMap::from([("contents".to_owned(), "read".to_owned())]);
    let other = app.token(&repository(), &read).await;
    assert_eq!(other.map(|minted| minted.token), Ok("ghs_1".to_owned()));
    assert_eq!(fake.minted.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn says_when_the_app_is_not_installed() {
    let (app, _) = app(false, "2999-01-01T00:00:00Z").await;
    assert_eq!(
        app.token(&repository(), &permissions()).await,
        Err(GitHubError::NotInstalled("o/r".into()))
    );
}

#[tokio::test]
async fn passes_on_why_github_refused() {
    let (_, fake) = app(true, "2999-01-01T00:00:00Z").await;
    // Signed with another key than the one GitHub has.
    let (other, _) = key();
    let stranger =
        GitHubApp::new(APP_ID, &other, &fake_root(&fake).await).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(
        stranger.token(&repository(), &permissions()).await,
        Err(GitHubError::Refused {
            status: 401,
            message: "A JSON web token could not be decoded".into(),
        })
    );
}

/// A second server on the same fake, for an App with another key.
async fn fake_root(fake: &Arc<Fake>) -> String {
    let router = axum::Router::new()
        .route("/repos/{owner}/{name}/installation", get(installation))
        .with_state(fake.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    let address = listener.local_addr().unwrap_or_else(|e| panic!("{e}"));
    tokio::spawn(async move { axum::serve(listener, router).await });
    format!("http://{address}")
}
