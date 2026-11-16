//! An agent pane while its session goes on: whatever the agent sends, in
//! whatever order, the pane shows each entry of the transcript as what it
//! is and in its place, says what the session is waiting for, and after
//! every event reads as a pane that was opened on the transcript as it
//! then stands.
//!
//! What a sequence of events makes of a transcript is `ukai_core`'s to
//! decide and to test; here the transcript the app holds is the given, and
//! the screen is held to it.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use proptest::prelude::*;
use ukai_acp::{
    FileDiff, PermissionChoice, PlanItem, PromptCaps, RestoreOutcome, SessionEvent, ToolStatus,
    Update,
};
use ukai_core::keymap::Platform;
use ukai_core::sidebar::Folds;
use ukai_core::{Connection, Entry, Layout, PaneContent, Projects, Status, Transcript};

use super::AgentSession;
use crate::app::tests::{Bench, Seed, config, root_of};
use crate::projects::AppSignals;
use crate::testing::{Content, Dom, NodeRef, Screen};
use dioxus::prelude::*;

/// The pane the agent is in.
const PANE: ukai_core::PaneId = 0;

/// An app with one project and its agent open in the only pane, the
/// session showing `transcript`.
fn open(transcript: Transcript) -> (Screen, AgentSession) {
    let mut projects = Projects::default();
    let id = projects.add(&root_of("alpha"));
    let mut layout = Layout::default();
    layout.set_content(
        PANE,
        PaneContent::Agent {
            project: id.clone(),
            agent: "claude".to_owned(),
            session: None,
            workspace: None,
        },
    );
    let mut screen = Screen::new(VirtualDom::new(Bench).with_root_context(Seed {
        layout,
        platform: Platform::Other,
        projects,
        configs: HashMap::from([(id, Ok(config(&["claude"])))]),
        folds: Folds::default(),
    }));
    let mut sessions = screen.with(consume_context::<AppSignals>).sessions;
    let session = screen.with(|| {
        let session = AgentSession::detached(transcript, false);
        sessions.write().insert(PANE, session.clone());
        session
    });
    screen.render();
    (screen, session)
}

// ---- what an agent sends ---------------------------------------------------

fn word() -> impl Strategy<Value = String> {
    "[a-z]{1,6}"
}

/// A piece of a message as it streams in: a word, with or without the
/// space that follows it.
fn chunk() -> impl Strategy<Value = String> {
    (word(), any::<bool>()).prop_map(
        |(word, space)| {
            if space { format!("{word} ") } else { word }
        },
    )
}

fn tool_id() -> impl Strategy<Value = String> {
    prop::sample::select(&["t1", "t2", "t3"][..]).prop_map(str::to_owned)
}

fn tool_status() -> impl Strategy<Value = ToolStatus> {
    prop_oneof![
        Just(ToolStatus::Pending),
        Just(ToolStatus::InProgress),
        Just(ToolStatus::Completed),
        Just(ToolStatus::Failed),
    ]
}

fn lines() -> impl Strategy<Value = String> {
    prop::collection::vec(word(), 0..4).prop_map(|lines| lines.join("\n"))
}

/// A change to a file inside the project or outside it.
fn diff() -> impl Strategy<Value = FileDiff> {
    (any::<bool>(), word(), prop::option::of(lines()), lines()).prop_map(
        |(inside, name, old_text, new_text)| FileDiff {
            path: if inside {
                root_of("alpha").join("src").join(format!("{name}.rs"))
            } else {
                PathBuf::from(format!("/elsewhere/{name}.rs"))
            },
            old_text,
            new_text,
        },
    )
}

fn diffs() -> impl Strategy<Value = Vec<FileDiff>> {
    prop_oneof![
        3 => Just(Vec::new()),
        1 => prop::collection::vec(diff(), 1..3),
    ]
}

/// What an agent, or a subagent of it, says and does.
fn work() -> impl Strategy<Value = SessionEvent> {
    prop_oneof![
        4 => chunk().prop_map(|text| SessionEvent::AgentMessage { text }),
        2 => chunk().prop_map(|text| SessionEvent::AgentThought { text }),
        3 => (tool_id(), word(), word(), tool_status(), diffs()).prop_map(
            |(id, title, kind, status, diffs)| SessionEvent::ToolCall {
                id,
                title,
                kind,
                status,
                diffs,
            }
        ),
        3 => (
            tool_id(),
            prop::option::of(word()),
            prop::option::of(tool_status()),
            prop::option::of(diffs()),
        )
            .prop_map(|(id, title, status, diffs)| SessionEvent::ToolCallUpdate {
                id,
                title,
                status,
                diffs,
            }),
    ]
}

