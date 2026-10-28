//! A pane's transcript: the update offer, the entries, and the message
//! queued behind a turn.

use dioxus::prelude::*;
use ukai_core::{PaneId, Queued, Status};

use super::Composer;
use super::entry::EntryView;
use super::session::restart_to_update;
use crate::icons::{Icon, Svg};
use crate::projects::AppSignals;

#[component]
pub(super) fn TranscriptView(pane: PaneId) -> Element {
    let app = use_context::<AppSignals>();
    let Some(session) = app.sessions.read().get(&pane).cloned() else {
        return rsx! {};
    };
    let mut transcript = session.transcript;
    let status = transcript.read().status.clone();
    let idle = status == Status::Idle;
    let busy = status == Status::Busy;
    let awaiting = transcript.read().awaiting_permission();
    let empty = transcript.read().entries.is_empty();
    let update = transcript.read().update.clone();
    let steer = transcript.read().queued.clone();

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
                                    let handle = session.handle.clone();
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
                    QueuedMessage { pane, steer, composer: session.composer }
                }
            }
        }
    }
}

/// The message waiting for the turn to stop, which the user can take back.
#[component]
fn QueuedMessage(pane: PaneId, steer: Queued, composer: Composer) -> Element {
    let app = use_context::<AppSignals>();
    let Some(mut transcript) = app.sessions.read().get(&pane).map(|s| s.transcript) else {
        return rsx! {};
    };
    rsx! {
        div { class: "steer",
            div { class: "msg user queued",
                "{steer.text}"
                if !steer.attachments.is_empty() {
                    div { class: "msg-attachments",
                        for (index, name) in steer.names().into_iter().enumerate() {
                            span { key: "{index}", class: "chip",
                                Svg { icon: Icon::Paperclip, size: 11 }
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
