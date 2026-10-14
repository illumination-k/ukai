//! Agent panes: one ACP session per pane, kept outside the component tree.
//!
//! Splitting a pane or switching tabs remounts its component, so sessions,
//! transcripts, and drafts live in [`Sessions`] (app-scoped signals keyed by
//! pane id) rather than in component state. Closing a pane drops its entry,
//! and with it the session.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use dioxus::core::spawn_forever;
use dioxus::html::HasFileData;
use dioxus::prelude::*;
use ukai_acp::{
    Attachment, ConfigInput, ConfigOption, ConfigValue, FileDiff, FsRoot, PaneWorkspace, Prompt,
    SessionEvent, SessionEvents, SessionHandle, SessionOptions, SlashCommand, ToolStatus,
};
use ukai_core::keymap::{Action, Scope};
use ukai_core::report::ReportKind;
use ukai_core::review::{self, LineKind, ReviewComment};
use ukai_core::slash;
use ukai_core::{Alert, Entry, PaneContent, PaneId, ProjectId, Status, Transcript};

use crate::alert;
use crate::clipboard;
use crate::icons::{Icon, Svg};
use crate::layout::{launch_target, open_report};
use crate::markdown::Markdown;
use crate::projects::{AppSignals, ProjectContext, project_context};
use crate::report_pane::project_relative;
use crate::shortcuts::Shortcuts;

#[derive(Clone)]
pub struct AgentSession {
    /// `None` when the session could not be started.
    handle: Option<SessionHandle>,
    transcript: Signal<Transcript>,
    composer: Composer,
    /// Something happened in the session that the user has not looked at.
    pub unread: Signal<bool>,
}

/// What the user is putting together for the next prompt. It outlives a
/// reconnect of the pane's session.
#[derive(Clone, Copy, PartialEq)]
struct Composer {
    draft: Signal<String>,
    attachments: Signal<Vec<Pending>>,
    /// Comments on diff lines, sent ahead of the draft.
    review: Signal<Vec<ReviewComment>>,
    /// Why the last file could not be attached.
    error: Signal<Option<String>>,
}

impl Composer {
    fn new() -> Self {
        Self {
            draft: Signal::new_in_scope(String::new(), ScopeId::APP),
            attachments: Signal::new_in_scope(Vec::new(), ScopeId::APP),
            review: Signal::new_in_scope(Vec::new(), ScopeId::APP),
            error: Signal::new_in_scope(None, ScopeId::APP),
        }
    }
}

