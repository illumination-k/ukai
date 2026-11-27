//! Agent panes: one ACP session per pane, kept outside the component tree.
//!
//! Sessions, transcripts, and drafts live in [`Sessions`] (signals keyed by
//! pane id) rather than in component state, so that the rest of the app
//! reaches them and they never depend on a mount. Closing a pane drops its
//! entry, and with it the session.
//!
//! No scope owns a session's signals: a [`Lease`] does, and frees them when
//! its last clone goes. The pane's entry holds one, and so does whatever
//! may still read or write the signals once the entry is gone: work that
//! outlives the pane ([`Lease::spawn`]) and the components showing the
//! session ([`use_lease`]). Reading a freed signal panics, so nothing keeps
//! a session's signal across an `await` or a render without its lease.

mod branch;
mod composer;
mod config;
mod entry;
mod history;
mod session;
mod transcript;

#[cfg(test)]
mod tests;

use std::cell::RefCell;
use std::collections::HashMap;
use std::panic::Location;
use std::rc::Rc;

use dioxus::core::{Runtime, spawn_forever};
use dioxus::prelude::*;
use ukai_acp::{Attachment, Checkout, SessionHandle};
use ukai_core::composer::{self as suggest, Files};
use ukai_core::mention::Mention;
use ukai_core::review::ReviewComment;
use ukai_core::{Connection, PaneId, Queued, Status, Transcript};

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
    /// Frees `transcript`, `composer`, and `unread` once nothing holds them.
    lease: Lease,
}

/// A signal no scope owns: it lives until `manually_drop`.
#[track_caller]
fn unowned<T: 'static>(value: T) -> Signal<T> {
    let caller = Location::caller();
    Runtime::current().in_scope(ScopeId::ROOT, || Signal::leak_with_caller(value, caller))
}

/// Frees a connection's transcript.
struct TranscriptOwner(Signal<Transcript>);

impl Drop for TranscriptOwner {
    fn drop(&mut self) {
        self.0.manually_drop();
    }
}

/// Frees what a pane keeps across its connections.
struct PaneOwner {
    composer: Composer,
    unread: Signal<bool>,
}

impl Drop for PaneOwner {
    fn drop(&mut self) {
        let Composer {
            draft,
            attachments,
            uploading,
            review,
            error,
            dictation,
            mentions,
            files,
        } = self.composer;
        draft.manually_drop();
        attachments.manually_drop();
        uploading.manually_drop();
        review.manually_drop();
        error.manually_drop();
        dictation.manually_drop();
        mentions.manually_drop();
        files.manually_drop();
        self.unread.manually_drop();
    }
}

/// Keeps a session's signals: its transcript, which a reconnect replaces,
/// and its composer and unread mark, which last as long as the pane. Each
/// is freed when the last lease on it goes.
#[derive(Clone)]
pub struct Lease {
    transcript: Rc<TranscriptOwner>,
    pane: Rc<PaneOwner>,
}

impl PartialEq for Lease {
    fn eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.transcript, &other.transcript) && Rc::ptr_eq(&self.pane, &other.pane)
    }
}

impl Lease {
    /// A new pane's: an empty transcript and composer.
    fn new() -> Self {
        Self {
            transcript: Rc::new(TranscriptOwner(unowned(Transcript::default()))),
            pane: Rc::new(PaneOwner {
                composer: Composer::new(),
                unread: unowned(false),
            }),
        }
    }

    /// The same pane's after a reconnect: an empty transcript, and the
    /// composer as it is.
    fn reconnected(&self) -> Self {
        Self {
            transcript: Rc::new(TranscriptOwner(unowned(Transcript::default()))),
            pane: self.pane.clone(),
        }
    }

    fn transcript(&self) -> Signal<Transcript> {
        self.transcript.0
    }

    fn composer(&self) -> Composer {
        self.pane.composer
    }

    fn unread(&self) -> Signal<bool> {
        self.pane.unread
    }