fn ready() -> impl Strategy<Value = SessionEvent> {
    prop_oneof![
        Just(RestoreOutcome::Fresh),
        Just(RestoreOutcome::Replayed),
        Just(RestoreOutcome::Resumed),
        Just(RestoreOutcome::Unsupported),
        Just(RestoreOutcome::Missing),
    ]
    .prop_map(|restore| SessionEvent::Ready {
        agent: None,
        session_id: "s".to_owned(),
        restore,
        prompt: PromptCaps::default(),
        config: Vec::new(),
        list_sessions: false,
    })
}

fn update() -> impl Strategy<Value = Update> {
    (word(), 1..9u8).prop_map(|(package, version)| Update {
        package,
        current: format!("1.{version}.0"),
        latest: format!("1.{}.0", version + 1),
    })
}

/// Any event of a session.
fn event() -> impl Strategy<Value = SessionEvent> {
    let plan = prop::collection::vec(
        (word(), any::<bool>(), any::<bool>()).prop_map(|(content, done, active)| PlanItem {
            content,
            done,
            active,
        }),
        0..4,
    );
    // The run numbers the requests.
    let ask = (word(), prop::collection::vec(word(), 1..4)).prop_map(|(title, names)| {
        SessionEvent::PermissionRequested {
            id: 0,
            title,
            options: names
                .into_iter()
                .enumerate()
                .map(|(index, name)| PermissionChoice {
                    id: format!("o{index}"),
                    name,
                    kind: "allow_once".to_owned(),
                })
                .collect(),
        }
    });
    prop_oneof![
        12 => work(),
        4 => (tool_id(), work()).prop_map(|(parent, event)| SessionEvent::Subagent {
            parent,
            event: Box::new(event),
        }),
        3 => chunk().prop_map(|text| SessionEvent::UserMessage { text }),
        1 => Just(SessionEvent::UserImage {
            url: "data:image/png;base64,AAAA".to_owned(),
        }),
        2 => plan.prop_map(|entries| SessionEvent::Plan { entries }),
        3 => ask,
        1 => word().prop_map(|name| SessionEvent::FileWritten {
            path: root_of("alpha").join(format!("{name}.rs")),
        }),
        3 => Just(SessionEvent::TurnRunning),
        3 => prop::sample::select(&["end_turn", "cancelled", "max_tokens"][..])
            .prop_map(|reason| SessionEvent::TurnEnded {
                stop_reason: reason.to_owned(),
            }),
        1 => word().prop_map(|message| SessionEvent::Preparing { message }),
        1 => word().prop_map(|message| SessionEvent::PromptFailed { message }),
        1 => word().prop_map(|message| SessionEvent::ConfigFailed { message }),
        1 => word().prop_map(|message| SessionEvent::SteerFailed { message }),
        3 => ready(),
        2 => (1..4u32).prop_map(|attempt| SessionEvent::Reconnecting { attempt }),
        1 => Just(SessionEvent::Detached),
        1 => prop::option::of(word()).prop_map(|error| SessionEvent::Closed { error }),
        1 => update().prop_map(SessionEvent::UpdateAvailable),
        1 => update().prop_map(SessionEvent::UpdateMissed),
    ]
}

/// What happens in the pane.
#[derive(Debug, Clone)]
enum Step {
    /// The agent sends an event.
    Event(SessionEvent),
    /// The user answers the `usize`th request still waiting, with the
    /// `usize`th of its choices.
    Answer(usize, usize),
    /// The user turns an offered update down.
    SkipUpdate,
}

fn step() -> impl Strategy<Value = Step> {
    prop_oneof![
        12 => event().prop_map(Step::Event),
        2 => (0..4usize, 0..4usize).prop_map(|(request, choice)| Step::Answer(request, choice)),
        1 => Just(Step::SkipUpdate),
    ]
}

// ---- what the pane shows ---------------------------------------------------

