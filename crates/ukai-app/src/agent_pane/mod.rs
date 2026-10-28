//! Agent panes: one ACP session per pane, kept outside the component tree.
//!
//! Splitting a pane or switching tabs remounts its component, so sessions,
//! transcripts, and drafts live in [`Sessions`] (app-scoped signals keyed by
//! pane id) rather than in component state. Closing a pane drops its entry,
//! and with it the session.

mod composer;
mod config;
mod entry;
mod history;
mod session;
mod transcript;

use std::collections::HashMap;
use std::rc::Rc;

use dioxus::prelude::*;
use ukai_acp::{Attachment, Checkout, SessionHandle};
use ukai_core::composer::{self as suggest, Files};
use ukai_core::mention::Mention;
use ukai_core::review::ReviewComment;
use ukai_core::{PaneId, Queued, Status, Transcript};

pub use self::history::HistoryMenu;
pub use self::session::connect;
use crate::projects::AppSignals;
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
        let text = suggest::restored(&steer.text, &self.draft.peek());
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

/// The comments on diff lines waiting in agent `pane`'s composer for its
/// next prompt.
pub fn review_of(app: AppSignals, pane: PaneId) -> Option<Signal<Vec<ReviewComment>>> {
    app.sessions
        .read()
        .get(&pane)
        .map(|session| session.composer.review)
}

/// `has_saved_session`: the pane recorded an ACP session in an earlier run,
/// so connecting offers to resume it.
#[component]
pub fn AgentView(pane: PaneId, has_saved_session: bool) -> Element {
    let app = use_context::<AppSignals>();
    if app.sessions.read().contains_key(&pane) {
        return rsx! {
            transcript::TranscriptView { pane }
            composer::ComposerView { pane }
        };
    }
    rsx! {
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
    }
}
