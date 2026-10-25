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
//!
//! Both want the relay's token, like everything but `/health`.

use std::path::PathBuf;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use ukai_acp::checkout::{self, DIFF_PATH, DiffQuery, FILES_PATH, RUN_PATH, RunRequest};

/// The relay's working directory: the checkout the agent works in, and the
/// file the agent's review of it is kept in.
#[derive(Debug, Clone)]
struct Root(PathBuf, PathBuf);

/// `GET /files`, `POST /run`, and `GET /diff` for the checkout at `root`,
/// whose review is kept in `review`.
pub fn router(root: PathBuf, review: PathBuf) -> Router {
    Router::new()
        .route(FILES_PATH, get(files))
        .route(RUN_PATH, post(run))
        .route(DIFF_PATH, get(diff))
        .with_state(Root(root, review))
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
        let response = router(root.path().to_owned(), root.path().join("review.json"))
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
