//! Knowledge a gateway keeps outside the repositories it is about: the
//! types, and the client side of the gateway's `ukai.gateway.v1.Knowledge`
//! and `WorkspaceKnowledge` (gRPC-Web, see `crate::grpc`).
//!
//! A note is a named piece of Markdown with a one-line description: a
//! convention, a decision, a pitfall, something learnt about the codebase
//! that should outlive one session without being committed to it. The
//! gateway keeps notes per scope: a project its `gateway.toml` names, which
//! agents on several repositories share, or else each agent's repository.
//! The app reads and edits them through an agent
//! ([`KnowledgeEndpoint::resolve`]), and agents in its pods through the
//! relay's MCP server, with the relay's token
//! ([`KnowledgeEndpoint::for_workspace`]). What a valid note is, is
//! `ukai_core::knowledge`'s business, shared by both ends; how the types go
//! over the wire is [`wire`]'s.
//!
//! Every write and delete makes a [`Revision`]; nothing is lost, and a
//! deleted note can be written again. What a person writes is approved as
//! it is written; what an agent writes waits for a person to approve it
//! ([`Note::approved`] keeps the last approved revision meanwhile), and only
//! approved notes are put before every session, except those [`NoteSpec::on_demand`],
//! which are only listed when asked for. A person can also reject what an
//! agent wrote, back to the approved revision. A write names the revision it read
//! ([`NoteWrite::base_revision`], 0 for a new note), and is refused with
//! [`KnowledgeError::Conflict`] when the note has moved on since.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use ukai_config::{AgentSpec, Source};
use ukai_proto::gateway::v1 as gw;
use ukai_proto::gateway::v1::knowledge_client::KnowledgeClient;
use ukai_proto::gateway::v1::workspace_knowledge_client::WorkspaceKnowledgeClient;
use ukai_proto::knowledge::v1 as pb;
use ukai_proto::knowledge::v1::change_response::Outcome;

use crate::grpc::{self, Channel};
use crate::{ResolveError, header_map};

pub mod wire;

#[cfg(test)]
mod web_tests;

const TIMEOUT: Duration = Duration::from_secs(20);

/// The part of a note its writer sets; the name is in the URL.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NoteSpec {
    /// One line telling what the note is about, so a reader can decide
    /// whether to read it.
    pub description: String,
    /// Markdown.
    pub body: String,
    /// Words to find the note by, like `research` or `build`.
    #[serde(
        default,
        deserialize_with = "or_default",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub tags: Vec<String>,
    /// Whether sessions are left to ask for the note (`list_knowledge` by
    /// tag) rather than shown it from the start: for research and other
    /// notes only some tasks need.
    #[serde(
        default,
        deserialize_with = "or_default",
        skip_serializing_if = "std::ops::Not::not"
    )]
    pub on_demand: bool,
}

/// Who last wrote a note.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Editor {
    /// Someone through the app.
    User,
    /// An agent, through the relay's MCP server.
    Agent,
}

/// What a write sends: the note, and the revision it was based on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NoteWrite {
    #[serde(flatten)]
    pub spec: NoteSpec,
    /// The revision the writer read: 0 to create a note (or write one that
    /// was deleted), refused when it is not the note's current one.
    #[serde(default)]
    pub base_revision: u64,
    /// The commit the writer's checkout was at, as it says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
}

/// Where a revision came from, beyond who wrote it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Provenance {
    /// The gateway workspace of the agent pod that wrote it, as the gateway
    /// found it from the pod's token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    /// The commit the writer's checkout was at, as the writer said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
}

/// A scope's notes, as `GET .../knowledge` lists them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Listing {
    /// What the notes are kept under: agents with the same scope share them.
    pub scope: String,
    pub notes: Vec<NoteSummary>,
}

/// A note as listed: everything but the body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NoteSummary {
    pub name: String,
    pub description: String,
    #[serde(
        default,
        deserialize_with = "or_default",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub tags: Vec<String>,
    #[serde(
        default,
        deserialize_with = "or_default",
        skip_serializing_if = "std::ops::Not::not"
    )]
    pub on_demand: bool,
    pub revision: u64,
    /// Unix seconds.
    pub updated: i64,
    pub updated_by: Editor,
    /// The last revision a person approved, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approved: Option<ApprovedSummary>,
}

