//! The runs of ayaito flows in the relay's checkout, for the user to watch.
//!
//! An agent starts a workflow of agents and code in its pod (through the
//! MCP server's `run_flow`, or `ayaito run` in its shell), and ayaito saves
//! each run as a folder outside the working tree (`ukai_acp::flows`). The
//! relay serves, next to `/acp`, and `ukai-gateway` passes on per workspace:
//!
//! - `GET /flows` … the runs kept for the checkout, the one written last
//!   first.
//! - `GET /flows/<run>?after=<bytes>` … the run's graph, and its events from
//!   that offset on.
//! - `POST /flows/<run>` … an [`Order`]: a message for the agent of a node,
//!   or stopping that agent or the run. It goes through ayaito's own
//!   command line.
//!
//! What is wrong with a request — no such run, a node that is not running —
//! is 422 with the reason as the body. All want the relay's token, like
//! everything but `/health`.

use std::path::PathBuf;

use axum::extract::{Path as UrlPath, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use ukai_acp::flows::{FLOWS_PATH, FlowError, Order, ReadQuery, Store};

/// `GET /flows`, and `GET` and `POST /flows/{run}`, for the checkout at
/// `root`.
pub fn router(root: PathBuf) -> Router {
    Router::new()
        .route(FLOWS_PATH, get(listing))
        .route(&format!("{FLOWS_PATH}/{{run}}"), get(read).post(order))
        .with_state(root)
}

fn refusal(error: FlowError) -> Response {
    match error {
        FlowError::Refused(why) => (StatusCode::UNPROCESSABLE_ENTITY, why).into_response(),
        other => {
            tracing::warn!(error = %other, "cannot serve the checkout's flows");
            (StatusCode::INTERNAL_SERVER_ERROR, other.to_string()).into_response()
        }
    }
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, FlowError> + Send + 'static,
) -> Result<T, FlowError> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|error| FlowError::Failed(error.to_string()))?
}

async fn listing(State(root): State<PathBuf>) -> Response {
    let store = Store::of(&root).await;
    match blocking(move || Ok(store.list())).await {
        Ok(listing) => Json(listing).into_response(),
        Err(error) => refusal(error),
    }
}

async fn read(
    State(root): State<PathBuf>,
    UrlPath(run): UrlPath<String>,
    Query(query): Query<ReadQuery>,
) -> Response {
    let store = Store::of(&root).await;
    match blocking(move || store.read(&run, query.after)).await {
        Ok(chunk) => Json(chunk).into_response(),
        Err(error) => refusal(error),
    }
}

