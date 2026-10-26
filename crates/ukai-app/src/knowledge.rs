//! Knowledge: notes a gateway keeps outside the repositories its agents
//! work on, per scope (a project `gateway.toml` names, else a repository).
//! The app lists every project's in the sidebar, the gateway agents sharing
//! a scope together, and shows one in a note pane, to read or edit;
//! the agents read and write the same notes through the relay's MCP server,
//! so a note can change under the pane, which then shows the new one. Notes
//! open in the note pane already there, so one pane follows the sidebar.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use dioxus::prelude::*;
use ukai_acp::knowledge::{KnowledgeEndpoint, KnowledgeError};
use ukai_config::AgentSpec;
use ukai_core::knowledge::{
    self, Editor, Listing, Note, NoteSpec, NoteSummary, NoteWrite, Revision,
};
use ukai_core::review::{self, LineKind};
use ukai_core::sidebar::{Fold, Node};
use ukai_core::{PaneContent, PaneId, ProjectId};

use crate::follow::{self, Endpoints, Workspace, list_gateway_agents, poll};
use crate::icons::{Icon, Svg};
use crate::layout::launch_target;
use crate::markdown::Markdown;
use crate::projects::AppSignals;
use crate::schedules::{local_now, when_unix};
use crate::sidebar::Head;

/// How often the app asks the gateways for their notes.
const LIST_EVERY: Duration = Duration::from_secs(15);
/// How often an open note pane asks for its note.
const PANE_EVERY: Duration = Duration::from_secs(5);

/// `error` as the user reads it, one problem per line.
fn explain(error: &KnowledgeError) -> Vec<String> {
    match error {
        KnowledgeError::Refused(problems) => problems.clone(),
        KnowledgeError::NotFound => {
            vec!["The gateway does not know this note or agent.".to_owned()]
        }
        other => vec![other.to_string()],
    }
}

/// Ask the note listings and open note panes to read again.
fn refresh(app: AppSignals) {
    let mut revision = app.knowledge_revision;
    *revision.write() += 1;
}

/// Show a note (or a new one, with `note` `None`): focus the pane that
/// already shows it, else replace the note in the nearest pane showing one,
/// else open it where [`launch_target`] puts it. A pane with a draft, a new
/// note or an edit not saved yet, is left alone, so the draft is not lost.
pub fn open_note(app: AppSignals, project: &ProjectId, agent: &str, note: Option<String>) {
    let mut layout = app.layout;
    let shown = note.as_ref().and_then(|name| {
        layout.peek().contents().find_map(|(pane, content)| {
            matches!(content, PaneContent::Note { agent: a, note: Some(n), .. } if a == agent && n == name)
                .then_some(pane)
        })
    });
    if let Some(pane) = shown {
        layout.write().focus(pane);
        return;
    }
    let editing = app.editing_notes.peek();
    let reused = layout.peek().nearest(|pane, content| {
        matches!(content, PaneContent::Note { note: Some(_), .. }) && !editing.contains(&pane)
    });
    drop(editing);
    if let Some(pane) = reused.or_else(|| launch_target(app)) {
        let mut layout = layout.write();
        layout.set_content(
            pane,
            PaneContent::Note {
                project: project.clone(),
                agent: agent.to_owned(),
                note,
            },
        );
        layout.focus(pane);
    }
}

fn editor_label(editor: Editor) -> &'static str {
    match editor {
        Editor::User => "a person",
        Editor::Agent => "an agent (not reviewed)",
    }
}

/// Where a revision came from, beyond who wrote it, as one line.
fn provenance_label(provenance: &knowledge::Provenance) -> String {
    let mut parts = Vec::new();
    if let Some(commit) = &provenance.commit {
        parts.push(format!("at {}", commit.get(..12).unwrap_or(commit)));
    }
    if let Some(workspace) = &provenance.workspace {
        parts.push(format!("in workspace {workspace}"));
    }
    parts.join(" ")
}

/// Tags and whether a note is on demand, as one line.
fn tags_label(tags: &[String], on_demand: bool) -> String {
    let mut label = if tags.is_empty() {
        "none".to_owned()
    } else {
        tags.join(", ")
    };
    if on_demand {
        label.push_str(" (on demand)");
    }
    label
}

