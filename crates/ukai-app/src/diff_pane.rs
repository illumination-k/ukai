//! The diff pane: everything that changed in a workspace's checkout, as
//! `git diff` has it ([`ukai_acp::checkout::diff`]) — this machine's, or a
//! gateway agent's in its pod — from where its branch left the default
//! branch, or from `HEAD`. Its lines take comments like the diffs of an
//! agent's tool calls, and they go to an agent pane working in the same
//! checkout, with its next prompt.
//!
//! A gateway agent may have reviewed its changes ([`ukai_acp::review`]): its
//! summary shows above the diff, and its annotations under the lines they
//! are about, filtered by priority, tag, and whether they are resolved. A
//! reply goes into the annotation's thread and, as a comment naming it, to
//! the agent with the next prompt, so the agent answers there.

use std::time::Duration;

use dioxus::prelude::*;
use ukai_acp::checkout::{CheckoutDiff, DiffBase};
use ukai_acp::review::{AnnotateRequest, Annotation, Priority, Review};
use ukai_acp::{Checkout, PaneWorkspace, WorkspaceScope};
use ukai_core::annotation::{self, Filter, Placement};
use ukai_core::diff::{self, ChangeKind, FileChange};
use ukai_core::{PaneContent, PaneId, ProjectId, ReportSource, sidebar};

use crate::agent_pane::review_of;
use crate::diff_view::{AnnotationCard, DiffLines, LineNote, NoteActions};
use crate::follow::poll;
use crate::icons::{Icon, Svg};
use crate::layout::launch_target;
use crate::markdown::Markdown;
use crate::projects::{AppSignals, project_context};

/// How often an open diff pane reads the checkout again.
const RELOAD: Duration = Duration::from_secs(10);

/// Lines of one file shown before the rest is cut off.
const MAX_FILE_LINES: usize = 3000;

/// Show the changes of `place` in `project`: focus the pane already showing
/// them, else open one where [`launch_target`] puts it.
pub fn open_diff(app: AppSignals, project: &ProjectId, place: &ReportSource) {
    let mut layout = app.layout;
    let content = PaneContent::Diff {
        project: project.clone(),
        place: place.clone(),
    };
    let shown = layout
        .peek()
        .contents()
        .find_map(|(pane, open)| (*open == content).then_some(pane));
    if let Some(pane) = shown {
        layout.write().focus(pane);
        return;
    }
    if let Some(pane) = launch_target(app) {
        let mut layout = layout.write();
        layout.set_content(pane, content);
        layout.focus(pane);
    }
    let mut projects = app.projects;
    projects.write().select(project);
}

/// Whether a diff pane shows the changes of `place` in `project`.
pub fn diff_open(layout: &ukai_core::Layout, project: &ProjectId, place: &ReportSource) -> bool {
    layout.contents().any(|(_, content)| {
        matches!(content, PaneContent::Diff { project: p, place: q } if p == project && q == place)
    })
}

/// The checkout `place` names in `project`.
fn checkout_at(
    app: AppSignals,
    project: &ProjectId,
    place: &ReportSource,
) -> Result<Checkout, String> {
    let context = project_context(&app.projects.peek(), &app.configs.peek(), project)?;
    let ReportSource::Agent { agent, workspace } = place else {
        return Ok(Checkout::Local(context.root));
    };
    let spec = context
        .config
        .agent(agent)
        .ok_or_else(|| format!("agent `{agent}` is not declared anymore"))?;
    // A place names the shared workspace by leaving the key out.
    let workspace = workspace.clone().unwrap_or(PaneWorkspace {
        key: String::new(),
        scope: WorkspaceScope::Shared,
    });
    Checkout::resolve(
        &spec,
        ukai_acp::system_lookup,
        Some(&workspace),
        &context.root,
    )
    .map_err(|error| error.to_string())?
    .ok_or_else(|| format!("agent `{agent}` names no gateway agent"))
}