/// One entry of a transcript as a reader sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Seen {
    User {
        text: String,
        attachments: usize,
    },
    /// The words of a message, which Markdown is free to space as it likes.
    Agent(Vec<String>),
    Thought(Vec<String>),
    Tool {
        state: &'static str,
        kind: String,
        title: String,
    },
    /// A tool call that changed files: each by its path, and whether it
    /// is new.
    Changes {
        state: &'static str,
        kind: String,
        title: String,
        files: Vec<(String, bool)>,
    },
    /// A tool call that started a subagent, with what the subagent did.
    Subagent {
        state: &'static str,
        kind: String,
        title: String,
        calls: String,
        entries: Vec<Seen>,
    },
    Plan {
        progress: String,
        /// Each step and how far it is: done, active, or neither.
        items: Vec<(String, String)>,
    },
    /// A request with the choices it still offers, or the answer it got.
    Permission {
        title: String,
        choices: Vec<String>,
        answer: Option<String>,
    },
    Wrote(String),
    Notice {
        text: String,
        error: bool,
    },
}

fn words(text: &str) -> Vec<String> {
    text.split_whitespace().map(str::to_owned).collect()
}

/// How a tool call's status is told.
fn told(status: ToolStatus) -> &'static str {
    match status {
        ToolStatus::Pending => "pending",
        ToolStatus::InProgress => "running",
        ToolStatus::Completed => "done",
        ToolStatus::Failed => "failed",
    }
}

/// A path as the pane names it: from the project's root when inside it.
fn named(path: &Path) -> String {
    path.strip_prefix(root_of("alpha"))
        .unwrap_or(path)
        .display()
        .to_string()
}

/// What `entries` should look like.
fn expected(entries: &[Entry]) -> Vec<Seen> {
    entries
        .iter()
        .map(|entry| match entry {
            Entry::User { text, attachments } => Seen::User {
                text: text.clone(),
                attachments: attachments.len(),
            },
            Entry::Agent { text } => Seen::Agent(words(text)),
            Entry::Thought { text } => Seen::Thought(words(text)),
            Entry::Tool {
                title,
                kind,
                status,
                diffs,
                children,
                ..
            } => {
                let (state, kind, title) = (told(*status), kind.clone(), title.clone());
                if !children.is_empty() {
                    let calls = children
                        .iter()
                        .filter(|child| matches!(child, Entry::Tool { .. }))
                        .count();
                    Seen::Subagent {
                        state,
                        kind,
                        title,
                        calls: if calls == 1 {
                            "1 tool call".to_owned()
                        } else {
                            format!("{calls} tool calls")
                        },
                        entries: expected(children),
                    }
                } else if diffs.is_empty() {
                    Seen::Tool { state, kind, title }
                } else {
                    Seen::Changes {
                        state,
                        kind,
                        title,
                        files: diffs
                            .iter()
                            .map(|diff| (named(&diff.path), diff.old_text.is_none()))
                            .collect(),
                    }
                }
            }
            Entry::Plan { items } => Seen::Plan {
                progress: format!(
                    "{}/{}",
                    items.iter().filter(|item| item.done).count(),
                    items.len()
                ),
                items: items
                    .iter()
                    .map(|item| {
                        let how = if item.done {
                            "done"
                        } else if item.active {
                            "active"
                        } else {
                            ""
                        };
                        (item.content.clone(), how.to_owned())
                    })
                    .collect(),
            },
            Entry::Permission {
                title,
                options,
                answer,
                ..
            } => Seen::Permission {
                title: title.clone(),
                choices: match answer {
                    Some(_) => Vec::new(),
                    None => options.iter().map(|option| option.name.clone()).collect(),
                },
                answer: answer.as_ref().map(|answer| format!("→ {answer}")),
            },
            Entry::FileWritten { path } => Seen::Wrote(path.display().to_string()),
            Entry::Notice { text, error } => Seen::Notice {
                text: text.clone(),
                error: *error,
            },
            Entry::Shell { .. } => panic!("no command was run"),
        })
        .collect()
}

fn text_in(dom: &Dom, node: NodeRef, class: &str) -> String {
    dom.by_class_in(node, class)
        .first()
        .map(|found| dom.text(*found))
        .unwrap_or_default()
}

/// The state a tool call's row shows: the class beside `tool`.
fn state_of(dom: &Dom, node: NodeRef) -> &'static str {
    ["pending", "running", "done", "failed"]
        .into_iter()
        .find(|state| dom.has_class(node, state))
        .unwrap_or("")
}

