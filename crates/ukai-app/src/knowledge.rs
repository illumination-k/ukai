//! Knowledge: notes a gateway keeps about the repository its agents work
//! on, outside the repository. The app lists them under the selected
//! project's gateway agents and shows one in a note pane, to read or edit;
//! the agents read and write the same notes through the relay's MCP server,
//! so a note can change under the pane, which then shows the new one.

use std::time::Duration;

use dioxus::prelude::*;
use ukai_acp::knowledge::{KnowledgeEndpoint, KnowledgeError};
use ukai_config::Config;
use ukai_core::knowledge::{self, Editor, Note, NoteSpec, NoteSummary, NoteWrite, Revision};
use ukai_core::review::{self, LineKind};
use ukai_core::{PaneContent, PaneId, ProjectId};

use crate::icons::{Icon, Svg};
use crate::layout::launch_target;
use crate::markdown::Markdown;
use crate::projects::AppSignals;
use crate::schedules::{config_of, gateway_agents, local_now, poll, when_unix};

/// How often the sidebar asks the gateways for their notes.
const LIST_EVERY: Duration = Duration::from_secs(15);
/// How often an open note pane asks for its note.
const PANE_EVERY: Duration = Duration::from_secs(5);

/// Where `agent`'s repository's notes are kept.
fn endpoint(config: &Config, agent: &str) -> Result<KnowledgeEndpoint, String> {
    let spec = config
        .agents
        .get(agent)
        .ok_or_else(|| format!("agent `{agent}` is not declared anymore"))?;
    KnowledgeEndpoint::resolve(spec, ukai_acp::system_lookup)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| format!("agent `{agent}` is not a gateway agent, so it keeps no knowledge"))
}

/// `error` as the user reads it, one problem per line.
fn explain(error: &KnowledgeError) -> Vec<String> {
    match error {
        KnowledgeError::Refused(problems) => problems.clone(),
        KnowledgeError::NotFound => {
            vec!["The gateway does not know this note or agent; it may need upgrading.".to_owned()]
        }
        other => vec![other.to_string()],
    }
}

/// Ask the sidebar and open note panes to read their notes again.
fn refresh(app: AppSignals) {
    let mut revision = app.knowledge_revision;
    *revision.write() += 1;
}

