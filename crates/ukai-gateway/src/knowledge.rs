//! The gateway's side of `ukai-knowledge`, which keeps the notes: a gRPC
//! client of `ukai.knowledge.v1.KnowledgeStore` that names who is asking.
//!
//! The gateway is what can tell who that is: a person through the gateway's
//! token, whose scope is their agent's in `gateway.toml`, or an agent pod
//! through its relay's token, whose scope and workspace its Job says. The
//! service believes the [`Caller`] given here, and is reached with the token
//! the two share.

use std::time::Duration;

use tonic::metadata::{Ascii, MetadataValue};
use tonic::transport::{Channel, Endpoint};
use tonic::{Code, Request, Status};
use ukai_acp::knowledge::wire::Malformed;
use ukai_core::knowledge::{Editor, Note, NoteSummary, NoteWrite, Revision};
use ukai_proto::knowledge::v1 as pb;
use ukai_proto::knowledge::v1::change_response::Outcome;
use ukai_proto::knowledge::v1::knowledge_store_client::KnowledgeStoreClient;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const TIMEOUT: Duration = Duration::from_secs(20);

/// Whose knowledge a request is about, and who is asking.
#[derive(Debug, Clone)]
pub struct Caller {
    pub scope: String,
    pub editor: Editor,
    /// The agent pod's workspace, for a pod.
    pub workspace: Option<String>,
}

