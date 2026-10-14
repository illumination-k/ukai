//! Agent panes: one ACP session per pane, kept outside the component tree.
//!
//! Splitting a pane or switching tabs remounts its component, so sessions,
//! transcripts, and drafts live in [`Sessions`] (app-scoped signals keyed by
//! pane id) rather than in component state. Closing a pane drops its entry,
//! and with it the session.

use std::collections::HashMap;

use dioxus::core::spawn_forever;
use dioxus::prelude::*;
use ukai_acp::{
    FsRoot, PaneWorkspace, SessionEvent, SessionEvents, SessionHandle, SessionOptions, ToolStatus,
};
use ukai_core::{Entry, PaneContent, PaneId, Status, Transcript};

use crate::icons::{Icon, Svg};
use crate::projects::{AppSignals, ProjectContext, project_context};

#[derive(Clone)]
pub struct AgentSession {
    /// `None` when the session could not be started.
    handle: Option<SessionHandle>,
    transcript: Signal<Transcript>,
    draft: Signal<String>,
}

/// Every pane's session, provided as context by the app root.
pub type Sessions = Signal<HashMap<PaneId, AgentSession>>;

/// Start a session for the agent `pane` holds. With `resume`, continue the
/// ACP session recorded for the pane (if any) instead of starting a new one.
pub fn connect(app: AppSignals, pane: PaneId, resume: bool) {
    let AppSignals {
        projects,
        configs,
        mut layout,
        mut sessions,
    } = app;
    let Some(PaneContent::Agent {
        project,
        agent,
        session,
        workspace,
    }) = layout.peek().content(pane).cloned()
    else {
        return;
    };
    let mut transcript = Signal::new_in_scope(Transcript::default(), ScopeId::APP);
    let draft = sessions
        .peek()
        .get(&pane)
        .map(|session| session.draft)
        .unwrap_or_else(|| Signal::new_in_scope(String::new(), ScopeId::APP));
    let restore = if resume { session } else { None };
    let started = project_context(&projects.peek(), &configs.peek(), &project)
        .and_then(|context| start_session(&context, &agent, workspace.as_ref(), restore));
    let handle = match started {
        Ok((handle, mut events)) => {
            spawn_forever(async move {
                while let Some(event) = events.recv().await {
                    if let SessionEvent::Ready { session_id, .. } = &event {
                        layout.write().record_session(pane, session_id.clone());
                    }
                    transcript.write().apply(event);
                }
            });
            Some(handle)
        }
        Err(error) => {
            transcript
                .write()
                .apply(SessionEvent::Closed { error: Some(error) });
            None
        }
    };
    // Replacing an entry drops the previous handle, which ends that session.
    sessions.write().insert(
        pane,
        AgentSession {
            handle,
            transcript,
            draft,
        },
    );
}

fn start_session(
    context: &ProjectContext,
    agent: &str,
    workspace: Option<&PaneWorkspace>,
    restore: Option<String>,
) -> Result<(SessionHandle, SessionEvents), String> {
    let spec = context
        .config
        .agents
        .get(agent)
        .ok_or_else(|| format!("agent `{agent}` is not declared for this project"))?;
    let transport = ukai_acp::resolve(spec, |name| std::env::var(name).ok(), workspace)
        .map_err(|e| e.to_string())?;
    let fs_root = FsRoot::new(&context.root).map_err(|e| e.to_string())?;
    Ok(ukai_acp::spawn_session(
        transport,
        SessionOptions {
            cwd: context.root.clone(),
            fs_root: Some(fs_root),
            restore,
        },
    ))
}

/// Header status of an agent pane: css class, label, and the agent's own name.
pub fn agent_status(
    sessions: Sessions,
    pane: PaneId,
) -> (&'static str, &'static str, Option<String>) {
    let Some(session) = sessions.read().get(&pane).cloned() else {
        return ("idle", "not connected", None);
    };
    let transcript = session.transcript.read();
    let (class, label) = match transcript.status {
        _ if transcript.awaiting_permission() => ("attention", "needs input"),
        Status::Connecting => ("busy", "connecting"),
        Status::Idle => ("ready", "ready"),
        Status::Busy => ("busy", "working"),
        Status::Closed { error: None } => ("idle", "closed"),
        Status::Closed { error: Some(_) } => ("closed", "disconnected"),
    };
    (class, label, transcript.agent.clone())
}

