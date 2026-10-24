//! The notes, in SurrealDB: what `ukai-knowledge` keeps, so what is learnt
//! about a codebase outlives a session without being committed to it.
//!
//! Notes live in SurrealDB (namespace [`NAMESPACE`], database [`DATABASE`]),
//! a server of its own (`deploy/gateway`), reached over WebSocket. They are
//! kept per scope: a project `gateway.toml` names, which agents on several
//! repositories share, or else an agent's repository; the gateway says
//! which. Table `note` holds each note as it is now, under the record id
//! `[scope, name]`; table `note_revision` keeps every write and delete for
//! good. A delete only marks the note, so it can be written again and its
//! history stays. Each change is one transaction that first checks the
//! writer's base revision, so two writers never silently overwrite each
//! other. A note also keeps the last revision a person approved
//! (`approved`): a person's writes are approved as they are made, an
//! agent's wait, and an agent cannot delete a note a person approved. A
//! person can reject an agent's revision instead of approving it, which
//! writes the approved revision back, or deletes a note that has none.
//!
//! What is wrong with a note is `ukai_core::knowledge`'s business; this
//! module stores.

use serde_json::{Value, json};
use surrealdb::Surreal;
use surrealdb::engine::any::Any;
use surrealdb::opt::auth::Root;
use ukai_core::knowledge::{Editor, Note, NoteSpec, NoteSummary, NoteWrite, Provenance, Revision};

pub const NAMESPACE: &str = "ukai";
pub const DATABASE: &str = "knowledge";

/// Applied on every connection; each statement leaves what exists alone.
/// Fields added later are optional, since notes written before lack them.
const SCHEMA: &str = "
DEFINE TABLE IF NOT EXISTS note SCHEMAFULL;
DEFINE FIELD IF NOT EXISTS scope ON note TYPE string;
DEFINE FIELD IF NOT EXISTS name ON note TYPE string;
DEFINE FIELD IF NOT EXISTS description ON note TYPE string;
DEFINE FIELD IF NOT EXISTS body ON note TYPE string;
DEFINE FIELD IF NOT EXISTS tags ON note TYPE option<array<string>>;
DEFINE FIELD IF NOT EXISTS on_demand ON note TYPE option<bool>;
DEFINE FIELD IF NOT EXISTS revision ON note TYPE int;
DEFINE FIELD IF NOT EXISTS updated ON note TYPE int;
DEFINE FIELD IF NOT EXISTS updated_by ON note TYPE string;
DEFINE FIELD IF NOT EXISTS provenance ON note TYPE object;
DEFINE FIELD IF NOT EXISTS provenance.workspace ON note TYPE option<string>;
DEFINE FIELD IF NOT EXISTS provenance.commit ON note TYPE option<string>;
DEFINE FIELD IF NOT EXISTS deleted ON note TYPE bool;
DEFINE FIELD IF NOT EXISTS approved ON note TYPE option<object>;
DEFINE FIELD IF NOT EXISTS approved.revision ON note TYPE option<int>;
DEFINE FIELD IF NOT EXISTS approved.description ON note TYPE option<string>;
DEFINE FIELD IF NOT EXISTS approved.body ON note TYPE option<string>;
DEFINE FIELD IF NOT EXISTS approved.tags ON note TYPE option<array<string>>;
DEFINE FIELD IF NOT EXISTS approved.on_demand ON note TYPE option<bool>;
DEFINE FIELD IF NOT EXISTS approved.updated ON note TYPE option<int>;
DEFINE FIELD IF NOT EXISTS approved.updated_by ON note TYPE option<string>;
DEFINE FIELD IF NOT EXISTS approved.provenance ON note TYPE option<object>;
DEFINE FIELD IF NOT EXISTS approved.provenance.workspace ON note TYPE option<string>;
DEFINE FIELD IF NOT EXISTS approved.provenance.commit ON note TYPE option<string>;
DEFINE INDEX IF NOT EXISTS note_scope ON note FIELDS scope;
DEFINE TABLE IF NOT EXISTS note_revision SCHEMAFULL;
DEFINE FIELD IF NOT EXISTS scope ON note_revision TYPE string;
DEFINE FIELD IF NOT EXISTS name ON note_revision TYPE string;
DEFINE FIELD IF NOT EXISTS description ON note_revision TYPE string;
DEFINE FIELD IF NOT EXISTS body ON note_revision TYPE string;
DEFINE FIELD IF NOT EXISTS tags ON note_revision TYPE option<array<string>>;
DEFINE FIELD IF NOT EXISTS on_demand ON note_revision TYPE option<bool>;
DEFINE FIELD IF NOT EXISTS revision ON note_revision TYPE int;
DEFINE FIELD IF NOT EXISTS updated ON note_revision TYPE int;
DEFINE FIELD IF NOT EXISTS updated_by ON note_revision TYPE string;
DEFINE FIELD IF NOT EXISTS provenance ON note_revision TYPE object;
DEFINE FIELD IF NOT EXISTS provenance.workspace ON note_revision TYPE option<string>;
DEFINE FIELD IF NOT EXISTS provenance.commit ON note_revision TYPE option<string>;
DEFINE FIELD IF NOT EXISTS deleted ON note_revision TYPE bool;
DEFINE INDEX IF NOT EXISTS note_revision_note ON note_revision FIELDS scope, name;
";