impl NoteSummary {
    /// Whether the latest revision waits for a person to approve it.
    pub fn awaits_review(&self) -> bool {
        self.approved
            .as_ref()
            .is_none_or(|approved| approved.revision != self.revision)
    }
}

/// The approved revision of a listed note.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovedSummary {
    pub revision: u64,
    pub description: String,
    #[serde(
        default,
        deserialize_with = "or_default",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub tags: Vec<String>,
    #[serde(
        default,
        deserialize_with = "or_default",
        skip_serializing_if = "std::ops::Not::not"
    )]
    pub on_demand: bool,
}

/// The last revision of a note a person approved, whole.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Approved {
    pub revision: u64,
    #[serde(flatten)]
    pub spec: NoteSpec,
    pub updated: i64,
    pub updated_by: Editor,
    #[serde(default)]
    pub provenance: Provenance,
}

/// A whole note, as it is now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Note {
    pub name: String,
    #[serde(flatten)]
    pub spec: NoteSpec,
    /// Counts every write and delete of the name, from 1.
    pub revision: u64,
    pub updated: i64,
    pub updated_by: Editor,
    #[serde(default)]
    pub provenance: Provenance,
    /// The last revision a person approved: this one, an earlier one while
    /// a later one waits, or none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approved: Option<Approved>,
}

impl Note {
    pub fn summary(&self) -> NoteSummary {
        NoteSummary {
            name: self.name.clone(),
            description: self.spec.description.clone(),
            tags: self.spec.tags.clone(),
            on_demand: self.spec.on_demand,
            revision: self.revision,
            updated: self.updated,
            updated_by: self.updated_by,
            approved: self.approved.as_ref().map(|approved| ApprovedSummary {
                revision: approved.revision,
                description: approved.spec.description.clone(),
                tags: approved.spec.tags.clone(),
                on_demand: approved.spec.on_demand,
            }),
        }
    }

    /// Whether the latest revision waits for a person to approve it.
    pub fn awaits_review(&self) -> bool {
        self.approved
            .as_ref()
            .is_none_or(|approved| approved.revision != self.revision)
    }
}

/// One write or delete of a note, kept for good.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Revision {
    pub revision: u64,
    /// The note as written; for a delete, as it was before.
    #[serde(flatten)]
    pub spec: NoteSpec,
    pub updated: i64,
    pub updated_by: Editor,
    #[serde(default)]
    pub provenance: Provenance,
    /// Whether this revision deleted the note.
    #[serde(default)]
    pub deleted: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum KnowledgeError {
    #[error("cannot reach the gateway: {0}")]
    Unreachable(String),
    #[error("{}", .0.join("; "))]
    Refused(Vec<String>),
    /// No such note, or the gateway does not know the agent.
    #[error("no such note")]
    NotFound,
    #[error("{}", conflict_message(.0))]
    Conflict(Box<Option<Note>>),
    #[error("too many writes for now; try again later")]
    TooMany,
    /// The gateway does not take the token, or does not let this caller do
    /// this.
    #[error("the gateway refused: {0}")]
    Denied(String),
    #[error("the gateway failed: {0}")]
    Failed(String),
    #[error("the gateway sent something that cannot be read: {0}")]
    Malformed(String),
}

impl From<wire::Malformed> for KnowledgeError {
    fn from(error: wire::Malformed) -> Self {
        Self::Malformed(error.to_string())
    }
}

impl From<tonic::Status> for KnowledgeError {
    fn from(status: tonic::Status) -> Self {
        match grpc::Failure::from(status) {
            grpc::Failure::Unreachable(why) => Self::Unreachable(why),
            grpc::Failure::NotFound => Self::NotFound,
            grpc::Failure::Denied(why) => Self::Denied(why),
            grpc::Failure::TooMany => Self::TooMany,
            grpc::Failure::NotRunning => Self::Failed("the workspace is not running".to_owned()),
            grpc::Failure::Failed(why) => Self::Failed(why),
        }
    }
}

