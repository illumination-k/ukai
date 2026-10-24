//! Knowledge a gateway keeps outside the repositories it is about: the wire
//! types, and the client side of `/agents/<id>/knowledge`.
//!
//! A note is a named piece of Markdown with a one-line description: a
//! convention, a decision, a pitfall, something learnt about the codebase
//! that should outlive one session without being committed to it. The
//! gateway keeps notes per scope: a project its `gateway.toml` names, which
//! agents on several repositories share, or else each agent's repository.
//! The app
//! reads and edits them through an agent, and agents in its pods through the
//! relay's MCP server, with the relay's token at [`WORKSPACE_KNOWLEDGE_PATH`].
//! What a valid note is, is `ukai_core::knowledge`'s business, shared by
//! both ends.
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

use crate::{ResolveError, header_map};

pub mod wire;

/// Suffix of an agent's ACP endpoint that lists the knowledge of its scope.
/// One note is `<it>/<name>`.
pub const KNOWLEDGE_PATH: &str = "/knowledge";

/// Where an agent pod reaches the knowledge of its own scope on the gateway,
/// authenticated with its relay's token.
pub const WORKSPACE_KNOWLEDGE_PATH: &str = "/workspace/knowledge";

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

/// The body of a 409: the note as it is now, `None` when it is gone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Conflict {
    pub current: Option<Note>,
}

/// The body of a 422: every reason a note was refused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Refusal {
    pub problems: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum KnowledgeError {
    #[error("cannot reach the gateway: {0}")]
    Unreachable(String),
    #[error("{}", .0.join("; "))]
    Refused(Vec<String>),
    #[error("no such note")]
    NotFound,
    #[error("{}", conflict_message(.0))]
    Conflict(Box<Option<Note>>),
    #[error("too many writes for now; try again later")]
    TooMany,
    #[error("the gateway answered {0}")]
    Status(u16),
    #[error("the gateway sent something that cannot be read: {0}")]
    Body(String),
}

/// The knowledge of one scope on a gateway.
#[derive(Debug, Clone)]
pub struct KnowledgeEndpoint {
    client: reqwest::Client,
    base: String,
}

impl PartialEq for KnowledgeEndpoint {
    fn eq(&self, other: &Self) -> bool {
        self.base == other.base
    }
}

impl KnowledgeEndpoint {
    /// The knowledge of `spec`'s agent's scope, or `None` when it is
    /// not a gateway agent: nothing else keeps knowledge.
    pub fn resolve(
        spec: &AgentSpec,
        lookup: impl Fn(Source, &str) -> Option<String> + Copy,
    ) -> Result<Option<Self>, ResolveError> {
        let AgentSpec::Gateway { url, headers, .. } = spec else {
            return Ok(None);
        };
        let Some(base) =
            crate::reports::endpoint_base(&ukai_config::interpolate(url, lookup)?, KNOWLEDGE_PATH)
        else {
            return Ok(None);
        };
        let client = reqwest::Client::builder()
            .default_headers(header_map(headers, &[], lookup)?)
            .timeout(TIMEOUT)
            .build()
            .map_err(|error| ResolveError::Client(error.to_string()))?;
        Ok(Some(Self { client, base }))
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
        let base = format!(
            "{}{WORKSPACE_KNOWLEDGE_PATH}",
            gateway.trim_end_matches('/')
        );
        Ok(Self { client, base })
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    /// Every note of the scope, by name.
    pub async fn list(&self) -> Result<Listing, KnowledgeError> {
        self.call(self.client.get(&self.base)).await
    }

    pub async fn get(&self, name: &str) -> Result<Note, KnowledgeError> {
        self.call(self.client.get(self.url(name, ""))).await
    }

    /// Create or replace `name`, if it is still at `write.base_revision`.
    pub async fn put(&self, name: &str, write: &NoteWrite) -> Result<Note, KnowledgeError> {
        self.call(self.client.put(self.url(name, "")).json(write))
            .await
    }

    /// Delete `name`, if it is still at `revision`.
    pub async fn delete(&self, name: &str, revision: u64) -> Result<(), KnowledgeError> {
        let url = format!("{}?revision={revision}", self.url(name, ""));
        self.call::<serde_json::Value>(self.client.delete(url))
            .await
            .map(drop)
    }

    /// Approve `name` as it is at `revision`, which must be its latest. Only
    /// a client of the gateway can; an agent pod cannot.
    pub async fn approve(&self, name: &str, revision: u64) -> Result<Note, KnowledgeError> {
        let url = format!("{}?revision={revision}", self.url(name, "/approve"));
        self.call(self.client.post(url)).await
    }

    /// Reject `name`'s latest revision, `revision`, which an agent wrote and
    /// no person approved: the note goes back to the approved revision, or
    /// is deleted when none was, and then `None`. Only a client of the
    /// gateway can.
    pub async fn reject(&self, name: &str, revision: u64) -> Result<Option<Note>, KnowledgeError> {
        let url = format!("{}?revision={revision}", self.url(name, "/reject"));
        self.call(self.client.post(url)).await
    }

    /// Every write and delete of `name`, newest first.
    pub async fn revisions(&self, name: &str) -> Result<Vec<Revision>, KnowledgeError> {
        self.call(self.client.get(self.url(name, "/revisions")))
            .await
    }

    fn url(&self, name: &str, suffix: &str) -> String {
        format!("{}/{}{suffix}", self.base, percent_encode(name))
    }

    async fn call<T: serde::de::DeserializeOwned>(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<T, KnowledgeError> {
        let response = request
            .send()
            .await
            .map_err(|error| KnowledgeError::Unreachable(error.to_string()))?;
        match response.status().as_u16() {
            200 | 201 => response
                .json::<T>()
                .await
                .map_err(|error| KnowledgeError::Body(error.to_string())),
            404 => Err(KnowledgeError::NotFound),
            409 => Err(match response.json::<Conflict>().await {
                Ok(conflict) => KnowledgeError::Conflict(Box::new(conflict.current)),
                Err(error) => KnowledgeError::Body(error.to_string()),
            }),
            429 => Err(KnowledgeError::TooMany),
            422 => Err(match response.json::<Refusal>().await {
                Ok(refusal) => KnowledgeError::Refused(refusal.problems),
                Err(error) => KnowledgeError::Body(error.to_string()),
            }),
            status => Err(KnowledgeError::Status(status)),
        }
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

/// Valid names are lowercase letters, digits, and `-`; anything else is
/// escaped rather than trusted, and the gateway refuses it.
fn percent_encode(name: &str) -> String {
    name.bytes()
        .map(|byte| match byte {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' => (byte as char).to_string(),
            other => format!("%{other:02X}"),
        })
        .collect()
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
        assert_eq!(
            endpoint.base(),
            "https://gw.example.com/agents/claude/knowledge"
        );
        assert_eq!(
            endpoint.url("a/../b", "/revisions"),
            "https://gw.example.com/agents/claude/knowledge/a%2F%2E%2E%2Fb/revisions"
        );
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
        assert_eq!(endpoint.base(), "http://ukai-gateway/workspace/knowledge");
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