/// `has_saved_session`: the pane recorded an ACP session in an earlier run,
/// so connecting offers to resume it.
#[component]
pub fn AgentView(pane: PaneId, has_saved_session: bool) -> Element {
    let app = use_context::<AppSignals>();
    let Some(session) = app.sessions.read().get(&pane).cloned() else {
        return rsx! {
            div { class: "pane-body placeholder",
                div { class: "connect",
                    if has_saved_session {
                        p { class: "title", "Session not running" }
                        p { class: "muted", "Resume the earlier session, or start a new one." }
                        div { class: "actions",
                            button {
                                class: "primary",
                                onclick: move |_| connect(app, pane, true),
                                "Resume session"
                            }
                            button {
                                class: "outline",
                                onclick: move |_| connect(app, pane, false),
                                "New session"
                            }
                        }
                    } else {
                        button {
                            class: "primary",
                            onclick: move |_| connect(app, pane, false),
                            "Connect"
                        }
                    }
                }
            }
        };
    };
    let AgentSession {
        handle,
        mut transcript,
        mut draft,
    } = session;
    let status = transcript.read().status.clone();

    let mut send = {
        let handle = handle.clone();
        move || {
            let text = draft.read().trim().to_owned();
            if text.is_empty() || transcript.read().status != Status::Idle {
                return;
            }
            if handle.as_ref().is_some_and(|h| h.prompt(text.clone())) {
                transcript.write().push_prompt(text);
                draft.set(String::new());
            }
        }
    };
    let cancel = move |_| {
        if let Some(handle) = &handle {
            handle.cancel();
        }
        transcript.write().cancel();
    };

    let mut send_on_click = send.clone();
    let busy = status == Status::Busy;
    let awaiting = transcript.read().awaiting_permission();
    let idle = status == Status::Idle;
    let empty = transcript.read().entries.is_empty();

    rsx! {
        // `column-reverse` keeps the view pinned to the newest entry.
        div { class: "pane-body transcript",
            div { class: "entries",
                if empty && idle {
                    div { class: "chat-empty",
                        p { class: "title", "Ready" }
                        p { class: "muted", "Send a message to start the session." }
                    }
                }
                for (index, entry) in transcript.read().entries.iter().enumerate() {
                    EntryView { key: "{index}", pane, entry: entry.clone() }
                }
                if busy && !awaiting {
                    div { class: "working",
                        span { class: "dot busy" }
                        "Working…"
                    }
                }
            }
        }
        footer { class: "composer",
            if matches!(status, Status::Closed { .. }) {
                div { class: "composer-notice",
                    span { "The session has ended." }
                    button {
                        class: "small primary",
                        onclick: move |_| connect(app, pane, true),
                        "Reconnect"
                    }
                }
            }
            div { class: if idle { "composer-card" } else { "composer-card disabled" },
                textarea {
                    rows: "2",
                    placeholder: match status {
                        Status::Idle => "Message the agent",
                        Status::Busy => "Waiting for the agent…",
                        Status::Connecting => "Connecting…",
                        Status::Closed { .. } => "Disconnected",
                    },
                    disabled: !idle,
                    value: "{draft}",
                    oninput: move |event| draft.set(event.value()),
                    onkeydown: move |event| {
                        if event.key() == Key::Enter && !event.modifiers().shift()
                            && !event.is_composing()
                        {
                            event.prevent_default();
                            send();
                        }
                    },
                }
                div { class: "composer-footer",
                    span { class: "hint",
                        kbd { "Enter" }
                        " send · "
                        kbd { "⇧Enter" }
                        " newline"
                    }
                    if busy {
                        button {
                            class: "round secondary",
                            title: "Stop",
                            onclick: cancel,
                            Svg { icon: Icon::Stop, size: 12 }
                        }
                    } else {
                        button {
                            class: "round primary",
                            title: "Send",
                            disabled: !idle || draft.read().trim().is_empty(),
                            onclick: move |_| send_on_click(),
                            Svg { icon: Icon::Send }
                        }
                    }
                }
            }
        }
    }
}

#[component]
fn EntryView(pane: PaneId, entry: Entry) -> Element {
    let sessions = use_context::<AppSignals>().sessions;
    match entry {
        Entry::User { text } => rsx! {
            div { class: "msg user", "{text}" }
        },
        Entry::Agent { text } => rsx! {
            div { class: "msg agent", "{text}" }
        },
        Entry::Thought { text } => rsx! {
            details { class: "thought",
                summary {
                    span { class: "chevron",
                        Svg { icon: Icon::Chevron, size: 12 }
                    }
                    "Thinking"
                }
                div { class: "body", "{text}" }
            }
        },
        Entry::Tool {
            title,
            kind,
            status,
            ..
        } => {
            let state = match status {
                ToolStatus::Pending => "pending",
                ToolStatus::InProgress => "running",
                ToolStatus::Completed => "done",
                ToolStatus::Failed => "failed",
            };
            rsx! {
                div { class: "tool {state}", title: "{kind} · {state}",
                    span { class: "tool-icon" }
                    span { class: "verb", "{kind}" }
                    span { class: "name", "{title}" }
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
        Entry::FileWritten { path } => rsx! {
            div { class: "tool done",
                span { class: "tool-icon" }
                span { class: "verb", "wrote" }
                span { class: "name", "{path.display()}" }
            }
        },
        Entry::Notice { text, error } => rsx! {
            div { class: if error { "notice-line error" } else { "notice-line" }, "{text}" }
        },
    }
}