/// Who the gateway is asked as.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Asker {
    /// A person, through the gateway agent with this id.
    Agent(String),
    /// An agent pod, by its relay's token.
    Workspace,
}

/// The knowledge of one scope on a gateway.
#[derive(Debug, Clone)]
pub struct KnowledgeEndpoint {
    channel: Channel,
    root: String,
    asker: Asker,
}

impl PartialEq for KnowledgeEndpoint {
    fn eq(&self, other: &Self) -> bool {
        self.root == other.root && self.asker == other.asker
    }
}

impl KnowledgeEndpoint {
    /// The knowledge of `spec`'s agent's scope, asked for with the agent's
    /// headers, or `None` when it is not a gateway agent: nothing else
    /// keeps knowledge.
    pub fn resolve(
        spec: &AgentSpec,
        lookup: impl Fn(Source, &str) -> Option<String> + Copy,
    ) -> Result<Option<Self>, ResolveError> {
        let AgentSpec::Gateway { url, headers, .. } = spec else {
            return Ok(None);
        };
        let Some((root, agent)) = grpc::agent_root(&ukai_config::interpolate(url, lookup)?) else {
            return Ok(None);
        };
        let client = reqwest::Client::builder()
            .default_headers(header_map(headers, &[], lookup)?)
            .timeout(TIMEOUT)
            .build()
            .map_err(|error| ResolveError::Client(error.to_string()))?;
        Ok(Some(Self {
            channel: grpc::channel(client, root.clone()),
            root,
            asker: Asker::Agent(agent),
        }))
    }