/// The entries among the element children of `list`, as they read. What is
/// not an entry (the offer of an update, the note that the agent is at
/// work) is left out; [`extras`] tells of those.
fn seen(dom: &Dom, list: NodeRef) -> Result<Vec<Seen>, TestCaseError> {
    let mut out = Vec::new();
    for node in dom.children(list) {
        let tag = dom.tag(node);
        let is = |class: &str| dom.has_class(node, class);
        if is("msg") && is("user") {
            let text: String = dom
                .content(node)
                .into_iter()
                .filter_map(|content| match content {
                    Content::Text(text) => Some(text),
                    Content::Element(_) => None,
                })
                .collect();
            let attachments = dom
                .find_in(node, |dom, inner| {
                    dom.tag(inner) == "img" || dom.has_class(inner, "chip")
                })
                .len();
            out.push(Seen::User { text, attachments });
        } else if is("agent-msg") {
            let message = dom.by_class_in(node, "agent");
            prop_assert_eq!(message.len(), 1);
            out.push(Seen::Agent(words(&dom.text(message[0]))));
        } else if tag == "details" && is("thought") {
            out.push(Seen::Thought(words(&text_in(dom, node, "body"))));
        } else if tag == "details" && is("subagent") {
            let summary = dom.children(node)[0];
            let body: Vec<NodeRef> = dom
                .children(node)
                .into_iter()
                .filter(|child| dom.has_class(*child, "body"))
                .collect();
            prop_assert_eq!(body.len(), 1);
            out.push(Seen::Subagent {
                state: state_of(dom, summary),
                kind: text_in(dom, summary, "verb"),
                title: text_in(dom, summary, "name"),
                calls: text_in(dom, summary, "count"),
                entries: seen(dom, body[0])?,
            });
        } else if tag == "details" && is("tool-diffs") {
            let summary = dom.children(node)[0];
            out.push(Seen::Changes {
                state: state_of(dom, summary),
                kind: text_in(dom, summary, "verb"),
                title: text_in(dom, summary, "name"),
                files: dom
                    .by_class_in(node, "diff-path")
                    .into_iter()
                    .map(|path| {
                        (
                            text_in(dom, path, "name"),
                            !dom.by_class_in(path, "badge").is_empty(),
                        )
                    })
                    .collect(),
            });
        } else if is("tool") && dom.attribute(node, "title").is_some() {
            out.push(Seen::Tool {
                state: state_of(dom, node),
                kind: text_in(dom, node, "verb"),
                title: text_in(dom, node, "name"),
            });
        } else if is("tool") {
            prop_assert_eq!(text_in(dom, node, "verb"), "wrote");
            out.push(Seen::Wrote(text_in(dom, node, "name")));
        } else if is("plan") {
            out.push(Seen::Plan {
                progress: text_in(dom, node, "count"),
                items: dom
                    .find_in(node, |dom, inner| dom.tag(inner) == "li")
                    .into_iter()
                    .map(|item| {
                        (
                            dom.text(item),
                            dom.attribute(item, "class").unwrap_or_default().to_owned(),
                        )
                    })
                    .collect(),
            });
        } else if is("permission") {
            let choices: Vec<String> = dom
                .find_in(node, |dom, inner| dom.tag(inner) == "button")
                .into_iter()
                .map(|button| dom.text(button))
                .collect();
            let answer = dom.by_class_in(node, "answer");
            // It stands out exactly while it waits.
            prop_assert_eq!(is("pending"), answer.is_empty());
            out.push(Seen::Permission {
                title: text_in(dom, node, "title"),
                choices,
                answer: answer.first().map(|answer| dom.text(*answer)),
            });
        } else if is("notice-line") {
            out.push(Seen::Notice {
                text: dom.text(node),
                error: is("error"),
            });
        } else {
            prop_assert!(
                is("update-offer") || is("chat-empty") || is("working") || is("steer"),
                "<{} class={:?}> among the entries",
                tag,
                dom.attribute(node, "class")
            );
        }
    }
    Ok(out)
}

/// What the pane says beside the entries.
#[derive(Debug, PartialEq, Eq)]
struct Extras {
    /// The release it offers to restart for.
    update: Option<String>,
    /// It invites a first message.
    empty: bool,
    /// It says the agent is at work.
    working: bool,
    /// It says the session has ended, and offers to reconnect.
    ended: bool,
    /// The message box takes text.
    writable: bool,
}

fn extras(dom: &Dom, pane: NodeRef) -> Extras {
    let boxes = dom.find_in(pane, |dom, node| dom.tag(node) == "textarea");
    Extras {
        update: dom
            .by_class_in(pane, "update-offer")
            .first()
            .map(|offer| text_in(dom, *offer, "title")),
        empty: !dom.by_class_in(pane, "chat-empty").is_empty(),
        working: !dom.by_class_in(pane, "working").is_empty(),
        ended: !dom.by_class_in(pane, "composer-notice").is_empty(),
        writable: boxes
            .first()
            .is_some_and(|field| dom.attribute(*field, "disabled") != Some("true")),
    }
}