/// What the pane last read.
#[derive(Debug, Clone, PartialEq)]
enum Read {
    Loading,
    Loaded {
        diff: CheckoutDiff,
        files: Vec<FileChange>,
    },
    Failed(String),
}

#[component]
pub fn DiffPane(pane: PaneId, project: ProjectId, place: ReportSource) -> Element {
    let app = use_context::<AppSignals>();
    let mut base = use_signal(DiffBase::default);
    let mut read = use_signal(|| Read::Loading);
    let mut selected = use_signal(|| None::<String>);
    let mut chosen = use_signal(|| None::<PaneId>);
    let mut revision = use_signal(|| 0_u64);
    use_future({
        let project = project.clone();
        let place = place.clone();
        move || {
            let project = project.clone();
            let place = place.clone();
            poll(revision, RELOAD, move || {
                let project = project.clone();
                let place = place.clone();
                async move {
                    let asked = *base.peek();
                    let now = match checkout_at(app, &project, &place) {
                        Ok(checkout) => match checkout.diff(asked).await {
                            Ok(diff) => {
                                let files = diff::parse(&diff.patch);
                                Read::Loaded { diff, files }
                            }
                            Err(error) => Read::Failed(error.to_string()),
                        },
                        Err(problem) => Read::Failed(problem),
                    };
                    // The base may have changed while git ran.
                    if *base.peek() == asked && *read.peek() != now {
                        read.set(now);
                    }
                }
            })
        }
    });
    let mut switch = move |to: DiffBase| {
        if *base.peek() != to {
            base.set(to);
            read.set(Read::Loading);
            *revision.write() += 1;
        }
    };
    let mut filter = use_signal(Filter::default);
    let mut summary_open = use_signal(|| true);
    let mut problem = use_signal(|| None::<String>);

    // The agent panes working in this checkout, which comments can go to.
    let agents = {
        let layout = app.layout.read();
        let configs = app.configs.read();
        let config = configs
            .get(&project)
            .and_then(|config| config.as_ref().ok());
        sidebar::tree(&layout, &project, config)
            .workspaces
            .into_iter()
            .find(|workspace| workspace.place == place)
            .map(|workspace| workspace.agents)
            .unwrap_or_default()
    };
    let target = chosen()
        .filter(|pane| agents.iter().any(|agent| agent.pane == *pane))
        .or_else(|| agents.first().map(|agent| agent.pane));
    // Where comments on lines wait for the target's next prompt.
    let comments = target.and_then(|pane| review_of(app, pane));
    let pending = comments.map_or(0, |comments| comments.read().len());
    let target_name = target
        .and_then(|pane| agents.iter().find(|agent| agent.pane == pane))
        .map(|agent| agent.agent.clone());

    let current = read();
    let (files, label, truncated) = match &current {
        Read::Loaded { diff, files } => (files.clone(), diff.base.clone(), diff.truncated),
        _ => (Vec::new(), String::new(), false),
    };
    let (count, added, removed) = diff::totals(&files);
    let shown = selected()
        .filter(|path| files.iter().any(|file| file.path == *path))
        .or_else(|| files.first().map(|file| file.path.clone()));
    let file = shown
        .as_ref()
        .and_then(|path| files.iter().find(|file| file.path == *path))
        .cloned();
    let agent_review: Option<Review> = match &current {
        Read::Loaded { diff, .. } => diff.review.clone(),
        _ => None,
    };

    // Reply to an annotation: in its thread, and to the agent with its next
    // prompt, naming the annotation.
    let reply = use_callback({
        let project = project.clone();
        let place = place.clone();
        move |(id, body): (String, String)| {
            let annotation = annotation_of(&read.peek(), &id);
            if let (Some(mut comments), Some(annotation)) = (comments, annotation) {
                comments
                    .write()
                    .push(annotation::reply_comment(&annotation, &body));
            }
            annotate(
                app,
                &project,
                &place,
                read,
                problem,
                AnnotateRequest {
                    id,
                    reply: Some(body),
                    resolved: None,
                },
            );
        }
    });
    let resolve = use_callback({
        let project = project.clone();
        let place = place.clone();
        move |(id, resolved): (String, bool)| {
            annotate(
                app,
                &project,
                &place,
                read,
                problem,
                AnnotateRequest {
                    id,
                    reply: None,
                    resolved: Some(resolved),
                },
            );
        }
    });
    let actions = NoteActions { reply, resolve };
    let shown_filter = filter();
    // The file's annotations the filter lets through: above its diff, or
    // under their lines.
    let (above, notes) = match (&agent_review, &file) {
        (Some(review), Some(file)) => {
            let mut above = Vec::new();
            let mut notes = Vec::new();
            for annotation in review
                .annotations
                .iter()
                .filter(|a| a.path == file.path && shown_filter.shows(a))
            {
                match annotation::place(annotation, file) {
                    Placement::Line {
                        hunk,
                        index,
                        outdated,
                    } => notes.push(LineNote {
                        hunk,
                        index,
                        annotation: annotation.clone(),
                        outdated,
                    }),
                    Placement::File => above.push((annotation.clone(), false)),
                    Placement::Outside => above.push((annotation.clone(), true)),
                }
            }
            (above, notes)
        }
        _ => (Vec::new(), Vec::new()),
    };
    // Annotations on files this diff does not show.
    let elsewhere = agent_review.as_ref().map_or(0, |review| {
        review
            .annotations
            .iter()
            .filter(|a| !a.resolved && !files.iter().any(|f| f.path == a.path))
            .count()
    });

    rsx! {
        div { class: "pane-body diff-pane",
            div { class: "diff-toolbar",
                div { class: "segmented",
                    for (choice, name, title) in [
                        (
                            DiffBase::Branch,
                            "Branch",
                            "Everything since the branch left the default branch",
                        ),
                        (DiffBase::Uncommitted, "Uncommitted", "What is not committed yet"),
                    ]
                    {
                        button {
                            key: "{name}",
                            class: if base() == choice { "active" } else { "" },
                            title: "{title}",
                            onclick: move |_| switch(choice),
                            "{name}"
                        }
                    }
                }
                if !label.is_empty() {
                    span {
                        class: "muted diff-base",
                        title: "What the diff is taken from",
                        "from {label}"
                    }
                }
                if count > 0 {
                    span { class: "diff-totals",
                        "{count} files "
                        span { class: "added", "+{added}" }
                        " "
                        span { class: "removed", "−{removed}" }
                    }
                }
                span { class: "spacer" }
                if agents.len() > 1 {
                    div {
                        class: "segmented",
                        title: "The agent your comments go to",
                        for agent in agents.clone() {
                            button {
                                key: "{agent.pane}",
                                class: if target == Some(agent.pane) { "active" } else { "" },
                                onclick: move |_| chosen.set(Some(agent.pane)),
                                "{agent.agent}"
                            }
                        }
                    }
                }
                if let (Some(pane), Some(name)) = (target, target_name.clone()) {
                    button {
                        class: "small outline",
                        title: "Go to {name}, where the comments are sent with your next message",
                        onclick: move |_| {
                            let mut layout = app.layout;
                            layout.write().focus(pane);
                        },
                        if pending > 0 {
                            "Send {pending} to {name}…"
                        } else {
                            "Go to {name}"
                        }
                    }
                }
                button {
                    class: "icon",
                    title: "Read the changes again",
                    onclick: move |_| *revision.write() += 1,
                    Svg { icon: Icon::Reload }
                }
            }
            if target.is_none() {
                p { class: "muted hint diff-hint",
                    "Open an agent in this workspace to comment on these lines; comments go with its next message."
                }
            }
            if truncated {
                p { class: "muted hint diff-hint",
                    "The changes are too large to show in full; what is left out is at the end."
                }
            }
            if let Some(why) = problem() {
                div { class: "config-error diff-hint review-problem",
                    span { "{why}" }
                    button {
                        class: "icon",
                        title: "Dismiss",
                        onclick: move |_| problem.set(None),
                        Svg { icon: Icon::Close, size: 11 }
                    }
                }
            }
            if let Some(review) = agent_review.clone() {
                {
                    let (counts, plain) = annotation::open_counts(&review);
                    let tags = annotation::tags(&review);
                    rsx! {
                        div { class: "review-bar",
                            div { class: "review-head",
                                button {
                                    class: if summary_open() { "fold open" } else { "fold" },
                                    title: "Show or hide the summary",
                                    onclick: move |_| summary_open.toggle(),
                                    Svg { icon: Icon::Chevron, size: 10 }
                                }
                                span { class: "review-title", "Agent's review" }
                                for (priority, count) in counts {
                                    button {
                                        key: "{priority.name()}",
                                        class: if shown_filter.priorities.contains(&priority) { "chip priority {priority.name()} active" } else { "chip priority {priority.name()}" },
                                        title: "Show only {priority.name()}, with the others chosen",
                                        onclick: move |_| filter.write().toggle(priority),
                                        "{priority.name()} {count}"
                                    }
                                }
                                if plain > 0 {
                                    span { class: "muted chip-note", "{plain} remarks" }
                                }
                                for tag in tags {
                                    button {
                                        key: "tag-{tag}",
                                        class: if shown_filter.tag.as_deref() == Some(tag.as_str()) { "chip active" } else { "chip" },
                                        onclick: {
                                            let tag = tag.clone();
                                            move |_| {
                                                let mut filter = filter.write();
                                                filter.tag = if filter.tag.as_deref() == Some(tag.as_str()) {
                                                    None
                                                } else {
                                                    Some(tag.clone())
                                                };
                                            }
                                        },
                                        "#{tag}"
                                    }
                                }
                                button {
                                    class: if shown_filter.resolved { "chip active" } else { "chip" },
                                    title: "Show resolved annotations too",
                                    onclick: move |_| {
                                        let mut filter = filter.write();
                                        filter.resolved = !filter.resolved;
                                    },
                                    "resolved"
                                }
                                if elsewhere > 0 {
                                    span {
                                        class: "muted chip-note",
                                        title: "Annotations on files this diff does not show; try the other base",
                                        "{elsewhere} on files not shown"
                                    }
                                }
                            }
                            if summary_open() && !review.summary.is_empty() {
                                div { class: "review-summary",
                                    Markdown { source: review.summary.clone() }
                                }
                            }
                        }
                    }
                }
            }
            match current {
                Read::Loading => rsx! {
                    p { class: "muted hint diff-hint", "Reading the changes…" }
                },
                Read::Failed(problem) => rsx! {
                    pre { class: "config-error diff-hint", "{problem}" }
                },
                Read::Loaded { .. } if files.is_empty() => rsx! {
                    p { class: "muted hint diff-hint", "No changes from {label}." }
                },
                Read::Loaded { .. } => rsx! {
                    div { class: "diff-split",
                        div { class: "diff-files",
                            for changed in files.iter().cloned() {
                                FileRow {
                                    key: "{changed.path}",
                                    mark: agent_review
                                        .as_ref()
                                        .and_then(|review| annotation::file_mark(review, &changed.path)),
                                    selected: shown.as_deref() == Some(changed.path.as_str()),
                                    on_select: move |path| selected.set(Some(path)),
                                    file: changed,
                                }
                            }
                        }
                        if let Some(file) = file {
                            div { class: "diff-file", key: "{file.path}",
                                div { class: "diff-path",
                                    span { class: "name", "{file.path}" }
                                    if let ChangeKind::Renamed { from } = &file.kind {
                                        span { class: "muted", "from {from}" }
                                    }
                                    span { class: "badge", "{kind_label(&file.kind)}" }
                                }
                                for (note, outside) in above {
                                    div { key: "above-{note.id}", class: "annotation-above",
                                        if outside {
                                            div { class: "muted annotation-where",
                                                "Line {note.shown_at().unwrap_or_default()}, not in this diff"
                                            }
                                        }
                                        AnnotationCard {
                                            annotation: note,
                                            outdated: false,
                                            actions: Some(actions),
                                        }
                                    }
                                }
                                if file.binary {
                                    div { class: "diff-gap", "Binary or too large; its lines are not shown." }
                                } else if file.hunks.is_empty() {
                                    div { class: "diff-gap", "Only moved, or its mode changed." }
                                } else {
                                    DiffLines {
                                        path: file.path.clone(),
                                        hunks: file.hunks.clone(),
                                        review: comments,
                                        max_lines: MAX_FILE_LINES,
                                        placeholder: match &target_name {
                                            Some(name) => format!("Comment for {name}; sent with your next message to it"),
                                            None => String::new(),
                                        },
                                        notes,
                                        actions: Some(actions),
                                    }
                                }
                            }
                        }
                    }
                },
            }
        }
    }
}

