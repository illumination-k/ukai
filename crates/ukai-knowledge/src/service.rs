//! `ukai.knowledge.v1.KnowledgeStore`, which the gateway calls: the store,
//! behind the token only the gateway holds, for the caller the gateway
//! names. The gateway is what tells a person from an agent pod and which
//! scope each may use (from `gateway.toml` and the pod's relay token); this
//! service believes it, and keeps to itself what follows from who asks: only
//! a person reviews, and each agent workspace changes at most
//! [`WORKSPACE_CHANGES`] notes per [`WORKSPACE_WINDOW`].

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use tokio::sync::OnceCell;
use tonic::{Request, Response, Status};
use ukai_acp::knowledge::wire;
use ukai_core::knowledge::{Editor, Note, NoteSpec, NoteWrite};
use ukai_proto::knowledge::v1 as pb;
use ukai_proto::knowledge::v1::change_response::Outcome;

use crate::store::{Knowledge, KnowledgeError};

/// Changes one agent pod's workspace may make per [`WORKSPACE_WINDOW`]: a
/// session that read something hostile cannot flood the store with notes
/// that outnumber the real ones.
pub const WORKSPACE_CHANGES: usize = 20;
pub const WORKSPACE_WINDOW: Duration = Duration::from_secs(3600);

/// Who asks, as the gateway said.
#[derive(Debug)]
struct Caller {
    scope: String,
    editor: Editor,
    workspace: Option<String>,
}

fn caller(caller: Option<pb::Caller>) -> Result<Caller, Status> {
    let caller = caller.ok_or_else(|| Status::invalid_argument("the request names no caller"))?;
    if caller.scope.is_empty() {
        return Err(Status::invalid_argument("the caller has no scope"));
    }
    let editor =
        wire::editor(caller.editor).map_err(|error| Status::invalid_argument(error.to_string()))?;
    Ok(Caller {
        scope: caller.scope,
        editor,
        workspace: caller.workspace,
    })
}

/// Refuse a review by anyone but a person.
fn person(caller: &Caller) -> Result<(), Status> {
    if caller.editor == Editor::User {
        Ok(())
    } else {
        Err(Status::permission_denied("only a person reviews notes"))
    }
}

pub struct Service {
    /// Set once the store is reached; until then every call is UNAVAILABLE.
    store: Arc<OnceCell<Knowledge>>,
    /// When each agent workspace last changed notes.
    changes: Mutex<HashMap<String, VecDeque<Instant>>>,
}

impl Service {
    pub fn new(store: Arc<OnceCell<Knowledge>>) -> Self {
        Self {
            store,
            changes: Mutex::default(),
        }
    }

    fn store(&self) -> Result<&Knowledge, Status> {
        self.store
            .get()
            .ok_or_else(|| Status::unavailable("the knowledge store is not reached yet"))
    }

    /// Count a change by `caller`, refusing it when its workspace has made
    /// too many. A person's are not counted.
    fn admit(&self, caller: &Caller) -> Result<(), Status> {
        let Some(workspace) = &caller.workspace else {
            return Ok(());
        };
        let now = Instant::now();
        let mut changes = self.changes.lock().unwrap_or_else(PoisonError::into_inner);
        let recent = changes.entry(workspace.clone()).or_default();
        recent.retain(|at| now.duration_since(*at) < WORKSPACE_WINDOW);
        if recent.len() >= WORKSPACE_CHANGES {
            return Err(Status::resource_exhausted(
                "this workspace has changed too many notes for now",
            ));
        }
        recent.push_back(now);
        Ok(())
    }
}

/// A failure that is not an answer of its own.
fn failed(error: KnowledgeError) -> Status {
    match error {
        KnowledgeError::NotFound => Status::not_found("no such note"),
        KnowledgeError::Refused(problems) => Status::failed_precondition(problems.join("; ")),
        KnowledgeError::Conflict(_) => Status::aborted("the note changed since it was read"),
        error @ (KnowledgeError::Store(_) | KnowledgeError::Unreadable(_)) => {
            tracing::warn!(%error, "knowledge request failed");
            Status::internal("the knowledge store failed")
        }
    }
}

