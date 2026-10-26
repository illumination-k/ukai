//! The agent's other sessions, to continue one in a pane instead.

use dioxus::prelude::*;
use ukai_core::{History, PaneId, Status, session_label, updated_label};

use super::session::{connect, open_session};
use crate::icons::{Icon, Svg};
use crate::projects::AppSignals;

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