/// A file attached to the next prompt.
#[derive(Debug, Clone, PartialEq)]
struct Pending {
    name: String,
    attachment: Attachment,
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
        ..
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
    let (composer, unread) = sessions.peek().get(&pane).map_or_else(
        || (Composer::new(), Signal::new_in_scope(false, ScopeId::APP)),
        |session| (session.composer, session.unread),
    );
    let restore = if resume { session } else { None };
    let source = match projects.peek().get(&project) {
        Some(project) => format!("{agent} · {}", project.name()),
        None => agent.clone(),
    };
    let started = project_context(&projects.peek(), &configs.peek(), &project)
        .and_then(|context| start_session(&context, &agent, workspace.as_ref(), restore));
    let handle = match started {
        Ok((handle, mut events)) => {
            let window = dioxus::desktop::window();
            spawn_forever(async move {
                while let Some(event) = events.recv().await {
                    if let SessionEvent::Ready { session_id, .. } = &event {
                        layout.write().record_session(pane, session_id.clone());
                    }
                    let raised = Alert::of(&event);
                    transcript.write().apply(event);
                    if let Some(raised) = raised {
                        alert::raise(&window, layout, pane, unread, &source, &raised);
                    }
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
            composer,
            unread,
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

/// Stop `pane`'s turn, if it is running one.
pub fn stop_turn(sessions: Sessions, pane: PaneId) {
    let Some(session) = sessions.peek().get(&pane).cloned() else {
        return;
    };
    let mut transcript = session.transcript;
    if transcript.peek().status != Status::Busy {
        return;
    }
    if let Some(handle) = &session.handle {
        handle.cancel();
    }
    transcript.write().cancel();
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
    let shortcuts = use_context::<Shortcuts>();
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
        composer,
        ..
    } = session;
    let Composer {
        mut draft,
        mut attachments,
        mut review,
        mut error,
    } = composer;
    let status = transcript.read().status.clone();
    let mut dropping = use_signal(|| false);
    // The highlighted slash command, and whether escape hid the list.
    let mut picked = use_signal(|| 0_usize);
    let mut dismissed = use_signal(|| false);

    let mut send = {
        let handle = handle.clone();
        move || {
            let text = draft.read().trim().to_owned();
            let comments = review.read().clone();
            let pending = attachments.read().clone();
            if (text.is_empty() && comments.is_empty() && pending.is_empty())
                || transcript.read().status != Status::Idle
            {
                return;
            }
            let text = review::review_prompt(&comments, &text);
            let names = pending.iter().map(|p| p.name.clone()).collect();
            let prompt = Prompt {
                text: text.clone(),
                attachments: pending.into_iter().map(|p| p.attachment).collect(),
            };
            if handle.as_ref().is_some_and(|h| h.prompt(prompt)) {
                transcript.write().push_prompt(text, names);
                draft.set(String::new());
                review.set(Vec::new());
                attachments.set(Vec::new());
                error.set(None);
            }
        }
    };
    let cancel = move |_| stop_turn(app.sessions, pane);

    let mut send_on_click = send.clone();
    let busy = status == Status::Busy;
    let awaiting = transcript.read().awaiting_permission();
    let idle = status == Status::Idle;
    // The next message can be written while a turn runs; it sends once idle.
    let writable = idle || busy;
    let empty = transcript.read().entries.is_empty();
    let has_extras = !review.read().is_empty() || !attachments.read().is_empty();
    let config = transcript.read().config.clone();
    let card_class = match (writable, dropping()) {
        (_, true) => "composer-card dropping",
        (true, false) => "composer-card",
        (false, false) => "composer-card disabled",
    };
    let send_hint = shortcuts.keymap.hint(Action::Send);
    let commands: Vec<SlashCommand> = if writable && !dismissed() {
        slash::suggestions(&draft.read(), &transcript.read().commands)
            .into_iter()
            .take(MAX_SUGGESTIONS)
            .cloned()
            .collect()
    } else {
        Vec::new()
    };
    let current = picked().min(commands.len().saturating_sub(1));
    let stop_title = shortcuts.title("Stop", Action::Stop);

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
            div {
                class: card_class,
                ondragover: move |event| {
                    event.prevent_default();
                    if !dropping() {
                        dropping.set(true);
                    }
                },
                ondragleave: move |_| dropping.set(false),
                ondrop: move |event| {
                    event.prevent_default();
                    dropping.set(false);
                    for file in event.files() {
                        spawn(async move {
                            let path = file.path();
                            match file.read_bytes().await {
                                Ok(bytes) => attach(composer, transcript, &path, bytes.to_vec()),
                                Err(e) => error.set(Some(format!("{}: {e}", path.display()))),
                            }
                        });
                    }
                },
                if !review.read().is_empty() {
                    div { class: "composer-review",
                        for (index, comment) in review.read().iter().enumerate() {
                            div { key: "{index}", class: "review-item",
                                span { class: "location", "{comment.location()}" }
                                span { class: "body", "{comment.body}" }
                                button {
                                    class: "icon",
                                    title: "Remove comment",
                                    onclick: move |_| {
                                        review.write().remove(index);
                                    },
                                    Svg { icon: Icon::Close, size: 12 }
                                }
                            }
                        }
                    }
                }
                if !attachments.read().is_empty() {
                    div { class: "composer-attachments",
                        for (index, pending) in attachments.read().iter().enumerate() {
                            span {
                                key: "{index}",
                                class: "chip",
                                title: "{pending.name}",
                                Svg { icon: Icon::Paperclip, size: 12 }
                                span { class: "name", "{pending.name}" }
                                button {
                                    class: "icon",
                                    title: "Remove",
                                    onclick: move |_| {
                                        attachments.write().remove(index);
                                    },
                                    Svg { icon: Icon::Close, size: 11 }
                                }
                            }
                        }
                    }
                }
                if let Some(message) = error() {
                    div { class: "composer-error", "{message}" }
                }
                if !commands.is_empty() {
                    div { class: "slash-list",
                        for (index, command) in commands.iter().enumerate() {
                            div {
                                key: "{command.name}",
                                class: if index == current { "slash-item selected" } else { "slash-item" },
                                onmouseenter: move |_| picked.set(index),
                                // Keep the focus in the textarea.
                                onmousedown: move |event| event.prevent_default(),
                                onclick: {
                                    let command = command.clone();
                                    move |_| {
                                        draft.set(slash::completion(&command));
                                        picked.set(0);
                                    }
                                },
                                span { class: "name", "/{command.name}" }
                                if let Some(hint) = &command.hint {
                                    span { class: "hint", "{hint}" }
                                }
                                span { class: "description", "{command.description}" }
                            }
                        }
                    }
                }
                textarea {
                    rows: "2",
                    placeholder: match status {
                        Status::Idle => "Message the agent — / for commands, drop files or images here",
                        Status::Busy => "Waiting for the agent…",
                        Status::Connecting => "Connecting…",
                        Status::Closed { .. } => "Disconnected",
                    },
                    disabled: !writable,
                    value: "{draft}",
                    oninput: move |event| {
                        draft.set(event.value());
                        picked.set(0);
                        dismissed.set(false);
                    },
                    onkeydown: {
                        let shortcuts = shortcuts.clone();
                        move |event: KeyboardEvent| {
                            if !commands.is_empty() && !event.is_composing() {
                                let len = commands.len() as isize;
                                match event.key() {
                                    Key::ArrowDown => {
                                        picked.set((current as isize + 1).rem_euclid(len) as usize);
                                    }
                                    Key::ArrowUp => {
                                        picked.set((current as isize - 1).rem_euclid(len) as usize);
                                    }
                                    Key::Tab | Key::Enter => {
                                        draft.set(slash::completion(&commands[current]));
                                        picked.set(0);
                                    }
                                    // Not the app's: escape here only hides the list.
                                    Key::Escape => dismissed.set(true),
                                    _ => return,
                                }
                                event.prevent_default();
                                event.stop_propagation();
                            } else if shortcuts.pressed(Scope::Composer, &event) == Some(Action::Send) {
                                event.prevent_default();
                                event.stop_propagation();
                                send();
                            }
                        }
                    },
                }
                div { class: "composer-footer",
                    button {
                        class: "icon",
                        title: "Attach files…",
                        onclick: move |_| pick_attachments(composer, transcript),
                        Svg { icon: Icon::Paperclip }
                    }
                    if !config.is_empty() {
                        ConfigControls { pane, options: config, disabled: !writable }
                    }
                    if let Some(hint) = send_hint {
                        span { class: "hint",
                            kbd { "{hint}" }
                            " send"
                        }
                    }
                    if busy {
                        button {
                            class: "round secondary",
                            title: "{stop_title}",
                            onclick: cancel,
                            Svg { icon: Icon::Stop, size: 12 }
                        }
                    } else {
                        button {
                            class: "round primary",
                            title: "Send",
                            disabled: !idle || (draft.read().trim().is_empty() && !has_extras),
                            onclick: move |_| send_on_click(),
                            Svg { icon: Icon::Send }
                        }
                    }
                }
            }
        }
    }
}

/// The agent's settings, e.g. model and reasoning effort, changed through
/// `session/set_config_option`. A change shows once the agent confirms it.
#[component]
fn ConfigControls(pane: PaneId, options: Vec<ConfigOption>, disabled: bool) -> Element {
    let sessions = use_context::<AppSignals>().sessions;
    let set = move |id: String, value: ConfigInput| {
        if let Some(handle) = sessions.read().get(&pane).and_then(|s| s.handle.clone()) {
            handle.set_config(id, value);
        }
    };
    rsx! {
        div { class: "composer-config",
            for option in options {
                {
                    let id = option.id.clone();
                    let title = match &option.description {
                        Some(description) => format!("{}: {description}", option.name),
                        None => option.name.clone(),
                    };
                    match option.value {
                        ConfigValue::Select { current, choices } => rsx! {
                            select {
                                key: "{option.id}",
                                class: "config-select",
                                title: "{title}",
                                aria_label: "{option.name}",
                                disabled,
                                onchange: move |event| set(id.clone(), ConfigInput::Select(event.value())),
                                for (group, run) in ukai_core::config::choice_groups(&choices) {
                                    match group {
                                        Some(group) => rsx! {
                                            optgroup { label: "{group}",
                                                for choice in run {
                                                    option {
                                                        value: "{choice.value}",
                                                        selected: choice.value == current,
                                                        title: choice.description.clone().unwrap_or_default(),
                                                        "{choice.name}"
                                                    }
                                                }
                                            }
                                        },
                                        None => rsx! {
                                            for choice in run {
                                                option {
                                                    value: "{choice.value}",
                                                    selected: choice.value == current,
                                                    title: choice.description.clone().unwrap_or_default(),
                                                    "{choice.name}"
                                                }
                                            }
                                        },
                                    }
                                }
                            }
                        },
                        ConfigValue::Toggle(on) => rsx! {
                            label { key: "{option.id}", class: "config-toggle", title: "{title}",
                                input {
                                    r#type: "checkbox",
                                    checked: on,
                                    disabled,
                                    onchange: move |event| set(id.clone(), ConfigInput::Toggle(event.checked())),
                                }
                                "{option.name}"
                            }
                        },
                    }
                }
            }
        }
    }
}

/// Slash commands listed at once while completing.
const MAX_SUGGESTIONS: usize = 8;

/// Add the file at `path` to the next prompt, in the form the agent accepts.
fn attach(composer: Composer, transcript: Signal<Transcript>, path: &Path, bytes: Vec<u8>) {
    let Composer {
        mut attachments,
        mut error,
        ..
    } = composer;
    let caps = transcript.peek().prompt_caps;
    match ukai_core::attachment::attachment(path, bytes, caps) {
        Ok(attachment) => {
            attachments.write().push(Pending {
                name: ukai_core::attachment::file_name(path),
                attachment,
            });
            error.set(None);
        }
        Err(e) => error.set(Some(e.to_string())),
    }
}

/// Ask for files with the native dialog and attach them.
fn pick_attachments(composer: Composer, transcript: Signal<Transcript>) {
    spawn(async move {
        let Some(files) = rfd::AsyncFileDialog::new()
            .set_title("Attach files")
            .pick_files()
            .await
        else {
            return;
        };
        for file in files {
            let bytes = file.read().await;
            attach(composer, transcript, file.path(), bytes);
        }
    });
}

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
fn EntryView(pane: PaneId, entry: Entry) -> Element {
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
            ..
        } => {
            let state = match status {
                ToolStatus::Pending => "pending",
                ToolStatus::InProgress => "running",
                ToolStatus::Completed => "done",
                ToolStatus::Failed => "failed",
            };
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
                                    open_report(app, target, &project, relative.clone());
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
    }
}

/// Lines of a diff shown before the rest is cut off.
const MAX_DIFF_LINES: usize = 400;

/// One file's change with its lines open to comments, which go to the
/// pane's next prompt.
#[component]
fn DiffView(pane: PaneId, diff: FileDiff) -> Element {
    let app = use_context::<AppSignals>();
    let shortcuts = use_context::<Shortcuts>();
    let mut editing = use_signal(|| None::<(usize, usize)>);
    let mut text = use_signal(String::new);
    let Some(mut review) = app
        .sessions
        .read()
        .get(&pane)
        .map(|session| session.composer.review)
    else {
        return rsx! {};
    };
    let path = shown_path(app, pane, &diff.path);
    let hunks = review::hunks(diff.old_text.as_deref(), &diff.new_text);
    let total: usize = hunks.iter().map(Vec::len).sum();
    let mut budget = MAX_DIFF_LINES;
    let shown: Vec<(usize, Vec<(usize, review::DiffLine)>)> = hunks
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
    let hidden = total.saturating_sub(MAX_DIFF_LINES);
    let comments = review.read().clone();
    let is_new = diff.old_text.is_none();

    rsx! {
        div { class: "diff",
            div { class: "diff-path",
                span { class: "name", "{path}" }
                if is_new {
                    span { class: "badge", "new" }
                }
            }
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
                        rsx! {
                            div { key: "{h}-{i}", class: "{class}",
                                span { class: "ln", "{old}" }
                                span { class: "ln", "{new}" }
                                button {
                                    class: "comment-add",
                                    title: "Comment on this line",
                                    onclick: move |_| {
                                        text.set(String::new());
                                        editing.set(Some((h, i)));
                                    },
                                    "+"
                                }
                                span { class: "code", "{marker}{line.text}" }
                            }
                            for (index, body) in on_line {
                                div { key: "c-{h}-{i}-{index}", class: "diff-comment",
                                    span { class: "body", "{body}" }
                                    button {
                                        class: "icon",
                                        title: "Remove comment",
                                        onclick: move |_| {
                                            review.write().remove(index);
                                        },
                                        Svg { icon: Icon::Close, size: 11 }
                                    }
                                }
                            }
                            if open {
                                div { key: "e-{h}-{i}", class: "diff-comment-editor",
                                    textarea {
                                        rows: "2",
                                        placeholder: "Comment for the agent; sent with your next message",
                                        value: "{text}",
                                        onmounted: move |event| async move {
                                            let _ = event.set_focus(true).await;
                                        },
                                        oninput: move |event| text.set(event.value()),
                                        onkeydown: {
                                            let path = path.clone();
                                            let line = line.clone();
                                            let shortcuts = shortcuts.clone();
                                            move |event: KeyboardEvent| {
                                                if event.key() == Key::Escape && !event.is_composing() {
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
}

fn add_comment(
    mut review: Signal<Vec<ReviewComment>>,
    text: &mut Signal<String>,
    path: &str,
    line: &review::DiffLine,
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