/// The fields of a note, without the record id, which is not JSON's.
const FIELDS: &str =
    "name, description, body, tags, on_demand, revision, updated, updated_by, provenance, approved";

/// What a transaction throws when the writer's base revision is not the
/// note's current one.
const CONFLICT: &str = "ukai:conflict";

/// Write or delete `$id` as one revision, if it is still at `$base`.
///
/// `$current` is the revision a writer must name: 0 when there is no note
/// or it is deleted. Revisions keep counting across a delete, so every
/// `note_revision` of a name has its own number. With `$approve` (a
/// person's write) the new revision is the approved one; otherwise the
/// approved one stays, and a delete clears it. With `$pending`, `$base` must
/// also not be the approved revision: a rejection, which must not undo an
/// approval made meanwhile.
const CHANGE: &str = "
BEGIN TRANSACTION;
LET $id = type::record('note', [$scope, $name]);
LET $existing = (SELECT * FROM ONLY $id);
LET $last = IF $existing = NONE { 0 } ELSE { $existing.revision };
LET $current = IF $existing = NONE OR $existing.deleted { 0 } ELSE { $existing.revision };
IF $current != $base OR ($deleted AND $current = 0) { THROW 'ukai:conflict' };
IF $pending AND $existing.approved.revision = $current { THROW 'ukai:conflict' };
LET $description = IF $deleted { $existing.description } ELSE { $description };
LET $body = IF $deleted { $existing.body } ELSE { $body };
LET $tags = IF $deleted { $existing.tags } ELSE { $tags };
LET $on_demand = IF $deleted { $existing.on_demand } ELSE { $on_demand };
LET $fields = {
    scope: $scope,
    name: $name,
    description: $description,
    body: $body,
    tags: $tags,
    on_demand: $on_demand,
    revision: $last + 1,
    updated: $updated,
    updated_by: $updated_by,
    provenance: $provenance,
    deleted: $deleted,
};
LET $approved = IF $deleted {
    NONE
} ELSE IF $approve {
    {
        revision: $last + 1,
        description: $description,
        body: $body,
        tags: $tags,
        on_demand: $on_demand,
        updated: $updated,
        updated_by: $updated_by,
        provenance: $provenance,
    }
} ELSE {
    $existing.approved
};
UPSERT $id CONTENT $fields RETURN NONE;
UPDATE $id SET approved = $approved RETURN NONE;
CREATE note_revision CONTENT $fields RETURN NONE;
COMMIT TRANSACTION;
";

/// Approve `$id` as it is, if it is still at `$base`.
const APPROVE: &str = "
BEGIN TRANSACTION;
LET $id = type::record('note', [$scope, $name]);
LET $existing = (SELECT * FROM ONLY $id);
IF $existing = NONE OR $existing.deleted OR $existing.revision != $base { THROW 'ukai:conflict' };
UPDATE $id SET approved = {
    revision: $existing.revision,
    description: $existing.description,
    body: $existing.body,
    tags: $existing.tags,
    on_demand: $existing.on_demand,
    updated: $existing.updated,
    updated_by: $existing.updated_by,
    provenance: $existing.provenance,
} RETURN NONE;
COMMIT TRANSACTION;
";

