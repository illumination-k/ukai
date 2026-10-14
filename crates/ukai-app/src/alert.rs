//! Telling the user about a session they are not looking at: its pane is
//! marked unread, and while the window is in the background a desktop
//! notification is shown as well.

use dioxus::desktop::DesktopContext;
use dioxus::desktop::tao::window::UserAttentionType;
use dioxus::prelude::*;
use ukai_core::{Alert, Layout, PaneId};

use crate::agent_pane::Sessions;

/// React to `alert` from the session in `pane`; `source` names it, e.g.
/// `agent · project`.
pub fn raise(
    window: &DesktopContext,
    layout: Signal<Layout>,
    pane: PaneId,
    mut unread: Signal<bool>,
    source: &str,
    alert: &Alert,
) {
    let window_focused = window.window.is_focused();
    if !(window_focused && layout.peek().focused() == pane) {
        unread.set(true);
    }
    if window_focused {
        return;
    }
    if matches!(alert, Alert::NeedsInput { .. }) {
        window
            .window
            .request_user_attention(Some(UserAttentionType::Informational));
    }
    let summary = source.to_owned();
    let body = alert.summary();
    // Showing can block on the notification daemon.
    std::thread::spawn(move || {
        if let Err(error) = notify_rust::Notification::new()
            .appname("ukai")
            .summary(&summary)
            .body(&body)
            .show()
        {
            tracing::warn!(%error, "cannot show a desktop notification");
        }
    });
}

/// The user is looking at `pane`: clear its unread mark.
pub fn mark_read(sessions: Sessions, pane: PaneId) {
    let unread = sessions.peek().get(&pane).map(|session| session.unread);
    if let Some(mut unread) = unread
        && *unread.peek()
    {
        unread.set(false);
    }
}

/// Whether `pane` has news the user has not looked at.
pub fn is_unread(sessions: Sessions, pane: PaneId) -> bool {
    sessions
        .read()
        .get(&pane)
        .is_some_and(|session| *session.unread.read())
}