/// Show a note (or a new one, with `note` `None`) where [`launch_target`]
/// puts it, or focus the pane that already shows it.
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
    if let Some(pane) = launch_target(app) {
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

/// What one gateway agent listed.
type Listing = (String, Result<Vec<NoteSummary>, String>);

/// The notes of the selected project's gateway agents' repositories.
#[component]
pub fn KnowledgeList(project: ProjectId, name: String) -> Element {
    let app = use_context::<AppSignals>();
    let endpoints = use_memo({
        let project = project.clone();
        move || -> Vec<(String, Result<KnowledgeEndpoint, String>)> {
            let Ok(config) = config_of(app, &project) else {
                return Vec::new();
            };
            gateway_agents(&config)
                .into_iter()
                .map(|agent| {
                    let endpoint = endpoint(&config, &agent);
                    (agent, endpoint)
                })
                .collect()
        }
    });
    let mut listings = use_signal(Vec::<Listing>::new);
    use_future(move || {
        poll(app.knowledge_revision, LIST_EVERY, move || async move {
            let mut fetched = Vec::new();
            for (agent, endpoint) in endpoints.peek().clone() {
                let listed = match endpoint {
                    Ok(endpoint) => endpoint.list().await.map_err(|error| error.to_string()),
                    Err(error) => Err(error),
                };
                fetched.push((agent, listed));
            }
            if *listings.peek() != fetched {
                listings.set(fetched);
            }
        })
    });
    let open = match app.layout.read().content(app.layout.read().focused()) {
        Some(PaneContent::Note { agent, note, .. }) => Some((agent.clone(), note.clone())),
        _ => None,
    };
    let agents: Vec<String> = endpoints
        .read()
        .iter()
        .map(|(agent, _)| agent.clone())
        .collect();
    let listings = listings.read().clone();
    let now = local_now();

    rsx! {
        div { class: "sidebar-section",
            div { class: "sidebar-header",
                span { class: "section-label", "Knowledge · {name}" }
            }
            if agents.is_empty() {
                p { class: "muted hint",
                    "A gateway keeps notes about its agents' repository outside it. This project has no gateway agent."
                }
            }
            for agent in agents {
                div { key: "{agent}", class: "schedule-agent",
                    div { class: "schedule-agent-head",
                        span { class: "muted", "{agent}" }
                        button {
                            class: "icon",
                            title: "New note for {agent}'s repository",
                            onclick: {
                                let project = project.clone();
                                let agent = agent.clone();
                                move |_| open_note(app, &project, &agent, None)
                            },
                            Svg { icon: Icon::Plus }
                        }
                    }
                    match listings.iter().find(|(a, _)| *a == agent).map(|(_, listed)| listed.clone()) {
                        None => rsx! {},
                        Some(Err(error)) => rsx! {
                            p { class: "muted hint", title: "{error}", "Cannot read notes: {error}" }
                        },
                        Some(Ok(list)) if list.is_empty() => rsx! {
                            p { class: "muted hint",
                                "No notes yet. Agents write what they learn about the repository here instead of committing it."
                            }
                        },
                        Some(Ok(list)) => {
                            let focused = open
                                .as_ref()
                                .filter(|(a, _)| *a == agent)
                                .and_then(|(_, note)| note.clone());
                            rsx! {
                                for note in list {
                                    NoteRow {
                                        key: "{note.name}",
                                        project: project.clone(),
                                        agent: agent.clone(),
                                        focused: focused.as_deref() == Some(note.name.as_str()),
                                        updated: when_unix(note.updated, now),
                                        note,
                                    }
                                }
                            }
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
) -> Element {
    let app = use_context::<AppSignals>();
    let waits = note.awaits_review();
    let title = if waits {
        format!(
            "{} · revision {} awaits your review",
            note.description, note.revision
        )
    } else {
        note.description.clone()
    };
    rsx! {
        div {
            class: if focused { "row schedule-row focused" } else { "row schedule-row" },
            title: "{title}",
            onclick: move |_| open_note(app, &project, &agent, Some(note.name.clone())),
            if waits {
                span { class: "dot attention" }
            }
            span { class: "name", "{note.name}" }
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
}

/// One note, read as Markdown, or edited as a draft and saved whole.
/// `note` is `None` for one not saved yet.
#[component]
pub fn NoteView(pane: PaneId, project: ProjectId, agent: String, note: Option<String>) -> Element {
    let app = use_context::<AppSignals>();
    let endpoint = use_memo({
        let agent = agent.clone();
        let project = project.clone();
        move || config_of(app, &project).and_then(|config| endpoint(&config, &agent))
    });
    let mut loaded = use_signal(|| {
        if note.is_some() {
            Loaded::Waiting
        } else {
            Loaded::New
        }
    });
    let mut draft = use_signal(|| {
        note.is_none().then(|| Draft {
            name: String::new(),
            spec: NoteSpec {
                description: String::new(),
                body: String::new(),
            },
            base: 0,
        })
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
            let (old, old_description) = saved
                .approved
                .as_ref()
                .map(|a| (Some(a.spec.body.clone()), Some(a.spec.description.clone())))
                .unwrap_or((None, None));
            (saved.revision, old, old_description)
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
                        }
                        button {
                            class: "outline",
                            onclick: {
                                let saved = saved.clone();
                                move |_| {
                                    errors.set(Vec::new());
                                    draft
                                        .set(
                                            Some(Draft {
                                                name: saved.name.clone(),
                                                spec: saved.spec.clone(),
                                                base: saved.revision,
                                            }),
                                        );
                                }
                            },
                            "Edit"
                        }
                    }
                    p { class: "muted field-hint",
                        "Kept on the gateway for {agent}'s repository, not in it. Revision {saved.revision}, {updated} by {by} {from}."
                    }
                    if !shown_errors.is_empty() {
                        ul { class: "problems",
                            for problem in shown_errors {
                                li { key: "{problem}", "{problem}" }
                            }
                        }
                    }
                    if let Some((revision, old, old_description)) = review {
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
                            NoteDiff { old, new: saved.spec.body.clone() }
                            div { class: "schedule-actions",
                                button { disabled: busy(), onclick: approve,
                                    "Approve revision {revision}"
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
                                        move |spec: NoteSpec| {
                                            save(Draft {
                                                name: name.clone(),
                                                spec,
                                                base,
                                            })
                                        }
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