/// What one gateway agent listed.
pub type Fetched = (String, Result<Listing, String>);

/// One section of a project's notes in the sidebar: the gateway agents that share a scope, and
/// its notes, or one agent whose notes cannot be read.
#[derive(Debug, Clone, PartialEq)]
struct Section {
    /// `None` when the notes cannot be read, so the scope is not known.
    scope: Option<String>,
    agents: Vec<String>,
    notes: Result<Vec<NoteSummary>, String>,
}

/// What `fetched` shows, agents with the same scope together, in the order
/// they were declared.
fn sections(fetched: &[Fetched]) -> Vec<Section> {
    let mut sections: Vec<Section> = Vec::new();
    for (agent, listed) in fetched {
        match listed {
            Ok(listing) => {
                let shared = sections
                    .iter_mut()
                    .find(|section| section.scope.as_deref() == Some(listing.scope.as_str()));
                match shared {
                    Some(section) => section.agents.push(agent.clone()),
                    None => sections.push(Section {
                        scope: Some(listing.scope.clone()),
                        agents: vec![agent.clone()],
                        notes: Ok(listing.notes.clone()),
                    }),
                }
            }
            Err(error) => sections.push(Section {
                scope: None,
                agents: vec![agent.clone()],
                notes: Err(error.clone()),
            }),
        }
    }
    sections
}

/// What each project's gateway agents last listed, kept by
/// [`FollowKnowledge`].
pub type KnowledgeListings = Signal<HashMap<ProjectId, Vec<Fetched>>>;

/// Where `workspace`'s agent keeps its notes.
fn resolve(spec: &AgentSpec, workspace: &Workspace) -> Result<KnowledgeEndpoint, String> {
    KnowledgeEndpoint::resolve(spec, ukai_acp::system_lookup)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| {
            format!(
                "agent `{}` is not a gateway agent, so it keeps no knowledge",
                workspace.agent
            )
        })
}

/// Reads the notes of every project's gateway agents, over and over, so the
/// sidebar can say what awaits review while it is folded. Rendered once by
/// the app; it draws nothing.
#[component]
pub fn FollowKnowledge() -> Element {
    let app = use_context::<AppSignals>();
    let mut listings = app.knowledge;
    use_future(move || {
        let endpoints = Rc::new(RefCell::new(Endpoints::new(resolve)));
        poll(app.knowledge_revision, LIST_EVERY, move || {
            let endpoints = endpoints.clone();
            async move {
                let fetched: HashMap<_, _> =
                    list_gateway_agents(app, &endpoints, async |endpoint| {
                        endpoint.list().await.map_err(|error| error.to_string())
                    })
                    .await
                    .into_iter()
                    .map(|(project, _, listed)| (project, listed))
                    .collect();
                if *listings.peek() != fetched {
                    listings.set(fetched);
                }
            }
        })
    });
    rsx! {}
}

/// How many notes of `sections` wait for the user's review.
fn awaiting(sections: &[Section]) -> usize {
    sections
        .iter()
        .filter_map(|section| section.notes.as_ref().ok())
        .flatten()
        .filter(|note| note.awaits_review())
        .count()
}

