//! Agent panes: one ACP session per pane, kept outside the component tree.
//!
//! Splitting a pane or switching tabs remounts its component, so sessions,
//! transcripts, and drafts live in [`Sessions`] (app-scoped signals keyed by
//! pane id) rather than in component state. Closing a pane drops its entry,
//! and with it the session.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

use dioxus::core::spawn_forever;
use dioxus::html::HasFileData;
use dioxus::prelude::*;
use ukai_acp::checkout::{CommandOutput, FileListing};
use ukai_acp::{
    Attachment, Checkout, ConfigChoice, ConfigInput, ConfigOption, ConfigValue, FileDiff, FsRoot,
    PaneWorkspace, Prompt, RestartEndpoint, SessionEvent, SessionEvents, SessionHandle,
    SessionOptions, SlashCommand, ToolStatus, UpdateCheck,
};
use ukai_core::config::{ApplyDefaults, Setting};
use ukai_core::keymap::{Action, Scope};
use ukai_core::mention::{self, Mention};
use ukai_core::recall::{self, Recall};
use ukai_core::report::ReportKind;
use ukai_core::review::{self, ReviewComment};
use ukai_core::{
    Alert, Entry, History, PaneContent, PaneId, ProjectId, Queued, ReportSource, Status,
    Transcript, session_label, updated_label, usage_label,
};
use ukai_core::{shell, slash};

use crate::alert;
use crate::clipboard::{self, Pasted};
use crate::diff_view::DiffLines;
use crate::icons::{Icon, Svg};
use crate::layout::{launch_target, open_report};
use crate::markdown::Markdown;
use crate::projects::{AppSignals, ProjectContext, project_context};
use crate::report_pane::project_relative;
use crate::shortcuts::{Shortcuts, use_ime};
use crate::voice::{self, Dictation, DictationOwner, Voice};

#[derive(Clone)]
pub struct AgentSession {
    /// `None` when the session could not be started.
    handle: Option<SessionHandle>,
    transcript: Signal<Transcript>,
    composer: Composer,
    /// Where the agent works, for `@` and `!`; `None` when it cannot be told.
    checkout: Option<Checkout>,
    /// Something happened in the session that the user has not looked at.
    pub unread: Signal<bool>,
    /// Ends the composer's recording once no session holds the composer.
    dictation: Rc<DictationOwner>,
}

/// What the user is putting together for the next prompt. It outlives a
/// reconnect of the pane's session.
#[derive(Clone, Copy, PartialEq)]
struct Composer {
    draft: Signal<String>,
    attachments: Signal<Vec<Pending>>,
    /// Comments on diff lines, sent ahead of the draft.
    review: Signal<Vec<ReviewComment>>,
    /// Why the last file could not be attached, or dictation failed.
    error: Signal<Option<String>>,
    dictation: Signal<Dictation>,
    /// Files picked with `@`, linked to the prompt while it names them.
    mentions: Signal<Vec<Mention>>,
    /// The checkout's files, for `@`.
    files: Signal<Files>,
}

/// The last listing of the checkout's files, read when an `@` is typed.
#[derive(Debug, Clone, PartialEq)]
enum Files {
    Unread,
    Loading,
    Listed(FileListing, Instant),
    Failed(String, Instant),
}

/// How long a listing of the checkout's files is used before an `@` reads
/// it again; a failed one is retried sooner.
const FILES_FRESH: Duration = Duration::from_secs(30);
const FILES_RETRY: Duration = Duration::from_secs(5);

impl Composer {
    fn new() -> Self {
        Self {
            draft: Signal::new_in_scope(String::new(), ScopeId::APP),
            attachments: Signal::new_in_scope(Vec::new(), ScopeId::APP),
            review: Signal::new_in_scope(Vec::new(), ScopeId::APP),
            error: Signal::new_in_scope(None, ScopeId::APP),
            dictation: Signal::new_in_scope(Dictation::Idle, ScopeId::APP),
            mentions: Signal::new_in_scope(Vec::new(), ScopeId::APP),
            files: Signal::new_in_scope(Files::Unread, ScopeId::APP),
        }
    }