fn kind_label(kind: &ChangeKind) -> &'static str {
    match kind {
        ChangeKind::Added => "added",
        ChangeKind::Deleted => "deleted",
        ChangeKind::Modified => "modified",
        ChangeKind::Renamed { .. } => "renamed",
    }
}

/// The annotation `id` of the review last read.
fn annotation_of(read: &Read, id: &str) -> Option<Annotation> {
    let Read::Loaded { diff, .. } = read else {
        return None;
    };
    diff.review
        .as_ref()?
        .annotations
        .iter()
        .find(|annotation| annotation.id == id)
        .cloned()
}

/// Send the user's `request` about an annotation to the checkout's relay,
/// and show the review as it then is, or why it was refused.
fn annotate(
    app: AppSignals,
    project: &ProjectId,
    place: &ReportSource,
    mut read: Signal<Read>,
    mut problem: Signal<Option<String>>,
    request: AnnotateRequest,
) {
    let checkout = match checkout_at(app, project, place) {
        Ok(checkout) => checkout,
        Err(why) => {
            problem.set(Some(why));
            return;
        }
    };
    spawn(async move {
        match checkout.annotate(request).await {
            Ok(review) => {
                problem.set(None);
                if let Read::Loaded { diff, .. } = &mut *read.write() {
                    diff.review = Some(review);
                }
            }
            Err(error) => problem.set(Some(error.to_string())),
        }
    });
}