/// A project's notes in the sidebar, under each scope its gateway agents
/// share when there are several.
#[component]
pub fn KnowledgeNode(project: ProjectId, depth: u8) -> Element {
    let app = use_context::<AppSignals>();
    let sections = sections(
        app.knowledge
            .read()
            .get(&project)
            .map(Vec::as_slice)
            .unwrap_or_default(),
    );
    let fold = Fold::new(&project, Node::Knowledge);
    let open = app.folds.read().is_open(&fold);
    let focused = match app.layout.read().content(app.layout.read().focused()) {
        Some(PaneContent::Note { agent, note, .. }) => Some((agent.clone(), note.clone())),
        _ => None,
    };
    let now = local_now();
    let review = awaiting(&sections);
    let (marker, meta) = match (review, sections.as_slice()) {
        (0, [only]) => (None, only.scope.clone().unwrap_or_default()),
        (0, _) => (None, String::new()),
        (n, _) => (Some("attention"), format!("{n} to review")),
    };
    let single = match sections.as_slice() {
        [only] => only.agents.first().cloned(),
        _ => None,
    };

    rsx! {
        Head {
            fold,
            depth,
            icon: Icon::Book,
            label: "Knowledge",
            meta,
            marker,
            title: "Notes the gateway keeps for its agents, outside the repository",
            if let Some(agent) = single.clone() {
                button {
                    class: "icon",
                    title: "New note",
                    onclick: {
                        let project = project.clone();
                        move |event: MouseEvent| {
                            event.stop_propagation();
                            open_note(app, &project, &agent, None);
                        }
                    },
                    Svg { icon: Icon::Plus }
                }
            }
        }
        if open {
            if sections.is_empty() {
                p {
                    class: "muted hint tree-hint",
                    style: "--depth: {depth + 1}",
                    "Listing…"
                }
            }
            for section in sections {
                {
                    let first = section.agents.first().cloned().unwrap_or_default();
                    let label = match &section.scope {
                        Some(scope) => format!("{scope} · {}", section.agents.join(", ")),
                        None => first.clone(),
                    };
                    let shown = focused
                        .as_ref()
                        .filter(|(agent, _)| section.agents.contains(agent))
                        .and_then(|(_, note)| note.clone());
                    let inner = if single.is_some() { depth + 1 } else { depth + 2 };
                    rsx! {
                        if single.is_none() {
                            div {
                                key: "head-{label}",
                                class: "row tree-subhead",
                                style: "--depth: {depth + 1}",
                                title: "Agents sharing these notes",
                                span { class: "name", "{label}" }
                                span { class: "row-actions",
                                    button {
                                        class: "icon",
                                        title: "New note",
                                        onclick: {
                                            let project = project.clone();
                                            let first = first.clone();
                                            move |_| open_note(app, &project, &first, None)
                                        },
                                        Svg { icon: Icon::Plus }
                                    }
                                }
                            }
                        }
                        match section.notes {
                            Err(error) => rsx! {
                                p {
                                    key: "error-{label}",
                                    class: "muted hint tree-hint",
                                    style: "--depth: {inner}",
                                    title: "{error}",
                                    "Cannot read notes: {error}"
                                }
                            },
                            Ok(list) if list.is_empty() => rsx! {
                                p {
                                    key: "empty-{label}",
                                    class: "muted hint tree-hint",
                                    style: "--depth: {inner}",
                                    "No notes yet. Agents write what they learn here."
                                }
                            },
                            Ok(list) => rsx! {
                                for note in list {
                                    NoteRow {
                                        key: "{label}-{note.name}",
                                        project: project.clone(),
                                        agent: first.clone(),
                                        focused: shown.as_deref() == Some(note.name.as_str()),
                                        updated: when_unix(note.updated, now),
                                        depth: inner,
                                        note,
                                    }
                                }
                            },
                        }
                    }
                }
            }
        }
    }
}

#[component]
fn NoteRow(
    project: ProjectId,
    agent: String,
    note: NoteSummary,
    focused: bool,
    updated: String,
    depth: u8,
) -> Element {
    let app = use_context::<AppSignals>();
    let waits = note.awaits_review();
    let mut title = note.description.clone();
    if !note.tags.is_empty() {
        title.push_str(&format!(" · {}", note.tags.join(", ")));
    }
    if note.on_demand {
        title.push_str(" · on demand");
    }
    if waits {
        title.push_str(&format!(" · revision {} awaits your review", note.revision));
    }
    rsx! {
        div {
            class: if focused { "row schedule-row focused" } else { "row schedule-row" },
            style: "--depth: {depth}",
            title: "{title}",
            onclick: move |_| open_note(app, &project, &agent, Some(note.name.clone())),
            if waits {
                span { class: "dot attention" }
            }
            span { class: "name", "{note.name}" }
            for tag in note.tags.iter().take(2) {
                span { key: "{tag}", class: "badge", "{tag}" }
            }
            span { class: "meta",
                if waits {
                    "review"
                } else {
                    "{updated}"
                }
            }
        }
    }
}