    /// Run `work` to its end whatever becomes of the pane, keeping the
    /// session's signals until then for it to write.
    fn spawn(&self, work: impl Future<Output = ()> + 'static) {
        let lease = self.clone();
        spawn_forever(async move {
            work.await;
            drop(lease);
        });
    }
}

/// Keep the session this component shows until it shows another or
/// unmounts: the event handlers of a render hold its signals that long.
/// Call it on every render, with `None` when the pane has no session.
fn use_lease(lease: Option<&Lease>) {
    let held = use_hook(|| Rc::new(RefCell::new(None)));
    held.replace(lease.cloned());
}

/// What the user is putting together for the next prompt. It outlives a
/// reconnect of the pane's session.
#[derive(Clone, Copy, PartialEq)]
struct Composer {
    draft: Signal<String>,
    attachments: Signal<Vec<Pending>>,
    /// Names of the files on their way to the agent's workspace.
    uploading: Signal<Vec<String>>,
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
            draft: unowned(String::new()),
            attachments: unowned(Vec::new()),
            uploading: unowned(Vec::new()),
            review: unowned(Vec::new()),
            error: unowned(None),
            dictation: unowned(Dictation::Idle),
            mentions: unowned(Vec::new()),
            files: unowned(Files::Unread),
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
    let Some(lease) = sessions.peek().get(&pane).map(|s| s.lease.clone()) else {
        return;
    };
    let composer = lease.composer();
    voice::toggle(
        voice,
        composer.dictation,
        composer.draft,
        composer.error,
        lease,
    );
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
        // A dropped connection is reached again quietly; while idle there is
        // nothing to tell (`Connection::Detached` reads as ready).
        Status::Busy | Status::Idle
            if matches!(transcript.connection, Connection::Reconnecting { .. }) =>
        {
            ("busy", "reconnecting")
        }
        Status::Connecting => ("busy", "connecting"),
        Status::Idle => ("ready", "ready"),
        Status::Busy => ("busy", "working"),
        Status::Closed { error: None } => ("idle", "closed"),
        Status::Closed { error: Some(_) } => ("closed", "disconnected"),
    };
    (class, label, transcript.agent.clone())
}

/// What agent `pane`'s session is about, once there is something to say.
pub fn session_title(sessions: Sessions, pane: PaneId) -> Option<String> {
    sessions
        .read()
        .get(&pane)
        .and_then(|session| session.transcript.read().title().map(str::to_owned))
}

/// The comments on diff lines waiting in agent `pane`'s composer for its
/// next prompt, kept while the component shows them.
pub fn use_review(app: AppSignals, pane: Option<PaneId>) -> Option<Signal<Vec<ReviewComment>>> {
    let lease = pane.and_then(|pane| app.sessions.read().get(&pane).map(|s| s.lease.clone()));
    use_lease(lease.as_ref());
    lease.map(|lease| lease.composer().review)
}

/// Whether agent `pane` is connected and between turns, so [`ask`] reaches it.
pub fn agent_idle(sessions: Sessions, pane: PaneId) -> bool {
    sessions.read().get(&pane).is_some_and(|session| {
        session.handle.is_some() && session.transcript.read().status == Status::Idle
    })
}

/// Send `text` to agent `pane` as a turn of its own, leaving what its
/// composer holds for the user's next message. `false` when the agent is not
/// there to take it: not connected, or in a turn.
pub fn ask(sessions: Sessions, pane: PaneId, text: &str) -> bool {
    let Some(session) = sessions.peek().get(&pane).cloned() else {
        return false;
    };
    let mut transcript = session.transcript;
    if transcript.peek().status != Status::Idle {
        return false;
    }
    let prompt = ukai_core::shell::prompt(&transcript.peek().unsent_shell(), text);
    let delivered = session.handle.is_some_and(|handle| {
        handle.prompt(ukai_acp::Prompt {
            text: prompt,
            attachments: Vec::new(),
        })
    });
    if delivered {
        transcript.write().push_prompt(text, Vec::new());
    }
    delivered
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
                            onclick: move |_| branch::new_session(app, pane),
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
