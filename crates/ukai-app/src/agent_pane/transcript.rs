//! A pane's transcript: the update offer, the entries, and the message
//! queued behind a turn.

use std::cell::RefCell;
use std::rc::Rc;

use dioxus::prelude::*;
use ukai_core::tail::Tail;
use ukai_core::{Entry, PaneId, Queued, Status, Transcript};

use super::entry::{Attachments, EntryView};
use super::session::restart_to_update;
use super::{Composer, use_lease, use_transcript};
use crate::projects::AppSignals;

const TAIL: &str = include_str!("tail.js");

#[component]
pub(super) fn TranscriptView(pane: PaneId) -> Element {
    let app = use_context::<AppSignals>();
    // Only this pane's session: another pane connecting must not render
    // this transcript again.
    let current = use_memo(move || {
        app.sessions
            .read()
            .get(&pane)
            .map(|session| (session.transcript, session.composer, session.lease.clone()))
    });
    // Only the end of a long transcript is mounted, until the user reaches
    // back for more.
    let mut tail = use_signal(Tail::default);
    // Not the entries: those are for `Entries` to render with every chunk.
    let shown = use_transcript(pane, |transcript| {
        (
            transcript.status.clone(),
            transcript.awaiting_permission(),
            transcript.entries.is_empty(),
            transcript.update.clone(),
            transcript.queued.clone(),
        )
    });
    let current = current();
    use_lease(current.as_ref().map(|(_, _, lease)| lease));
    let Some((mut transcript, composer, _)) = current else {
        return rsx! {};
    };
    let Some((status, awaiting, empty, update, steer)) = shown else {
        return rsx! {};
    };
    let idle = status == Status::Idle;
    let busy = status == Status::Busy;

    rsx! {
        // `column-reverse` keeps the view pinned to the newest entry;
        // `tail.js` says when the view leaves it and comes back.
        div {
            id: "transcript-{pane}",
            class: "pane-body transcript",
            onmounted: move |_| {
                let mut view = document::eval(
                    &TAIL.replace("__ID__", &format!("transcript-{pane}")),
                );
                spawn(async move {
                    while let Ok(following) = view.recv::<bool>().await {
                        tail.write().follow(following, transcript.peek().entries.len());
                    }
                });
            },
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
                                onclick: move |_| {
                                    transcript.write().skip_update();
                                    let handle = app.sessions.peek().get(&pane).and_then(|s| s.handle.clone());
                                    if let Some(handle) = handle {
                                        handle.proceed();
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
                Entries { pane, transcript, tail }
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
                    QueuedMessage { pane, steer, composer }
                }
            }
        }
    }
}

/// The transcript's entries, from where `tail` has them start. This is what
/// renders with every chunk an agent streams, so only the entries that
/// changed are drawn again.
#[component]
pub fn Entries(pane: PaneId, transcript: Signal<Transcript>, mut tail: Signal<Tail>) -> Element {
    let entries = use_shared(&transcript.read().entries);
    // Whole pages: pages starting an entry later with each entry that
    // arrives would all be drawn again.
    let start = tail().start(entries.len()) / PAGE * PAGE;
    rsx! {
        if start > 0 {
            button {
                class: "small outline earlier",
                onclick: move |_| tail.write().earlier(transcript.peek().entries.len()),
                "Show earlier messages"
            }
        }
        {pages(pane, entries.get(start..).unwrap_or_default())}
    }
}

/// Entries a [`Page`] draws. Each render of a list builds something for
/// every page and for every entry of the pages that changed, so a long
/// transcript wants pages neither few nor long.
const PAGE: usize = 16;

/// `entries` drawn in order, a [`Page`] at a time.
pub(super) fn pages(pane: PaneId, entries: &[Rc<Entry>]) -> Element {
    rsx! {
        for page in entries.chunks(PAGE) {
            Page { pane, entries: page.to_vec() }
        }
    }
}

/// A run of entries, drawn again only when one of them is another [`Rc`]:
/// while an agent streams that is the last page alone, whatever came before.
#[component]
fn Page(pane: PaneId, entries: Vec<Rc<Entry>>) -> Element {
    rsx! {
        for entry in entries {
            EntryView { pane, entry }
        }
    }
}

/// `entries` behind [`Rc`]s that stay the same from one render to the next
/// for the entries that did, so that a component given one sees at once
/// that it need not draw it again.
pub(super) fn use_shared(entries: &[Entry]) -> Vec<Rc<Entry>> {
    let shared = use_hook(|| Rc::new(RefCell::new(Vec::new())));
    let mut shared = shared.borrow_mut();
    share(&mut shared, entries);
    shared.clone()
}

/// Makes `shared` hold `entries`, keeping the [`Rc`] of each entry that is
/// as it was: comparing an entry costs far less than cloning and drawing it.
fn share(shared: &mut Vec<Rc<Entry>>, entries: &[Entry]) {
    shared.truncate(entries.len());
    for (index, entry) in entries.iter().enumerate() {
        match shared.get_mut(index) {
            Some(kept) if **kept == *entry => {}
            Some(kept) => *kept = Rc::new(entry.clone()),
            None => shared.push(Rc::new(entry.clone())),
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
                Attachments { attachments: steer.shown() }
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

#[cfg(test)]
mod tests {
    use std::rc::Rc;

    use ukai_core::Entry;

    use super::share;

    fn agent(text: &str) -> Entry {
        Entry::Agent {
            text: text.to_owned(),
        }
    }

    /// Which of `after` are the very entries of `before`.
    fn kept(before: &[Rc<Entry>], after: &[Rc<Entry>]) -> Vec<bool> {
        after
            .iter()
            .enumerate()
            .map(|(index, entry)| before.get(index).is_some_and(|was| Rc::ptr_eq(was, entry)))
            .collect()
    }

    #[test]
    fn only_entries_that_changed_are_shared_anew() {
        let mut shared = Vec::new();
        share(&mut shared, &[agent("one"), agent("two"), agent("thr")]);
        let before = shared.clone();

        let entries = [agent("one"), agent("two!"), agent("thr"), agent("four")];
        share(&mut shared, &entries);
        assert_eq!(kept(&before, &shared), [true, false, true, false]);
        assert!(shared.iter().map(|entry| &**entry).eq(&entries));
    }

    #[test]
    fn entries_that_are_gone_are_dropped() {
        let mut shared = Vec::new();
        share(&mut shared, &[agent("one"), agent("two")]);
        let before = shared.clone();

        share(&mut shared, &[agent("one")]);
        assert_eq!(kept(&before, &shared), [true]);
        assert_eq!(shared.len(), 1);
    }
}
