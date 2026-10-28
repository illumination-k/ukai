//! The composer: the draft, what goes with it, and sending it.

use std::path::Path;
use std::time::Instant;

use dioxus::core::spawn_forever;
use dioxus::html::HasFileData;
use dioxus::prelude::*;
use ukai_acp::{Attachment, Checkout, Prompt};
use ukai_core::composer::{self as suggest, Files, Suggestions};
use ukai_core::keymap::{Action, Scope};
use ukai_core::mention::{self, Mention};
use ukai_core::recall::{self, Recall};
use ukai_core::{PaneId, Queued, Status, Transcript, review, shell, slash, usage_label};

use super::config::ConfigControls;
use super::{Composer, Pending, stop_turn};
use crate::clipboard::{self, Pasted};
use crate::icons::{Icon, Svg};
use crate::projects::AppSignals;
use crate::shortcuts::{Shortcuts, use_ime};
use crate::voice::{self, Dictation, Voice};

#[component]
pub(super) fn ComposerView(pane: PaneId) -> Element {
    let app = use_context::<AppSignals>();
    let shortcuts = use_context::<Shortcuts>();
    let voice = use_context::<Voice>();
    let ime = use_ime();
    // How far ↑ has gone back through the prompts sent.
    let mut recall = use_signal(Recall::default);
    let mut dropping = use_signal(|| false);
    // The highlighted suggestion, and whether escape hid the list.
    let mut picked = use_signal(|| 0_usize);
    let mut dismissed = use_signal(|| false);
    let Some(session) = app.sessions.read().get(&pane).cloned() else {
        return rsx! {};
    };
    let handle = session.handle;
    let transcript = session.transcript;
    let composer = session.composer;
    let checkout = session.checkout;
    let Composer {
        mut draft,
        mut error,
        dictation,
        files,
        ..
    } = composer;

    let send = {
        let checkout = checkout.clone();
        move || {
            send_draft(
                app,
                pane,
                composer,
                transcript,
                handle.as_ref(),
                checkout.clone(),
            )
        }
    };
    let send_on_click = send.clone();
    let status = transcript.read().status.clone();
    let busy = status == Status::Busy;
    // A message sent while a turn runs steers it: it joins the turn, or for
    // an agent that cannot take that, the turn stops and the message follows.
    let writable = status == Status::Idle || busy;
    let steering = transcript.read().prompt_caps.steering;
    let has_extras = !composer.review.read().is_empty() || !composer.attachments.read().is_empty();
    let config = transcript.read().config.clone();
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
    let suggestions = if writable && !dismissed() {
        Suggestions::of(&draft.read(), &transcript.read().commands, &files.read())
    } else {
        Suggestions::None
    };
    let listed = suggestions.len();
    let current = picked().min(listed.saturating_sub(1));
    let mut pick = {
        let suggestions = suggestions.clone();
        move |index: usize| {
            pick_suggestion(composer, &suggestions, index);
            picked.set(0);
        }
    };
    let pick_on_click = pick.clone();
    let stop_title = shortcuts.title("Stop", Action::Stop);
    let (mic_class, mic_label) = match &*dictation.read() {
        Dictation::Idle => ("icon mic", "Dictate"),
        Dictation::Recording(_) => ("icon mic recording", "Stop dictating"),
        Dictation::Transcribing => ("icon mic transcribing", "Transcribing…"),
    };
    let mic_title = shortcuts.title(mic_label, Action::Dictate);

    rsx! {
        footer { class: "composer",
            if matches!(status, Status::Closed { .. }) {
                div { class: "composer-notice",
                    span { "The session has ended." }
                    button {
                        class: "small primary",
                        onclick: move |_| super::connect(app, pane, true),
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
                PendingItems { composer }
                SuggestionList {
                    suggestions: suggestions.clone(),
                    current,
                    onhighlight: move |index| picked.set(index),
                    onpick: pick_on_click,
                }
                // Grows with the draft: the hidden copy after the textarea
                // (`::after`) sizes the grid cell both share.
                div { class: "composer-input", "data-value": "{draft}",
                    textarea {
                        rows: "1",
                        placeholder: placeholder(&status, steering),
                        disabled: !writable,
                        value: "{draft}",
                        oninput: move |event: FormEvent| {
                            let value = event.value();
                            if mention::query(&value).is_some() {
                                refresh_files(checkout.clone(), files);
                            }
                            draft.set(value);
                            picked.set(0);
                            dismissed.set(false);
                        },
                        oncompositionend: move |_| ime.end(),
                        onkeyup: move |_| ime.settle(),
                        onpaste: move |event| {
                            let Some(pasted) = clipboard::pasted() else {
                                return;
                            };
                            event.prevent_default();
                            paste(composer, transcript, pasted);
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
                                    match event.key() {
                                        Key::ArrowDown => picked.set(suggest::step(current, listed, 1)),
                                        Key::ArrowUp => picked.set(suggest::step(current, listed, -1)),
                                        Key::Tab | Key::Enter => pick(current),
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
                            onclick: move |_| stop_turn(app.sessions, pane),
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

/// What the empty textarea says the composer does now.
fn placeholder(status: &Status, steering: bool) -> &'static str {
    match status {
        Status::Idle => {
            "Message the agent — / for commands, @ for files, ! to run a command, ↑ for earlier messages"
        }
        Status::Busy if steering => "Steer the agent — your message joins this turn",
        Status::Busy => "Steer the agent — sending stops this turn and sends your message",
        Status::Connecting => "Connecting…",
        Status::Closed { .. } => "Disconnected",
    }
}

/// The review comments and files that go with the draft, and why the last
/// one could not be added.
#[component]
fn PendingItems(composer: Composer) -> Element {
    let Composer {
        mut review,
        mut attachments,
        error,
        ..
    } = composer;
    rsx! {
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
    }
}

/// The slash commands or files the draft may be completed with.
#[component]
fn SuggestionList(
    suggestions: Suggestions,
    current: usize,
    onhighlight: EventHandler<usize>,
    onpick: EventHandler<usize>,
) -> Element {
    let class = move |index: usize| {
        if index == current {
            "slash-item selected"
        } else {
            "slash-item"
        }
    };
    match suggestions {
        Suggestions::None => rsx! {},
        Suggestions::Commands(commands) => rsx! {
            div { class: "slash-list",
                for (index, command) in commands.into_iter().enumerate() {
                    div {
                        key: "{command.name}",
                        class: class(index),
                        onmouseenter: move |_| onhighlight.call(index),
                        // Keep the focus in the textarea.
                        onmousedown: move |event| event.prevent_default(),
                        onclick: move |_| onpick.call(index),
                        span { class: "name", "/{command.name}" }
                        if let Some(hint) = &command.hint {
                            span { class: "hint", "{hint}" }
                        }
                        span { class: "description", "{command.description}" }
                    }
                }
            }
        },
        Suggestions::Files { found, note, .. } => rsx! {
            div { class: "slash-list",
                if let Some(note) = note {
                    div { class: "slash-note", "{note}" }
                }
                for (index, path) in found.into_iter().enumerate() {
                    div {
                        key: "{path}",
                        class: class(index),
                        onmouseenter: move |_| onhighlight.call(index),
                        onmousedown: move |event| event.prevent_default(),
                        onclick: move |_| onpick.call(index),
                        span { class: "name", "@{path}" }
                    }
                }
            }
        },
    }
}

/// Complete the draft with suggestion `index`: a slash command's name, or a
/// file for the `@` the draft ends with, linked to the prompt.
fn pick_suggestion(composer: Composer, suggestions: &Suggestions, index: usize) {
    let Composer {
        mut draft,
        mut mentions,
        ..
    } = composer;
    match suggestions {
        Suggestions::None => {}
        Suggestions::Commands(commands) => {
            if let Some(command) = commands.get(index) {
                draft.set(slash::completion(command));
            }
        }
        Suggestions::Files { found, root, .. } => {
            if let Some(path) = found.get(index) {
                let completed = mention::complete(&draft.peek(), path);
                draft.set(completed);
                mentions.write().push(Mention {
                    path: path.clone(),
                    uri: ukai_core::attachment::file_uri(&Path::new(root).join(path)),
                });
            }
        }
    }
}

/// Send the draft with what goes with it: as a turn, as a steer of the
/// running one, or queued behind it for an agent that cannot be steered. A
/// `!` command runs in the checkout instead.
fn send_draft(
    app: AppSignals,
    pane: PaneId,
    composer: Composer,
    mut transcript: Signal<Transcript>,
    handle: Option<&ukai_acp::SessionHandle>,
    checkout: Option<Checkout>,
) {
    let Composer {
        mut draft,
        mut attachments,
        mut review,
        mut error,
        mut mentions,
        ..
    } = composer;
    let text = draft.read().trim().to_owned();
    if let Some(command) = shell::command(&text) {
        run_shell(checkout, transcript, command.to_owned());
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
    // Files picked with `@` go as links, without a chip: the text names them.
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
    let sent = handle.is_some_and(|h| {
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

/// Read the checkout's files again, unless a recent listing will do.
fn refresh_files(checkout: Option<Checkout>, mut files: Signal<Files>) {
    if !files.peek().stale(Instant::now()) {
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

/// Attach what was pasted: an image, or the files copied in a file manager.
fn paste(composer: Composer, transcript: Signal<Transcript>, pasted: Pasted) {
    let mut error = composer.error;
    match pasted {
        Pasted::Image(bytes) => {
            attach(composer, transcript, Path::new("pasted-image.png"), bytes);
        }
        Pasted::Files(paths) => {
            for path in paths {
                spawn(async move {
                    let read = {
                        let path = path.clone();
                        tokio::task::spawn_blocking(move || std::fs::read(path)).await
                    };
                    match read {
                        Ok(Ok(bytes)) => attach(composer, transcript, &path, bytes),
                        Ok(Err(e)) => error.set(Some(format!("{}: {e}", path.display()))),
                        Err(e) => error.set(Some(e.to_string())),
                    }
                });
            }
        }
    }
}

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