/// One changed file in the list: its kind, name, folder, line counts, and
/// the open annotations on it, by the most pressing priority.
#[component]
fn FileRow(
    file: FileChange,
    selected: bool,
    on_select: EventHandler<String>,
    mark: Option<(Option<Priority>, usize)>,
) -> Element {
    let (added, removed) = file.stats();
    let (dir, name) = match file.path.rsplit_once('/') {
        Some((dir, name)) => (dir.to_owned(), name.to_owned()),
        None => (String::new(), file.path.clone()),
    };
    let letter = file.kind.letter();
    let path = file.path.clone();
    rsx! {
        div {
            class: if selected { "row diff-file-row focused" } else { "row diff-file-row" },
            title: "{file.path}",
            onclick: move |_| on_select.call(path.clone()),
            span { class: "kind {letter}", "{letter}" }
            span { class: "name", "{name}" }
            span { class: "dir", "{dir}" }
            if let Some((priority, count)) = mark {
                span {
                    class: "note-mark {priority.map_or(\"plain\", Priority::name)}",
                    title: "Open annotations",
                    "{count}"
                }
            }
            span { class: "counts",
                if added > 0 {
                    span { class: "added", "+{added}" }
                }
                if removed > 0 {
                    span { class: "removed", "−{removed}" }
                }
            }
        }
    }
}