/// What the pane should say beside the entries of `transcript`: it offers
/// an update there is, invites a first message when the session is ready
/// and nothing was said, says the agent is at work unless the agent waits
/// for the user, and takes a message while the session is on.
fn expected_extras(transcript: &Transcript) -> Extras {
    let waiting = transcript
        .entries
        .iter()
        .any(|entry| matches!(entry, Entry::Permission { answer: None, .. }));
    Extras {
        update: transcript
            .update
            .as_ref()
            .map(|update| format!("{} {} is available", update.package, update.latest)),
        empty: transcript.entries.is_empty() && transcript.status == Status::Idle,
        working: transcript.status == Status::Busy && !waiting,
        ended: matches!(transcript.status, Status::Closed { .. }),
        writable: matches!(transcript.status, Status::Idle | Status::Busy),
    }
}

fn pane(dom: &Dom) -> NodeRef {
    let found = dom.find_in(dom.root(), |dom, node| dom.tag(node) == "section");
    assert_eq!(found.len(), 1, "panes");
    found[0]
}

fn check(screen: &Screen, transcript: &Transcript) -> Result<(), TestCaseError> {
    let dom = &screen.dom;
    let pane = pane(dom);
    let list = dom.by_class_in(pane, "entries");
    prop_assert_eq!(list.len(), 1);
    prop_assert_eq!(seen(dom, list[0])?, expected(&transcript.entries));
    prop_assert_eq!(extras(dom, pane), expected_extras(transcript));
    // A pane opened on this transcript shows the same.
    let (fresh, _) = open(transcript.clone());
    prop_assert_eq!(dom.html(), fresh.dom.html());
    Ok(())
}

proptest! {
    #[test]
    fn the_pane_shows_the_transcript_as_it_stands(
        steps in prop::collection::vec(step(), 0..40),
    ) {
        let (mut screen, session) = open(Transcript::default());
        let mut transcript = session.transcript();
        check(&screen, &Transcript::default())?;
        let mut next_request = 0;
        for step in steps {
            match step {
                Step::Event(mut event) => {
                    // An agent numbers its requests one after another, and
                    // starts over on a connection that replaces a lost one.
                    let reconnecting = screen.with(|| {
                        matches!(transcript.peek().connection, Connection::Reconnecting { .. })
                    });
                    match &mut event {
                        SessionEvent::PermissionRequested { id, .. } => {
                            *id = next_request;
                            next_request += 1;
                        }
                        SessionEvent::Ready { .. } if reconnecting => next_request = 0,
                        _ => {}
                    }
                    screen.with(|| transcript.write().apply(event));
                    screen.render();
                }
                Step::Answer(request, choice) => {
                    let waiting = screen.dom.find_in(pane(&screen.dom), |dom, node| {
                        dom.has_class(node, "permission") && dom.has_class(node, "pending")
                    });
                    if !waiting.is_empty() {
                        let request = waiting[request % waiting.len()];
                        let title = text_in(&screen.dom, request, "title");
                        let buttons = screen
                            .dom
                            .find_in(request, |dom, node| dom.tag(node) == "button");
                        let button = buttons[choice % buttons.len()];
                        let name = screen.dom.text(button);
                        prop_assert!(screen.click(button));
                        // The request whose choice was clicked has that
                        // answer now, and no other request changed.
                        let answered = screen.with(|| transcript.peek().entries.clone());
                        let now = screen.dom.find_in(pane(&screen.dom), |dom, node| {
                            dom.has_class(node, "permission") && dom.has_class(node, "pending")
                        });
                        prop_assert!(now.len() < waiting.len(), "still waiting");
                        prop_assert!(
                            answered.iter().any(|entry| matches!(
                                entry,
                                Entry::Permission { title: t, answer: Some(a), .. }
                                    if *t == title && *a == name
                            )),
                            "no request `{}` answered `{}`",
                            title,
                            name
                        );
                    }
                }
                Step::SkipUpdate => {
                    let offer = screen.dom.by_class("update-offer");
                    if let Some(offer) = offer.first() {
                        let skip = screen.dom.find_in(*offer, |dom, node| {
                            dom.tag(node) == "button" && dom.has_class(node, "outline")
                        });
                        prop_assert_eq!(skip.len(), 1);
                        prop_assert!(screen.click(skip[0]));
                        prop_assert!(screen.dom.by_class("update-offer").is_empty());
                    }
                }
            }
            let now = screen.with(|| transcript.peek().clone());
            check(&screen, &now)?;
        }
    }
}
