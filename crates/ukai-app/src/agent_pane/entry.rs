//! One entry of a pane's transcript.

use std::path::{Path, PathBuf};

use dioxus::prelude::*;
use ukai_acp::checkout::CommandOutput;
use ukai_acp::{FileDiff, ToolStatus};
use ukai_core::report::ReportKind;
use ukai_core::{Entry, PaneContent, PaneId, ProjectId, ReportSource, review, shell};

use super::review_of;
use crate::clipboard;
use crate::diff_view::DiffLines;
use crate::icons::{Icon, Svg};
use crate::layout::{launch_target, open_report};
use crate::markdown::Markdown;
use crate::projects::AppSignals;
use crate::report_pane::project_relative;

/// An agent message, with a button that copies its text whole.
#[component]
fn AgentMessage(text: String) -> Element {
    let mut copied = use_signal(|| false);
    let copy = {
        let text = text.clone();
        move |_| {
            if clipboard::copy(&text) {
                copied.set(true);
                spawn(async move {
                    tokio::time::sleep(COPIED_FOR).await;
                    copied.set(false);
                });
            }
        }
    };
    rsx! {
        div { class: "agent-msg",
            div { class: "msg agent",
                Markdown { source: text.clone() }
            }
            div { class: "msg-actions",
                button {
                    class: if copied() { "icon copied" } else { "icon" },
                    title: if copied() { "Copied" } else { "Copy" },
                    onclick: copy,
                    Svg { icon: if copied() { Icon::Check } else { Icon::Copy } }
                }
            }
        }
    }
}

/// How long the copy button shows that it copied.
const COPIED_FOR: std::time::Duration = std::time::Duration::from_millis(1500);

#[component]
pub(super) fn EntryView(pane: PaneId, entry: Entry) -> Element {
    let app = use_context::<AppSignals>();
    let sessions = app.sessions;
    match entry {
        Entry::User { text, attachments } => rsx! {
            div { class: "msg user",
                "{text}"
                if !attachments.is_empty() {
                    div { class: "msg-attachments",
                        for (index, name) in attachments.iter().enumerate() {
                            span { key: "{index}", class: "chip",
                                Svg { icon: Icon::Paperclip, size: 11 }
                                span { class: "name", "{name}" }
                            }
                        }
                    }
                }
            }
        },
        Entry::Agent { text } => rsx! {
            AgentMessage { text }
        },
        Entry::Thought { text } => rsx! {
            details { class: "thought",
                summary {
                    span { class: "chevron",
                        Svg { icon: Icon::Chevron, size: 12 }
                    }
                    "Thinking"
                }
                div { class: "body",
                    Markdown { source: text }
                }
            }
        },
        Entry::Tool {
            title,
            kind,
            status,
            diffs,
            children,
            ..
        } => {
            let state = match status {
                ToolStatus::Pending => "pending",
                ToolStatus::InProgress => "running",
                ToolStatus::Completed => "done",
                ToolStatus::Failed => "failed",
            };
            if !children.is_empty() {
                return rsx! {
                    Subagent {
                        pane,
                        title,
                        kind,
                        state,
                        entries: children,
                    }
                };
            }
            if diffs.is_empty() {
                return rsx! {
                    div { class: "tool {state}", title: "{kind} · {state}",
                        span { class: "tool-icon" }
                        span { class: "verb", "{kind}" }
                        span { class: "name", "{title}" }
                    }
                };
            }
            let (added, removed) = diffs
                .iter()
                .map(|d| review::stats(&review::hunks(d.old_text.as_deref(), &d.new_text)))
                .fold((0, 0), |(a, r), (da, dr)| (a + da, r + dr));
            rsx! {
                details { class: "tool-diffs",
                    summary {
                        class: "tool {state}",
                        title: "{kind} · {state} · click to review",
                        span { class: "tool-icon" }
                        span { class: "verb", "{kind}" }
                        span { class: "name", "{title}" }
                        span { class: "diffstat",
                            span { class: "added", "+{added}" }
                            span { class: "removed", "−{removed}" }
                        }
                    }
                    for (index, diff) in diffs.into_iter().enumerate() {
                        DiffView { key: "{index}", pane, diff }
                    }
                }
            }
        }
        Entry::Plan { items } => {
            let done = items.iter().filter(|item| item.done).count();
            rsx! {
                div { class: "plan",
                    div { class: "plan-head",
                        "Plan"
                        span { class: "count", "{done}/{items.len()}" }
                    }
                    ul {
                        for (index, item) in items.iter().enumerate() {
                            li {
                                key: "{index}",
                                class: if item.done { "done" } else if item.active { "active" } else { "" },
                                "{item.content}"
                            }
                        }
                    }
                }
            }
        }
        Entry::Permission {
            id,
            title,
            options,
            answer,
        } => {
            let pending = answer.is_none();
            rsx! {
                div { class: if pending { "permission pending" } else { "permission" },
                    div { class: "permission-head",
                        span { class: "label", "Permission" }
                        span { class: "title", "{title}" }
                    }
                    match answer {
                        Some(answer) => rsx! {
                            div { class: "answer", "→ {answer}" }
                        },
                        None => rsx! {
                            div { class: "options",
                                for (index, option) in options.into_iter().enumerate() {
                                    button {
                                        key: "{option.id}",
                                        class: if index == 0 { "small primary" } else { "small outline" },
                                        onclick: move |_| {
                                            let Some(session) = sessions.read().get(&pane).cloned() else {
                                                return;
                                            };
                                            if let Some(handle) = &session.handle {
                                                handle.resolve_permission(id, Some(option.id.clone()));
                                            }
                                            let mut transcript = session.transcript;
                                            transcript.write().answer_permission(id, option.name.clone());
                                        },
                                        "{option.name}"
                                    }
                                }
                            }
                        },
                    }
                }
            }
        }
        Entry::FileWritten { path } => {
            let report = written_report(app, pane, &path);
            rsx! {
                div { class: "tool done",
                    span { class: "tool-icon" }
                    span { class: "verb", "wrote" }
                    span { class: "name", "{path.display()}" }
                    if let Some((project, relative)) = report {
                        button {
                            class: "small outline open-report",
                            title: "Show it in a pane",
                            onclick: move |_| {
                                if let Some(target) = launch_target(app) {
                                    open_report(app, target, &project, relative.clone(), ReportSource::Project);
                                }
                            },
                            Svg { icon: Icon::External, size: 12 }
                            "Open"
                        }
                    }
                }
            }
        }
        Entry::Notice { text, error } => rsx! {
            div { class: if error { "notice-line error" } else { "notice-line" }, "{text}" }
        },
        Entry::Shell {
            command,
            result,
            sent,
            ..
        } => rsx! {
            ShellRun { command, result, sent }
        },
    }
}

