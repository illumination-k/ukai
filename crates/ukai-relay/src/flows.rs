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
//! The relay also wakes the agent when a run it started ends ([`watch`]):
//! the MCP server cannot start a turn, and a run outlives the turn that
//! started it, so without this the result would wait for the user to ask.
//!
//! What is wrong with a request — no such run, a node that is not running —
//! is 422 with the reason as the body. All want the relay's token, like
//! everything but `/health`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};
use ukai_acp::flows::RunState;

use crate::hub::PromptRefused;
use crate::mcp::flows::{Wait, report, waiting};
use crate::sessions::Query as SessionQuery;

use axum::extract::{Path as UrlPath, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use ukai_acp::flows::{FLOWS_PATH, FlowError, Order, ReadQuery, Store, blocking};

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

/// How often the runs sessions wait for are looked at.
const WATCH: Duration = Duration::from_millis(if cfg!(test) { 30 } else { 2000 });

/// What a session is sent when a run it started has ended.
fn wake_text(run: &str, report: &str) -> String {
    format!(
        "[ukai] The ayaito flow run `{run}` that this session started has ended. Nobody typed \
         this message: it is the run's outcome, for you to go on from.\n\n{report}"
    )
}

/// Watches the runs recorded in `waits` (by the MCP servers' `run_flow`,
/// for the checkout at `root`), and when one has ended sends the session
/// that started it the run's report as a prompt, through `queries`. A
/// session in the middle of a turn is sent it once the turn is over; one
/// the relay does not know anymore is not.
///
/// Answers the count of runs still awaited, going or not yet told of: while
/// it is not zero the relay has work, though no turn may be running.
pub fn watch(
    root: PathBuf,
    waits: PathBuf,
    queries: mpsc::UnboundedSender<SessionQuery>,
) -> Arc<AtomicUsize> {
    let awaited = Arc::new(AtomicUsize::new(0));
    let count = awaited.clone();
    tokio::spawn(async move {
        let mut told: HashSet<Wait> = HashSet::new();
        loop {
            tokio::time::sleep(WATCH).await;
            let pending: Vec<Wait> = waiting(&waits)
                .into_iter()
                .filter(|wait| !told.contains(wait))
                .collect();
            let mut left = 0;
            for wait in pending {
                match wake(&root, &wait, &queries).await {
                    Woken::NotYet => left += 1,
                    Woken::Done => {
                        told.insert(wait);
                    }
                    Woken::Closed => return,
                }
            }
            count.store(left, Ordering::Relaxed);
        }
    });
    awaited
}

enum Woken {
    /// The run goes on, or its session is in a turn.
    NotYet,
    /// The session was told, or there is nobody to tell.
    Done,
    /// The relay is ending.
    Closed,
}

async fn wake(root: &Path, wait: &Wait, queries: &mpsc::UnboundedSender<SessionQuery>) -> Woken {
    let store = Store::of(root).await;
    match store.state(&wait.run) {
        Ok(RunState::Running) => return Woken::NotYet,
        Ok(_) => {}
        Err(error) => {
            tracing::warn!(run = %wait.run, %error, "a flow's run a session waits for is gone");
            return Woken::Done;
        }
    }
    let text = match report(root, &wait.run).await {
        Ok((text, _)) => text,
        Err(problem) => format!("Its outcome cannot be read: {problem}"),
    };
    let (reply, answer) = oneshot::channel();
    let query = SessionQuery::Prompt {
        session: wait.session.clone(),
        text: wake_text(&wait.run, &text),
        reply,
    };
    if queries.send(query).is_err() {
        return Woken::Closed;
    }
    match answer.await {
        Ok(Ok(())) => {
            tracing::info!(run = %wait.run, session = %wait.session, "woke a session for its flow's end");
            Woken::Done
        }
        Ok(Err(PromptRefused::Running)) => Woken::NotYet,
        Ok(Err(PromptRefused::Unknown)) => {
            tracing::info!(run = %wait.run, session = %wait.session, "the session that started a flow is gone");
            Woken::Done
        }
        Err(_) => Woken::Closed,
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

    /// A git checkout with a run `r1` in its store, and the file sessions
    /// record their waits in.
    fn awaited(result: Option<&str>) -> Option<(tempfile::TempDir, PathBuf)> {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("a temporary directory: {e:?}"));
        let git = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(dir.path())
            .status();
        if !git.is_ok_and(|status| status.success()) {
            return None;
        }
        let run = dir.path().join(".git").join("ayaito-runs").join("r1");
        std::fs::create_dir_all(&run).unwrap_or_else(|e| panic!("the run's folder: {e:?}"));
        std::fs::write(run.join("graph.json"), GRAPH)
            .unwrap_or_else(|e| panic!("the graph: {e:?}"));
        if let Some(result) = result {
            std::fs::write(run.join("result.json"), result)
                .unwrap_or_else(|e| panic!("the result: {e:?}"));
        }
        let waits = dir.path().join("flows.jsonl");
        std::fs::write(&waits, "{\"run\":\"r1\",\"session\":\"s1\"}\n")
            .unwrap_or_else(|e| panic!("the waits: {e:?}"));
        Some((dir, waits))
    }

    async fn next_prompt(
        queries: &mut mpsc::UnboundedReceiver<SessionQuery>,
    ) -> Option<(String, String, oneshot::Sender<Result<(), PromptRefused>>)> {
        let query = tokio::time::timeout(Duration::from_secs(10), queries.recv()).await;
        match query {
            Ok(Some(SessionQuery::Prompt {
                session,
                text,
                reply,
            })) => Some((session, text, reply)),
            Ok(other) => panic!("not a prompt: {other:?}"),
            Err(_) => None,
        }
    }

    #[tokio::test]
    async fn a_session_is_woken_once_when_its_run_has_ended() {
        let Some((dir, waits)) = awaited(Some(r#"{"status":"done","output":42}"#)) else {
            return;
        };
        let (tx, mut queries) = mpsc::unbounded_channel();
        let left = watch(dir.path().to_owned(), waits, tx);
        // In the middle of a turn: asked again.
        let (session, _, reply) = next_prompt(&mut queries)
            .await
            .unwrap_or_else(|| panic!("no prompt"));
        assert_eq!(session, "s1");
        let _ = reply.send(Err(PromptRefused::Running));
        let (_, text, reply) = next_prompt(&mut queries)
            .await
            .unwrap_or_else(|| panic!("no second prompt"));
        assert!(
            text.contains("`r1`") && text.contains("Run is done"),
            "{text}"
        );
        assert!(text.contains(r#""output":42"#), "{text}");
        assert_eq!(left.load(Ordering::Relaxed), 1);
        let _ = reply.send(Ok(()));
        // Told: never again, and nothing is left to wait for.
        let again = tokio::time::timeout(Duration::from_millis(300), queries.recv()).await;
        assert!(again.is_err(), "woken twice");
        assert_eq!(left.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn a_run_that_goes_on_wakes_nobody_and_keeps_the_relay_busy() {
        let Some((dir, waits)) = awaited(None) else {
            return;
        };
        let (tx, mut queries) = mpsc::unbounded_channel();
        let left = watch(dir.path().to_owned(), waits, tx);
        let early = tokio::time::timeout(Duration::from_millis(300), queries.recv()).await;
        assert!(early.is_err(), "woken for a run that goes on");
        assert_eq!(left.load(Ordering::Relaxed), 1);
        let result = dir.path().join(".git/ayaito-runs/r1/result.json");
        std::fs::write(result, r#"{"status":"failed","error":"boom"}"#)
            .unwrap_or_else(|e| panic!("the result: {e:?}"));
        let (_, text, reply) = next_prompt(&mut queries)
            .await
            .unwrap_or_else(|| panic!("no prompt"));
        assert!(
            text.contains("Run is failed") && text.contains("boom"),
            "{text}"
        );
        // The session is gone: nobody to tell, and nothing to wait for.
        let _ = reply.send(Err(PromptRefused::Unknown));
        let again = tokio::time::timeout(Duration::from_millis(300), queries.recv()).await;
        assert!(again.is_err(), "asked again for a session that is gone");
        assert_eq!(left.load(Ordering::Relaxed), 0);
    }
}