    /// What an agent pod uses: the gateway at `gateway` (e.g.
    /// `http://ukai-gateway`), with its relay's `token`.
    pub fn for_workspace(gateway: &str, token: &str) -> Result<Self, ResolveError> {
        let mut headers = reqwest::header::HeaderMap::new();
        let mut value = reqwest::header::HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|_| ResolveError::InvalidHeader("authorization".to_owned()))?;
        value.set_sensitive(true);
        headers.insert(reqwest::header::AUTHORIZATION, value);
        let client = reqwest::Client::builder()
            .default_headers(headers)
            .timeout(TIMEOUT)
            .build()
            .map_err(|error| ResolveError::Client(error.to_string()))?;
        let root = gateway.trim_end_matches('/').to_owned();
        Ok(Self {
            channel: grpc::channel(client, root.clone()),
            root,
            asker: Asker::Workspace,
        })
    }

    /// The gateway this reaches.
    pub fn root(&self) -> &str {
        &self.root
    }

    fn person(&self) -> KnowledgeClient<Channel> {
        KnowledgeClient::new(self.channel.clone())
    }

    fn pod(&self) -> WorkspaceKnowledgeClient<Channel> {
        WorkspaceKnowledgeClient::new(self.channel.clone())
    }

    /// Every note of the scope, by name.
    pub async fn list(&self) -> Result<Listing, KnowledgeError> {
        let listing = match &self.asker {
            Asker::Agent(agent) => {
                self.person()
                    .list(gw::AgentRequest {
                        agent: agent.clone(),
                    })
                    .await
            }
            Asker::Workspace => self.pod().list(gw::WorkspaceListRequest {}).await,
        }?
        .into_inner();
        Ok(Listing {
            scope: listing.scope,
            notes: listing
                .notes
                .into_iter()
                .map(NoteSummary::try_from)
                .collect::<Result<_, _>>()?,
        })
    }

    pub async fn get(&self, name: &str) -> Result<Note, KnowledgeError> {
        let name = name.to_owned();
        let response = match &self.asker {
            Asker::Agent(agent) => {
                self.person()
                    .get(gw::NoteRequest {
                        agent: agent.clone(),
                        name,
                    })
                    .await
            }
            Asker::Workspace => self.pod().get(gw::WorkspaceNoteRequest { name }).await,
        }?
        .into_inner();
        Ok(Note::try_from(
            response.note.ok_or(KnowledgeError::NotFound)?,
        )?)
    }

    /// Create or replace `name`, if it is still at `write.base_revision`.
    pub async fn put(&self, name: &str, write: &NoteWrite) -> Result<Note, KnowledgeError> {
        let name = name.to_owned();
        let spec = Some(pb::NoteSpec::from(write.spec.clone()));
        let response = match &self.asker {
            Asker::Agent(agent) => {
                self.person()
                    .put(gw::PutRequest {
                        agent: agent.clone(),
                        name,
                        spec,
                        base_revision: write.base_revision,
                    })
                    .await
            }
            Asker::Workspace => {
                self.pod()
                    .put(gw::WorkspacePutRequest {
                        name,
                        spec,
                        base_revision: write.base_revision,
                        commit: write.commit.clone(),
                    })
                    .await
            }
        }?;
        changed(response.into_inner())?.ok_or(KnowledgeError::NotFound)
    }

    /// Delete `name`, if it is still at `revision`.
    pub async fn delete(&self, name: &str, revision: u64) -> Result<(), KnowledgeError> {
        let name = name.to_owned();
        let response = match &self.asker {
            Asker::Agent(agent) => {
                self.person()
                    .delete(gw::RevisionRequest {
                        agent: agent.clone(),
                        name,
                        revision,
                    })
                    .await
            }
            Asker::Workspace => {
                self.pod()
                    .delete(gw::WorkspaceRevisionRequest { name, revision })
                    .await
            }
        }?;
        changed(response.into_inner()).map(drop)
    }

    /// Approve `name` as it is at `revision`, which must be its latest. Only
    /// a person can, through an agent; an agent pod cannot.
    pub async fn approve(&self, name: &str, revision: u64) -> Result<Note, KnowledgeError> {
        let request = self.review(name, revision)?;
        let response = self.person().approve(request).await?;
        changed(response.into_inner())?.ok_or(KnowledgeError::NotFound)
    }

    /// Reject `name`'s latest revision, `revision`, which an agent wrote and
    /// no person approved: the note goes back to the approved revision, or
    /// is deleted when none was, and then `None`. Only a person can.
    pub async fn reject(&self, name: &str, revision: u64) -> Result<Option<Note>, KnowledgeError> {
        let request = self.review(name, revision)?;
        let response = self.person().reject(request).await?;
        changed(response.into_inner())
    }

    fn review(&self, name: &str, revision: u64) -> Result<gw::RevisionRequest, KnowledgeError> {
        match &self.asker {
            Asker::Agent(agent) => Ok(gw::RevisionRequest {
                agent: agent.clone(),
                name: name.to_owned(),
                revision,
            }),
            Asker::Workspace => Err(KnowledgeError::Denied(
                "only a person reviews notes".to_owned(),
            )),
        }
    }

    /// Every write and delete of `name`, newest first; none for a name
    /// never written.
    pub async fn revisions(&self, name: &str) -> Result<Vec<Revision>, KnowledgeError> {
        let name = name.to_owned();
        let response = match &self.asker {
            Asker::Agent(agent) => {
                self.person()
                    .revisions(gw::NoteRequest {
                        agent: agent.clone(),
                        name,
                    })
                    .await
            }
            Asker::Workspace => {
                self.pod()
                    .revisions(gw::WorkspaceNoteRequest { name })
                    .await
            }
        }?;
        Ok(response
            .into_inner()
            .revisions
            .into_iter()
            .map(Revision::try_from)
            .collect::<Result<_, _>>()?)
    }
}

/// What a change came to: the note as it is now (`None` once deleted), or
/// why there is none.
fn changed(response: pb::ChangeResponse) -> Result<Option<Note>, KnowledgeError> {
    match response.outcome {
        Some(Outcome::Changed(changed)) => Ok(changed.note.map(Note::try_from).transpose()?),
        Some(Outcome::Conflict(conflict)) => Err(KnowledgeError::Conflict(Box::new(
            conflict.current.map(Note::try_from).transpose()?,
        ))),
        Some(Outcome::Refused(refused)) => Err(KnowledgeError::Refused(refused.problems)),
        None => Err(wire::Malformed("outcome").into()),
    }
}

