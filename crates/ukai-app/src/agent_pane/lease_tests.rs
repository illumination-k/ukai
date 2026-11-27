//! When a session's signals are freed: with the pane's entry, unless work
//! that still writes them is under way.

use std::collections::HashMap;
use std::time::Duration;

use dioxus::core::NoOpMutations;
use dioxus::prelude::*;
use ukai_acp::Checkout;
use ukai_core::state::AppState;
use ukai_core::{Entry, PaneContent, PaneId, Status};

use super::composer::run_shell;
use super::{AgentSession, connect};
use crate::layout::close_pane;
use crate::projects::AppSignals;

fn root() -> Element {
    rsx! {}
}

/// An app with one agent pane whose agent cannot be started (its project
/// has no configuration), so connecting needs no window and no agent.
fn app() -> (VirtualDom, AppSignals, PaneId) {
    let mut dom = VirtualDom::new(root);
    dom.rebuild_in_place();
    let mut state = AppState::default();
    let project = state.projects.add(std::path::Path::new("/nowhere"));
    let pane = state.layout.focused();
    state.layout.set_content(
        pane,
        PaneContent::Agent {
            project,
            agent: "agent".to_owned(),
            session: None,
            workspace: None,
        },
    );
    let app = dom.in_runtime(|| AppSignals::new(state, HashMap::new()));
    (dom, app, pane)
}

fn session(app: AppSignals, pane: PaneId) -> AgentSession {
    let Some(session) = app.sessions.peek().get(&pane).cloned() else {
        panic!("the pane has no session");
    };
    session
}

#[test]
fn reconnecting_frees_the_transcript_and_keeps_the_composer() {
    let (dom, app, pane) = app();
    dom.in_runtime(|| {
        connect(app, pane, false);
        let AgentSession {
            transcript,
            composer,
            unread,
            ..
        } = session(app, pane);
        let mut draft = composer.draft;
        draft.set("half a thought".to_owned());

        connect(app, pane, false);
        assert!(transcript.try_read().is_err(), "the transcript is freed");
        let again = session(app, pane);
        assert!(again.transcript != transcript);
        assert!(matches!(
            again.transcript.peek().status,
            Status::Closed { error: Some(_) }
        ));
        assert!(again.composer == composer && again.unread == unread);
        assert_eq!(*draft.peek(), "half a thought");
    });
}

#[test]
fn closing_a_pane_frees_its_signals() {
    let (dom, app, pane) = app();
    dom.in_runtime(|| {
        connect(app, pane, false);
        let AgentSession {
            transcript,
            composer,
            unread,
            ..
        } = session(app, pane);
        close_pane(app, pane);
        assert!(transcript.try_read().is_err());
        assert!(composer.draft.try_read().is_err());
        assert!(composer.attachments.try_read().is_err());
        assert!(composer.uploading.try_read().is_err());
        assert!(composer.review.try_read().is_err());
        assert!(composer.error.try_read().is_err());
        assert!(composer.dictation.try_read().is_err());
        assert!(composer.mentions.try_read().is_err());
        assert!(composer.files.try_read().is_err());
        assert!(unread.try_read().is_err());
    });
}

/// A `!` command that ends after its pane closed still has its transcript
/// to write, and the transcript goes once it has.
#[tokio::test]
async fn a_command_outlives_its_pane_and_then_frees_the_transcript() {
    let (mut dom, app, pane) = app();
    let transcript = dom.in_runtime(|| {
        connect(app, pane, false);
        let session = session(app, pane);
        let checkout = Checkout::Local(std::env::temp_dir());
        run_shell(&session.lease, Some(checkout), "true".to_owned());
        session.transcript
    });
    dom.in_runtime(|| {
        close_pane(app, pane);
        let Ok(transcript) = transcript.try_read() else {
            panic!("the transcript went before the command ended");
        };
        assert!(matches!(
            transcript.entries.last(),
            Some(Entry::Shell { .. })
        ));
    });
    // Polls the command's task; the wait only ends by the timeout.
    for _ in 0..500 {
        if dom.in_runtime(|| transcript.try_read().is_err()) {
            return;
        }
        let _ = tokio::time::timeout(Duration::from_millis(20), dom.wait_for_work()).await;
        dom.render_immediate(&mut NoOpMutations);
    }
    panic!("the transcript is still kept after the command ended");
}