/// What changed in a note's body since the approved revision: every line
/// added when nothing was approved yet.
#[component]
fn NoteDiff(old: Option<String>, new: String) -> Element {
    let hunks = review::hunks(old.as_deref(), &new);
    rsx! {
        div { class: "diff",
            if hunks.is_empty() {
                div { class: "diff-gap", "The body is unchanged." }
            }
            for (h, hunk) in hunks.into_iter().enumerate() {
                if h > 0 {
                    div { key: "gap-{h}", class: "diff-gap", "⋯" }
                }
                for (i, line) in hunk.into_iter().enumerate() {
                    {
                        let (class, marker) = match line.kind {
                            LineKind::Added => ("diff-line added", "+"),
                            LineKind::Removed => ("diff-line removed", "-"),
                            LineKind::Context => ("diff-line", " "),
                        };
                        let old = line.old.map(|n| n.to_string()).unwrap_or_default();
                        let new = line.new.map(|n| n.to_string()).unwrap_or_default();
                        rsx! {
                            div { key: "{h}-{i}", class: "{class}",
                                span { class: "ln", "{old}" }
                                span { class: "ln", "{new}" }
                                span { class: "code", "{marker}{line.text}" }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// One revision in a note's history, with its text on demand.
#[component]
fn RevisionRow(
    revision: Revision,
    current: u64,
    busy: bool,
    on_restore: EventHandler<NoteSpec>,
) -> Element {
    let now = local_now();
    let when = when_unix(revision.updated, now);
    let by = editor_label(revision.updated_by);
    let what = if revision.deleted {
        "deleted"
    } else {
        "written"
    };
    let from = provenance_label(&revision.provenance);
    rsx! {
        details { class: "run",
            summary {
                span { class: "run-time", "#{revision.revision}" }
                span { class: "run-outcome", "{what} {when} by {by} {from}" }
            }
            div { class: "revision-body",
                p { class: "muted", "{revision.spec.description}" }
                Markdown { source: revision.spec.body.clone() }
                if !revision.deleted && revision.revision != current {
                    button {
                        class: "outline small",
                        disabled: busy,
                        onclick: {
                            let spec = revision.spec.clone();
                            move |_| on_restore.call(spec.clone())
                        },
                        "Restore this revision"
                    }
                }
            }
        }
    }
}

/// What the gateway last said about the pane's note.
#[derive(Clone, PartialEq)]
enum Loaded {
    /// Not saved yet; nothing to read.
    New,
    Waiting,
    Found(Box<Note>),
    Failed(String),
}

/// What is being written: the name only for a note not saved yet, since a
/// saved note is found by it.
#[derive(Clone, PartialEq)]
struct Draft {
    name: String,
    spec: NoteSpec,
    /// The revision being edited, 0 for a new note: the gateway refuses
    /// the save if the note has moved on since.
    base: u64,
    /// The tags as typed; `spec.tags` is what they parse to.
    tags: String,
}

impl Draft {
    fn of(name: String, spec: NoteSpec, base: u64) -> Self {
        let tags = spec.tags.join(", ");
        Self {
            name,
            spec,
            base,
            tags,
        }
    }
}

/// A note's tags, and whether it is on demand, as badges.
#[component]
fn Tags(tags: Vec<String>, on_demand: bool) -> Element {
    rsx! {
        if !tags.is_empty() || on_demand {
            div { class: "note-tags",
                for tag in tags {
                    span { key: "{tag}", class: "badge", "{tag}" }
                }
                if on_demand {
                    span {
                        class: "muted",
                        title: "Sessions are told how many notes there are per tag, and read this one when their task calls for it",
                        "on demand"
                    }
                }
            }
        }
    }
}

/// One note, read as Markdown, or edited as a draft and saved whole.
/// `note` is `None` for one not saved yet.
#[component]
pub fn NoteView(pane: PaneId, project: ProjectId, agent: String, note: Option<String>) -> Element {
    let app = use_context::<AppSignals>();
    let endpoint = use_memo({
        let agent = agent.clone();
        let project = project.clone();
        move || {
            let workspace = Workspace::of(&project, &agent, None);
            follow::endpoint(&app.configs.read(), &workspace, resolve)
        }
    });
    let mut loaded = use_signal(|| {
        if note.is_some() {
            Loaded::Waiting
        } else {
            Loaded::New
        }
    });
    let mut draft = use_signal(|| {
        note.is_none().then(|| {
            Draft::of(
                String::new(),
                NoteSpec {
                    description: String::new(),
                    body: String::new(),
                    tags: Vec::new(),
                    on_demand: false,
                },
                0,
            )
        })
    });
    // Tell `open_note` whether this pane holds a draft.
    use_effect(move || {
        let drafting = draft.read().is_some();
        let mut editing = app.editing_notes;
        if editing.peek().contains(&pane) != drafting {
            if drafting {
                editing.write().insert(pane);
            } else {
                editing.write().remove(&pane);
            }
        }
    });
    use_drop(move || {
        let mut editing = app.editing_notes;
        editing.write().remove(&pane);
    });
    // The note's history, once asked for.
    let mut history = use_signal(|| None::<Result<Vec<Revision>, String>>);
    let mut errors = use_signal(Vec::<String>::new);
    let mut busy = use_signal(|| false);

    use_future({
        let note = note.clone();
        move || {
            let note = note.clone();
            async move {
                let Some(name) = note else {
                    return;
                };
                poll(app.knowledge_revision, PANE_EVERY, move || {
                    let name = name.clone();
                    async move {
                        let result = match endpoint.peek().clone() {
                            Ok(endpoint) => match endpoint.get(&name).await {
                                Ok(note) => Loaded::Found(Box::new(note)),
                                Err(KnowledgeError::NotFound) => {
                                    Loaded::Failed("This note was deleted.".to_owned())
                                }
                                Err(error) => Loaded::Failed(error.to_string()),
                            },
                            Err(error) => Loaded::Failed(error),
                        };
                        if *loaded.peek() != result {
                            loaded.set(result);
                        }
                    }
                })
                .await;
            }
        }
    });

    let saved = match loaded() {
        Loaded::Waiting => {
            return rsx! {
                div { class: "pane-body placeholder",
                    p { class: "muted", "Reading the note from the gateway…" }
                }
            };
        }
        Loaded::Failed(error) => {
            return rsx! {
                div { class: "pane-body placeholder",
                    p { class: "muted", "{error}" }
                }
            };
        }
        Loaded::New => None,
        Loaded::Found(note) => Some(*note),
    };
    let endpoint_error = endpoint.read().as_ref().err().cloned();

    let save = {
        let project = project.clone();
        let agent = agent.clone();
        move |draft_now: Draft| {
            // Copies, so this stays callable from every handler.
            let (mut busy, mut errors, mut draft, mut loaded) = (busy, errors, draft, loaded);
            let Ok(endpoint) = endpoint.peek().clone() else {
                return;
            };
            let is_new = matches!(*loaded.peek(), Loaded::New);
            let project = project.clone();
            let agent = agent.clone();
            busy.set(true);
            spawn(async move {
                let write = NoteWrite {
                    spec: draft_now.spec,
                    base_revision: draft_now.base,
                    commit: None,
                };
                let result = endpoint.put(&draft_now.name, &write).await;
                busy.set(false);
                match result {
                    Ok(note) if is_new => {
                        // The pane now shows the saved note.
                        let mut layout = app.layout;
                        layout.write().set_content(
                            pane,
                            PaneContent::Note {
                                project,
                                agent,
                                note: Some(note.name),
                            },
                        );
                        refresh(app);
                    }
                    Ok(note) => {
                        errors.set(Vec::new());
                        draft.set(None);
                        loaded.set(Loaded::Found(Box::new(note)));
                        history.set(None);
                        refresh(app);
                    }
                    Err(error) => {
                        // Read the note as it is now, so the pane offers to
                        // replace it.
                        if matches!(error, KnowledgeError::Conflict(_)) {
                            refresh(app);
                        }
                        errors.set(explain(&error));
                    }
                }
            });
        }
    };
    let delete = {
        let note = note.clone();
        move |_| {
            let (Some(name), Ok(endpoint)) = (note.clone(), endpoint.peek().clone()) else {
                return;
            };
            let Loaded::Found(current) = &*loaded.peek() else {
                return;
            };
            let revision = current.revision;
            busy.set(true);
            spawn(async move {
                let result = endpoint.delete(&name, revision).await;
                busy.set(false);
                match result {
                    Ok(()) => {
                        let mut layout = app.layout;
                        layout.write().set_content(pane, PaneContent::Empty);
                        refresh(app);
                    }
                    Err(error) => errors.set(explain(&error)),
                }
            });
        }
    };

    let shown_errors: Vec<String> = errors.read().clone();
    let Some(current) = draft() else {
        // Reading.
        let Some(saved) = saved else {
            return rsx! {};
        };
        let now = local_now();
        let updated = when_unix(saved.updated, now);
        let by = editor_label(saved.updated_by);
        let from = provenance_label(&saved.provenance);
        let review = saved.awaits_review().then(|| {
            let (old, old_description, old_tags) = saved
                .approved
                .as_ref()
                .map(|a| {
                    (
                        Some(a.spec.body.clone()),
                        Some(a.spec.description.clone()),
                        (a.spec.tags.clone(), a.spec.on_demand),
                    )
                })
                .unwrap_or((None, None, (saved.spec.tags.clone(), saved.spec.on_demand)));
            (saved.revision, old, old_description, old_tags)
        });
        let approve = {
            let name = saved.name.clone();
            let revision = saved.revision;
            move |_| {
                let Ok(endpoint) = endpoint.peek().clone() else {
                    return;
                };
                let name = name.clone();
                busy.set(true);
                spawn(async move {
                    let result = endpoint.approve(&name, revision).await;
                    busy.set(false);
                    match result {
                        Ok(note) => {
                            errors.set(Vec::new());
                            loaded.set(Loaded::Found(Box::new(note)));
                            refresh(app);
                        }
                        Err(error) => {
                            refresh(app);
                            errors.set(explain(&error));
                        }
                    }
                });
            }
        };
        let reject = {
            let name = saved.name.clone();
            let revision = saved.revision;
            move |_| {
                let Ok(endpoint) = endpoint.peek().clone() else {
                    return;
                };
                let name = name.clone();
                busy.set(true);
                spawn(async move {
                    let result = endpoint.reject(&name, revision).await;
                    busy.set(false);
                    match result {
                        Ok(Some(note)) => {
                            errors.set(Vec::new());
                            loaded.set(Loaded::Found(Box::new(note)));
                            history.set(None);
                            refresh(app);
                        }
                        Ok(None) => {
                            let mut layout = app.layout;
                            layout.write().set_content(pane, PaneContent::Empty);
                            refresh(app);
                        }
                        Err(error) => {
                            refresh(app);
                            errors.set(explain(&error));
                        }
                    }
                });
            }
        };
        let show_history = {
            let name = saved.name.clone();
            move |_| {
                let Ok(endpoint) = endpoint.peek().clone() else {
                    return;
                };
                let name = name.clone();
                history.set(Some(Ok(Vec::new())));
                spawn(async move {
                    let result = endpoint
                        .revisions(&name)
                        .await
                        .map_err(|error| error.to_string());
                    history.set(Some(result));
                });
            }
        };
        return rsx! {
            div { class: "pane-body schedule note",
                div { class: "schedule-panel",
                    div { class: "note-head",
                        div { class: "note-title",
                            h2 { "{saved.name}" }
                            p { class: "muted", "{saved.spec.description}" }
                            Tags {
                                tags: saved.spec.tags.clone(),
                                on_demand: saved.spec.on_demand,
                            }
                        }
                        div { class: "note-head-actions",
                            button {
                                class: "outline",
                                onclick: {
                                    let saved = saved.clone();
                                    move |_| {
                                        errors.set(Vec::new());
                                        draft
                                            .set(
                                                Some(
                                                    Draft::of(saved.name.clone(), saved.spec.clone(), saved.revision),
                                                ),
                                            );
                                    }
                                },
                                "Edit"
                            }
                            button {
                                class: "outline danger",
                                disabled: busy(),
                                title: "Delete the note; its history is kept",
                                onclick: delete,
                                "Delete"
                            }
                        }
                    }
                    p { class: "muted field-hint",
                        "Kept on the gateway, outside the repositories, for the agents sharing {agent}'s notes. Revision {saved.revision}, {updated} by {by} {from}."
                    }
                    if !shown_errors.is_empty() {
                        ul { class: "problems",
                            for problem in shown_errors {
                                li { key: "{problem}", "{problem}" }
                            }
                        }
                    }
                    if let Some((revision, old, old_description, old_tags)) = review {
                        div { class: "review",
                            p {
                                if old.is_some() {
                                    "Revision {revision} by {by} awaits your review. Agents are shown the approved revision until you approve this one."
                                } else {
                                    "This note awaits your review. Agents are not shown it until you approve it; they can still list and read it, marked as not reviewed."
                                }
                            }
                            if let Some(old_description) = old_description.filter(|d| *d != saved.spec.description) {
                                p { class: "muted", "Description was: {old_description}" }
                            }
                            NoteDiff {
                                old: old.clone(),
                                new: saved.spec.body.clone(),
                            }
                            if old_tags != (saved.spec.tags.clone(), saved.spec.on_demand) {
                                p { class: "muted",
                                    "Tags were: {tags_label(&old_tags.0, old_tags.1)}"
                                }
                            }
                            div { class: "schedule-actions",
                                button { disabled: busy(), onclick: approve,
                                    "Approve revision {revision}"
                                }
                                button {
                                    class: "outline danger",
                                    disabled: busy(),
                                    title: if old.is_some() { "Write the approved revision back; this one stays in the history" } else { "Delete the note; it stays in the history" },
                                    onclick: reject,
                                    if old.is_some() {
                                        "Reject: back to the approved revision"
                                    } else {
                                        "Reject: delete it"
                                    }
                                }
                            }
                        }
                    }
                    Markdown { source: saved.spec.body.clone() }
                }
                div { class: "schedule-panel",
                    match history() {
                        None => rsx! {
                            button { class: "outline small", onclick: show_history, "Show history" }
                        },
                        Some(Err(error)) => rsx! {
                            p { class: "muted hint", "Cannot read the history: {error}" }
                        },
                        Some(Ok(revisions)) => rsx! {
                            span { class: "section-label", "History" }
                            for revision in revisions {
                                RevisionRow {
                                    key: "{revision.revision}",
                                    current: saved.revision,
                                    busy: busy(),
                                    revision: revision.clone(),
                                    on_restore: {
                                        let save = save.clone();
                                        let name = saved.name.clone();
                                        let base = saved.revision;
                                        move |spec: NoteSpec| save(Draft::of(name.clone(), spec, base))
                                    },
                                }
                            }
                        },
                    }
                }
            }
        };
    };

    let problems: Vec<String> = knowledge::problems(&current.name, &current.spec)
        .iter()
        .map(ToString::to_string)
        .collect();
    let changed = saved
        .as_ref()
        .is_none_or(|saved| saved.spec != current.spec);
    // The revision the note moved on to while it was being edited.
    let moved = saved
        .as_ref()
        .map(|saved| saved.revision)
        .filter(|revision| current.base != 0 && *revision != current.base);
    let shown_problems: Vec<String> = problems
        .iter()
        .chain(errors.read().iter())
        .chain(endpoint_error.iter())
        .cloned()
        .collect();
    let is_new = note.is_none();

    rsx! {
        div { class: "pane-body schedule note",
            div { class: "schedule-panel",
                if is_new {
                    div { class: "field",
                        span { class: "field-label", "Name" }
                        input {
                            class: "mono",
                            value: "{current.name}",
                            spellcheck: false,
                            placeholder: "release-process",
                            oninput: move |event| {
                                if let Some(draft) = draft.write().as_mut() {
                                    draft.name = event.value();
                                }
                            },
                        }
                    }
                } else {
                    h2 { class: "note-name", "{current.name}" }
                }
                div { class: "field",
                    span { class: "field-label", "Description" }
                    input {
                        value: "{current.spec.description}",
                        placeholder: "One line: what this note is about",
                        oninput: move |event| {
                            if let Some(draft) = draft.write().as_mut() {
                                draft.spec.description = event.value();
                            }
                        },
                    }
                    span { class: "muted field-hint",
                        "Agents see every note's name and description, and read the note when it bears on their task."
                    }
                }
                div { class: "field",
                    span { class: "field-label", "Tags" }
                    input {
                        class: "mono",
                        value: "{current.tags}",
                        spellcheck: false,
                        placeholder: "research, build",
                        oninput: move |event| {
                            if let Some(draft) = draft.write().as_mut() {
                                draft.spec.tags = knowledge::parse_tags(&event.value());
                                draft.tags = event.value();
                            }
                        },
                    }
                    label { class: "toggle",
                        input {
                            r#type: "checkbox",
                            checked: current.spec.on_demand,
                            onchange: move |event: FormEvent| {
                                if let Some(draft) = draft.write().as_mut() {
                                    draft.spec.on_demand = event.checked();
                                }
                            },
                        }
                        "On demand: agents are only told how many notes there are per tag, and look this one up when their task calls for it"
                    }
                }
                div { class: "field",
                    span { class: "field-label", "Note" }
                    textarea {
                        class: "mono",
                        rows: 18,
                        placeholder: "Markdown",
                        value: "{current.spec.body}",
                        oninput: move |event| {
                            if let Some(draft) = draft.write().as_mut() {
                                draft.spec.body = event.value();
                            }
                        },
                    }
                }
                if let Some(revision) = moved {
                    div { class: "problems",
                        p {
                            "This note changed on the gateway (now revision {revision}) since you started editing, so saving is refused. Compare it in another pane, or replace that revision with yours."
                        }
                        button {
                            class: "outline small",
                            onclick: move |_| {
                                if let Some(draft) = draft.write().as_mut() {
                                    draft.base = revision;
                                }
                                errors.set(Vec::new());
                            },
                            "Replace revision {revision} with mine"
                        }
                    }
                }
                if !shown_problems.is_empty() {
                    ul { class: "problems",
                        for problem in shown_problems {
                            li { key: "{problem}", "{problem}" }
                        }
                    }
                }
                div { class: "schedule-actions",
                    button {
                        disabled: !changed || !problems.is_empty() || busy(),
                        onclick: {
                            let save = save.clone();
                            move |_| {
                                if let Some(current) = draft.peek().clone() {
                                    save(current);
                                }
                            }
                        },
                        if is_new {
                            "Create"
                        } else {
                            "Save"
                        }
                    }
                    if !is_new {
                        button {
                            class: "outline",
                            onclick: move |_| {
                                errors.set(Vec::new());
                                draft.set(None);
                            },
                            "Cancel"
                        }
                        span { class: "spacer" }
                        button {
                            class: "outline danger",
                            disabled: busy(),
                            onclick: delete,
                            "Delete"
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn listing(scope: &str, names: &[&str]) -> Result<Listing, String> {
        Ok(Listing {
            scope: scope.to_owned(),
            notes: names
                .iter()
                .map(|name| NoteSummary {
                    name: (*name).to_owned(),
                    description: String::new(),
                    tags: Vec::new(),
                    on_demand: false,
                    revision: 1,
                    updated: 0,
                    updated_by: Editor::User,
                    approved: None,
                })
                .collect(),
        })
    }

    #[test]
    fn agents_sharing_a_scope_share_a_section() {
        let fetched = vec![
            ("api".to_owned(), listing("plekti", &["build"])),
            ("down".to_owned(), Err("unreachable".to_owned())),
            ("web".to_owned(), listing("plekti", &["build"])),
            ("tool".to_owned(), listing("github.com/o/tool", &[])),
        ];
        let shown: Vec<(Option<String>, Vec<String>)> = sections(&fetched)
            .into_iter()
            .map(|section| (section.scope, section.agents))
            .collect();
        assert_eq!(
            shown,
            [
                (
                    Some("plekti".to_owned()),
                    vec!["api".to_owned(), "web".to_owned()]
                ),
                (None, vec!["down".to_owned()]),
                (
                    Some("github.com/o/tool".to_owned()),
                    vec!["tool".to_owned()]
                ),
            ]
        );
    }
}