async fn order(
    State(root): State<PathBuf>,
    UrlPath(run): UrlPath<String>,
    Json(sent): Json<Order>,
) -> Response {
    let store = Store::of(&root).await;
    match ukai_acp::flows::order(&root, &store, &run, &sent).await {
        Ok(ordered) => Json(ordered).into_response(),
        Err(error) => refusal(error),
    }
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::Request;
    use rstest::rstest;
    use tower::ServiceExt;
    use ukai_acp::flows::{FlowListing, Ordered, RunChunk, RunState};

    use super::*;

    const GRAPH: &str = r#"{"nodes":[],"edges":[]}"#;

    /// A checkout that is no git repository, so its runs are kept under the
    /// XDG state dir: `.runs`, where runs used to be kept, is still read.
    fn checkout() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("a temporary directory: {e:?}"));
        let run = dir.path().join(".runs").join("weekly");
        std::fs::create_dir_all(&run).unwrap_or_else(|e| panic!("the run's folder: {e:?}"));
        std::fs::write(run.join("graph.json"), GRAPH)
            .unwrap_or_else(|e| panic!("the graph: {e:?}"));
        std::fs::write(run.join("events.jsonl"), "{\"a\":1}\n{\"b\":2}\n")
            .unwrap_or_else(|e| panic!("the events: {e:?}"));
        dir
    }

    async fn ask(root: &std::path::Path, request: Request<Body>) -> (StatusCode, Vec<u8>) {
        let response = router(root.to_owned())
            .oneshot(request)
            .await
            .unwrap_or_else(|e| panic!("the router answers: {e:?}"));
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap_or_else(|e| panic!("the body is read: {e:?}"));
        (status, body.to_vec())
    }

    fn get_request(path: &str) -> Request<Body> {
        Request::get(path)
            .body(Body::empty())
            .unwrap_or_else(|e| panic!("a request: {e:?}"))
    }

    fn post(path: &str, body: &str) -> Request<Body> {
        Request::post(path)
            .header("content-type", "application/json")
            .body(Body::from(body.to_owned()))
            .unwrap_or_else(|e| panic!("a request: {e:?}"))
    }

    #[tokio::test]
    async fn lists_the_runs_of_the_checkout() {
        let dir = checkout();
        let (status, body) = ask(dir.path(), get_request("/flows")).await;
        assert_eq!(status, StatusCode::OK);
        let listing: FlowListing =
            serde_json::from_slice(&body).unwrap_or_else(|e| panic!("a listing: {e:?}"));
        let run = listing
            .runs
            .iter()
            .find(|run| run.id == "weekly")
            .unwrap_or_else(|| panic!("the run is listed"));
        assert_eq!(run.state, RunState::Running);
    }

    #[tokio::test]
    async fn reads_a_run_from_an_offset() {
        let dir = checkout();
        let (status, body) = ask(dir.path(), get_request("/flows/weekly")).await;
        assert_eq!(status, StatusCode::OK);
        let whole: RunChunk =
            serde_json::from_slice(&body).unwrap_or_else(|e| panic!("a run: {e:?}"));
        assert_eq!(whole.graph, GRAPH);
        assert_eq!(whole.events, "{\"a\":1}\n{\"b\":2}\n");
        assert_eq!(whole.next, 16);
        let (_, body) = ask(dir.path(), get_request("/flows/weekly?after=8")).await;
        let rest: RunChunk =
            serde_json::from_slice(&body).unwrap_or_else(|e| panic!("a run: {e:?}"));
        assert_eq!(rest.events, "{\"b\":2}\n");
    }

    #[rstest]
    #[case::unknown("/flows/nope", "there is no run")]
    #[case::not_an_id("/flows/.hidden", "is not a run id")]
    #[tokio::test]
    async fn says_what_is_wrong_with_a_run(#[case] path: &str, #[case] why: &str) {
        let dir = checkout();
        let (status, body) = ask(dir.path(), get_request(path)).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        let text = String::from_utf8_lossy(&body);
        assert!(text.contains(why), "{text}");
    }

    #[rstest]
    #[case::message_for_nobody(r#"{"message":"hi"}"#, "a message needs the node")]
    #[case::empty_message(r#"{"node":"fix","message":" "}"#, "the message is empty")]
    #[tokio::test]
    async fn refuses_an_order_that_says_nothing(#[case] body: &str, #[case] why: &str) {
        let dir = checkout();
        let (status, answer) = ask(dir.path(), post("/flows/weekly", body)).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        let text = String::from_utf8_lossy(&answer);
        assert!(text.contains(why), "{text}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn passes_an_order_to_ayaito() {
        use std::os::unix::fs::PermissionsExt;
        let dir = checkout();
        let bin = dir.path().join("node_modules").join(".bin");
        std::fs::create_dir_all(&bin).unwrap_or_else(|e| panic!("the bin directory: {e:?}"));
        let file = bin.join("ayaito");
        std::fs::write(&file, "#!/bin/sh\necho \"told $1 $5 $6\"\n")
            .unwrap_or_else(|e| panic!("the script: {e:?}"));
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755))
            .unwrap_or_else(|e| panic!("it runs: {e:?}"));
        let (status, body) = ask(
            dir.path(),
            post("/flows/weekly", r#"{"node":"fix","message":"go on"}"#),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
        let ordered: Ordered =
            serde_json::from_slice(&body).unwrap_or_else(|e| panic!("an answer: {e:?}"));
        assert_eq!(ordered.said, "told steer weekly fix");

        std::fs::write(
            &file,
            "#!/bin/sh\necho 'Run weekly has ended.' >&2\nexit 1\n",
        )
        .unwrap_or_else(|e| panic!("the script: {e:?}"));
        let (status, body) = ask(dir.path(), post("/flows/weekly", "{}")).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(String::from_utf8_lossy(&body), "Run weekly has ended.");
    }
}