#[derive(Debug, thiserror::Error)]
pub enum KnowledgeError {
    #[error("{}", .0.join("; "))]
    Refused(Vec<String>),
    /// The note is not at the revision the writer read; it is as given now.
    #[error("the note changed since it was read")]
    Conflict(Box<Option<Note>>),
    #[error("no such note")]
    NotFound,
    #[error("the knowledge store failed: {0}")]
    Store(#[from] surrealdb::Error),
    #[error("the knowledge store returned something that cannot be read: {0}")]
    Unreadable(#[from] serde_json::Error),
}

/// Where the store is.
#[derive(Clone)]
pub struct StoreConfig {
    /// `ws://host:port`, or `mem://` for one that lives with the process.
    pub url: String,
    /// The root user and password, from a Secret. Never logged.
    pub credentials: Option<(String, String)>,
}

impl std::fmt::Debug for StoreConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StoreConfig")
            .field("url", &self.url)
            .field("credentials", &self.credentials.as_ref().map(|_| "…"))
            .finish()
    }
}

/// One write or delete, as [`Knowledge::change`] makes it.
struct Change<'a> {
    scope: &'a str,
    name: &'a str,
    spec: Option<NoteSpec>,
    base: u64,
    editor: Editor,
    provenance: Provenance,
    /// Only if `base` is not approved: see [`CHANGE`].
    pending: bool,
}

/// The notes, by scope and name.
pub struct Knowledge {
    db: Surreal<Any>,
}

impl Knowledge {
    /// Connect, sign in, and make sure the schema is there. A WebSocket
    /// connection reconnects by itself once made.
    pub async fn connect(config: &StoreConfig) -> Result<Self, KnowledgeError> {
        let db = surrealdb::engine::any::connect(config.url.as_str()).await?;
        if let Some((username, password)) = &config.credentials {
            db.signin(Root {
                username: username.clone(),
                password: password.clone(),
            })
            .await?;
        }
        db.use_ns(NAMESPACE).use_db(DATABASE).await?;
        db.query(SCHEMA).await?.check()?;
        Ok(Self { db })
    }

    /// `scope`'s notes, by name; deleted ones are not listed.
    pub async fn list(&self, scope: &str) -> Result<Vec<NoteSummary>, KnowledgeError> {
        let rows: Vec<Value> = self
            .db
            .query(format!(
                "SELECT {FIELDS} FROM note WHERE scope = $scope AND !deleted ORDER BY name"
            ))
            .bind(json!({ "scope": scope }))
            .await?
            .check()?
            .take(0)?;
        rows.into_iter()
            .map(|row| Ok(serde_json::from_value::<Note>(row)?.summary()))
            .collect()
    }

    /// The note as it is now; `None` when there is none or it is deleted.
    pub async fn get(&self, scope: &str, name: &str) -> Result<Option<Note>, KnowledgeError> {
        let rows: Vec<Value> = self
            .db
            .query(format!(
                "SELECT {FIELDS} FROM type::record('note', [$scope, $name]) WHERE !deleted"
            ))
            .bind(json!({ "scope": scope, "name": name }))
            .await?
            .check()?
            .take(0)?;
        rows.into_iter()
            .next()
            .map(serde_json::from_value)
            .transpose()
            .map_err(Into::into)
    }

    /// Every write and delete of `name` in `scope`, newest first.
    pub async fn revisions(
        &self,
        scope: &str,
        name: &str,
    ) -> Result<Vec<Revision>, KnowledgeError> {
        let rows: Vec<Value> = self
            .db
            .query(
                "SELECT revision, description, body, tags, on_demand, updated, updated_by, \
                 provenance, deleted \
                 FROM note_revision WHERE scope = $scope AND name = $name \
                 ORDER BY revision DESC",
            )
            .bind(json!({ "scope": scope, "name": name }))
            .await?
            .check()?
            .take(0)?;
        rows.into_iter()
            .map(|row| serde_json::from_value(row).map_err(Into::into))
            .collect()
    }