/// A tool call that started a subagent, with what the subagent said and did
/// folded under it: subagents run side by side, and their output would
/// otherwise interleave with the agent's own.
#[component]
fn Subagent(
    pane: PaneId,
    title: String,
    kind: String,
    state: &'static str,
    entries: Vec<Entry>,
) -> Element {
    let calls = entries
        .iter()
        .filter(|child| matches!(child, Entry::Tool { .. }))
        .count();
    rsx! {
        details { class: "subagent",
            summary { class: "tool {state}", title: "{kind} · {state}",
                span { class: "tool-icon" }
                span { class: "verb", "{kind}" }
                span { class: "name", "{title}" }
                span { class: "count",
                    if calls == 1 {
                        "1 tool call"
                    } else {
                        "{calls} tool calls"
                    }
                }
            }
            div { class: "body",
                for (index, entry) in entries.into_iter().enumerate() {
                    EntryView { key: "{index}", pane, entry }
                }
            }
        }
    }
}

/// A command the user ran with `!`, and what it printed.
#[component]
fn ShellRun(command: String, result: Option<Result<CommandOutput, String>>, sent: bool) -> Element {
    let (outcome, failed) = match &result {
        None => ("running…".to_owned(), false),
        Some(Ok(output)) => (shell::outcome(output), output.exit_code != Some(0)),
        Some(Err(_)) => ("could not run".to_owned(), true),
    };
    rsx! {
        div { class: "shell-run",
            div { class: "shell-head",
                Svg { icon: Icon::Terminal, size: 12 }
                code { class: "command", "{command}" }
                span { class: if failed { "outcome failed" } else { "outcome" }, "{outcome}" }
            }
            match &result {
                Some(Ok(output)) if !output.output.is_empty() => rsx! {
                    pre { class: "shell-output",
                        if output.truncated {
                            span { class: "muted", "…\n" }
                        }
                        "{output.output}"
                    }
                },
                Some(Err(message)) => rsx! {
                    div { class: "shell-output error", "{message}" }
                },
                _ => rsx! {},
            }
            if !sent && matches!(result, Some(Ok(_))) {
                div { class: "shell-note", "Goes to the agent with your next message" }
            }
        }
    }
}

/// Lines of a diff shown before the rest is cut off.
const MAX_DIFF_LINES: usize = 400;

/// One file's change with its lines open to comments, which go to the
/// pane's next prompt.
#[component]
fn DiffView(pane: PaneId, diff: FileDiff) -> Element {
    let app = use_context::<AppSignals>();
    let Some(review) = review_of(app, pane) else {
        return rsx! {};
    };
    let path = shown_path(app, pane, &diff.path);
    let hunks = review::hunks(diff.old_text.as_deref(), &diff.new_text);
    let is_new = diff.old_text.is_none();

    rsx! {
        div { class: "diff",
            div { class: "diff-path",
                span { class: "name", "{path}" }
                if is_new {
                    span { class: "badge", "new" }
                }
            }
            DiffLines {
                path,
                hunks,
                review: Some(review),
                max_lines: MAX_DIFF_LINES,
                placeholder: "Comment for the agent; sent with your next message",
            }
        }
    }
}

/// `path` relative to the root of `pane`'s project when inside it.
fn shown_path(app: AppSignals, pane: PaneId, path: &Path) -> String {
    let root = app
        .layout
        .peek()
        .content(pane)
        .and_then(PaneContent::project)
        .and_then(|project| app.projects.peek().get(project).map(|p| p.root.clone()));
    root.and_then(|root| path.strip_prefix(root).ok().map(Path::to_owned))
        .unwrap_or_else(|| path.to_owned())
        .display()
        .to_string()
}

/// The project of agent `pane` and `path` relative to its root, when `path`
/// is a report or image inside it.
fn written_report(app: AppSignals, pane: PaneId, path: &Path) -> Option<(ProjectId, PathBuf)> {
    ReportKind::of(path)?;
    let project = app.layout.peek().content(pane)?.project()?.clone();
    let root = app.projects.peek().get(&project)?.root.clone();
    let relative = project_relative(&root, path)?;
    Some((project, relative))
}
