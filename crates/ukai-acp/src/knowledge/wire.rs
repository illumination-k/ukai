//! The knowledge types as `ukai.knowledge.v1` carries them, both ways. A
//! message from the wire can lack what a type needs (a field proto3 makes
//! optional, an editor this side does not know), so reading one is
//! fallible; writing one is not.

use ukai_proto::knowledge::v1 as pb;

use super::{Approved, ApprovedSummary, Editor, Note, NoteSpec, NoteSummary, Provenance, Revision};

/// What a message from the wire lacks.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("a knowledge message has no {0}")]
pub struct Malformed(pub &'static str);

impl From<Editor> for pb::Editor {
    fn from(editor: Editor) -> Self {
        match editor {
            Editor::User => pb::Editor::User,
            Editor::Agent => pb::Editor::Agent,
        }
    }
}

/// The editor a message names, as prost keeps it.
pub fn editor(value: i32) -> Result<Editor, Malformed> {
    match pb::Editor::try_from(value) {
        Ok(pb::Editor::User) => Ok(Editor::User),
        Ok(pb::Editor::Agent) => Ok(Editor::Agent),
        Ok(pb::Editor::Unspecified) | Err(_) => Err(Malformed("editor it knows")),
    }
}

fn spec(spec: Option<pb::NoteSpec>) -> Result<NoteSpec, Malformed> {
    spec.map(NoteSpec::from).ok_or(Malformed("note spec"))
}

impl From<NoteSpec> for pb::NoteSpec {
    fn from(spec: NoteSpec) -> Self {
        Self {
            description: spec.description,
            body: spec.body,
            tags: spec.tags,
            on_demand: spec.on_demand,
        }
    }
}

impl From<pb::NoteSpec> for NoteSpec {
    fn from(spec: pb::NoteSpec) -> Self {
        Self {
            description: spec.description,
            body: spec.body,
            tags: spec.tags,
            on_demand: spec.on_demand,
        }
    }
}

impl From<Provenance> for pb::Provenance {
    fn from(provenance: Provenance) -> Self {
        Self {
            workspace: provenance.workspace,
            commit: provenance.commit,
        }
    }
}

impl From<pb::Provenance> for Provenance {
    fn from(provenance: pb::Provenance) -> Self {
        Self {
            workspace: provenance.workspace,
            commit: provenance.commit,
        }
    }
}

impl From<Approved> for pb::Approved {
    fn from(approved: Approved) -> Self {
        Self {
            revision: approved.revision,
            spec: Some(approved.spec.into()),
            updated: approved.updated,
            updated_by: pb::Editor::from(approved.updated_by).into(),
            provenance: Some(approved.provenance.into()),
        }
    }
}

impl TryFrom<pb::Approved> for Approved {
    type Error = Malformed;

    fn try_from(approved: pb::Approved) -> Result<Self, Malformed> {
        Ok(Self {
            revision: approved.revision,
            spec: spec(approved.spec)?,
            updated: approved.updated,
            updated_by: editor(approved.updated_by)?,
            provenance: approved.provenance.map(Into::into).unwrap_or_default(),
        })
    }
}

impl From<Note> for pb::Note {
    fn from(note: Note) -> Self {
        Self {
            name: note.name,
            spec: Some(note.spec.into()),
            revision: note.revision,
            updated: note.updated,
            updated_by: pb::Editor::from(note.updated_by).into(),
            provenance: Some(note.provenance.into()),
            approved: note.approved.map(Into::into),
        }
    }
}

impl TryFrom<pb::Note> for Note {
    type Error = Malformed;

    fn try_from(note: pb::Note) -> Result<Self, Malformed> {
        Ok(Self {
            name: note.name,
            spec: spec(note.spec)?,
            revision: note.revision,
            updated: note.updated,
            updated_by: editor(note.updated_by)?,
            provenance: note.provenance.map(Into::into).unwrap_or_default(),
            approved: note.approved.map(Approved::try_from).transpose()?,
        })
    }
}

impl From<ApprovedSummary> for pb::ApprovedSummary {
    fn from(approved: ApprovedSummary) -> Self {
        Self {
            revision: approved.revision,
            description: approved.description,
            tags: approved.tags,
            on_demand: approved.on_demand,
        }
    }
}

impl From<pb::ApprovedSummary> for ApprovedSummary {
    fn from(approved: pb::ApprovedSummary) -> Self {
        Self {
            revision: approved.revision,
            description: approved.description,
            tags: approved.tags,
            on_demand: approved.on_demand,
        }
    }
}

impl From<NoteSummary> for pb::NoteSummary {
    fn from(note: NoteSummary) -> Self {
        Self {
            name: note.name,
            description: note.description,
            tags: note.tags,
            on_demand: note.on_demand,
            revision: note.revision,
            updated: note.updated,
            updated_by: pb::Editor::from(note.updated_by).into(),
            approved: note.approved.map(Into::into),
        }
    }
}

impl TryFrom<pb::NoteSummary> for NoteSummary {
    type Error = Malformed;

    fn try_from(note: pb::NoteSummary) -> Result<Self, Malformed> {
        Ok(Self {
            name: note.name,
            description: note.description,
            tags: note.tags,
            on_demand: note.on_demand,
            revision: note.revision,
            updated: note.updated,
            updated_by: editor(note.updated_by)?,
            approved: note.approved.map(Into::into),
        })
    }
}

impl From<Revision> for pb::Revision {
    fn from(revision: Revision) -> Self {
        Self {
            revision: revision.revision,
            spec: Some(revision.spec.into()),
            updated: revision.updated,
            updated_by: pb::Editor::from(revision.updated_by).into(),
            provenance: Some(revision.provenance.into()),
            deleted: revision.deleted,
        }
    }
}

impl TryFrom<pb::Revision> for Revision {
    type Error = Malformed;

    fn try_from(revision: pb::Revision) -> Result<Self, Malformed> {
        Ok(Self {
            revision: revision.revision,
            spec: spec(revision.spec)?,
            updated: revision.updated,
            updated_by: editor(revision.updated_by)?,
            provenance: revision.provenance.map(Into::into).unwrap_or_default(),
            deleted: revision.deleted,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(body: &str) -> NoteSpec {
        NoteSpec {
            description: "Building".into(),
            body: body.into(),
            tags: vec!["build".into(), "macos".into()],
            on_demand: true,
        }
    }

    fn note() -> Note {
        Note {
            name: "build".into(),
            spec: spec("new"),
            revision: 3,
            updated: 30,
            updated_by: Editor::Agent,
            provenance: Provenance {
                workspace: Some("0a1b2c3d4e5f".into()),
                commit: Some("0123abcd".into()),
            },
            approved: Some(Approved {
                revision: 2,
                spec: spec("old"),
                updated: 20,
                updated_by: Editor::User,
                provenance: Provenance::default(),
            }),
        }
    }

    #[test]
    fn a_note_crosses_the_wire_unchanged() {
        let note = note();
        assert_eq!(
            Note::try_from(pb::Note::from(note.clone())),
            Ok(note.clone())
        );
        let summary = note.summary();
        assert_eq!(
            NoteSummary::try_from(pb::NoteSummary::from(summary.clone())),
            Ok(summary)
        );
        let revision = Revision {
            revision: 4,
            spec: spec("gone"),
            updated: 40,
            updated_by: Editor::User,
            provenance: Provenance::default(),
            deleted: true,
        };
        assert_eq!(
            Revision::try_from(pb::Revision::from(revision.clone())),
            Ok(revision)
        );
    }

    #[test]
    fn a_message_lacking_what_a_note_needs_is_malformed() {
        let mut wire = pb::Note::from(note());
        wire.spec = None;
        assert_eq!(Note::try_from(wire), Err(Malformed("note spec")));
        let mut wire = pb::Note::from(note());
        wire.updated_by = pb::Editor::Unspecified.into();
        assert!(Note::try_from(wire).is_err());
        let mut wire = pb::Note::from(note());
        wire.updated_by = 99;
        assert!(Note::try_from(wire).is_err());
    }
}