/// What a change came to, as the wire says it.
fn change(
    result: Result<Option<Note>, KnowledgeError>,
) -> Result<Response<pb::ChangeResponse>, Status> {
    let outcome = match result {
        Ok(note) => Outcome::Changed(pb::Changed {
            note: note.map(Into::into),
        }),
        Err(KnowledgeError::Conflict(current)) => Outcome::Conflict(pb::Conflict {
            current: (*current).map(Into::into),
        }),
        Err(KnowledgeError::Refused(problems)) => Outcome::Refused(pb::Refused { problems }),
        Err(error) => return Err(failed(error)),
    };
    Ok(Response::new(pb::ChangeResponse {
        outcome: Some(outcome),
    }))
}

#[tonic::async_trait]
impl pb::knowledge_store_server::KnowledgeStore for Service {
    async fn list(
        &self,
        request: Request<pb::ListRequest>,
    ) -> Result<Response<pb::ListResponse>, Status> {
        let caller = caller(request.into_inner().caller)?;
        let notes = self.store()?.list(&caller.scope).await.map_err(failed)?;
        Ok(Response::new(pb::ListResponse {
            notes: notes.into_iter().map(Into::into).collect(),
        }))
    }

    async fn get(
        &self,
        request: Request<pb::GetRequest>,
    ) -> Result<Response<pb::GetResponse>, Status> {
        let request = request.into_inner();
        let caller = caller(request.caller)?;
        let note = self
            .store()?
            .get(&caller.scope, &request.name)
            .await
            .map_err(failed)?;
        Ok(Response::new(pb::GetResponse {
            note: note.map(Into::into),
        }))
    }

    async fn revisions(
        &self,
        request: Request<pb::RevisionsRequest>,
    ) -> Result<Response<pb::RevisionsResponse>, Status> {
        let request = request.into_inner();
        let caller = caller(request.caller)?;
        let revisions = self
            .store()?
            .revisions(&caller.scope, &request.name)
            .await
            .map_err(failed)?;
        Ok(Response::new(pb::RevisionsResponse {
            revisions: revisions.into_iter().map(Into::into).collect(),
        }))
    }

    async fn put(
        &self,
        request: Request<pb::PutRequest>,
    ) -> Result<Response<pb::ChangeResponse>, Status> {
        let request = request.into_inner();
        let caller = caller(request.caller)?;
        let spec = request
            .spec
            .map(NoteSpec::from)
            .ok_or_else(|| Status::invalid_argument("the write has no note"))?;
        let store = self.store()?;
        self.admit(&caller)?;
        let write = NoteWrite {
            spec,
            base_revision: request.base_revision,
            commit: request.commit,
        };
        change(
            store
                .put(
                    &caller.scope,
                    &request.name,
                    write,
                    caller.editor,
                    caller.workspace,
                )
                .await
                .map(Some),
        )
    }

    async fn delete(
        &self,
        request: Request<pb::DeleteRequest>,
    ) -> Result<Response<pb::ChangeResponse>, Status> {
        let request = request.into_inner();
        let caller = caller(request.caller)?;
        let store = self.store()?;
        self.admit(&caller)?;
        change(
            store
                .delete(
                    &caller.scope,
                    &request.name,
                    request.revision,
                    caller.editor,
                    caller.workspace,
                )
                .await
                .map(|()| None),
        )
    }

    async fn approve(
        &self,
        request: Request<pb::ReviewRequest>,
    ) -> Result<Response<pb::ChangeResponse>, Status> {
        let request = request.into_inner();
        let caller = caller(request.caller)?;
        person(&caller)?;
        change(
            self.store()?
                .approve(&caller.scope, &request.name, request.revision)
                .await
                .map(Some),
        )
    }

    async fn reject(
        &self,
        request: Request<pb::ReviewRequest>,
    ) -> Result<Response<pb::ChangeResponse>, Status> {
        let request = request.into_inner();
        let caller = caller(request.caller)?;
        person(&caller)?;
        change(
            self.store()?
                .reject(&caller.scope, &request.name, request.revision)
                .await,
        )
    }
}

/// Let through only requests bearing `token`, which the gateway holds.
pub fn authorize(token: String) -> impl FnMut(Request<()>) -> Result<Request<()>, Status> + Clone {
    move |request: Request<()>| {
        let presented = request
            .metadata()
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "));
        if presented.is_some_and(|presented| same_secret(&token, presented)) {
            Ok(request)
        } else {
            Err(Status::unauthenticated(
                "this service answers the gateway only",
            ))
        }
    }
}

/// Whether `presented` is `secret`, taking as long for every `presented` of
/// the same length.
fn same_secret(secret: &str, presented: &str) -> bool {
    presented.len() == secret.len()
        && presented
            .bytes()
            .zip(secret.bytes())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0
}

#[cfg(test)]
mod tests;
