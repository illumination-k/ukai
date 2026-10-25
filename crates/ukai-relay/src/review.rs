//! The agent's review of its checkout's changes ([`ukai_acp::review`]),
//! kept in a file of the relay's own next to the previews, so the checkout
//! stays clean. The MCP server writes it (`review_diff`, `reply_annotation`),
//! `GET /diff` sends it along with the diff (`crate::checkout`), and
//! `POST /review` takes the user's replies and resolves:
//!
//! - `POST /review` … an [`AnnotateRequest`] as JSON, answered with the
//!   review as it is now; 422 with what is wrong, one problem per line.
//!
//! It lasts as long as the relay, as the pod's other state does. Each write
//! replaces the file whole, so a reader never sees half of one.

use std::io;
use std::path::{Path, PathBuf};

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use ukai_acp::review::{AnnotateRequest, Author, REVIEW_PATH, Review};
use ukai_core::annotation;

/// The review in `file`; `None` when the agent has written none, or it
/// cannot be read.
pub fn load(file: &Path) -> Option<Review> {
    let bytes = std::fs::read(file).ok()?;
    serde_json::from_slice(&bytes)
        .inspect_err(|error| tracing::warn!(%error, "ignoring an unreadable review"))
        .ok()
}

/// Replace the review in `file` with `review`.
pub fn save(file: &Path, review: &Review) -> io::Result<()> {
    let json = serde_json::to_vec(review).map_err(io::Error::other)?;
    let mut name = file.file_name().unwrap_or_default().to_os_string();
    name.push(".tmp");
    let tmp = file.with_file_name(name);
    std::fs::write(&tmp, json)?;
    std::fs::rename(tmp, file)
}

/// Seconds since the epoch.
pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| {
            i64::try_from(since.as_secs()).unwrap_or(i64::MAX)
        })
}

/// `POST /review` for the review kept in `file`.
pub fn router(file: PathBuf) -> Router {
    Router::new()
        .route(REVIEW_PATH, post(annotate))
        .with_state(file)
}

async fn annotate(State(file): State<PathBuf>, Json(request): Json<AnnotateRequest>) -> Response {
    let refused = |text: String| (StatusCode::UNPROCESSABLE_ENTITY, text).into_response();
    let Some(mut review) = load(&file) else {
        return refused("the agent has not reviewed its changes".to_owned());
    };
    if let Err(problems) = annotation::respond(
        &mut review,
        &request.id,
        Author::User,
        request.reply.as_deref(),
        request.resolved,
        now(),
    ) {
        let lines: Vec<String> = problems.iter().map(ToString::to_string).collect();
        return refused(lines.join("\n"));
    }
    match save(&file, &review) {
        Ok(()) => Json(review).into_response(),
        Err(error) => {
            tracing::warn!(%error, "cannot keep the review");
            (StatusCode::INTERNAL_SERVER_ERROR, "cannot keep the review").into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::{Request, header};
    use tower::ServiceExt;
    use ukai_acp::review::Annotation;

    use super::*;

    fn kept(dir: &tempfile::TempDir) -> PathBuf {
        let file = dir.path().join("review.json");
        let review = Review {
            summary: "Moves the parser.".into(),
            annotations: vec![Annotation {
                id: "a1".into(),
                path: "src/lib.rs".into(),
                line: Some(3),
                body: "Unwraps.".into(),
                ..Annotation::default()
            }],
            updated: 1,
        };
        save(&file, &review).unwrap_or_else(|e| panic!("save: {e}"));
        file
    }

    async fn post_json(file: PathBuf, body: &str) -> (StatusCode, Vec<u8>) {
        let request = Request::post(REVIEW_PATH)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_owned()))
            .unwrap_or_else(|e| panic!("{e}"));
        let response = router(file)
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
    async fn the_user_replies_and_resolves() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
        let file = kept(&dir);
        let (status, body) = post_json(
            file.clone(),
            r#"{"id":"a1","reply":"Why?","resolved":true}"#,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let answered: Review =
            serde_json::from_slice(&body).unwrap_or_else(|e| panic!("review: {e}"));
        assert_eq!(answered.annotations[0].replies[0].body, "Why?");
        assert_eq!(answered.annotations[0].replies[0].by, Author::User);
        assert!(answered.annotations[0].resolved);
        // What was answered is what is kept.
        assert_eq!(load(&file), Some(answered));
    }

    #[tokio::test]
    async fn says_what_is_wrong() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
        let file = kept(&dir);
        let (status, body) = post_json(file, r#"{"id":"a9","reply":" "}"#).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(
            String::from_utf8_lossy(&body),
            "the reply is empty\nthere is no annotation `a9`"
        );
        let none = dir.path().join("none.json");
        let (status, body) = post_json(none, r#"{"id":"a1","resolved":true}"#).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(
            String::from_utf8_lossy(&body),
            "the agent has not reviewed its changes"
        );
    }
}
