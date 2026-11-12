//! The checkout the agent works in, for the user rather than the agent: its
//! files, to mention one in a prompt, and running a command in it. The
//! client cannot reach either itself, the checkout being in a pod, so the
//! relay serves them next to `/acp`, and `ukai-gateway` passes them on per
//! workspace (`ukai.gateway.v1.Files` and `Commands`):
//!
//! - `GET /files` … the checkout's files, as git would see them
//!   ([`ukai_acp::checkout::list_files`]).
//! - `POST /run` … a command (`{"command": "…"}`) run with the shell in the
//!   checkout, answered once it ends with its output and exit code
//!   ([`ukai_acp::checkout::run`]). It runs as the agent does, with nothing
//!   the agent could not do itself.
//! - `GET /diff?base=branch|uncommitted` … the checkout's changes
//!   ([`ukai_acp::checkout::diff`]), with the agent's review of them when it
//!   wrote one (`crate::review`); 422 with what is wrong when there are none
//!   to show, as in a checkout that is not a git repository.
//! - `POST /upload?name=…` … a file the user attached to a prompt, the body,
//!   kept outside the checkout ([`ukai_acp::checkout::store_upload`]) and
//!   answered with its path, which the prompt then links: the agent cannot
//!   read the user's machine. 422 for one without a name, 413 for one over
//!   [`ukai_acp::checkout::MAX_UPLOAD_BYTES`].
//!
//! All want the relay's token, like everything but `/health`.

use std::path::PathBuf;

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use ukai_acp::checkout::{
    self, DIFF_PATH, DiffQuery, FILES_PATH, MAX_UPLOAD_BYTES, RUN_PATH, RunRequest, UPLOAD_PATH,
    UploadQuery, Uploaded,
};

/// The relay's working directory: the checkout the agent works in, and the
/// file the agent's review of it is kept in.
#[derive(Debug, Clone)]
struct Root(PathBuf, PathBuf);

/// The directory the user's attachments are kept in.
#[derive(Debug, Clone)]
struct Uploads(PathBuf);

/// `GET /files`, `POST /run`, and `GET /diff` for the checkout at `root`,
/// whose review is kept in `review`, and `POST /upload` into `uploads`.
pub fn router(root: PathBuf, review: PathBuf, uploads: PathBuf) -> Router {
    Router::new()
        .route(FILES_PATH, get(files))
        .route(RUN_PATH, post(run))
        .route(DIFF_PATH, get(diff))
        .with_state(Root(root, review))
        .merge(
            Router::new()
                .route(UPLOAD_PATH, post(upload))
                .layer(DefaultBodyLimit::max(MAX_UPLOAD_BYTES))
                .with_state(Uploads(uploads)),
        )
}