/// A field notes written before it existed have as `null`, read as its
/// default.
fn or_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(Option::unwrap_or_default)
}

fn conflict_message(current: &Option<Note>) -> String {
    match current {
        Some(note) => format!(
            "the note changed since it was read: it is at revision {} now",
            note.revision
        ),
        None => "the note changed since it was read: it was deleted".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use rstest::rstest;
    use ukai_config::WorkspaceScope;

    use super::*;

    #[test]
    fn resolves_a_gateway_agents_knowledge() {
        let spec = AgentSpec::Gateway {
            url: "wss://gw.example.com/agents/claude/acp".into(),
            headers: BTreeMap::from([("Authorization".into(), "Bearer t".into())]),
            workspace: WorkspaceScope::Pod,
        };
        let endpoint = KnowledgeEndpoint::resolve(&spec, |_, _| None)
            .unwrap_or_else(|e| panic!("{e}"))
            .unwrap_or_else(|| panic!("a gateway agent has knowledge"));
        assert_eq!(endpoint.root(), "https://gw.example.com");
        assert_eq!(endpoint.asker, Asker::Agent("claude".into()));
    }

    #[rstest]
    #[case::stdio(AgentSpec::Stdio { command: "agent".into(), args: Vec::new(), env: BTreeMap::new() })]
    #[case::websocket(AgentSpec::Websocket { url: "wss://example.com/acp".into(), headers: BTreeMap::new() })]
    fn other_transports_have_no_knowledge(#[case] spec: AgentSpec) {
        assert!(matches!(
            KnowledgeEndpoint::resolve(&spec, |_, _| None),
            Ok(None)
        ));
    }

    #[rstest]
    #[case::bare("http://ukai-gateway")]
    #[case::trailing_slash("http://ukai-gateway/")]
    fn a_workspace_reaches_the_gateway_inside_the_cluster(#[case] gateway: &str) {
        let endpoint =
            KnowledgeEndpoint::for_workspace(gateway, "t0k").unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(endpoint.root(), "http://ukai-gateway");
        assert_eq!(endpoint.asker, Asker::Workspace);
    }

    #[test]
    fn a_note_reads_back_flat() {
        let note = Note {
            name: "build".into(),
            spec: NoteSpec {
                description: "How to build on macOS".into(),
                body: "Set `GGML_NATIVE=OFF`.".into(),
                tags: vec!["build".into()],
                on_demand: false,
            },
            revision: 3,
            updated: 1,
            updated_by: Editor::Agent,
            provenance: Provenance {
                workspace: Some("0a1b2c3d4e5f".into()),
                commit: None,
            },
            approved: None,
        };
        assert!(note.awaits_review());
        let json = serde_json::to_value(&note).unwrap_or_default();
        assert_eq!(json["description"], "How to build on macOS");
        assert_eq!(json["updated_by"], "agent");
        assert_eq!(json["tags"], serde_json::json!(["build"]));
        assert_eq!(json.get("on_demand"), None);
        assert_eq!(
            json["provenance"],
            serde_json::json!({ "workspace": "0a1b2c3d4e5f" })
        );
        assert_eq!(serde_json::from_value::<Note>(json).ok(), Some(note));
    }

    #[test]
    fn a_note_written_before_tags_reads_with_none() {
        let note: NoteSummary = serde_json::from_value(serde_json::json!({
            "name": "build", "description": "d", "tags": null, "on_demand": null,
            "revision": 1, "updated": 0, "updated_by": "user",
        }))
        .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!((note.tags, note.on_demand), (Vec::new(), false));
    }

    #[test]
    fn a_write_without_a_base_creates() {
        let write: NoteWrite =
            serde_json::from_value(serde_json::json!({ "description": "d", "body": "b" }))
                .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(write.base_revision, 0);
        assert_eq!(write.commit, None);
        assert_eq!(write.spec.tags, Vec::<String>::new());
        assert!(!write.spec.on_demand);
    }
}