    /// Create or replace `name` in `scope`, if it is still at
    /// `write.base_revision`.
    pub async fn put(
        &self,
        scope: &str,
        name: &str,
        write: NoteWrite,
        editor: Editor,
        workspace: Option<String>,
    ) -> Result<Note, KnowledgeError> {
        let spec = NoteSpec {
            tags: ukai_core::knowledge::normalize_tags(write.spec.tags),
            ..write.spec
        };
        let problems = ukai_core::knowledge::problems(name, &spec);
        if !problems.is_empty() {
            return Err(KnowledgeError::Refused(
                problems.iter().map(ToString::to_string).collect(),
            ));
        }
        let spec = NoteSpec {
            description: spec.description.trim().to_owned(),
            ..spec
        };
        self.change(Change {
            scope,
            name,
            spec: Some(spec),
            base: write.base_revision,
            editor,
            provenance: Provenance {
                workspace,
                commit: write.commit.filter(|commit| is_commit(commit)),
            },
            pending: false,
        })
        .await?;
        self.get(scope, name).await?.ok_or(KnowledgeError::NotFound)
    }

    /// Delete `name` from `scope`, if it is still at `revision`. Its
    /// revisions stay, and so does a revision recording the delete.
    pub async fn delete(
        &self,
        scope: &str,
        name: &str,
        revision: u64,
        editor: Editor,
        workspace: Option<String>,
    ) -> Result<(), KnowledgeError> {
        let Some(note) = self.get(scope, name).await? else {
            return Err(KnowledgeError::NotFound);
        };
        if editor == Editor::Agent && note.approved.is_some() {
            return Err(KnowledgeError::Refused(vec![
                "a person approved this note, so only a person can delete it; tell the user \
                 why it should go"
                    .to_owned(),
            ]));
        }
        self.change(Change {
            scope,
            name,
            spec: None,
            base: revision,
            editor,
            provenance: Provenance {
                workspace,
                commit: None,
            },
            pending: false,
        })
        .await
    }

    /// Reject `name`'s latest revision, `revision`, which no person
    /// approved: write the approved revision back as a person's, or delete
    /// the note when nothing was approved. The rejected revision stays in
    /// the history. Only a person can, like [`Knowledge::approve`].
    pub async fn reject(
        &self,
        scope: &str,
        name: &str,
        revision: u64,
    ) -> Result<Option<Note>, KnowledgeError> {
        let Some(note) = self.get(scope, name).await? else {
            return Err(KnowledgeError::NotFound);
        };
        if note.revision != revision {
            return Err(KnowledgeError::Conflict(Box::new(Some(note))));
        }
        if !note.awaits_review() {
            return Err(KnowledgeError::Refused(vec![
                "a person approved this revision; edit or delete the note instead".to_owned(),
            ]));
        }
        self.change(Change {
            scope,
            name,
            spec: note.approved.map(|approved| approved.spec),
            base: revision,
            editor: Editor::User,
            provenance: Provenance::default(),
            pending: true,
        })
        .await?;
        self.get(scope, name).await
    }

    /// Approve `name` in `scope` as it is at `revision`, which must be
    /// its latest. Only a person can: the service refuses an agent.
    pub async fn approve(
        &self,
        scope: &str,
        name: &str,
        revision: u64,
    ) -> Result<Note, KnowledgeError> {
        if self.get(scope, name).await?.is_none() {
            return Err(KnowledgeError::NotFound);
        }
        self.run(
            APPROVE,
            json!({ "scope": scope, "name": name, "base": revision }),
            scope,
            name,
        )
        .await?;
        self.get(scope, name).await?.ok_or(KnowledgeError::NotFound)
    }

    async fn change(&self, change: Change<'_>) -> Result<(), KnowledgeError> {
        let deleted = change.spec.is_none();
        let spec = change.spec.unwrap_or(NoteSpec {
            description: String::new(),
            body: String::new(),
            tags: Vec::new(),
            on_demand: false,
        });
        let vars = json!({
            "scope": change.scope,
            "name": change.name,
            "description": spec.description,
            "body": spec.body,
            "tags": spec.tags,
            "on_demand": spec.on_demand,
            "base": change.base,
            "pending": change.pending,
            "deleted": deleted,
            "approve": change.editor == Editor::User,
            "updated": time::OffsetDateTime::now_utc().unix_timestamp(),
            "updated_by": serde_json::to_value(change.editor)?,
            "provenance": serde_json::to_value(&change.provenance)?,
        });
        self.run(CHANGE, vars, change.scope, change.name).await
    }

