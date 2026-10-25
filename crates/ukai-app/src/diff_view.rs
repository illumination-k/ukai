//! The lines of a file's diff, numbered on both sides, each open to a
//! comment that goes to an agent pane's next prompt, with the agent's
//! annotations under the lines they are about. Shared by the diffs of an
//! agent's tool calls and the diff pane of a whole checkout.

use dioxus::prelude::*;
use ukai_acp::review::{Annotation, Author};
use ukai_core::keymap::{Action, Scope};
use ukai_core::review::{DiffLine, Hunk, LineKind, ReviewComment};

use crate::icons::{Icon, Svg};
use crate::markdown::Markdown;
use crate::shortcuts::{Shortcuts, use_ime};

/// An annotation shown under line `index` of hunk `hunk`; `outdated` when
/// that line changed since it was written.
#[derive(Debug, Clone, PartialEq)]
pub struct LineNote {
    pub hunk: usize,
    pub index: usize,
    pub annotation: Annotation,
    pub outdated: bool,
}

/// What the user does to an annotation: reply to it (`id`, text), or
/// resolve or reopen it (`id`, resolved).
#[derive(Clone, Copy, PartialEq)]
pub struct NoteActions {
    pub reply: EventHandler<(String, String)>,
    pub resolve: EventHandler<(String, bool)>,
}

