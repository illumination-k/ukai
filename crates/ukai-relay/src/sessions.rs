//! The relay's sessions over HTTP, for the gateway to answer a secretary
//! agent (see "秘書" in `docs/design.md`): `GET /sessions` lists them,
//! `GET /sessions/<id>/transcript?limit=` reads the end of one, `POST
//! /sessions/<id>/prompt` sends one a prompt with no client attached, and
//! `POST /sessions` starts one with a prompt. The hub is in the relay's
//! event loop, so each request is a [`Query`] it answers there.

use std::collections::HashMap;

use axum::extract::{Path, Query as UrlQuery, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use tokio::sync::{mpsc, oneshot};
use ukai_acp::secretary::{
    Entry, PromptBody, SESSIONS_PATH, SessionSummary, Started, TRANSCRIPT_DEFAULT, TRANSCRIPT_MAX,
    TranscriptQuery,
};

use crate::hub::{Hub, Output, PromptRefused};

/// How long a start waits for the agent to set the session up.
const START_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// What a request asks the hub.
#[derive(Debug)]
pub enum Query {
    List(oneshot::Sender<Vec<SessionSummary>>),
    Transcript {
        session: String,
        limit: usize,
        reply: oneshot::Sender<Option<Vec<Entry>>>,
    },
    Prompt {
        session: String,
        text: String,
        reply: oneshot::Sender<Result<(), PromptRefused>>,
    },
    Start {
        text: String,
        reply: oneshot::Sender<Result<String, String>>,
    },
}

/// Answers [`Query`]s against the hub, keeping the starts that wait for
/// the agent.
#[derive(Debug, Default)]
pub struct Answers {
    next_ticket: u64,
    starting: HashMap<u64, oneshot::Sender<Result<String, String>>>,
}

impl Answers {
    /// Answer `query`, returning what the hub has to send.
    pub fn answer(&mut self, hub: &mut Hub, query: Query) -> Vec<Output> {
        match query {
            Query::List(reply) => {
                let _ = reply.send(hub.sessions());
                Vec::new()
            }
            Query::Transcript {
                session,
                limit,
                reply,
            } => {
                let _ = reply.send(hub.transcript(&session, limit));
                Vec::new()
            }
            Query::Prompt {
                session,
                text,
                reply,
            } => match hub.prompt_unattached(&session, &text) {
                Ok(outputs) => {
                    let _ = reply.send(Ok(()));
                    outputs
                }
                Err(refused) => {
                    let _ = reply.send(Err(refused));
                    Vec::new()
                }
            },
            Query::Start { text, reply } => {
                self.next_ticket += 1;
                let ticket = self.next_ticket;
                self.starting.insert(ticket, reply);
                hub.start_unattached(ticket, text)
            }
        }
    }

    /// Tell the start waiting on `ticket` how it went.
    pub fn started(&mut self, ticket: u64, outcome: Result<String, String>) {
        if let Some(reply) = self.starting.remove(&ticket) {
            let _ = reply.send(outcome);
        }
    }
}

pub fn router(queries: mpsc::UnboundedSender<Query>) -> Router {
    Router::new()
        .route(SESSIONS_PATH, get(list).post(start))
        .route(
            &format!("{SESSIONS_PATH}/{{id}}/transcript"),
            get(transcript),
        )
        .route(&format!("{SESSIONS_PATH}/{{id}}/prompt"), post(prompt))
        .with_state(queries)
}

type Queries = State<mpsc::UnboundedSender<Query>>;

/// Ask the hub, or 503 when the relay is shutting down.
async fn ask<T>(
    queries: &mpsc::UnboundedSender<Query>,
    query: impl FnOnce(oneshot::Sender<T>) -> Query,
) -> Result<T, Response> {
    let (reply, answer) = oneshot::channel();
    let gone = || StatusCode::SERVICE_UNAVAILABLE.into_response();
    queries.send(query(reply)).map_err(|_| gone())?;
    answer.await.map_err(|_| gone())
}

async fn list(State(queries): Queries) -> Response {
    match ask(&queries, Query::List).await {
        Ok(sessions) => Json(sessions).into_response(),
        Err(response) => response,
    }
}

async fn transcript(
    State(queries): Queries,
    Path(session): Path<String>,
    UrlQuery(TranscriptQuery { limit }): UrlQuery<TranscriptQuery>,
) -> Response {
    let limit = limit
        .filter(|limit| *limit > 0)
        .unwrap_or(TRANSCRIPT_DEFAULT)
        .min(TRANSCRIPT_MAX);
    match ask(&queries, |reply| Query::Transcript {
        session,
        limit,
        reply,
    })
    .await
    {
        Ok(Some(entries)) => Json(entries).into_response(),
        Ok(None) => (StatusCode::NOT_FOUND, "no such session").into_response(),
        Err(response) => response,
    }
}

async fn prompt(
    State(queries): Queries,
    Path(session): Path<String>,
    Json(body): Json<PromptBody>,
) -> Response {
    if body.text.trim().is_empty() {
        return (StatusCode::UNPROCESSABLE_ENTITY, "the prompt is empty").into_response();
    }
    match ask(&queries, |reply| Query::Prompt {
        session,
        text: body.text,
        reply,
    })
    .await
    {
        Ok(Ok(())) => StatusCode::ACCEPTED.into_response(),
        Ok(Err(PromptRefused::Unknown)) => {
            (StatusCode::NOT_FOUND, "no such session").into_response()
        }
        Ok(Err(PromptRefused::Running)) => {
            (StatusCode::CONFLICT, "the session's turn is running").into_response()
        }
        Err(response) => response,
    }
}

async fn start(State(queries): Queries, Json(body): Json<PromptBody>) -> Response {
    if body.text.trim().is_empty() {
        return (StatusCode::UNPROCESSABLE_ENTITY, "the prompt is empty").into_response();
    }
    let started = ask(&queries, |reply| Query::Start {
        text: body.text,
        reply,
    });
    match tokio::time::timeout(START_TIMEOUT, started).await {
        Ok(Ok(Ok(session))) => Json(Started { session }).into_response(),
        Ok(Ok(Err(why))) => (StatusCode::UNPROCESSABLE_ENTITY, why).into_response(),
        Ok(Err(response)) => response,
        Err(_) => (
            StatusCode::GATEWAY_TIMEOUT,
            "the agent did not set the session up in time",
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::Request;
    use rstest::rstest;
    use tower::ServiceExt;

    use super::*;

    /// The routes, answered by a hub whose agent has not initialized: it
    /// knows no session and can start none yet.
    fn routes() -> Router {
        let (queries, mut asked) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            let (mut hub, _) = Hub::new(None, Vec::new());
            let mut answers = Answers::default();
            while let Some(query) = asked.recv().await {
                for output in answers.answer(&mut hub, query) {
                    if let Output::Started { ticket, outcome } = output {
                        answers.started(ticket, outcome);
                    }
                }
            }
        });
        router(queries)
    }

    async fn status(method: &str, uri: &str, body: &str) -> StatusCode {
        let request = Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", "application/json")
            .body(Body::from(body.to_owned()))
            .unwrap_or_default();
        routes()
            .oneshot(request)
            .await
            .map(|response| response.status())
            .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR)
    }

    #[rstest]
    #[case::list("GET", "/sessions", "", StatusCode::OK)]
    #[case::unknown_transcript("GET", "/sessions/s1/transcript", "", StatusCode::NOT_FOUND)]
    #[case::unknown_prompt(
        "POST",
        "/sessions/s1/prompt",
        r#"{"text":"hi"}"#,
        StatusCode::NOT_FOUND
    )]
    #[case::empty_prompt(
        "POST",
        "/sessions/s1/prompt",
        r#"{"text":" "}"#,
        StatusCode::UNPROCESSABLE_ENTITY
    )]
    #[case::start_too_early(
        "POST",
        "/sessions",
        r#"{"text":"hi"}"#,
        StatusCode::UNPROCESSABLE_ENTITY
    )]
    #[tokio::test]
    async fn answers(
        #[case] method: &str,
        #[case] uri: &str,
        #[case] body: &str,
        #[case] expected: StatusCode,
    ) {
        assert_eq!(status(method, uri, body).await, expected);
    }
}