    /// Run a transaction on one note, telling a conflict apart.
    async fn run(
        &self,
        query: &str,
        vars: Value,
        scope: &str,
        name: &str,
    ) -> Result<(), KnowledgeError> {
        let mut response = self.db.query(query).bind(vars).await?;
        let errors = response.take_errors();
        if errors.is_empty() {
            return Ok(());
        }
        if errors
            .values()
            .any(|error| error.to_string().contains(CONFLICT))
        {
            let current = self.get(scope, name).await?;
            return Err(KnowledgeError::Conflict(Box::new(current)));
        }
        let first = errors
            .into_iter()
            .min_by_key(|(index, _)| *index)
            .map(|(_, error)| error);
        Err(first.map_or(KnowledgeError::NotFound, KnowledgeError::Store))
    }
}

/// Whether `commit` looks like a git object name; anything else a writer
/// says is not kept.
fn is_commit(commit: &str) -> bool {
    (7..=64).contains(&commit.len()) && commit.bytes().all(|b| b.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    const SCOPE: &str = "plekti";

    async fn store() -> Knowledge {
        Knowledge::connect(&StoreConfig {
            url: "mem://".into(),
            credentials: None,
        })
        .await
        .unwrap_or_else(|e| panic!("connect: {e}"))
    }

    fn write(description: &str, body: &str, base_revision: u64) -> NoteWrite {
        NoteWrite {
            spec: NoteSpec {
                description: description.into(),
                body: body.into(),
                tags: Vec::new(),
                on_demand: false,
            },
            base_revision,
            commit: None,
        }
    }

    #[tokio::test]
    async fn a_note_is_written_read_listed_and_deleted() -> Result<(), KnowledgeError> {
        let store = store().await;
        let written = store
            .put(
                SCOPE,
                "release-process",
                NoteWrite {
                    commit: Some("0123abcd".into()),
                    ..write("  How a release is cut ", "1. Tag `v*`.\n", 0)
                },
                Editor::Agent,
                Some("0a1b2c3d4e5f".into()),
            )
            .await?;
        assert_eq!(written.spec.description, "How a release is cut");
        assert_eq!(written.revision, 1);
        assert_eq!(
            written.provenance,
            Provenance {
                workspace: Some("0a1b2c3d4e5f".into()),
                commit: Some("0123abcd".into()),
            }
        );
        assert_eq!(
            store.get(SCOPE, "release-process").await?,
            Some(written.clone())
        );
        assert_eq!(store.list(SCOPE).await?, vec![written.summary()]);
        store
            .delete(SCOPE, "release-process", 1, Editor::User, None)
            .await?;
        assert_eq!(store.get(SCOPE, "release-process").await?, None);
        assert_eq!(store.list(SCOPE).await?, Vec::new());
        assert!(matches!(
            store
                .delete(SCOPE, "release-process", 2, Editor::User, None)
                .await,
            Err(KnowledgeError::NotFound)
        ));
        Ok(())
    }

    #[tokio::test]
    async fn every_change_is_kept_as_a_revision() -> Result<(), KnowledgeError> {
        let store = store().await;
        store
            .put(SCOPE, "build", write("Old", "old", 0), Editor::Agent, None)
            .await?;
        let new = store
            .put(SCOPE, "build", write("New", "new", 1), Editor::User, None)
            .await?;
        assert_eq!(new.revision, 2);
        store.delete(SCOPE, "build", 2, Editor::User, None).await?;
        let revisions = store.revisions(SCOPE, "build").await?;
        let summary: Vec<(u64, &str, Editor, bool)> = revisions
            .iter()
            .map(|r| (r.revision, r.spec.body.as_str(), r.updated_by, r.deleted))
            .collect();
        assert_eq!(
            summary,
            [
                (3, "new", Editor::User, true),
                (2, "new", Editor::User, false),
                (1, "old", Editor::Agent, false),
            ]
        );
        // A deleted name is written again from 0, and keeps counting.
        let again = store
            .put(
                SCOPE,
                "build",
                write("Again", "again", 0),
                Editor::Agent,
                None,
            )
            .await?;
        assert_eq!(again.revision, 4);
        Ok(())
    }

    #[rstest]
    #[case::create_over_existing(0)]
    #[case::stale(1)]
    #[case::ahead(9)]
    #[tokio::test]
    async fn a_write_from_a_stale_read_is_refused(#[case] base: u64) -> Result<(), KnowledgeError> {
        let store = store().await;
        store
            .put(SCOPE, "build", write("A", "a", 0), Editor::Agent, None)
            .await?;
        let current = store
            .put(SCOPE, "build", write("B", "b", 1), Editor::User, None)
            .await?;
        let result = store
            .put(SCOPE, "build", write("C", "c", base), Editor::Agent, None)
            .await;
        assert!(
            matches!(&result, Err(KnowledgeError::Conflict(now)) if **now == Some(current.clone())),
            "{result:?}"
        );
        assert_eq!(store.get(SCOPE, "build").await?, Some(current));
        assert_eq!(store.revisions(SCOPE, "build").await?.len(), 2);
        Ok(())
    }

    #[tokio::test]
    async fn a_stale_delete_is_refused() -> Result<(), KnowledgeError> {
        let store = store().await;
        store
            .put(SCOPE, "build", write("A", "a", 0), Editor::Agent, None)
            .await?;
        store
            .put(SCOPE, "build", write("B", "b", 1), Editor::User, None)
            .await?;
        let result = store.delete(SCOPE, "build", 1, Editor::User, None).await;
        assert!(
            matches!(result, Err(KnowledgeError::Conflict(_))),
            "{result:?}"
        );
        assert!(store.get(SCOPE, "build").await?.is_some());
        Ok(())
    }

    #[tokio::test]
    async fn scopes_keep_their_own_notes() -> Result<(), KnowledgeError> {
        let store = store().await;
        let other = "another-project";
        store
            .put(SCOPE, "build", write("R", "r", 0), Editor::User, None)
            .await?;
        store
            .put(other, "build", write("S", "s", 0), Editor::User, None)
            .await?;
        store
            .put(other, "a-first", write("S", "s", 0), Editor::User, None)
            .await?;
        let names = |list: Vec<NoteSummary>| list.into_iter().map(|n| n.name).collect::<Vec<_>>();
        assert_eq!(names(store.list(SCOPE).await?), ["build"]);
        assert_eq!(names(store.list(other).await?), ["a-first", "build"]);
        store.delete(other, "build", 1, Editor::User, None).await?;
        assert!(store.get(SCOPE, "build").await?.is_some());
        assert_eq!(store.revisions(SCOPE, "build").await?.len(), 1);
        Ok(())
    }

    #[rstest]
    #[case::bad_name("Bad Name", write("d", "b", 0))]
    #[case::empty("n", write("", "", 0))]
    #[case::invisible("n", write("d", "hidden\u{202E}", 0))]
    #[tokio::test]
    async fn refuses_what_is_wrong_with_a_note(#[case] name: &str, #[case] write: NoteWrite) {
        let store = store().await;
        let result = store.put(SCOPE, name, write, Editor::Agent, None).await;
        assert!(matches!(result, Err(KnowledgeError::Refused(problems)) if !problems.is_empty()));
        assert_eq!(store.list(SCOPE).await.map(|l| l.len()).ok(), Some(0));
    }

    #[rstest]
    #[case::short_sha("0123abc", true)]
    #[case::full_sha(&"a".repeat(40), true)]
    #[case::too_short("0123ab", false)]
    #[case::not_hex("main", false)]
    #[case::injection("abc1234; rm -rf /", false)]
    fn keeps_only_what_looks_like_a_commit(#[case] commit: &str, #[case] kept: bool) {
        assert_eq!(is_commit(commit), kept);
    }

    #[tokio::test]
    async fn a_persons_write_is_approved_and_an_agents_waits() -> Result<(), KnowledgeError> {
        let store = store().await;
        let by_person = store
            .put(SCOPE, "build", write("Person", "p", 0), Editor::User, None)
            .await?;
        assert!(!by_person.awaits_review());
        let by_agent = store
            .put(SCOPE, "build", write("Agent", "a", 1), Editor::Agent, None)
            .await?;
        assert!(by_agent.awaits_review());
        let approved = by_agent
            .approved
            .clone()
            .unwrap_or_else(|| panic!("the approved revision is kept"));
        assert_eq!((approved.revision, approved.spec.body.as_str()), (1, "p"));
        let listed = store.list(SCOPE).await?;
        assert_eq!(
            listed
                .first()
                .and_then(|n| n.approved.clone())
                .map(|a| a.description),
            Some("Person".to_owned())
        );
        assert!(listed.first().is_some_and(NoteSummary::awaits_review));
        // Approving the agent's revision makes it the approved one.
        let now = store.approve(SCOPE, "build", 2).await?;
        assert!(!now.awaits_review());
        assert_eq!(now.approved.map(|a| a.spec.body), Some("a".to_owned()));
        Ok(())
    }

    #[tokio::test]
    async fn an_agents_new_note_has_nothing_approved() -> Result<(), KnowledgeError> {
        let store = store().await;
        let note = store
            .put(SCOPE, "build", write("Agent", "a", 0), Editor::Agent, None)
            .await?;
        assert_eq!(note.approved, None);
        assert!(
            store
                .list(SCOPE)
                .await?
                .iter()
                .all(NoteSummary::awaits_review)
        );
        // An agent may take back what nobody approved.
        store.delete(SCOPE, "build", 1, Editor::Agent, None).await?;
        Ok(())
    }

    #[tokio::test]
    async fn only_a_person_deletes_an_approved_note() -> Result<(), KnowledgeError> {
        let store = store().await;
        store
            .put(SCOPE, "build", write("Person", "p", 0), Editor::User, None)
            .await?;
        let result = store.delete(SCOPE, "build", 1, Editor::Agent, None).await;
        assert!(
            matches!(result, Err(KnowledgeError::Refused(_))),
            "{result:?}"
        );
        store.delete(SCOPE, "build", 1, Editor::User, None).await?;
        // Written again by an agent, it starts unapproved.
        let again = store
            .put(SCOPE, "build", write("Agent", "a", 0), Editor::Agent, None)
            .await?;
        assert_eq!(again.approved, None);
        Ok(())
    }

    #[rstest]
    #[case::stale(1)]
    #[case::ahead(3)]
    #[tokio::test]
    async fn approving_a_revision_that_is_not_the_latest_is_refused(
        #[case] revision: u64,
    ) -> Result<(), KnowledgeError> {
        let store = store().await;
        store
            .put(SCOPE, "build", write("A", "a", 0), Editor::Agent, None)
            .await?;
        store
            .put(SCOPE, "build", write("B", "b", 1), Editor::Agent, None)
            .await?;
        let result = store.approve(SCOPE, "build", revision).await;
        assert!(
            matches!(result, Err(KnowledgeError::Conflict(_))),
            "{result:?}"
        );
        assert_eq!(
            store.get(SCOPE, "build").await?.and_then(|n| n.approved),
            None
        );
        Ok(())
    }

    #[tokio::test]
    async fn tags_and_on_demand_are_kept_with_each_revision() -> Result<(), KnowledgeError> {
        let store = store().await;
        let mut tagged = write("Papers on retrieval", "…", 0);
        tagged.spec.tags = vec!["research".into(), " papers ".into(), "research".into()];
        tagged.spec.on_demand = true;
        let note = store
            .put(SCOPE, "retrieval", tagged, Editor::User, None)
            .await?;
        assert_eq!(note.spec.tags, ["papers", "research"]);
        assert!(note.spec.on_demand);
        let approved = note.approved.clone().map(|a| a.spec);
        assert_eq!(approved, Some(note.spec.clone()));
        let listed = store.list(SCOPE).await?;
        assert_eq!(
            listed.first().map(|n| n.tags.clone()),
            Some(note.spec.tags.clone())
        );
        assert!(listed.first().is_some_and(|n| n.on_demand));
        assert!(
            listed
                .first()
                .and_then(|n| n.approved.as_ref())
                .is_some_and(|a| a.on_demand && a.tags == note.spec.tags)
        );
        // Untagged again, it is shown to every session again.
        let plain = store
            .put(
                SCOPE,
                "retrieval",
                write("Papers", "…", 1),
                Editor::User,
                None,
            )
            .await?;
        assert_eq!((plain.spec.tags.len(), plain.spec.on_demand), (0, false));
        let revisions = store.revisions(SCOPE, "retrieval").await?;
        assert_eq!(
            revisions
                .iter()
                .map(|r| r.spec.on_demand)
                .collect::<Vec<_>>(),
            [false, true]
        );
        Ok(())
    }

    #[tokio::test]
    async fn rejecting_writes_the_approved_revision_back() -> Result<(), KnowledgeError> {
        let store = store().await;
        store
            .put(SCOPE, "build", write("Person", "p", 0), Editor::User, None)
            .await?;
        store
            .put(SCOPE, "build", write("Agent", "a", 1), Editor::Agent, None)
            .await?;
        let now = store
            .reject(SCOPE, "build", 2)
            .await?
            .unwrap_or_else(|| panic!("the note stays"));
        assert_eq!(
            (now.revision, now.spec.body.as_str(), now.updated_by),
            (3, "p", Editor::User)
        );
        assert!(!now.awaits_review());
        let revisions = store.revisions(SCOPE, "build").await?;
        assert_eq!(
            revisions
                .iter()
                .map(|r| r.spec.body.as_str())
                .collect::<Vec<_>>(),
            ["p", "a", "p"]
        );
        Ok(())
    }

    #[tokio::test]
    async fn rejecting_a_note_nobody_approved_deletes_it() -> Result<(), KnowledgeError> {
        let store = store().await;
        store
            .put(SCOPE, "build", write("Agent", "a", 0), Editor::Agent, None)
            .await?;
        assert_eq!(store.reject(SCOPE, "build", 1).await?, None);
        assert_eq!(store.list(SCOPE).await?, Vec::new());
        let revisions = store.revisions(SCOPE, "build").await?;
        assert_eq!(
            revisions
                .iter()
                .map(|r| (r.revision, r.updated_by, r.deleted))
                .collect::<Vec<_>>(),
            [(2, Editor::User, true), (1, Editor::Agent, false)]
        );
        Ok(())
    }

    #[tokio::test]
    async fn only_an_unapproved_latest_revision_can_be_rejected() -> Result<(), KnowledgeError> {
        let store = store().await;
        store
            .put(SCOPE, "build", write("Person", "p", 0), Editor::User, None)
            .await?;
        let approved = store.reject(SCOPE, "build", 1).await;
        assert!(
            matches!(approved, Err(KnowledgeError::Refused(_))),
            "{approved:?}"
        );
        store
            .put(SCOPE, "build", write("Agent", "a", 1), Editor::Agent, None)
            .await?;
        let stale = store.reject(SCOPE, "build", 1).await;
        assert!(
            matches!(stale, Err(KnowledgeError::Conflict(_))),
            "{stale:?}"
        );
        assert!(matches!(
            store.reject(SCOPE, "nothing", 1).await,
            Err(KnowledgeError::NotFound)
        ));
        assert_eq!(store.revisions(SCOPE, "build").await?.len(), 2);
        Ok(())
    }

    #[tokio::test]
    async fn notes_written_before_tags_still_read_and_change() -> Result<(), KnowledgeError> {
        let store = store().await;
        store
            .put(SCOPE, "build", write("Agent", "a", 0), Editor::Agent, None)
            .await?;
        store.approve(SCOPE, "build", 1).await?;
        store
            .db
            .query(
                "UPDATE note SET tags = NONE, on_demand = NONE, approved.tags = NONE, \
                 approved.on_demand = NONE; UPDATE note_revision SET tags = NONE, on_demand = NONE",
            )
            .await?
            .check()?;
        let old = store.get(SCOPE, "build").await?;
        assert_eq!(
            old.map(|n| (n.spec.tags, n.spec.on_demand)),
            Some((Vec::new(), false))
        );
        assert_eq!(store.list(SCOPE).await?.len(), 1);
        assert_eq!(store.revisions(SCOPE, "build").await?.len(), 1);
        store
            .put(SCOPE, "build", write("Agent", "b", 1), Editor::Agent, None)
            .await?;
        assert!(store.reject(SCOPE, "build", 2).await?.is_some());
        Ok(())
    }

    #[tokio::test]
    async fn the_schema_can_be_applied_again() -> Result<(), KnowledgeError> {
        let store = store().await;
        store.db.query(SCHEMA).await?.check()?;
        Ok(())
    }
}