impl From<&Caller> for pb::Caller {
    fn from(caller: &Caller) -> Self {
        Self {
            scope: caller.scope.clone(),
            editor: pb::Editor::from(caller.editor).into(),
            workspace: caller.workspace.clone(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum KnowledgeError {
    #[error("{}", .0.join("; "))]
    Refused(Vec<String>),
    /// The note is not at the revision the writer read; it is as given now.
    #[error("the note changed since it was read")]
    Conflict(Box<Option<Note>>),
    #[error("no such note")]
    NotFound,
    #[error("this workspace has changed too many notes for now")]
    TooMany,
    #[error("only a person reviews notes")]
    Forbidden,
    /// The service, or its store, cannot be reached now.
    #[error("{0}")]
    Unavailable(String),
    #[error("the knowledge service failed: {0}")]
    Failed(String),
}

impl From<Status> for KnowledgeError {
    fn from(status: Status) -> Self {
        match status.code() {
            Code::NotFound => Self::NotFound,
            Code::ResourceExhausted => Self::TooMany,
            Code::PermissionDenied => Self::Forbidden,
            Code::Unavailable => Self::Unavailable(format!(
                "the knowledge service is unavailable: {}",
                status.message()
            )),
            code => Self::Failed(format!("{code:?}: {}", status.message())),
        }
    }
}

impl From<Malformed> for KnowledgeError {
    fn from(error: Malformed) -> Self {
        Self::Failed(error.to_string())
    }
}

/// `ukai-knowledge`, at the address the gateway was given.
#[derive(Clone)]
pub struct KnowledgeClient {
    client: KnowledgeStoreClient<Channel>,
    /// `Bearer <token>`, marked sensitive so it is never logged.
    token: MetadataValue<Ascii>,
}

impl KnowledgeClient {
    /// A client of the service at `url` (`http://ukai-knowledge:50051`),
    /// connecting on first use, so the gateway starts without it.
    pub fn new(url: &str, token: &str) -> Result<Self, String> {
        let channel = Endpoint::from_shared(url.to_owned())
            .map_err(|error| format!("{url}: {error}"))?
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(TIMEOUT)
            .connect_lazy();
        let mut token: MetadataValue<Ascii> = format!("Bearer {token}")
            .parse()
            .map_err(|_| "the knowledge token is not a valid header value".to_owned())?;
        token.set_sensitive(true);
        Ok(Self {
            client: KnowledgeStoreClient::new(channel),
            token,
        })
    }

    fn request<T>(&self, message: T) -> Request<T> {
        let mut request = Request::new(message);
        request
            .metadata_mut()
            .insert("authorization", self.token.clone());
        request
    }

    pub async fn list(&self, caller: &Caller) -> Result<Vec<NoteSummary>, KnowledgeError> {
        let response = self
            .client
            .clone()
            .list(self.request(pb::ListRequest {
                caller: Some(caller.into()),
            }))
            .await?;
        response
            .into_inner()
            .notes
            .into_iter()
            .map(|note| NoteSummary::try_from(note).map_err(Into::into))
            .collect()
    }

    /// The note as it is now; `None` when there is none or it is deleted.
    pub async fn get(&self, caller: &Caller, name: &str) -> Result<Option<Note>, KnowledgeError> {
        let response = self
            .client
            .clone()
            .get(self.request(pb::GetRequest {
                caller: Some(caller.into()),
                name: name.to_owned(),
            }))
            .await?;
        Ok(response.into_inner().note.map(Note::try_from).transpose()?)
    }

    pub async fn revisions(
        &self,
        caller: &Caller,
        name: &str,
    ) -> Result<Vec<Revision>, KnowledgeError> {
        let response = self
            .client
            .clone()
            .revisions(self.request(pb::RevisionsRequest {
                caller: Some(caller.into()),
                name: name.to_owned(),
            }))
            .await?;
        response
            .into_inner()
            .revisions
            .into_iter()
            .map(|revision| Revision::try_from(revision).map_err(Into::into))
            .collect()
    }

    pub async fn put(
        &self,
        caller: &Caller,
        name: &str,
        write: NoteWrite,
    ) -> Result<Note, KnowledgeError> {
        let response = self
            .client
            .clone()
            .put(self.request(pb::PutRequest {
                caller: Some(caller.into()),
                name: name.to_owned(),
                spec: Some(write.spec.into()),
                base_revision: write.base_revision,
                commit: write.commit,
            }))
            .await?;
        changed(response.into_inner())?.ok_or(KnowledgeError::NotFound)
    }

    pub async fn delete(
        &self,
        caller: &Caller,
        name: &str,
        revision: u64,
    ) -> Result<(), KnowledgeError> {
        let response = self
            .client
            .clone()
            .delete(self.request(pb::DeleteRequest {
                caller: Some(caller.into()),
                name: name.to_owned(),
                revision,
            }))
            .await?;
        changed(response.into_inner()).map(drop)
    }

    pub async fn approve(
        &self,
        caller: &Caller,
        name: &str,
        revision: u64,
    ) -> Result<Note, KnowledgeError> {
        let response = self
            .client
            .clone()
            .approve(self.review(caller, name, revision))
            .await?;
        changed(response.into_inner())?.ok_or(KnowledgeError::NotFound)
    }

    /// The note as the rejection left it; `None` once it is deleted.
    pub async fn reject(
        &self,
        caller: &Caller,
        name: &str,
        revision: u64,
    ) -> Result<Option<Note>, KnowledgeError> {
        let response = self
            .client
            .clone()
            .reject(self.review(caller, name, revision))
            .await?;
        changed(response.into_inner())
    }

    fn review(&self, caller: &Caller, name: &str, revision: u64) -> Request<pb::ReviewRequest> {
        self.request(pb::ReviewRequest {
            caller: Some(caller.into()),
            name: name.to_owned(),
            revision,
        })
    }
}

/// What a change came to: the note as it is now, or why there is none.
fn changed(response: pb::ChangeResponse) -> Result<Option<Note>, KnowledgeError> {
    match response.outcome {
        Some(Outcome::Changed(changed)) => Ok(changed.note.map(Note::try_from).transpose()?),
        Some(Outcome::Conflict(conflict)) => Err(KnowledgeError::Conflict(Box::new(
            conflict.current.map(Note::try_from).transpose()?,
        ))),
        Some(Outcome::Refused(refused)) => Err(KnowledgeError::Refused(refused.problems)),
        None => Err(Malformed("outcome").into()),
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;
    use ukai_core::knowledge::{NoteSpec, Provenance};

    use super::*;

    fn note() -> Note {
        Note {
            name: "build".into(),
            spec: NoteSpec {
                description: "Building".into(),
                body: "b".into(),
                tags: Vec::new(),
                on_demand: false,
            },
            revision: 2,
            updated: 1,
            updated_by: Editor::User,
            provenance: Provenance::default(),
            approved: None,
        }
    }

    fn response(outcome: Option<Outcome>) -> pb::ChangeResponse {
        pb::ChangeResponse { outcome }
    }

    #[test]
    fn a_change_comes_back_as_the_note_or_why_not() {
        let changed = |note: Option<Note>| {
            Some(Outcome::Changed(pb::Changed {
                note: note.map(Into::into),
            }))
        };
        assert_eq!(
            super::changed(response(changed(Some(note())))).ok(),
            Some(Some(note()))
        );
        assert_eq!(super::changed(response(changed(None))).ok(), Some(None));
        assert!(matches!(
            super::changed(response(Some(Outcome::Conflict(pb::Conflict {
                current: Some(note().into()),
            })))),
            Err(KnowledgeError::Conflict(current)) if *current == Some(note())
        ));
        assert!(matches!(
            super::changed(response(Some(Outcome::Refused(pb::Refused {
                problems: vec!["no".into()],
            })))),
            Err(KnowledgeError::Refused(problems)) if problems == ["no"]
        ));
        assert!(matches!(
            super::changed(response(None)),
            Err(KnowledgeError::Failed(_))
        ));
    }

    #[rstest]
    #[case::not_found(Code::NotFound, "NotFound")]
    #[case::too_many(Code::ResourceExhausted, "TooMany")]
    #[case::forbidden(Code::PermissionDenied, "Forbidden")]
    #[case::unavailable(Code::Unavailable, "Unavailable")]
    #[case::other(Code::Internal, "Failed")]
    fn statuses_say_what_went_wrong(#[case] code: Code, #[case] expected: &str) {
        let error = KnowledgeError::from(Status::new(code, "m"));
        assert!(format!("{error:?}").starts_with(expected), "{error:?}");
    }

    #[tokio::test]
    async fn the_token_is_never_shown() {
        let client = KnowledgeClient::new("http://ukai-knowledge:50051", "s3cret")
            .unwrap_or_else(|e| panic!("{e}"));
        assert!(!format!("{:?}", client.token).contains("s3cret"));
    }
}