/// The hunks of `path`, the first `max_lines` lines of them. Comments are
/// kept in `review`, an agent pane's; with none, lines take no comments.
/// `notes` show under their lines.
#[component]
pub fn DiffLines(
    path: String,
    hunks: Vec<Hunk>,
    review: Option<Signal<Vec<ReviewComment>>>,
    max_lines: usize,
    placeholder: String,
    #[props(default)] notes: Vec<LineNote>,
    #[props(default)] actions: Option<NoteActions>,
) -> Element {
    let shortcuts = use_context::<Shortcuts>();
    let ime = use_ime();
    let mut editing = use_signal(|| None::<(usize, usize)>);
    let mut text = use_signal(String::new);
    let total: usize = hunks.iter().map(Vec::len).sum();
    let mut budget = max_lines;
    let shown: Vec<(usize, Vec<(usize, DiffLine)>)> = hunks
        .into_iter()
        .enumerate()
        .map_while(|(h, hunk)| {
            (budget > 0).then(|| {
                let take = hunk.len().min(budget);
                budget -= take;
                (h, hunk.into_iter().take(take).enumerate().collect())
            })
        })
        .collect();
    let hidden = total.saturating_sub(max_lines);
    let comments = review
        .map(|review| review.read().clone())
        .unwrap_or_default();

    rsx! {
        if shown.is_empty() {
            div { class: "diff-gap", "No changes" }
        }
        for (h, lines) in shown {
            if h > 0 {
                div { key: "gap-{h}", class: "diff-gap", "⋯" }
            }
            for (i, line) in lines {
                {
                    let class = match line.kind {
                        LineKind::Added => "diff-line added",
                        LineKind::Removed => "diff-line removed",
                        LineKind::Context => "diff-line",
                    };
                    let marker = match line.kind {
                        LineKind::Added => "+",
                        LineKind::Removed => "-",
                        LineKind::Context => " ",
                    };
                    let old = line.old.map(|n| n.to_string()).unwrap_or_default();
                    let new = line.new.map(|n| n.to_string()).unwrap_or_default();
                    let on_line: Vec<(usize, String)> = comments
                        .iter()
                        .enumerate()
                        .filter(|(_, c)| c.path == path && c.line == line)
                        .map(|(index, c)| (index, c.body.clone()))
                        .collect();
                    let open = editing() == Some((h, i));
                    let path = path.clone();
                    let placeholder = placeholder.clone();
                    let here: Vec<LineNote> = notes
                        .iter()
                        .filter(|note| note.hunk == h && note.index == i)
                        .cloned()
                        .collect();
                    rsx! {
                        div { key: "{h}-{i}", class: "{class}",
                            span { class: "ln", "{old}" }
                            span { class: "ln", "{new}" }
                            if review.is_some() {
                                button {
                                    class: "comment-add",
                                    title: "Comment on this line",
                                    onclick: move |_| {
                                        text.set(String::new());
                                        editing.set(Some((h, i)));
                                    },
                                    "+"
                                }
                            }
                            span { class: "code", "{marker}{line.text}" }
                        }
                        for note in here {
                            AnnotationCard {
                                key: "n-{note.annotation.id}",
                                annotation: note.annotation,
                                outdated: note.outdated,
                                actions,
                            }
                        }
                        for (index, body) in on_line {
                            div { key: "c-{h}-{i}-{index}", class: "diff-comment",
                                span { class: "body", "{body}" }
                                button {
                                    class: "icon",
                                    title: "Remove comment",
                                    onclick: move |_| {
                                        if let Some(mut review) = review {
                                            review.write().remove(index);
                                        }
                                    },
                                    Svg { icon: Icon::Close, size: 11 }
                                }
                            }
                        }
                        if let (true, Some(review)) = (open, review) {
                            div { key: "e-{h}-{i}", class: "diff-comment-editor",
                                textarea {
                                    rows: "2",
                                    placeholder: "{placeholder}",
                                    value: "{text}",
                                    onmounted: move |event| async move {
                                        let _ = event.set_focus(true).await;
                                    },
                                    oninput: move |event| text.set(event.value()),
                                    oncompositionend: move |_| ime.end(),
                                    onkeyup: move |_| ime.settle(),
                                    onkeydown: {
                                        let path = path.clone();
                                        let line = line.clone();
                                        let shortcuts = shortcuts.clone();
                                        move |event: KeyboardEvent| {
                                            if ime.composing(&event) {
                                                event.stop_propagation();
                                            } else if event.key() == Key::Escape {
                                                // Not the app's: escape here drops the comment.
                                                event.stop_propagation();
                                                editing.set(None);
                                            } else if shortcuts.pressed(Scope::Composer, &event)
                                                == Some(Action::Send)
                                            {
                                                event.prevent_default();
                                                event.stop_propagation();
                                                add_comment(review, &mut text, &path, &line);
                                                editing.set(None);
                                            }
                                        }
                                    },
                                }
                                div { class: "actions",
                                    button { class: "small outline", onclick: move |_| editing.set(None), "Cancel" }
                                    button {
                                        class: "small primary",
                                        disabled: text.read().trim().is_empty(),
                                        onclick: {
                                            let path = path.clone();
                                            let line = line.clone();
                                            move |_| {
                                                add_comment(review, &mut text, &path, &line);
                                                editing.set(None);
                                            }
                                        },
                                        "Add comment"
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        if hidden > 0 {
            div { class: "diff-gap", "{hidden} more lines not shown" }
        }
    }
}

fn add_comment(
    mut review: Signal<Vec<ReviewComment>>,
    text: &mut Signal<String>,
    path: &str,
    line: &DiffLine,
) {
    let body = text.read().trim().to_owned();
    if body.is_empty() {
        return;
    }
    review.write().push(ReviewComment {
        path: path.to_owned(),
        line: line.clone(),
        body,
    });
    text.set(String::new());
}

/// One of the agent's annotations: its priority, tags, and remark, the
/// thread of replies under it, and, with `actions`, a way to reply and to
/// resolve or reopen it.
#[component]
pub fn AnnotationCard(
    annotation: Annotation,
    outdated: bool,
    actions: Option<NoteActions>,
) -> Element {
    let shortcuts = use_context::<Shortcuts>();
    let ime = use_ime();
    let mut replying = use_signal(|| false);
    let mut text = use_signal(String::new);
    let id = annotation.id.clone();
    let resolved = annotation.resolved;
    let priority = annotation.priority.map(|p| p.name());
    let class = match (resolved, priority) {
        (true, _) => "annotation resolved".to_owned(),
        (false, Some(priority)) => format!("annotation {priority}"),
        (false, None) => "annotation".to_owned(),
    };
    let mut send = {
        let id = id.clone();
        move || {
            let body = text.peek().trim().to_owned();
            if let (false, Some(actions)) = (body.is_empty(), actions) {
                actions.reply.call((id.clone(), body));
                text.set(String::new());
                replying.set(false);
            }
        }
    };
    let mut send_on_click = send.clone();

    rsx! {
        div { class: "{class}",
            div { class: "annotation-head",
                if let Some(priority) = priority {
                    span { class: "priority {priority}", "{priority}" }
                }
                for tag in annotation.tags.iter() {
                    span { key: "{tag}", class: "badge", "{tag}" }
                }
                if outdated {
                    span {
                        class: "badge outdated",
                        title: "The line changed since this was written",
                        "outdated"
                    }
                }
                if resolved {
                    span { class: "badge", "resolved" }
                }
                span { class: "spacer" }
                span { class: "muted id", "{annotation.id}" }
            }
            Markdown { source: annotation.body.clone() }
            for (index, reply) in annotation.replies.iter().enumerate() {
                div {
                    key: "{index}",
                    class: if reply.by == Author::User { "annotation-reply user" } else { "annotation-reply" },
                    span { class: "by",
                        if reply.by == Author::User {
                            "You"
                        } else {
                            "Agent"
                        }
                    }
                    Markdown { source: reply.body.clone() }
                }
            }
            if let Some(actions) = actions {
                if replying() {
                    div { class: "diff-comment-editor annotation-editor",
                        textarea {
                            rows: "2",
                            placeholder: "Reply; it goes to the agent with your next message to it",
                            value: "{text}",
                            onmounted: move |event| async move {
                                let _ = event.set_focus(true).await;
                            },
                            oninput: move |event| text.set(event.value()),
                            oncompositionend: move |_| ime.end(),
                            onkeyup: move |_| ime.settle(),
                            onkeydown: move |event: KeyboardEvent| {
                                if ime.composing(&event) {
                                    event.stop_propagation();
                                } else if event.key() == Key::Escape {
                                    event.stop_propagation();
                                    replying.set(false);
                                } else if shortcuts.pressed(Scope::Composer, &event) == Some(Action::Send) {
                                    event.prevent_default();
                                    event.stop_propagation();
                                    send();
                                }
                            },
                        }
                        div { class: "actions",
                            button {
                                class: "small outline",
                                onclick: move |_| replying.set(false),
                                "Cancel"
                            }
                            button {
                                class: "small primary",
                                disabled: text.read().trim().is_empty(),
                                onclick: move |_| send_on_click(),
                                "Reply"
                            }
                        }
                    }
                } else {
                    div { class: "annotation-actions",
                        button {
                            class: "small outline",
                            onclick: move |_| replying.set(true),
                            "Reply"
                        }
                        button {
                            class: "small outline",
                            onclick: {
                                let id = id.clone();
                                move |_| actions.resolve.call((id.clone(), !resolved))
                            },
                            if resolved {
                                "Reopen"
                            } else {
                                "Resolve"
                            }
                        }
                    }
                }
            }
        }
    }
}