async fn upload(
    State(Uploads(dir)): State<Uploads>,
    Query(query): Query<UploadQuery>,
    body: Bytes,
) -> Response {
    tracing::info!(bytes = body.len(), "keeping a file the user attached");
    let stored =
        tokio::task::spawn_blocking(move || checkout::store_upload(&dir, &query.name, &body)).await;
    match stored {
        Ok(Ok(path)) => Json(Uploaded {
            path: path.to_string_lossy().into_owned(),
        })
        .into_response(),
        Ok(Err(error)) if error.kind() == std::io::ErrorKind::InvalidInput => {
            (StatusCode::UNPROCESSABLE_ENTITY, error.to_string()).into_response()
        }
        Ok(Err(error)) => {
            tracing::warn!(%error, "cannot keep the upload");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
        Err(error) => {
            tracing::warn!(%error, "cannot keep the upload");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

async fn files(State(Root(root, _)): State<Root>) -> Response {
    match tokio::task::spawn_blocking(move || checkout::list_files(&root)).await {
        Ok(listing) => Json(listing).into_response(),
        Err(error) => {
            tracing::warn!(%error, "cannot list the checkout");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

async fn diff(State(Root(root, review)): State<Root>, Query(query): Query<DiffQuery>) -> Response {
    match checkout::diff(&root, query.base).await {
        Ok(mut diff) => {
            diff.review = crate::review::load(&review);
            Json(diff).into_response()
        }
        Err(problem) => (StatusCode::UNPROCESSABLE_ENTITY, problem).into_response(),
    }
}

async fn run(State(Root(root, _)): State<Root>, Json(request): Json<RunRequest>) -> Response {
    tracing::info!("running a command for the user");
    match checkout::run(&root, &request.command).await {
        Ok(output) => Json(output).into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("cannot run the command: {error}"),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::{Request, header};
    use tower::ServiceExt;
    use ukai_acp::checkout::{CommandOutput, FileListing};

    use super::*;

    fn checkout() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
        std::fs::write(root.path().join("a.txt"), "hello").unwrap_or_else(|e| panic!("write: {e}"));
        root
    }

    async fn send(root: &tempfile::TempDir, request: Request<Body>) -> (StatusCode, Vec<u8>) {
        let response = router(
            root.path().to_owned(),
            root.path().join("review.json"),
            root.path().join("uploads"),
        )
        .oneshot(request)
        .await
        .unwrap_or_else(|e| panic!("request: {e}"));
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap_or_else(|e| panic!("body: {e}"));
        (status, body.to_vec())
    }

    #[tokio::test]
    async fn lists_the_checkouts_files() {
        let root = checkout();
        let request = Request::get(FILES_PATH)
            .body(Body::empty())
            .unwrap_or_else(|e| panic!("{e}"));
        let (status, body) = send(&root, request).await;
        assert_eq!(status, StatusCode::OK);
        let listing: FileListing =
            serde_json::from_slice(&body).unwrap_or_else(|e| panic!("listing: {e}"));
        assert_eq!(listing.files, ["a.txt"]);
    }

    #[tokio::test]
    async fn says_why_a_checkout_has_no_diff() {
        // A temporary directory is no git repository.
        let root = checkout();
        let request = Request::get(format!("{DIFF_PATH}?base=uncommitted"))
            .body(Body::empty())
            .unwrap_or_else(|e| panic!("{e}"));
        let (status, body) = send(&root, request).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(
            String::from_utf8_lossy(&body),
            "this checkout is not a git repository"
        );
    }

    #[tokio::test]
    async fn keeps_an_upload_outside_the_checkout_listing() {
        let root = checkout();
        let request = Request::post(format!("{UPLOAD_PATH}?name=my%20paper.pdf"))
            .body(Body::from(&b"%PDF"[..]))
            .unwrap_or_else(|e| panic!("{e}"));
        let (status, body) = send(&root, request).await;
        assert_eq!(status, StatusCode::OK);
        let uploaded: Uploaded =
            serde_json::from_slice(&body).unwrap_or_else(|e| panic!("uploaded: {e}"));
        let path = PathBuf::from(uploaded.path);
        assert!(path.starts_with(root.path().join("uploads")));
        assert!(path.ends_with("my paper.pdf"));
        assert_eq!(std::fs::read(&path).ok().as_deref(), Some(&b"%PDF"[..]));
    }

    #[rstest::rstest]
    #[case::no_name("?name=..", vec![1], StatusCode::UNPROCESSABLE_ENTITY)]
    #[case::too_large("?name=a.bin", vec![0; MAX_UPLOAD_BYTES + 1], StatusCode::PAYLOAD_TOO_LARGE)]
    #[tokio::test]
    async fn refuses_an_upload(
        #[case] query: &str,
        #[case] body: Vec<u8>,
        #[case] expected: StatusCode,
    ) {
        let root = checkout();
        let request = Request::post(format!("{UPLOAD_PATH}{query}"))
            .body(Body::from(body))
            .unwrap_or_else(|e| panic!("{e}"));
        let (status, _) = send(&root, request).await;
        assert_eq!(status, expected);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn runs_a_command_in_the_checkout() {
        let root = checkout();
        let request = Request::post(RUN_PATH)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(r#"{"command":"cat a.txt"}"#))
            .unwrap_or_else(|e| panic!("{e}"));
        let (status, body) = send(&root, request).await;
        assert_eq!(status, StatusCode::OK);
        let output: CommandOutput =
            serde_json::from_slice(&body).unwrap_or_else(|e| panic!("output: {e}"));
        assert_eq!(
            (output.exit_code, output.output.as_str()),
            (Some(0), "hello")
        );
    }
}
