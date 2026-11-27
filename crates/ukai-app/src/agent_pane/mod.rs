//! Agent panes: one ACP session per pane, kept outside the component tree.
//!
//! Sessions, transcripts, and drafts live in [`Sessions`] (app-scoped
//! signals keyed by pane id) rather than in component state, so that the
//! rest of the app reaches them and they never depend on a mount. Closing a pane drops its entry,
//! and with it the session.

mod branch;
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
use ukai_core::{Connection, PaneId, Queued, Status, Transcript};

pub use self::history::HistoryMenu;
pub use self::session::connect;
pub use self::transcript::Entries;
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
            draft: Signal::new_in_scope(String::new(), ScopeId::ROOT),
            attachments: Signal::new_in_scope(Vec::new(), ScopeId::ROOT),
            uploading: Signal::new_in_scope(Vec::new(), ScopeId::ROOT),
            review: Signal::new_in_scope(Vec::new(), ScopeId::ROOT),
            error: Signal::new_in_scope(None, ScopeId::ROOT),
            dictation: Signal::new_in_scope(Dictation::Idle, ScopeId::ROOT),
            mentions: Signal::new_in_scope(Vec::new(), ScopeId::ROOT),
            files: Signal::new_in_scope(Files::Unread, ScopeId::ROOT),
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

/// What `read` makes of `pane`'s session, rendering the component again
/// only when that changes. A transcript changes with every chunk its agent
/// streams, and what a header, a tab, a sidebar row, or the composer shows
/// of it rarely does: they read it through this rather than subscribe to
/// the transcript, or each would render with every chunk of every agent.
pub fn use_session<T: Clone + PartialEq + 'static>(
    pane: PaneId,
    read: fn(Sessions, PaneId) -> T,
) -> T {
    let sessions = use_context::<AppSignals>().sessions;
    use_memo(use_reactive((&pane,), move |(pane,)| read(sessions, pane)))()
}

/// [`use_session`] of `pane`'s transcript; `None` without a session.
fn use_transcript<T: Clone + PartialEq + 'static>(
    pane: PaneId,
    read: fn(&Transcript) -> T,
) -> Option<T> {
    let sessions = use_context::<AppSignals>().sessions;
    use_memo(use_reactive((&pane,), move |(pane,)| {
        let transcript = sessions
            .read()
            .get(&pane)
            .map(|session| session.transcript)?;
        Some(read(&transcript.read()))
    }))()
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
/// next prompt.
pub fn review_of(app: AppSignals, pane: PaneId) -> Option<Signal<Vec<ReviewComment>>> {
    app.sessions
        .read()
        .get(&pane)
        .map(|session| session.composer.review)
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

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use dioxus::dioxus_core::NoOpMutations;
    use ukai_acp::SessionEvent;
    use ukai_core::state::AppState;

    use super::*;

    /// How many times [`Header`] rendered.
    type Renders = Rc<Cell<usize>>;

    /// What a pane's header reads of its session.
    #[component]
    fn Header() -> Element {
        let renders = use_context::<Renders>();
        renders.set(renders.get() + 1);
        let (_, label, _) = use_session(0, agent_status);
        rsx! { "{label}" }
    }

    #[component]
    fn Root() -> Element {
        let transcript = use_context::<Signal<Transcript>>();
        let mut app = use_context_provider(|| AppSignals::new(AppState::default(), HashMap::new()));
        use_hook(|| {
            let composer = Composer::new();
            app.sessions.write().insert(
                0,
                AgentSession {
                    handle: None,
                    transcript,
                    composer,
                    checkout: None,
                    unread: Signal::new_in_scope(false, ScopeId::ROOT),
                    dictation: Rc::new(DictationOwner(composer.dictation)),
                },
            );
        });
        rsx! {
            Header {}
        }
    }

    /// A header renders when its session's state changes, not with each
    /// chunk the agent streams in between.
    #[test]
    fn what_shows_a_sessions_state_does_not_render_with_every_chunk() {
        let renders = Renders::default();
        let mut dom = VirtualDom::new(Root);
        let mut transcript =
            dom.in_runtime(|| Signal::new_in_scope(Transcript::default(), ScopeId::ROOT));
        dom.provide_root_context(renders.clone());
        dom.provide_root_context(transcript);
        dom.rebuild_in_place();
        let mut apply = |event: SessionEvent| {
            dom.in_runtime(|| transcript.write().apply(event));
            // The memo recomputes in a task, then the header renders.
            dom.process_events();
            dom.render_immediate(&mut NoOpMutations);
            renders.get()
        };

        assert_eq!(apply(SessionEvent::TurnRunning), 2);
        for chunk in ["one", " two", " three"] {
            let text = chunk.to_owned();
            assert_eq!(apply(SessionEvent::AgentMessage { text }), 2);
        }
        let ended = SessionEvent::TurnEnded {
            stop_reason: "end_turn".to_owned(),
            tokens: None,
        };
        assert_eq!(apply(ended), 3);
    }
}