    /// Put an unsent message back in front of what is being written.
    fn restore(mut self, steer: Queued) {
        let draft = self.draft.peek().clone();
        let text = match (steer.text.is_empty(), draft.is_empty()) {
            (_, true) => steer.text,
            (true, false) => draft,
            (false, false) => format!("{}\n\n{draft}", steer.text),
        };
        self.draft.set(text);
        let mut attachments: Vec<Pending> = steer
            .attachments
            .into_iter()
            .map(|(name, attachment)| Pending { name, attachment })
            .collect();
        attachments.extend(self.attachments.peek().iter().cloned());
        self.attachments.set(attachments);
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
    connect_expecting(app, pane, resume, None);
}

/// [`connect`] after restarting the agent to reach release `expecting`.
fn connect_expecting(app: AppSignals, pane: PaneId, resume: bool, expecting: Option<String>) {
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
    let (composer, unread, dictation) = sessions.peek().get(&pane).map_or_else(
        || {
            let composer = Composer::new();
            let owner = Rc::new(DictationOwner(composer.dictation));
            (composer, Signal::new_in_scope(false, ScopeId::APP), owner)
        },
        |session| (session.composer, session.unread, session.dictation.clone()),
    );
    let restore = if resume { session } else { None };
    let source = match projects.peek().get(&project) {
        Some(project) => format!("{agent} · {}", project.name()),
        None => agent.clone(),
    };
    let mut defaults = ApplyDefaults::new(projects.peek().defaults(&project, &agent));
    let context = project_context(&projects.peek(), &configs.peek(), &project);
    let checkout = context
        .as_ref()
        .ok()
        .and_then(|context| checkout_of(context, &agent, workspace.as_ref()));
    let started = context.and_then(|context| {
        start_session(&context, &agent, workspace.as_ref(), restore, expecting)
    });
    let handle = match started {
        Ok((handle, mut events)) => {
            let window = dioxus::desktop::window();
            let control = handle.clone();
            spawn_forever(async move {
                while let Some(event) = events.recv().await {
                    if let SessionEvent::Ready { session_id, .. } = &event {
                        layout.write().record_session(pane, session_id.clone());
                    }
                    if let Some((id, value)) = defaults.on_event(&event) {
                        control.set_config(id, value);
                    }
                    let raised = Alert::of(&event);
                    transcript.write().apply(event);
                    if transcript.peek().queued.is_some() {
                        follow_queued(sessions, pane, transcript, composer);
                    }
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
            checkout,
            unread,
            dictation,
        },
    );
}

/// Continue the agent's session `id` in `pane`, replacing the one there.
fn open_session(app: AppSignals, pane: PaneId, id: String) {
    let mut layout = app.layout;
    layout.write().record_session(pane, id);
    connect(app, pane, true);
}

/// The agent's other sessions, to continue one in this pane instead; shown
/// only for agents that can list them.
#[component]
pub fn HistoryMenu(pane: PaneId) -> Element {
    let app = use_context::<AppSignals>();
    let mut open = use_signal(|| false);
    let Some(session) = app.sessions.read().get(&pane).cloned() else {
        return rsx! {};
    };
    let mut transcript = session.transcript;
    let history = transcript.read().history.clone();
    if history == History::Unsupported {
        return rsx! {};
    }
    // Switching drops the connection, which would cut a running turn short.
    let busy = transcript.read().status == Status::Busy;
    let handle = session.handle;

    rsx! {
        div { class: "history",
            button {
                class: if open() { "icon open" } else { "icon" },
                // The tooltip would cover the open menu.
                title: if !open() { "Sessions" },
                aria_haspopup: "menu",
                aria_expanded: open(),
                onclick: move |_| {
                    if !open() && handle.as_ref().is_some_and(|h| h.list_sessions()) {
                        transcript.write().history_requested();
                    }
                    open.toggle();
                },
                Svg { icon: Icon::History }
            }
            if open() {
                div {
                    class: "config-backdrop",
                    onclick: move |_| open.set(false),
                }
                div { class: "config-menu history-menu", role: "menu",
                    if busy {
                        div { class: "history-note", "Stop the turn to switch sessions." }
                    } else {
                        div {
                            class: "config-item",
                            role: "menuitem",
                            onclick: move |_| {
                                open.set(false);
                                connect(app, pane, false);
                            },
                            span { class: "check",
                                Svg { icon: Icon::Plus, size: 12 }
                            }
                            span { class: "label", "New session" }
                        }
                        match history {
                            History::Loaded(sessions) if sessions.is_empty() => rsx! {
                                div { class: "history-note", "No earlier sessions." }
                            },
                            History::Loaded(sessions) => rsx! {
                                div { class: "config-group", "Earlier sessions" }
                                for past in sessions {
                                    div {
                                        key: "{past.id}",
                                        class: "config-item",
                                        role: "menuitem",
                                        title: "{past.id}",
                                        onclick: {
                                            let id = past.id.clone();
                                            move |_| {
                                                open.set(false);
                                                open_session(app, pane, id.clone());
                                            }
                                        },
                                        span { class: "check" }
                                        match session_label(&past) {
                                            Some(title) => rsx! {
                                                span { class: "label", "{title}" }
                                            },
                                            None => rsx! {
                                                span { class: "label untitled", "Untitled session" }
                                            },
                                        }
                                        if let Some(updated) = &past.updated_at {
                                            span { class: "description", "{updated_label(updated)}" }
                                        }
                                    }
                                }
                            },
                            History::Failed(message) => rsx! {
                                div { class: "history-note error", "Could not list sessions: {message}" }
                            },
                            _ => rsx! {
                                div { class: "history-note", "Loading…" }
                            },
                        }
                    }
                }
            }
        }
    }
}

/// Where the pane's agent works: the project root, or its gateway workspace.
fn checkout_of(
    context: &ProjectContext,
    agent: &str,
    workspace: Option<&PaneWorkspace>,
) -> Option<Checkout> {
    let spec = context.config.agents.get(agent)?;
    Checkout::resolve(spec, ukai_acp::system_lookup, workspace, &context.root)
        .inspect_err(|error| tracing::warn!(%error, "cannot reach the agent's checkout"))
        .ok()
        .flatten()
}

/// Read the checkout's files again, unless a recent listing will do.
fn refresh_files(checkout: Option<Checkout>, mut files: Signal<Files>) {
    let stale = match &*files.peek() {
        Files::Unread => true,
        Files::Loading => false,
        Files::Listed(_, at) => at.elapsed() > FILES_FRESH,
        Files::Failed(_, at) => at.elapsed() > FILES_RETRY,
    };
    if !stale {
        return;
    }
    let Some(checkout) = checkout else {
        files.set(Files::Failed(
            "this agent's files cannot be listed".to_owned(),
            Instant::now(),
        ));
        return;
    };
    // A listing just read stays shown while the next one loads.
    if matches!(*files.peek(), Files::Unread | Files::Failed(..)) {
        files.set(Files::Loading);
    }
    spawn_forever(async move {
        let listed = match checkout.files().await {
            Ok(listing) => Files::Listed(listing, Instant::now()),
            Err(error) => Files::Failed(error.to_string(), Instant::now()),
        };
        files.set(listed);
    });
}

/// Run `command` in the checkout, as the user's `!`, into the transcript.
fn run_shell(checkout: Option<Checkout>, mut transcript: Signal<Transcript>, command: String) {
    let id = transcript.write().start_shell(command.clone());
    spawn_forever(async move {
        let result = match checkout {
            Some(checkout) => checkout
                .run(&command)
                .await
                .map_err(|error| error.to_string()),
            None => Err("this agent has nowhere to run commands".to_owned()),
        };
        transcript.write().finish_shell(id, result);
    });
}

fn start_session(
    context: &ProjectContext,
    agent: &str,
    workspace: Option<&PaneWorkspace>,
    restore: Option<String>,
    expecting: Option<String>,
) -> Result<(SessionHandle, SessionEvents), String> {
    let spec = context
        .config
        .agents
        .get(agent)
        .ok_or_else(|| format!("agent `{agent}` is not declared for this project"))?;
    let transport =
        ukai_acp::resolve(spec, ukai_acp::system_lookup, workspace).map_err(|e| e.to_string())?;
    let fs_root = FsRoot::new(&context.root).map_err(|e| e.to_string())?;
    Ok(ukai_acp::spawn_session(
        transport,
        SessionOptions {
            cwd: context.root.clone(),
            fs_root: Some(fs_root),
            restore,
            update_check: Some(match expecting {
                Some(latest) => UpdateCheck::npm().expecting(latest),
                None => UpdateCheck::npm(),
            }),
        },
    ))
}

/// Restart `pane`'s agent so that it comes back at release `latest`, then
/// start a new session on it: a gateway agent's workspace pod is replaced,
/// and a local agent's process is started again.
fn restart_to_update(app: AppSignals, pane: PaneId, latest: String) {
    let Some(PaneContent::Agent {
        project,
        agent,
        workspace,
        ..
    }) = app.layout.peek().content(pane).cloned()
    else {
        return;
    };
    let endpoint =
        project_context(&app.projects.peek(), &app.configs.peek(), &project).and_then(|context| {
            let spec = context
                .config
                .agents
                .get(&agent)
                .ok_or_else(|| format!("agent `{agent}` is not declared for this project"))?;
            RestartEndpoint::resolve(spec, ukai_acp::system_lookup, workspace.as_ref())
                .map_err(|e| e.to_string())
        });
    let transcript = app.sessions.peek().get(&pane).map(|s| s.transcript);
    let report = move |event: SessionEvent| {
        if let Some(mut transcript) = transcript {
            transcript.write().apply(event);
        }
    };
    match endpoint {
        Ok(None) => connect_expecting(app, pane, false, Some(latest)),
        Ok(Some(endpoint)) => {
            if let Some(mut transcript) = transcript {
                transcript.write().skip_update();
            }
            // Leave the pod before it goes, rather than be cut off by it.
            let mut sessions = app.sessions;
            if let Some(session) = sessions.write().get_mut(&pane) {
                session.handle = None;
            }
            report(SessionEvent::Preparing {
                message: "Restarting the agent's workspace".to_owned(),
            });
            spawn_forever(async move {
                match endpoint.restart().await {
                    Ok(()) => connect_expecting(app, pane, false, Some(latest)),
                    Err(error) => report(SessionEvent::Closed {
                        error: Some(format!("could not restart the agent: {error}")),
                    }),
                }
            });
        }
        Err(error) => report(SessionEvent::Closed {
            error: Some(format!("could not restart the agent: {error}")),
        }),
    }
}

/// Send the message queued behind a turn that has stopped, or hand it back to
/// the composer when the session cannot take it.
fn follow_queued(
    sessions: Sessions,
    pane: PaneId,
    mut transcript: Signal<Transcript>,
    composer: Composer,
) {
    let Some(steer) = transcript.write().take_queued() else {
        return;
    };
    // A reconnect replaces the pane's session; this transcript's handle is gone then.
    let handle = sessions
        .peek()
        .get(&pane)
        .filter(|session| session.transcript == transcript)
        .and_then(|session| session.handle.clone());
    let idle = transcript.peek().status == Status::Idle;
    if idle && handle.is_some_and(|handle| handle.prompt(steer.prompt())) {
        let names = steer.names();
        transcript.write().push_prompt(steer.text, names);
    } else {
        composer.restore(steer);
    }
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

/// Start or stop dictating into `pane`'s composer.
pub fn toggle_dictation(sessions: Sessions, pane: PaneId, voice: &Voice) {
    let Some(composer) = sessions.peek().get(&pane).map(|session| session.composer) else {
        return;
    };
    voice::toggle(voice, composer.dictation, composer.draft, composer.error);
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
    let voice = use_context::<Voice>();
    let ime = use_ime();
    // How far ↑ has gone back through the prompts sent.
    let mut recall = use_signal(Recall::default);
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
        checkout,
        ..
    } = session;
    let Composer {
        mut draft,
        mut attachments,
        mut review,
        mut error,
        dictation,
        mut mentions,
        files,
    } = composer;
    let status = transcript.read().status.clone();
    let mut dropping = use_signal(|| false);
    // The highlighted slash command or file, and whether escape hid the list.
    let mut picked = use_signal(|| 0_usize);
    let mut dismissed = use_signal(|| false);

    let mut send = {
        let handle = handle.clone();
        let checkout = checkout.clone();
        move || {
            let text = draft.read().trim().to_owned();
            if let Some(command) = shell::command(&text) {
                run_shell(checkout.clone(), transcript, command.to_owned());
                draft.set(String::new());
                return;
            }
            let comments = review.read().clone();
            let pending = attachments.read().clone();
            let status = transcript.read().status.clone();
            if (text.is_empty() && comments.is_empty() && pending.is_empty())
                || !matches!(status, Status::Idle | Status::Busy)
            {
                return;
            }
            // Files picked with `@` go as links, without a chip: the text
            // names them.
            let linked: Vec<(String, Attachment)> = mention::named(&text, &mentions.read())
                .into_iter()
                .map(|mention| {
                    let link = Attachment::Link {
                        name: mention.path.clone(),
                        uri: mention.uri.clone(),
                    };
                    (mention.path.clone(), link)
                })
                .collect();
            let shown = review::review_prompt(&comments, &text);
            let text = shell::prompt(&transcript.read().unsent_shell(), &shown);
            let busy = status == Status::Busy;
            let mut clear = move || {
                draft.set(String::new());
                review.set(Vec::new());
                attachments.set(Vec::new());
                mentions.set(Vec::new());
                error.set(None);
            };
            // An agent that cannot take a message mid-turn is stopped first.
            if busy && !transcript.read().prompt_caps.steering {
                stop_turn(app.sessions, pane);
                transcript.write().queue(Queued {
                    text,
                    attachments: pending
                        .into_iter()
                        .map(|p| (p.name, p.attachment))
                        .chain(linked)
                        .collect(),
                });
                clear();
                return;
            }
            let names = pending.iter().map(|p| p.name.clone()).collect();
            let prompt = Prompt {
                text,
                attachments: pending
                    .into_iter()
                    .map(|p| p.attachment)
                    .chain(linked.into_iter().map(|(_, link)| link))
                    .collect(),
            };
            let sent = handle.as_ref().is_some_and(|h| {
                if busy {
                    h.steer(prompt)
                } else {
                    h.prompt(prompt)
                }
            });
            if sent {
                transcript.write().push_prompt(shown, names);
                clear();
            }
        }
    };
    let cancel = move |_| stop_turn(app.sessions, pane);

    let mut send_on_click = send.clone();
    let busy = status == Status::Busy;
    let awaiting = transcript.read().awaiting_permission();
    let idle = status == Status::Idle;
    // A message sent while a turn runs steers it: it joins the turn, or for
    // an agent that cannot take that, the turn stops and the message follows.
    let writable = idle || busy;
    let steering = transcript.read().prompt_caps.steering;
    let steer = transcript.read().queued.clone();
    let empty = transcript.read().entries.is_empty();
    let has_extras = !review.read().is_empty() || !attachments.read().is_empty();
    let config = transcript.read().config.clone();
    let update = transcript.read().update.clone();
    let shell_mode = draft.read().trim_start().starts_with('!');
    let card_class = match (writable, dropping(), shell_mode) {
        (_, true, _) => "composer-card dropping",
        (true, false, true) => "composer-card shell",
        (true, false, false) => "composer-card",
        (false, false, _) => "composer-card disabled",
    };
    let send_hint = shortcuts.keymap.hint(Action::Send);
    let usage = transcript.read().usage.as_ref().map(|usage| {
        (
            usage_label(usage),
            format!("{} of {} context tokens in use", usage.used, usage.size),
        )
    });
    let commands: Vec<SlashCommand> = if writable && !dismissed() {
        slash::suggestions(&draft.read(), &transcript.read().commands)
            .into_iter()
            .take(MAX_SUGGESTIONS)
            .cloned()
            .collect()
    } else {
        Vec::new()
    };
    let file_query = if writable && !dismissed() && commands.is_empty() {
        mention::query(&draft.read()).map(str::to_owned)
    } else {
        None
    };
    let (found, files_root, files_note): (Vec<String>, String, Option<String>) =
        match (&file_query, &*files.read()) {
            (None, _) => (Vec::new(), String::new(), None),
            (Some(query), Files::Listed(listing, _)) => {
                let found: Vec<String> =
                    mention::suggestions(query, &listing.files, MAX_SUGGESTIONS)
                        .into_iter()
                        .cloned()
                        .collect();
                let note = found.is_empty().then(|| "No matching files".to_owned());
                (found, listing.root.clone(), note)
            }
            (Some(_), Files::Failed(message, _)) => {
                (Vec::new(), String::new(), Some(message.clone()))
            }
            (Some(_), Files::Unread | Files::Loading) => {
                (Vec::new(), String::new(), Some("Listing files…".to_owned()))
            }
        };
    let listed = if commands.is_empty() {
        found.len()
    } else {
        commands.len()
    };
    let current = picked().min(listed.saturating_sub(1));
    // Picking the file `path` for the `@` the draft ends with.
    let mut pick_file = {
        let root = files_root.clone();
        move |path: &str| {
            let completed = mention::complete(&draft.peek(), path);
            draft.set(completed);
            mentions.write().push(Mention {
                path: path.to_owned(),
                uri: ukai_core::attachment::file_uri(&Path::new(&root).join(path)),
            });
            picked.set(0);
        }
    };
    let stop_title = shortcuts.title("Stop", Action::Stop);
    let (mic_class, mic_label) = match &*dictation.read() {
        Dictation::Idle => ("icon mic", "Dictate"),
        Dictation::Recording(_) => ("icon mic recording", "Stop dictating"),
        Dictation::Transcribing => ("icon mic transcribing", "Transcribing…"),
    };
    let mic_title = shortcuts.title(mic_label, Action::Dictate);

    rsx! {
        // `column-reverse` keeps the view pinned to the newest entry.
        div { class: "pane-body transcript",
            div { class: "entries",
                if let Some(update) = update {
                    div { class: "connect update-offer",
                        p { class: "title", "{update.package} {update.latest} is available" }
                        p { class: "muted",
                            "This agent runs {update.current}. Restarting it fetches the new release "
                            "when its command does, such as "
                            code { "npx -y {update.package}@latest" }
                            ". A gateway agent's workspace pod is replaced, which disconnects other panes on it."
                        }
                        div { class: "actions",
                            button {
                                class: "primary",
                                onclick: {
                                    let latest = update.latest.clone();
                                    move |_| restart_to_update(app, pane, latest.clone())
                                },
                                "Restart and update"
                            }
                            button {
                                class: "outline",
                                onclick: {
                                    let handle = handle.clone();
                                    move |_| {
                                        transcript.write().skip_update();
                                        if let Some(handle) = &handle {
                                            handle.proceed();
                                        }
                                    }
                                },
                                "Start without updating"
                            }
                        }
                    }
                }
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
                        if steer.is_some() {
                            "Stopping to take your message…"
                        } else {
                            "Working…"
                        }
                    }
                }
                if let Some(steer) = steer {
                    div { class: "steer",
                        div { class: "msg user queued",
                            "{steer.text}"
                            if !steer.attachments.is_empty() {
                                div { class: "msg-attachments",
                                    for (index, name) in steer.names().into_iter().enumerate() {
                                        span { key: "{index}", class: "chip",
                                            Svg {
                                                icon: Icon::Paperclip,
                                                size: 11,
                                            }
                                            span { class: "name", "{name}" }
                                        }
                                    }
                                }
                            }
                        }
                        div { class: "steer-note",
                            span { "Sends once the turn stops" }
                            button {
                                class: "small outline",
                                title: "Move back to the message box",
                                onclick: move |_| {
                                    if let Some(steer) = transcript.write().queued.take() {
                                        composer.restore(steer);
                                    }
                                },
                                "Edit"
                            }
                        }
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
                if file_query.is_some() {
                    div { class: "slash-list",
                        if let Some(note) = &files_note {
                            div { class: "slash-note", "{note}" }
                        }
                        for (index, path) in found.iter().enumerate() {
                            div {
                                key: "{path}",
                                class: if index == current { "slash-item selected" } else { "slash-item" },
                                onmouseenter: move |_| picked.set(index),
                                onmousedown: move |event| event.prevent_default(),
                                onclick: {
                                    let path = path.clone();
                                    let mut pick_file = pick_file.clone();
                                    move |_| pick_file(&path)
                                },
                                span { class: "name", "@{path}" }
                            }
                        }
                    }
                }
                // Grows with the draft: the hidden copy after the textarea
                // (`::after`) sizes the grid cell both share.
                div { class: "composer-input", "data-value": "{draft}",
                    textarea {
                        rows: "1",
                        placeholder: match status {
                            Status::Idle => {
                                "Message the agent — / for commands, @ for files, ! to run a command, ↑ for earlier messages"
                            }
                            Status::Busy if steering => "Steer the agent — your message joins this turn",
                            Status::Busy => {
                                "Steer the agent — sending stops this turn and sends your message"
                            }
                            Status::Connecting => "Connecting…",
                            Status::Closed { .. } => "Disconnected",
                        },
                        disabled: !writable,
                        value: "{draft}",
                        oninput: {
                            let checkout = checkout.clone();
                            move |event: FormEvent| {
                                let value = event.value();
                                if mention::query(&value).is_some() {
                                    refresh_files(checkout.clone(), files);
                                }
                                draft.set(value);
                                picked.set(0);
                                dismissed.set(false);
                            }
                        },
                        oncompositionend: move |_| ime.end(),
                        onkeyup: move |_| ime.settle(),
                        onpaste: move |event| {
                            let Some(pasted) = clipboard::pasted() else {
                                return;
                            };
                            event.prevent_default();
                            match pasted {
                                Pasted::Image(bytes) => {
                                    attach(composer, transcript, Path::new("pasted-image.png"), bytes);
                                }
                                Pasted::Files(paths) => {
                                    for path in paths {
                                        spawn(async move {
                                            let read = {
                                                let path = path.clone();
                                                tokio::task::spawn_blocking(move || std::fs::read(path))
                                                    .await
                                            };
                                            match read {
                                                Ok(Ok(bytes)) => attach(composer, transcript, &path, bytes),
                                                Ok(Err(e)) => {
                                                    error.set(Some(format!("{}: {e}", path.display())))
                                                }
                                                Err(e) => error.set(Some(e.to_string())),
                                            }
                                        });
                                    }
                                }
                            }
                        },
                        onkeydown: {
                            let shortcuts = shortcuts.clone();
                            move |event: KeyboardEvent| {
                                if ime.composing(&event) {
                                    // The input method's, so not the app's either.
                                    event.stop_propagation();
                                    return;
                                }
                                if listed > 0 {
                                    let len = listed as isize;
                                    match event.key() {
                                        Key::ArrowDown => {
                                            picked.set((current as isize + 1).rem_euclid(len) as usize);
                                        }
                                        Key::ArrowUp => {
                                            picked.set((current as isize - 1).rem_euclid(len) as usize);
                                        }
                                        Key::Tab | Key::Enter => {
                                            if let Some(command) = commands.get(current) {
                                                draft.set(slash::completion(command));
                                                picked.set(0);
                                            } else if let Some(path) = found.get(current) {
                                                pick_file(path);
                                            }
                                        }
                                        // Not the app's: escape here only hides the list.
                                        Key::Escape => dismissed.set(true),
                                        _ => return,
                                    }
                                    event.prevent_default();
                                    event.stop_propagation();
                                    return;
                                }
                                let recalled = match event.key() {
                                    _ if !event.modifiers().is_empty() => None,
                                    Key::ArrowUp => {
                                        recall
                                            .write()
                                            .older(&recall::prompts(&transcript.peek()), &draft.peek())
                                    }
                                    Key::ArrowDown => {
                                        recall
                                            .write()
                                            .newer(&recall::prompts(&transcript.peek()), &draft.peek())
                                    }
                                    _ => None,
                                };
                                if let Some(text) = recalled {
                                    event.prevent_default();
                                    event.stop_propagation();
                                    draft.set(text);
                                    dismissed.set(true);
                                } else if shortcuts.pressed(Scope::Composer, &event) == Some(Action::Send) {
                                    event.prevent_default();
                                    event.stop_propagation();
                                    send();
                                }
                            }
                        },
                    }
                }
                div { class: "composer-footer",
                    button {
                        class: "icon",
                        title: "Attach files…",
                        onclick: move |_| pick_attachments(composer, transcript),
                        Svg { icon: Icon::Paperclip }
                    }
                    button {
                        class: mic_class,
                        title: "{mic_title}",
                        onclick: move |_| voice::toggle(&voice, dictation, draft, error),
                        Svg { icon: Icon::Mic }
                    }
                    if !config.is_empty() {
                        ConfigControls { pane, options: config, disabled: !writable }
                    }
                    if let Some(hint) = send_hint {
                        span { class: "hint",
                            kbd { "{hint}" }
                            if shell_mode {
                                " run in the agent's workspace"
                            } else {
                                " send"
                            }
                        }
                    }
                    if let Some((label, title)) = usage {
                        span { class: "usage", title: "{title}", "{label}" }
                    }
                    if busy {
                        button {
                            class: "round secondary",
                            title: "{stop_title}",
                            onclick: cancel,
                            Svg { icon: Icon::Stop, size: 12 }
                        }
                    }
                    button {
                        class: "round primary",
                        title: match (shell_mode, busy, steering) {
                            (true, _, _) => "Run in the agent's workspace",
                            (false, false, _) => "Send",
                            (false, true, true) => "Steer: add to this turn",
                            (false, true, false) => "Steer: stop this turn and send",
                        },
                        disabled: !writable || (draft.read().trim().is_empty() && !has_extras),
                        onclick: move |_| send_on_click(),
                        Svg { icon: Icon::Send }
                    }
                }
            }
        }
    }
}

/// The agent's settings, e.g. model and reasoning effort, changed through
/// `session/set_config_option`. A change shows once the agent confirms it.
/// The model and effort picked become what the project's new sessions of
/// the agent start with.
#[component]
fn ConfigControls(pane: PaneId, options: Vec<ConfigOption>, disabled: bool) -> Element {
    let AppSignals {
        sessions,
        mut projects,
        layout,
        ..
    } = use_context::<AppSignals>();
    let set = move |id: String, value: ConfigInput| {
        if let Some(handle) = sessions.read().get(&pane).and_then(|s| s.handle.clone()) {
            handle.set_config(id, value);
        }
    };
    let mut remember = move |setting: Setting, value: String| {
        if let Some(PaneContent::Agent { project, agent, .. }) = layout.peek().content(pane) {
            projects.write().set_default(project, agent, setting, value);
        }
    };
    rsx! {
        div { class: "composer-config",
            for option in options {
                {
                    let id = option.id.clone();
                    let setting = Setting::of(&option);
                    let title = match &option.description {
                        Some(description) => format!("{}: {description}", option.name),
                        None => option.name.clone(),
                    };
                    match option.value {
                        ConfigValue::Select { current, choices } => rsx! {
                            ConfigSelect {
                                key: "{option.id}",
                                name: option.name.clone(),
                                title,
                                current,
                                choices,
                                disabled,
                                onpick: move |value: String| {
                                    if let Some(setting) = setting {
                                        remember(setting, value.clone());
                                    }
                                    set(id.clone(), ConfigInput::Select(value));
                                },
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

/// One select setting as a button and a menu drawn like the app's other
/// lists, instead of the native `select` whose popup the web view leaves to
/// the platform.
#[component]
fn ConfigSelect(
    name: String,
    title: String,
    current: String,
    choices: Vec<ConfigChoice>,
    disabled: bool,
    onpick: EventHandler<String>,
) -> Element {
    let mut open = use_signal(|| false);
    let mut highlighted = use_signal(|| 0usize);
    let shown = open() && !disabled;
    let len = choices.len();
    let selected = choices.iter().position(|choice| choice.value == current);
    let label = ukai_core::config::current_label(&current, &choices);
    let mut pick = move |value: String| {
        open.set(false);
        onpick.call(value);
    };
    let on_key = {
        let choices = choices.clone();
        move |event: KeyboardEvent| {
            let step = |delta: isize| {
                (highlighted() as isize + delta).rem_euclid(len.max(1) as isize) as usize
            };
            match event.key() {
                Key::ArrowDown => highlighted.set(step(1)),
                Key::ArrowUp => highlighted.set(step(-1)),
                Key::Enter if !event.is_composing() => {
                    if let Some(choice) = choices.get(highlighted()) {
                        pick(choice.value.clone());
                    }
                }
                // Not the app's: escape here only closes the menu.
                Key::Escape | Key::Tab => open.set(false),
                _ => return,
            }
            event.prevent_default();
            event.stop_propagation();
        }
    };
    // Each choice with its index in `choices` and, first in a group, the
    // group's name to head it.
    let rows: Vec<(usize, Option<String>, ConfigChoice)> =
        ukai_core::config::choice_groups(&choices)
            .into_iter()
            .flat_map(|(group, run)| {
                run.iter().enumerate().map(move |(i, choice)| {
                    (group.filter(|_| i == 0).map(str::to_owned), choice.clone())
                })
            })
            .enumerate()
            .map(|(index, (group, choice))| (index, group, choice))
            .collect();

    rsx! {
        div { class: "config-select",
            button {
                class: if shown { "config-trigger open" } else { "config-trigger" },
                title: "{title}",
                aria_label: "{name}",
                aria_haspopup: "listbox",
                aria_expanded: shown,
                disabled,
                onclick: move |_| {
                    highlighted.set(selected.unwrap_or(0));
                    open.toggle();
                },
                span { class: "name", "{label}" }
                span { class: "caret",
                    Svg { icon: Icon::Chevron, size: 10 }
                }
            }
            if shown {
                div {
                    class: "config-backdrop",
                    onclick: move |_| open.set(false),
                }
                div {
                    class: "config-menu",
                    role: "listbox",
                    aria_label: "{name}",
                    tabindex: "-1",
                    onmounted: move |event| async move {
                        let _ = event.set_focus(true).await;
                    },
                    onkeydown: on_key,
                    for (at, group, choice) in rows {
                        if let Some(group) = group {
                            div { class: "config-group", "{group}" }
                        }
                        div {
                            class: if at == highlighted() { "config-item highlighted" } else { "config-item" },
                            role: "option",
                            aria_selected: Some(at) == selected,
                            title: choice.description.clone().unwrap_or_default(),
                            onmouseenter: move |_| highlighted.set(at),
                            onclick: {
                                let value = choice.value.clone();
                                move |_| pick(value.clone())
                            },
                            span { class: "check",
                                if Some(at) == selected {
                                    Svg { icon: Icon::Check, size: 12 }
                                }
                            }
                            span { class: "label", "{choice.name}" }
                            if let Some(description) = &choice.description {
                                span { class: "description", "{description}" }
                            }
                        }
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

/// The comments on diff lines waiting in agent `pane`'s composer for its
/// next prompt.
pub fn review_of(app: AppSignals, pane: PaneId) -> Option<Signal<Vec<ReviewComment>>> {
    app.sessions
        .read()
        .get(&pane)
        .map(|session| session.composer.review)
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
