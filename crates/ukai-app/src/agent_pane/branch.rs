//! Keeping a pane's checkout on its session's branch: a new session starts
//! on the default branch, and an earlier one goes back to the branch it last
//! worked on (`ukai_core::branch`).

use dioxus::core::spawn_forever;
use dioxus::prelude::*;
use ukai_acp::Checkout;
use ukai_acp::checkout::CheckoutError;
use ukai_core::branch::{self, SessionKey, Switched, Target};
use ukai_core::{Entry, PaneContent, PaneId, Status};

use super::session::{checkout_for, connect};
use crate::projects::AppSignals;

/// Start a new session in `pane`, on the default branch.
pub(super) fn new_session(app: AppSignals, pane: PaneId) {
    switch_then(app, pane, Target::Default, move || {
        connect(app, pane, false)
    });
}

/// Continue the agent's session `id` in `pane`, on the branch it last
/// worked on; on the branch the checkout is on when none is recorded.
pub(super) fn open_session(app: AppSignals, pane: PaneId, id: String) {
    let target =
        key(app, pane, id.clone()).and_then(|key| app.branches.peek().get(&key).map(str::to_owned));
    let open = move || {
        let mut layout = app.layout;
        layout.write().record_session(pane, id);
        connect(app, pane, true);
    };
    match target {
        Some(branch) => switch_then(app, pane, Target::Branch(branch), open),
        None => open(),
    }
}

/// Record the branch `pane`'s session is on now.
pub(super) fn record(app: AppSignals, pane: PaneId) {
    let Some((key, checkout)) = current(app, pane) else {
        return;
    };
    spawn_forever(async move {
        record_from(app, key, &checkout).await;
    });
}

/// Switch `pane`'s checkout to `target`, then `then` (which replaces the
/// pane's session), and tell the new session what became of the branch.
/// The session that is leaving keeps the branch it was on first. Another
/// agent at work in the same checkout keeps the branch as it is.
fn switch_then(app: AppSignals, pane: PaneId, target: Target, then: impl FnOnce() + 'static) {
    let Some(checkout) = checkout_for(app, pane) else {
        then();
        return;
    };
    let leaving = current(app, pane);
    let busy = app.sessions.peek().iter().any(|(other, session)| {
        *other != pane
            && session.transcript.peek().status == Status::Busy
            && session
                .checkout
                .as_ref()
                .is_some_and(|theirs| theirs.same_as(&checkout))
    });
    spawn_forever(async move {
        if let Some((key, _)) = leaving {
            record_from(app, key, &checkout).await;
        }
        let switched = if busy {
            Switched::Failed(
                "another agent is working in this checkout, so its branch stays as it is"
                    .to_owned(),
            )
        } else {
            switch(&checkout, &target).await
        };
        then();
        if let Some((text, error)) = branch::notice(&switched)
            && let Some(mut transcript) = app.sessions.peek().get(&pane).map(|s| s.transcript)
        {
            transcript
                .write()
                .entries
                .push(Entry::Notice { text, error });
        }
    });
}

async fn switch(checkout: &Checkout, target: &Target) -> Switched {
    let Some(command) = branch::switch_command(target) else {
        return Switched::Nothing;
    };
    match checkout.run(&command).await {
        Ok(output) => branch::parse_switch(&output),
        // A pod that is not running is checked out afresh when it starts.
        Err(CheckoutError::NotRunning) => Switched::Nothing,
        Err(error) => Switched::Failed(error.to_string()),
    }
}

async fn record_from(app: AppSignals, key: SessionKey, checkout: &Checkout) {
    let Ok(output) = checkout.run(branch::CURRENT).await else {
        return;
    };
    if let Some(found) = branch::parse_current(&output) {
        let mut branches = app.branches;
        // Peek first: an unchanged record need not wake the autosave.
        if branches.peek().get(&key) != Some(found.as_str()) {
            branches.write().record(key, found);
        }
    }
}

/// The session `pane` holds and where it works, if it has started one.
fn current(app: AppSignals, pane: PaneId) -> Option<(SessionKey, Checkout)> {
    let session = app.sessions.peek().get(&pane)?.clone();
    let id = session.transcript.peek().session_id.clone()?;
    Some((key(app, pane, id)?, session.checkout?))
}

fn key(app: AppSignals, pane: PaneId, session: String) -> Option<SessionKey> {
    let Some(PaneContent::Agent { project, agent, .. }) = app.layout.peek().content(pane).cloned()
    else {
        return None;
    };
    Some(SessionKey {
        project,
        agent,
        session,
    })
}
