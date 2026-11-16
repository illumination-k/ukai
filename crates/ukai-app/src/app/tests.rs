//! The app under a user's hands, with agents at work in its panes.
//!
//! Whatever the user clicks, drags, types, and presses — in the pane area,
//! the sidebar, an empty pane's picker, the command palette — the app comes
//! to the state a model of it comes to, and every part of the screen shows
//! that state: tabs, panes and their headers, the sidebar's tree, the
//! palette's list.
//!
//! The model keeps the layout in a `ukai_core::Layout`, which its own
//! property tests hold to its invariants, and says the rest in the user's
//! terms: what a session's state reads as, which pane has news, what the
//! palette offers.
//!
//! Projects live at paths that do not exist, so a session a gesture starts
//! fails at once instead of running an agent. The test then plays the
//! agent: it writes what a session's events would have made of its
//! transcript.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;

use dioxus::html::input_data::MouseButton;
use proptest::prelude::*;
use ukai_acp::{PermissionChoice, PromptCaps, RestoreOutcome, SessionEvent};
use ukai_config::Config;
use ukai_core::keymap::{Action, Keymap, Keystroke, Platform, Scope};
use ukai_core::sidebar::{Fold, Folds, Node as TreeNode};
use ukai_core::{
    Direction, Layout, Node, PaneContent, PaneId, ProjectId, Projects, Side, Transcript, palette,
};

use super::*;
use crate::agent_pane::AgentSession;
use crate::layout::{DRAG_THRESHOLD, Drag};
use crate::projects::Paths;
use crate::testing::{Dom, KeyPress, NodeRef, Screen};

// ---- the world the app starts in -------------------------------------------

/// A project: its name and the agents its configuration declares, or
/// `None` for a configuration that did not load.
type ProjectSpec = (&'static str, Option<Vec<&'static str>>);

const PROJECTS: [&str; 3] = ["alpha", "beta", "gamma"];
const AGENTS: [&str; 3] = ["claude", "codex", "gemini"];
const BROKEN: &str = "ukai.toml: not a table";

fn world() -> impl Strategy<Value = Vec<ProjectSpec>> {
    let agents = prop::option::weighted(
        0.85,
        prop_oneof![
            1 => Just(Vec::new()),
            6 => prop::sample::subsequence(AGENTS.to_vec(), 1..=AGENTS.len()),
        ],
    );
    let count =
        prop_oneof![1 => Just(0usize), 4 => Just(1usize), 3 => Just(2usize), 2 => Just(3usize)];
    count
        .prop_flat_map(move |count| prop::collection::vec(agents.clone(), count))
        .prop_map(|specs| PROJECTS.into_iter().zip(specs).collect())
}

pub(crate) fn config(agents: &[&str]) -> Config {
    let source: String = agents
        .iter()
        .map(|agent| {
            format!("[agents.{agent}]\ntransport = \"stdio\"\ncommand = \"{agent}-acp\"\n")
        })
        .collect();
    ukai_config::parse(&source).unwrap_or_else(|error| panic!("{error}"))
}

/// What the app starts from.
#[derive(Clone)]
pub(crate) struct Seed {
    pub layout: Layout,
    pub platform: Platform,
    pub projects: Projects,
    pub configs: HashMap<ProjectId, Result<Config, String>>,
    pub folds: Folds,
}

/// The app as [`App`] sets it up, without the window: its state, the
/// default shortcuts, no voice model, and the [`Shell`].
#[component]
pub(crate) fn Bench() -> Element {
    let seed = use_context::<Seed>();
    let paths = use_context_provider(|| Paths {
        user_config: None,
        state_file: None,
    });
    use_context_provider(|| {
        AppSignals::new(
            seed.projects.clone(),
            seed.configs.clone(),
            seed.layout.clone(),
            seed.folds.clone(),
        )
    });
    use_context_provider(|| Shortcuts {
        keymap: Keymap::default_for(seed.platform),
        problems: Vec::new(),
    });
    use_context_provider(|| Voice::load(&paths));
    rsx! {
        Shell {}
    }
}

// ---- what a user, and an agent, can do -------------------------------------

/// Where a dragged pane is let go.
#[derive(Debug, Clone)]
enum Release {
    /// On a zone of the `usize`th other pane: a side of it, or its center.
    /// With `true`, the pointer first wanders off the zone and comes back.
    Zone(usize, Option<Side>, bool),
    Tab(usize),
    NewTab,
    /// Over the panes, on no target.
    Nowhere,
    /// Outside the window, which the app learns from the next move.
    Lost,
}

/// What a session is doing, as its agent would have it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Connecting,
    Ready,
    Working,
    /// Working, and waiting for the user to allow something.
    NeedsInput,
    /// The connection dropped and is being reached again.
    Reconnecting,
    /// The session ended as it should.
    Ended,
    /// The session could not start, or broke.
    Failed,
}

impl State {
    /// How the app tells the user: the color of the dot and the words.
    fn reads_as(self) -> (&'static str, &'static str) {
        match self {
            State::Connecting => ("busy", "connecting"),
            State::Ready => ("ready", "ready"),
            State::Working => ("busy", "working"),
            State::NeedsInput => ("attention", "needs input"),
            State::Reconnecting => ("busy", "reconnecting"),
            State::Ended => ("idle", "closed"),
            State::Failed => ("closed", "disconnected"),
        }
    }

    /// How much a tab's dot wants to show this state over its other panes':
    /// what needs the user first, then what broke, then what is at work.
    fn urgency(self) -> u8 {
        match self.reads_as().0 {
            "attention" => 0,
            "closed" => 1,
            "busy" => 2,
            "ready" => 3,
            _ => 4,
        }
    }

    /// The transcript a session in this state has, about `title` if any.
    fn transcript(self, title: Option<&str>) -> Transcript {
        let ready = SessionEvent::Ready {
            agent: None,
            session_id: "s".to_owned(),
            restore: RestoreOutcome::Fresh,
            prompt: PromptCaps::default(),
            config: Vec::new(),
            list_sessions: false,
        };
        let failed = SessionEvent::Closed {
            error: Some("the agent went away".to_owned()),
        };
        let ask = SessionEvent::PermissionRequested {
            id: 1,
            title: "Run a command".to_owned(),
            options: vec![PermissionChoice {
                id: "allow".to_owned(),
                name: "Allow".to_owned(),
                kind: "allow_once".to_owned(),
            }],
        };
        let events = match self {
            State::Connecting => vec![],
            State::Ready => vec![ready],
            State::Working => vec![ready, SessionEvent::TurnRunning],
            State::NeedsInput => vec![ready, SessionEvent::TurnRunning, ask],
            State::Reconnecting => vec![ready, SessionEvent::Reconnecting { attempt: 1 }],
            State::Ended => vec![ready, SessionEvent::Closed { error: None }],
            State::Failed => vec![failed],
        };
        let mut transcript = Transcript::default();
        for event in events {
            transcript.apply(event);
        }
        if let Some(title) = title {
            transcript.apply(SessionEvent::Title {
                title: Some(title.to_owned()),
            });
        }
        transcript
    }
}

/// Something a user does, or an agent. Indices pick among what is on
/// screen, wrapping, so most of what is generated hits something.
#[derive(Debug, Clone)]
enum Act {
    NewTab,
    SelectTab(usize),
    CloseTab(usize),
    Split(usize, Direction),
    ClosePane(usize),
    /// Press the mouse on a pane's body.
    Focus(usize),
    /// Double-click a boundary between panes.
    Equalize(usize),
    /// Drag a boundary between panes to `f64` of the way along the two it
    /// separates (or past either end).
    Resize(usize, f64),
    /// Hold a pane by its title with `button`, move the pointer to each of
    /// `to` in turn (offsets from where it went down), and let go.
    Drag {
        pane: usize,
        button: MouseButton,
        to: Vec<(f64, f64)>,
        release: Release,
    },
    Key(KeyPress),
    /// In an empty pane's picker: choose a project, then one of its agents.
    Pick {
        pane: usize,
        project: usize,
        agent: usize,
    },
    /// In the sidebar: click an agent's row.
    Row(usize),
    /// In the sidebar: click a project's name.
    Fold(usize),
    /// In the sidebar: open a project's menu of agents and choose one.
    Launch {
        project: usize,
        agent: usize,
    },
    /// In the sidebar: close a project.
    CloseProject(usize),
    /// In a pane whose session has ended: click "Reconnect".
    Reconnect(usize),
    /// The agent of a session comes to a state, about a title if any.
    Agent(usize, State, Option<String>),
    /// A session has news while the user looks elsewhere.
    News(usize),
    /// The session of the pane the user was on has news while the window
    /// is away; back at the window, the user clicks that pane.
    NewsWhileAway,
    /// In the palette: click its text field.
    ClickField,
    /// In the palette: type so that the field holds this.
    Type(String),
    /// In the palette: move the pointer over a row.
    Hover(usize),
    /// In the palette: click a row.
    ClickItem(usize),
    /// Click beside the palette.
    Backdrop,
}

fn side() -> impl Strategy<Value = Side> {
    prop_oneof![
        Just(Side::Left),
        Just(Side::Right),
        Just(Side::Top),
        Just(Side::Bottom),
    ]
}

fn direction() -> impl Strategy<Value = Direction> {
    prop_oneof![Just(Direction::Horizontal), Just(Direction::Vertical)]
}

fn release() -> impl Strategy<Value = Release> {
    prop_oneof![
        4 => (0..8usize, prop::option::of(side()), any::<bool>())
            .prop_map(|(pane, side, wander)| Release::Zone(pane, side, wander)),
        2 => (0..4usize).prop_map(Release::Tab),
        1 => Just(Release::NewTab),
        1 => Just(Release::Nowhere),
        1 => Just(Release::Lost),
    ]
}

fn state() -> impl Strategy<Value = State> {
    prop_oneof![
        Just(State::Connecting),
        Just(State::Ready),
        Just(State::Working),
        Just(State::NeedsInput),
        Just(State::Reconnecting),
        Just(State::Ended),
        Just(State::Failed),
    ]
}

/// The chord of the platform's "mod" key: (ctrl, alt, shift, meta).
fn main_modifier(platform: Platform) -> (bool, bool, bool, bool) {
    match platform {
        Platform::Mac => (false, false, false, true),
        Platform::Other => (true, false, false, false),
    }
}

/// Keys the default keymap binds, the ones the palette takes, and some
/// nothing takes. Most come with the modifiers shortcuts are made of on
/// `platform` (its "mod" key, alone or with shift, or control) or with
/// none, the rest with any.
fn key_press(platform: Platform) -> impl Strategy<Value = KeyPress> {
    const KEYS: &[(&str, &str)] = &[
        ("t", "KeyT"),
        ("w", "KeyW"),
        ("d", "KeyD"),
        ("k", "KeyK"),
        ("m", "KeyM"),
        ("x", "KeyX"),
        ("]", "BracketRight"),
        ("[", "BracketLeft"),
        ("1", "Digit1"),
        ("2", "Digit2"),
        ("3", "Digit3"),
        ("4", "Digit4"),
        ("9", "Digit9"),
        ("Tab", "Tab"),
        ("Escape", "Escape"),
        ("Enter", "Enter"),
        ("ArrowDown", "ArrowDown"),
        ("ArrowUp", "ArrowUp"),
    ];
    let main = main_modifier(platform);
    let modifiers = prop_oneof![
        6 => Just(main),
        3 => Just((main.0, false, true, main.3)),
        1 => Just((true, false, false, false)),
        1 => Just((true, false, true, false)),
        3 => Just((false, false, false, false)),
        1 => any::<(bool, bool, bool, bool)>(),
    ];
    (
        prop::sample::select(KEYS),
        modifiers,
        prop::bool::weighted(0.1),
    )
        .prop_map(
            |((key, code), (ctrl, alt, shift, meta), composing)| KeyPress {
                ctrl,
                alt,
                shift,
                meta,
                key,
                code,
                composing,
            },
        )
}

/// The default shortcut that opens and closes the palette.
fn palette_key(platform: Platform) -> KeyPress {
    let (ctrl, alt, shift, meta) = main_modifier(platform);
    KeyPress {
        ctrl,
        alt,
        shift,
        meta,
        key: "k",
        code: "KeyK",
        composing: false,
    }
}

/// What a user might type into the palette: parts of what it lists.
fn query() -> impl Strategy<Value = String> {
    prop_oneof![
        2 => Just(String::new()),
        6 => prop::sample::select(&[
            "new", "split", "close", "open", "go", "tab", "agent", "cla", "codex", "gem", "alpha",
            "be", "sr", "pn", "zzz", "Command", " ", "o a",
        ][..])
        .prop_map(str::to_owned),
        1 => "[a-z ]{0,4}",
    ]
}

/// One thing done.
fn act(platform: Platform) -> impl Strategy<Value = Act> {
    prop_oneof![
        2 => Just(Act::NewTab),
        3 => (0..4usize).prop_map(Act::SelectTab),
        1 => (0..4usize).prop_map(Act::CloseTab),
        5 => (0..8usize, direction()).prop_map(|(pane, direction)| Act::Split(pane, direction)),
        2 => (0..8usize).prop_map(Act::ClosePane),
        2 => (0..8usize).prop_map(Act::Focus),
        1 => (0..8usize).prop_map(Act::Equalize),
        2 => (0..8usize, -0.2..1.2f64).prop_map(|(boundary, share)| Act::Resize(boundary, share)),
        5 => (
            0..8usize,
            prop_oneof![
                8 => Just(MouseButton::Primary),
                1 => Just(MouseButton::Secondary),
                1 => Just(MouseButton::Auxiliary),
            ],
            prop::collection::vec((-6.0..6.0f64, -6.0..6.0f64), 1..4),
            release(),
        )
            .prop_map(|(pane, button, to, release)| Act::Drag { pane, button, to, release }),
        10 => key_press(platform).prop_map(Act::Key),
        8 => (0..8usize, 0..3usize, 0..3usize)
            .prop_map(|(pane, project, agent)| Act::Pick { pane, project, agent }),
        4 => (0..8usize).prop_map(Act::Row),
        1 => (0..3usize).prop_map(Act::Fold),
        5 => (0..3usize, 0..3usize).prop_map(|(project, agent)| Act::Launch { project, agent }),
        1 => (0..3usize).prop_map(Act::CloseProject),
        2 => (0..8usize).prop_map(Act::Reconnect),
        6 => (0..8usize, state(), prop::option::of("[a-z]{1,6}"))
            .prop_map(|(session, state, title)| Act::Agent(session, state, title)),
        8 => (0..8usize).prop_map(Act::News),
        2 => Just(Act::NewsWhileAway),
        // With the palette closed there is nothing these can reach.
        1 => palette_act(platform),
    ]
}

/// Something done in the palette.
fn palette_act(platform: Platform) -> impl Strategy<Value = Act> {
    let key = |key: &'static str| {
        Act::Key(KeyPress {
            ctrl: false,
            alt: false,
            shift: false,
            meta: false,
            key,
            code: key,
            composing: false,
        })
    };
    prop_oneof![
        5 => query().prop_map(Act::Type),
        3 => Just(key("ArrowDown")),
        2 => Just(key("ArrowUp")),
        2 => (0..12usize).prop_map(Act::Hover),
        1 => Just(Act::ClickField),
        2 => key_press(platform).prop_map(Act::Key),
        1 => (0..8usize, state(), prop::option::of("[a-z]{1,6}"))
            .prop_map(|(session, state, title)| Act::Agent(session, state, title)),
    ]
}

/// A few things done one after another, towards something: most parts of
/// the app are reached only through others, which acts drawn one by one
/// seldom line up for.
fn burst(platform: Platform) -> impl Strategy<Value = Vec<Act>> {
    let plain = |key: &'static str| {
        Act::Key(KeyPress {
            ctrl: false,
            alt: false,
            shift: false,
            meta: false,
            key,
            code: key,
            composing: false,
        })
    };
    // Open the palette, look for something, and choose it or leave.
    let in_palette = (
        prop::collection::vec(palette_act(platform), 0..5),
        prop_oneof![
            4 => Just(Some(plain("Enter"))),
            3 => (0..12usize).prop_map(|row| Some(Act::ClickItem(row))),
            1 => Just(Some(plain("Escape"))),
            1 => Just(Some(Act::Backdrop)),
            1 => Just(Some(Act::Key(palette_key(platform)))),
            // Or go on with it open.
            1 => Just(None),
        ],
    )
        .prop_map(move |(acts, end)| {
            let mut all = vec![Act::Key(palette_key(platform))];
            all.extend(acts);
            all.extend(end);
            all
        });
    // An agent asks for the user's leave, and the user stops it instead.
    let stop_asked = (0..8usize).prop_map(move |session| {
        vec![
            Act::Agent(session, State::NeedsInput, None),
            plain("Escape"),
        ]
    });
    // Open an agent, as most of what follows needs one.
    let open = (0..8usize, 0..3usize, 0..3usize, any::<bool>()).prop_map(
        |(pane, project, agent, picker)| {
            vec![if picker {
                Act::Pick {
                    pane,
                    project,
                    agent,
                }
            } else {
                Act::Launch { project, agent }
            }]
        },
    );
    // Open tabs and go between them, as closing and choosing one need.
    let tabs = (
        1..3usize,
        prop::collection::vec(
            prop_oneof![
                (0..4usize).prop_map(Act::SelectTab),
                (0..4usize).prop_map(Act::CloseTab),
                (0..8usize, direction()).prop_map(|(pane, direction)| Act::Split(pane, direction)),
            ],
            1..4,
        ),
    )
        .prop_map(|(new, then)| {
            let mut all = vec![Act::NewTab; new];
            all.extend(then);
            all
        });
    prop_oneof![
        12 => act(platform).prop_map(|act| vec![act]),
        2 => tabs,
        3 => in_palette,
        1 => stop_asked,
        2 => open,
    ]
}

// ---- the model -------------------------------------------------------------

/// A session as the user knows it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Session {
    state: State,
    title: Option<String>,
    /// It has news the user has not looked at.
    unread: bool,
}

/// What a row of the palette does.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Command {
    GoTo(PaneId),
    Launch(ProjectId, String),
    NewTab,
    Split(Direction),
    ClosePane,
    /// Asks for a folder with the system's dialog, which no test can answer.
    OpenProject,
}

/// A row of the palette: what kind of thing it is, its name, a detail.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Item {
    kind: &'static str,
    label: String,
    detail: String,
    command: Command,
}

/// The palette while it is open.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PaletteModel {
    /// Everything it offers: what there was when it opened.
    items: Vec<Item>,
    query: String,
    selected: usize,
}

impl PaletteModel {
    /// The rows shown for the query, best match first.
    fn matches(&self) -> Vec<Item> {
        palette::filter(
            &self.query,
            self.items.iter().map(|item| {
                (
                    format!("{} {} {}", item.kind, item.label, item.detail),
                    item.clone(),
                )
            }),
        )
        .into_iter()
        .take(50)
        .collect()
    }

    /// The selected row, which stays on the list when the list shrinks.
    fn current(&self) -> usize {
        self.selected.min(self.matches().len().saturating_sub(1))
    }
}

#[derive(Debug)]
struct Model {
    layout: Layout,
    /// The open projects, in the order they were opened.
    projects: Vec<(ProjectId, ProjectSpec)>,
    /// The projects whose tree is folded away.
    folded: BTreeSet<ProjectId>,
    sessions: BTreeMap<PaneId, Session>,
    palette: Option<PaletteModel>,
    keymap: Keymap,
}

impl Model {
    /// The panes of the active tab, in the order they are shown.
    fn panes(&self) -> Vec<PaneId> {
        self.layout.active_tab().root.panes()
    }

    fn project(&self, id: &ProjectId) -> Option<&ProjectSpec> {
        self.projects
            .iter()
            .find(|(project, _)| project == id)
            .map(|(_, spec)| spec)
    }

    /// The agent `pane` holds and the project it is of.
    fn agent(&self, pane: PaneId) -> Option<(&ProjectId, &str)> {
        match self.layout.content(pane) {
            Some(PaneContent::Agent { project, agent, .. }) => Some((project, agent)),
            _ => None,
        }
    }

    fn close_pane(&mut self, pane: PaneId) {
        self.layout.close_pane(pane);
        self.sessions.remove(&pane);
    }

    fn close_tab(&mut self, index: usize) {
        for pane in self.layout.close_tab(index) {
            self.sessions.remove(&pane);
        }
    }

    /// `agent` of `project` starts in `pane`. Nothing is at the project's
    /// path, so its session fails at once.
    fn start(&mut self, pane: PaneId, project: &ProjectId, agent: &str) {
        self.layout.set_content(
            pane,
            PaneContent::Agent {
                project: project.clone(),
                agent: agent.to_owned(),
                session: None,
                workspace: None,
            },
        );
        let unread = self
            .sessions
            .get(&pane)
            .is_some_and(|session| session.unread);
        self.sessions.insert(
            pane,
            Session {
                state: State::Failed,
                title: None,
                unread,
            },
        );
    }

    /// `agent` of `project` starts from outside the panes: in the focused
    /// pane if it is empty, else in a new one to its right.
    fn launch(&mut self, project: &ProjectId, agent: &str) {
        let focused = self.layout.focused();
        let target = if self.layout.content(focused) == Some(&PaneContent::Empty) {
            Some(focused)
        } else {
            self.layout.split(focused, Direction::Horizontal)
        };
        if let Some(target) = target {
            self.start(target, project, agent);
        }
    }

    /// What the palette offers when it opens now: every other pane that
    /// shows something, every agent of every project, and the commands.
    fn offer(&self) -> Vec<Item> {
        let mut items = Vec::new();
        for (index, tab) in self.layout.tabs().iter().enumerate() {
            for pane in tab.root.panes() {
                if pane == self.layout.focused() {
                    continue;
                }
                if let Some((project, agent)) = self.agent(pane) {
                    let project = self.project(project).map_or("", |(name, _)| name);
                    items.push(Item {
                        kind: "Go to",
                        label: agent.to_owned(),
                        detail: format!("{project} · tab {}", index + 1),
                        command: Command::GoTo(pane),
                    });
                }
            }
        }
        for (id, (name, agents)) in &self.projects {
            // As the configuration lists them: by name.
            let mut agents = agents.clone().unwrap_or_default();
            agents.sort_unstable();
            for agent in agents {
                items.push(Item {
                    kind: "Open agent",
                    label: agent.to_owned(),
                    detail: (*name).to_owned(),
                    command: Command::Launch(id.clone(), agent.to_owned()),
                });
            }
        }
        let hint = |action| self.keymap.hint(action).unwrap_or_default();
        for (label, detail, command) in [
            ("New tab", hint(Action::NewTab), Command::NewTab),
            (
                "Split right",
                hint(Action::SplitRight),
                Command::Split(Direction::Horizontal),
            ),
            (
                "Split down",
                hint(Action::SplitDown),
                Command::Split(Direction::Vertical),
            ),
            ("Close pane", hint(Action::ClosePane), Command::ClosePane),
            ("Open project…", String::new(), Command::OpenProject),
        ] {
            items.push(Item {
                kind: "Command",
                label: label.to_owned(),
                detail,
                command,
            });
        }
        items
    }

    /// Choosing a row closes the palette and does what the row says.
    fn run(&mut self, command: Command) {
        self.palette = None;
        let focused = self.layout.focused();
        match command {
            Command::GoTo(pane) => self.layout.focus(pane),
            Command::Launch(project, agent) => self.launch(&project, &agent),
            Command::NewTab => {
                self.layout.new_tab();
            }
            Command::Split(direction) => {
                self.layout.split(focused, direction);
            }
            Command::ClosePane => self.close_pane(focused),
            Command::OpenProject => panic!("the test asked for a system dialog"),
        }
    }

    /// The action of the app's the keys of `press` are bound to, unless an
    /// input method is taking them.
    fn shortcut(&self, press: &KeyPress) -> Option<Action> {
        if press.composing {
            return None;
        }
        self.keymap.action(
            Scope::App,
            &Keystroke {
                ctrl: press.ctrl,
                alt: press.alt,
                shift: press.shift,
                meta: press.meta,
                key: press.key,
                code: press.code,
            },
        )
    }

    /// What a shortcut does.
    fn perform(&mut self, action: Action) {
        let focused = self.layout.focused();
        match action {
            Action::Palette => {
                self.palette = match self.palette {
                    Some(_) => None,
                    None => Some(PaletteModel {
                        items: self.offer(),
                        query: String::new(),
                        selected: 0,
                    }),
                };
            }
            Action::NewTab => {
                self.layout.new_tab();
            }
            Action::ClosePane => self.close_pane(focused),
            Action::SplitRight => {
                self.layout.split(focused, Direction::Horizontal);
            }
            Action::SplitDown => {
                self.layout.split(focused, Direction::Vertical);
            }
            Action::NextTab => self.layout.step_tab(1),
            Action::PrevTab => self.layout.step_tab(-1),
            Action::NextPane => self.layout.step_focus(1),
            Action::PrevPane => self.layout.step_focus(-1),
            Action::Tab(n) => self.layout.select_tab(usize::from(n).saturating_sub(1)),
            // Stopping a turn that waits for the user's leave takes the
            // request back: the agent is at work again until it stops.
            Action::Stop => {
                if let Some(session) = self.sessions.get_mut(&focused)
                    && session.state == State::NeedsInput
                {
                    session.state = State::Working;
                }
            }
            // No voice model is set up, and nothing is sent from here.
            Action::Dictate | Action::Send => {}
        }
    }

    /// A key pressed while the palette has the keyboard, which takes the
    /// keys that move through its list and leaves the rest to the app.
    fn press_in_palette(&mut self, press: &KeyPress) {
        let Some(open) = &mut self.palette else {
            return;
        };
        if press.composing {
            return;
        }
        let matches = open.matches();
        let current = open.current();
        let step = |by: isize| (current as isize + by).rem_euclid(matches.len().max(1) as isize);
        match press.key {
            "ArrowDown" => open.selected = step(1) as usize,
            "ArrowUp" => open.selected = step(-1) as usize,
            "Escape" => self.palette = None,
            "Enter" => {
                if let Some(item) = matches.get(current) {
                    self.run(item.command.clone());
                }
            }
            _ => {
                if let Some(action) = self.shortcut(press) {
                    self.perform(action);
                }
            }
        }
    }

    /// Whether `press` would have the palette ask for a system dialog.
    fn asks_for_dialog(&self, press: &KeyPress) -> bool {
        self.palette.as_ref().is_some_and(|open| {
            press.key == "Enter"
                && !press.composing
                && open
                    .matches()
                    .get(open.current())
                    .is_some_and(|item| item.command == Command::OpenProject)
        })
    }

    /// Pressing the mouse on a pane focuses it, and its news is read then,
    /// wherever the focus goes next.
    fn touch(&mut self, pane: PaneId) {
        self.layout.focus(pane);
        self.look();
    }

    /// Looking at a pane takes its news as read.
    fn look(&mut self) {
        let focused = self.layout.focused();
        if let Some(session) = self.sessions.get_mut(&focused) {
            session.unread = false;
        }
    }
}

/// The boundaries of the tree under `node`, in the order they are shown:
/// the path of each one's split and the child it follows.
fn boundaries(node: &Node, path: &mut Vec<usize>, out: &mut Vec<(Vec<usize>, usize)>) {
    if let Node::Split { children, .. } = node {
        for (index, child) in children.iter().enumerate() {
            path.push(index);
            boundaries(child, path, out);
            path.pop();
            if index + 1 < children.len() {
                out.push((path.clone(), index));
            }
        }
    }
}

// ---- what the screen shows -------------------------------------------------

/// What the pane area shows, or should.
#[derive(Debug, PartialEq)]
enum Shape {
    Pane {
        id: String,
        class: String,
    },
    Split {
        class: String,
        /// Each child with its `style`, and `None` for a divider.
        parts: Vec<Option<(String, Shape)>>,
    },
}

/// The shape `layout` should show when nothing is being dragged.
fn expected_shape(layout: &Layout, node: &Node) -> Shape {
    match node {
        Node::Pane(pane) => Shape::Pane {
            id: pane.to_string(),
            class: if layout.focused() == *pane {
                "pane focused".to_owned()
            } else {
                "pane".to_owned()
            },
        },
        Node::Split {
            direction,
            children,
            sizes,
        } => {
            let mut parts = Vec::new();
            for (index, (child, size)) in children.iter().zip(sizes).enumerate() {
                if index > 0 {
                    parts.push(None);
                }
                parts.push(Some((
                    format!("flex: {size} 1 0"),
                    expected_shape(layout, child),
                )));
            }
            Shape::Split {
                class: match direction {
                    Direction::Horizontal => "split horizontal".to_owned(),
                    Direction::Vertical => "split vertical".to_owned(),
                },
                parts,
            }
        }
    }
}

fn attribute(dom: &Dom, node: NodeRef, name: &str) -> String {
    dom.attribute(node, name).unwrap_or_default().to_owned()
}

/// The text of each element of class `class` under `node`.
fn texts(dom: &Dom, node: NodeRef, class: &str) -> Vec<String> {
    dom.by_class_in(node, class)
        .into_iter()
        .map(|found| dom.text(found))
        .collect()
}

/// The `class` of each element under `node` whose classes start with
/// `class`: for a dot, the state it shows.
fn classes(dom: &Dom, node: NodeRef, class: &str) -> Vec<String> {
    dom.by_class_in(node, class)
        .into_iter()
        .map(|found| attribute(dom, found, "class"))
        .collect()
}

/// The one element child of `node`.
fn only_child(dom: &Dom, node: NodeRef) -> Result<NodeRef, TestCaseError> {
    let children = dom.children(node);
    prop_assert_eq!(children.len(), 1, "children of a <{}>", dom.tag(node));
    Ok(children[0])
}

fn shown_shape(dom: &Dom, node: NodeRef) -> Result<Shape, TestCaseError> {
    if dom.tag(node) == "section" {
        return Ok(Shape::Pane {
            id: attribute(dom, node, "data-pane"),
            class: attribute(dom, node, "class"),
        });
    }
    prop_assert!(dom.has_class(node, "split"), "neither a pane nor a split");
    let mut parts = Vec::new();
    for child in dom.children(node) {
        parts.push(if dom.has_class(child, "divider") {
            None
        } else {
            prop_assert!(dom.has_class(child, "split-child"));
            Some((
                attribute(dom, child, "style"),
                shown_shape(dom, only_child(dom, child)?)?,
            ))
        });
    }
    Ok(Shape::Split {
        class: attribute(dom, node, "class"),
        parts,
    })
}

/// What a pane's header says of it.
#[derive(Debug, PartialEq)]
struct Header {
    project: Vec<String>,
    title: Vec<String>,
    session: Vec<String>,
    dot: Vec<String>,
    status: Vec<String>,
    words: Vec<String>,
    unread: bool,
}

/// What a tab says of its panes.
#[derive(Debug, PartialEq)]
struct TabLook {
    class: String,
    name: Vec<String>,
    dot: Vec<String>,
    count: Vec<String>,
}

/// What a row of the sidebar says of an agent's pane.
#[derive(Debug, PartialEq)]
struct RowLook {
    pane: String,
    class: String,
    name: Vec<String>,
    session: Vec<String>,
    dot: Vec<String>,
    words: Vec<String>,
    tab: Vec<String>,
}

/// What the palette's field holds, and its rows as (kind, name, detail,
/// whether selected).
type PaletteLook = (String, Vec<(String, String, String, bool)>);

/// What the sidebar says of a project.
#[derive(Debug, PartialEq)]
struct ProjectLook {
    name: Vec<String>,
    /// How many agents it has open, when any.
    count: Vec<String>,
    problem: Vec<String>,
    rows: Vec<RowLook>,
}

fn some(text: impl Into<String>) -> Vec<String> {
    vec![text.into()]
}

fn optional(text: Option<&str>) -> Vec<String> {
    text.map(str::to_owned).into_iter().collect()
}

// ---- a run -----------------------------------------------------------------

/// A screen with the model of what it should show.
struct Run {
    screen: Screen,
    app: AppSignals,
    model: Model,
    platform: Platform,
}

/// Where the two children beside a dragged boundary are taken to lie: from
/// the first, for the second, in pixels along their split.
const SPAN: (f64, f64) = (40.0, 400.0);

/// Where the pointer goes down when a gesture needs a place.
const FROM: (f64, f64) = (100.0, 100.0);

/// Where project `name` is said to be. Nothing is there.
pub(crate) fn root_of(name: &str) -> PathBuf {
    PathBuf::from(format!("/nonexistent/ukai-test/{name}"))
}

impl Run {
    fn new(platform: Platform, world: &[ProjectSpec]) -> Self {
        let mut projects = Projects::default();
        let mut configs = HashMap::new();
        let mut listed = Vec::new();
        for (name, agents) in world {
            let id = projects.add(&root_of(name));
            configs.insert(
                id.clone(),
                match agents {
                    Some(agents) => Ok(config(agents)),
                    None => Err(BROKEN.to_owned()),
                },
            );
            listed.push((id, (*name, agents.clone())));
        }
        let layout = Layout::default();
        let screen = Screen::new(VirtualDom::new(Bench).with_root_context(Seed {
            layout: layout.clone(),
            platform,
            projects,
            configs,
            folds: Folds::default(),
        }));
        let app = screen.with(consume_context::<AppSignals>);
        Self {
            screen,
            app,
            model: Model {
                layout,
                projects: listed,
                folded: BTreeSet::new(),
                sessions: BTreeMap::new(),
                palette: None,
                keymap: Keymap::default_for(platform),
            },
            platform,
        }
    }

    fn one(&self, class: &str) -> NodeRef {
        let found = self.screen.dom.by_class(class);
        assert_eq!(found.len(), 1, "elements of class `{class}`");
        found[0]
    }

    /// The `<section>` of pane `pane` of the active tab.
    fn section(&self, pane: PaneId) -> NodeRef {
        let id = pane.to_string();
        let found = self
            .screen
            .dom
            .find_in(self.screen.dom.root(), |dom, node| {
                dom.tag(node) == "section" && dom.attribute(node, "data-pane") == Some(&id)
            });
        assert_eq!(found.len(), 1, "sections of pane {pane}");
        found[0]
    }

    /// The buttons under `node` whose title starts with `title`.
    fn buttons(&self, node: NodeRef, title: &str) -> Vec<NodeRef> {
        self.screen.dom.find_in(node, |dom, node| {
            dom.tag(node) == "button"
                && dom
                    .attribute(node, "title")
                    .is_some_and(|t| t.starts_with(title))
        })
    }

    /// The button of `pane`'s header whose title starts with `title`.
    fn tool(&self, pane: PaneId, title: &str) -> NodeRef {
        let found = self.buttons(self.section(pane), title);
        assert_eq!(found.len(), 1, "`{title}` buttons of pane {pane}");
        found[0]
    }

    /// Press and release the mouse on `node`, as a click is made.
    fn click(&mut self, node: NodeRef) {
        self.screen.mouse_down(node, MouseButton::Primary, FROM);
        self.screen.mouse_up(node, MouseButton::Primary, FROM);
        self.screen.click(node);
    }

    /// Click the button of `pane`'s header titled `title`. Finding it again
    /// for each event keeps the test from holding on to a node a render
    /// replaced.
    fn click_tool(&mut self, pane: PaneId, title: &str) {
        let button = self.tool(pane, title);
        self.screen.mouse_down(button, MouseButton::Primary, FROM);
        let button = self.tool(pane, title);
        self.screen.mouse_up(button, MouseButton::Primary, FROM);
        let button = self.tool(pane, title);
        self.screen.click(button);
    }

    /// The part of the sidebar that is project `index`'s.
    fn group(&self, index: usize) -> NodeRef {
        self.screen.dom.by_class("project-group")[index]
    }

    /// The session of `pane`, to play its agent.
    fn session(&self, pane: PaneId) -> AgentSession {
        self.screen
            .with(|| self.app.sessions.peek().get(&pane).cloned())
            .unwrap_or_else(|| panic!("pane {pane} has no session"))
    }

    /// Do `act` and tell the model what it should come to. The palette
    /// lies over everything while it is open, so a pointer first lands
    /// beside it, which closes it.
    fn perform(&mut self, act: &Act) -> Result<(), TestCaseError> {
        let with_pointer = !matches!(
            act,
            Act::Key(_)
                | Act::Agent(..)
                | Act::News(_)
                | Act::Type(_)
                | Act::ClickField
                | Act::Hover(_)
                | Act::ClickItem(_)
                | Act::Backdrop
        );
        if with_pointer && self.model.palette.is_some() {
            self.click(self.one("palette-backdrop"));
            self.model.palette = None;
        }
        let panes = self.model.panes();
        let pick = |index: usize| panes[index % panes.len()];
        let tabs = self.model.layout.tabs().len();
        match act {
            Act::NewTab => {
                self.click(self.one("new-tab"));
                self.model.layout.new_tab();
            }
            Act::SelectTab(index) => {
                let index = index % tabs;
                self.click(self.screen.dom.by_class("tab")[index]);
                self.model.layout.select_tab(index);
            }
            Act::CloseTab(index) => {
                let index = index % tabs;
                let tab = self.screen.dom.by_class("tab")[index];
                let close = self.screen.dom.by_class_in(tab, "close");
                prop_assert_eq!(close.len(), 1);
                self.click(close[0]);
                self.model.close_tab(index);
            }
            Act::Split(pane, direction) => {
                let pane = pick(*pane);
                let title = match direction {
                    Direction::Horizontal => "Split right",
                    Direction::Vertical => "Split down",
                };
                self.click_tool(pane, title);
                // The press focuses the pane before the click splits it.
                self.model.touch(pane);
                self.model.layout.split(pane, *direction);
            }
            Act::ClosePane(pane) => {
                let pane = pick(*pane);
                self.click_tool(pane, "Close pane");
                self.model.touch(pane);
                self.model.close_pane(pane);
            }
            Act::Focus(pane) => {
                let pane = pick(*pane);
                let body = self.screen.dom.by_class_in(self.section(pane), "pane-body");
                prop_assert_eq!(body.len(), 1);
                self.click(body[0]);
                self.model.touch(pane);
            }
            Act::Equalize(index) => {
                let mut all = Vec::new();
                boundaries(
                    &self.model.layout.active_tab().root,
                    &mut Vec::new(),
                    &mut all,
                );
                let dividers = self.screen.dom.by_class("divider");
                prop_assert_eq!(dividers.len(), all.len());
                if !all.is_empty() {
                    let index = index % all.len();
                    prop_assert!(self.screen.double_click(dividers[index]));
                    let (path, boundary) = &all[index];
                    self.model.layout.resize(path, *boundary, 0.5);
                }
            }
            Act::Resize(index, share) => {
                let mut all = Vec::new();
                boundaries(
                    &self.model.layout.active_tab().root,
                    &mut Vec::new(),
                    &mut all,
                );
                if !all.is_empty() {
                    let (path, boundary) = all[index % all.len()].clone();
                    self.resize(&path, boundary, *share)?;
                }
            }
            Act::Drag {
                pane,
                button,
                to,
                release,
            } => {
                self.drag(pick(*pane), *button, to, release)?;
            }
            Act::Key(press) => self.press(press),
            Act::Pick {
                pane,
                project,
                agent,
            } => self.pick(*pane, *project, *agent)?,
            Act::Row(index) => {
                let rows = self.screen.dom.by_class("agent-row");
                if !rows.is_empty() {
                    let row = rows[index % rows.len()];
                    let pane: PaneId = attribute(&self.screen.dom, row, "data-pane")
                        .parse()
                        .unwrap_or_else(|_| panic!("a row of no pane"));
                    self.click(row);
                    self.model.layout.focus(pane);
                }
            }
            Act::Fold(index) => {
                if !self.model.projects.is_empty() {
                    let index = index % self.model.projects.len();
                    let name = self.screen.dom.by_class_in(self.group(index), "name")[0];
                    self.click(name);
                    let id = self.model.projects[index].0.clone();
                    if !self.model.folded.remove(&id) {
                        self.model.folded.insert(id);
                    }
                }
            }
            Act::Launch { project, agent } => self.launch(*project, *agent)?,
            Act::CloseProject(index) => {
                if !self.model.projects.is_empty() {
                    let index = index % self.model.projects.len();
                    let close = self.buttons(self.group(index), "Close project");
                    prop_assert_eq!(close.len(), 1);
                    self.click(close[0]);
                    // Its panes close with it, and their sessions end.
                    let (id, _) = self.model.projects.remove(index);
                    let panes: Vec<PaneId> = self
                        .model
                        .layout
                        .contents()
                        .filter(|(_, content)| content.project() == Some(&id))
                        .map(|(pane, _)| pane)
                        .collect();
                    for pane in panes {
                        self.model.close_pane(pane);
                    }
                    self.model.folded.remove(&id);
                }
            }
            Act::Reconnect(index) => {
                let ended: Vec<PaneId> = panes
                    .iter()
                    .copied()
                    .filter(|pane| {
                        self.model.sessions.get(pane).is_some_and(|session| {
                            matches!(session.state, State::Ended | State::Failed)
                        })
                    })
                    .collect();
                if !ended.is_empty() {
                    let pane = ended[index % ended.len()];
                    let notice = self
                        .screen
                        .dom
                        .by_class_in(self.section(pane), "composer-notice");
                    prop_assert_eq!(notice.len(), 1);
                    let button = self
                        .screen
                        .dom
                        .find_in(notice[0], |dom, node| dom.tag(node) == "button");
                    prop_assert_eq!(button.len(), 1);
                    self.click(button[0]);
                    self.model.touch(pane);
                    if let Some((project, agent)) = self.model.agent(pane) {
                        let (project, agent) = (project.clone(), agent.to_owned());
                        self.model.start(pane, &project, &agent);
                    }
                }
            }
            Act::Agent(index, state, title) => {
                let open: Vec<PaneId> = self.model.sessions.keys().copied().collect();
                if !open.is_empty() {
                    let pane = open[index % open.len()];
                    let mut transcript = self.session(pane).transcript();
                    let next = state.transcript(title.as_deref());
                    self.screen.with(|| transcript.set(next));
                    self.screen.render();
                    if let Some(session) = self.model.sessions.get_mut(&pane) {
                        session.state = *state;
                        session.title = title.clone();
                    }
                }
            }
            Act::News(index) => {
                // News of the pane the user is looking at is no news.
                let focused = self.model.layout.focused();
                let away: Vec<PaneId> = self
                    .model
                    .sessions
                    .keys()
                    .copied()
                    .filter(|pane| *pane != focused)
                    .collect();
                if !away.is_empty() {
                    let pane = away[index % away.len()];
                    let mut unread = self.session(pane).unread;
                    self.screen.with(|| unread.set(true));
                    self.screen.render();
                    if let Some(session) = self.model.sessions.get_mut(&pane) {
                        session.unread = true;
                    }
                }
            }
            Act::NewsWhileAway => {
                let focused = self.model.layout.focused();
                if self.model.sessions.contains_key(&focused) {
                    let mut unread = self.session(focused).unread;
                    self.screen.with(|| unread.set(true));
                    self.screen.render();
                    let badge = self
                        .screen
                        .dom
                        .by_class_in(self.section(focused), "unread-badge");
                    prop_assert_eq!(badge.len(), 1);
                    let body = self
                        .screen
                        .dom
                        .by_class_in(self.section(focused), "pane-body");
                    self.click(body[0]);
                }
            }
            Act::ClickField => {
                if self.model.palette.is_some() {
                    let field = self
                        .screen
                        .dom
                        .find_in(self.one("palette"), |dom, node| dom.tag(node) == "input");
                    self.click(field[0]);
                }
            }
            Act::Type(query) => {
                if self.model.palette.is_some() {
                    let field = self
                        .screen
                        .dom
                        .find_in(self.one("palette"), |dom, node| dom.tag(node) == "input");
                    prop_assert_eq!(field.len(), 1);
                    prop_assert!(self.screen.input(field[0], query));
                }
                if let Some(open) = &mut self.model.palette {
                    open.query = query.clone();
                    open.selected = 0;
                }
            }
            Act::Hover(index) => {
                let rows = self.screen.dom.by_class("palette-item");
                if self.model.palette.is_some() && !rows.is_empty() {
                    let index = index % rows.len();
                    prop_assert!(self.screen.mouse_enter(rows[index], None));
                    if let Some(open) = &mut self.model.palette {
                        open.selected = index;
                    }
                }
            }
            Act::ClickItem(index) => {
                if let Some(open) = &self.model.palette {
                    let matches = open.matches();
                    let rows = self.screen.dom.by_class("palette-item");
                    prop_assert_eq!(rows.len(), matches.len());
                    if !rows.is_empty() {
                        let index = index % rows.len();
                        let command = matches[index].command.clone();
                        if command != Command::OpenProject {
                            self.click(rows[index]);
                            self.model.run(command);
                        }
                    }
                }
            }
            Act::Backdrop => {
                if self.model.palette.is_some() {
                    self.click(self.one("palette-backdrop"));
                    self.model.palette = None;
                }
            }
        }
        self.model.look();
        Ok(())
    }

    /// Press a key: into the palette's field while it is open, which is
    /// where the keyboard is then, and to the app otherwise.
    fn press(&mut self, press: &KeyPress) {
        if self.model.asks_for_dialog(press) {
            return;
        }
        if self.model.palette.is_some() {
            let field = self
                .screen
                .dom
                .find_in(self.one("palette"), |dom, node| dom.tag(node) == "input");
            self.screen.key_down(field[0], press);
            self.model.press_in_palette(press);
        } else {
            self.screen.key_down(self.one("shell"), press);
            if let Some(action) = self.model.shortcut(press) {
                self.model.perform(action);
            }
        }
    }

    /// In the picker of the `pane`th empty pane: choose the `project`th
    /// project if it asks which, then the `agent`th of its agents.
    fn pick(&mut self, pane: usize, project: usize, agent: usize) -> Result<(), TestCaseError> {
        let empty: Vec<PaneId> = self
            .model
            .panes()
            .into_iter()
            .filter(|pane| self.model.layout.content(*pane) == Some(&PaneContent::Empty))
            .collect();
        if empty.is_empty() || self.model.projects.is_empty() {
            return Ok(());
        }
        let pane = empty[pane % empty.len()];
        let index = project % self.model.projects.len();
        let (id, (name, agents)) = self.model.projects[index].clone();
        let choose = |run: &Run| {
            run.screen
                .dom
                .by_class_in(run.section(pane), "segmented")
                .first()
                .map(|bar| run.screen.dom.children(*bar))
                .unwrap_or_default()
        };
        // It asks which project only when there are several.
        let offered = choose(self);
        if self.model.projects.len() > 1 {
            let names: Vec<String> = offered
                .iter()
                .map(|button| self.screen.dom.text(*button))
                .collect();
            let all: Vec<String> = self
                .model
                .projects
                .iter()
                .map(|(_, (name, _))| (*name).to_owned())
                .collect();
            prop_assert_eq!(names, all);
            self.click(offered[index]);
            self.model.touch(pane);
            let offered = choose(self);
            prop_assert!(self.screen.dom.has_class(offered[index], "active"));
        } else {
            prop_assert!(offered.is_empty());
            let said = texts(&self.screen.dom, self.section(pane), "muted");
            prop_assert_eq!(said.first(), Some(&format!("in {name}")));
        }
        // Then it offers that project's agents, or says why it cannot.
        let section = self.section(pane);
        let choices = self.screen.dom.by_class_in(section, "choice");
        let names: Vec<String> = choices
            .iter()
            .flat_map(|choice| texts(&self.screen.dom, *choice, "name"))
            .collect();
        let mut expected: Vec<String> = agents
            .iter()
            .flatten()
            .map(|agent| (*agent).to_owned())
            .collect();
        expected.sort_unstable();
        prop_assert_eq!(&names, &expected);
        let problem = texts(&self.screen.dom, section, "config-error");
        prop_assert_eq!(problem, optional(agents.is_none().then_some(BROKEN)));
        if !choices.is_empty() {
            let chosen = agent % choices.len();
            self.click(choices[chosen]);
            self.model.touch(pane);
            self.model.start(pane, &id, &names[chosen]);
        }
        Ok(())
    }

    /// In the sidebar: open the `project`th project's menu of agents and
    /// choose the `agent`th, or close the menu again if it has none.
    fn launch(&mut self, project: usize, agent: usize) -> Result<(), TestCaseError> {
        if self.model.projects.is_empty() {
            return Ok(());
        }
        let index = project % self.model.projects.len();
        let (id, (_, agents)) = self.model.projects[index].clone();
        let menu = |run: &Run| {
            let found = run.buttons(run.group(index), "Open an agent");
            assert_eq!(found.len(), 1);
            found[0]
        };
        self.click(menu(self));
        // The menu shows under the project, so it unfolds.
        self.model.folded.remove(&id);
        let rows = self.screen.dom.by_class_in(self.group(index), "launch-row");
        let names: Vec<String> = rows
            .iter()
            .flat_map(|row| texts(&self.screen.dom, *row, "name"))
            .collect();
        let mut expected: Vec<String> = agents
            .iter()
            .flatten()
            .map(|agent| (*agent).to_owned())
            .collect();
        expected.sort_unstable();
        prop_assert_eq!(&names, &expected);
        if rows.is_empty() {
            self.click(menu(self));
        } else {
            let chosen = agent % rows.len();
            self.click(rows[chosen]);
            self.model.launch(&id, &names[chosen]);
        }
        // Either way the menu is closed again.
        let rows = self.screen.dom.by_class_in(self.group(index), "launch-row");
        prop_assert!(rows.is_empty());
        Ok(())
    }

    fn drag(
        &mut self,
        pane: PaneId,
        button: MouseButton,
        to: &[(f64, f64)],
        release: &Release,
    ) -> Result<(), TestCaseError> {
        let header = self
            .screen
            .dom
            .find_in(self.section(pane), |dom, node| dom.tag(node) == "header");
        prop_assert_eq!(header.len(), 1);
        let title = self.screen.dom.by_class_in(header[0], "title");
        prop_assert_eq!(title.len(), 1);
        prop_assert!(self.screen.mouse_down(title[0], button, FROM));
        // Any button's press focuses the pane; only the primary one holds it.
        self.model.touch(pane);

        // It is a drag once the pointer has been far enough from where it
        // went down, and stays one wherever it goes after.
        let mut moved = false;
        let mut at = FROM;
        for by in to {
            at = (FROM.0 + by.0, FROM.1 + by.1);
            self.screen.mouse_move(self.one("panes"), Some(button), at);
            moved |= button == MouseButton::Primary && by.0.hypot(by.1) >= DRAG_THRESHOLD;
            self.check_dragging(pane, moved)?;
        }
        let to = at;

        let others: Vec<PaneId> = self
            .model
            .panes()
            .into_iter()
            .filter(|other| *other != pane)
            .collect();
        let tabs = self.model.layout.tabs().len();
        match release {
            Release::Zone(target, side, wander) if moved && !others.is_empty() => {
                let target = others[target % others.len()];
                let name = match side {
                    Some(Side::Left) => "left",
                    Some(Side::Right) => "right",
                    Some(Side::Top) => "top",
                    Some(Side::Bottom) => "bottom",
                    None => "center",
                };
                let zone = |run: &Run| -> Result<NodeRef, TestCaseError> {
                    let found = run.screen.dom.find_in(run.section(target), |dom, node| {
                        dom.has_class(node, "drop-zone") && dom.has_class(node, name)
                    });
                    prop_assert_eq!(found.len(), 1);
                    Ok(found[0])
                };
                // The zone under the pointer, and no other, previews where
                // the pane would go.
                let previews = |run: &Run| -> Vec<String> {
                    let dom = &run.screen.dom;
                    dom.find_in(dom.root(), |dom, node| {
                        dom.attribute(node, "class")
                            .is_some_and(|class| class.starts_with("drop-preview"))
                    })
                    .into_iter()
                    .map(|node| attribute(dom, node, "class"))
                    .collect()
                };
                prop_assert_eq!(previews(self), Vec::<String>::new());
                prop_assert!(self.screen.mouse_enter(zone(self)?, Some(button)));
                prop_assert_eq!(previews(self), vec![format!("drop-preview {name}")]);
                if *wander {
                    prop_assert!(self.screen.mouse_leave(zone(self)?, Some(button)));
                    prop_assert_eq!(previews(self), Vec::<String>::new());
                    prop_assert!(self.screen.mouse_enter(zone(self)?, Some(button)));
                    prop_assert_eq!(previews(self), vec![format!("drop-preview {name}")]);
                }
                self.screen.mouse_up(zone(self)?, button, to);
                match side {
                    Some(side) => self.model.layout.move_pane(pane, target, *side),
                    None => self.model.layout.swap_panes(pane, target),
                };
            }
            // No zone to let go on: the pane it was held over, then.
            Release::Zone(..) | Release::Nowhere => {
                self.screen.mouse_up(self.section(pane), button, to);
            }
            Release::Tab(index) => {
                let index = index % tabs;
                self.screen
                    .mouse_up(self.screen.dom.by_class("tab")[index], button, to);
                if moved {
                    self.model.layout.move_to_tab(pane, Some(index));
                }
            }
            Release::NewTab => {
                self.screen.mouse_up(self.one("new-tab"), button, to);
                if moved {
                    self.model.layout.move_to_tab(pane, None);
                }
            }
            Release::Lost => {
                self.screen.mouse_move(self.one("panes"), None, to);
            }
        }
        Ok(())
    }

    /// Drag the boundary after child `index` of the split at `path`.
    ///
    /// Pressing a divider measures the split on screen to know where the
    /// two children lie, which nothing here can answer. So the drag starts
    /// as that press would leave it, with the children lying from
    /// [`SPAN`].0 for [`SPAN`].1 pixels; the moves and the release are the
    /// user's.
    fn resize(&mut self, path: &[usize], index: usize, share: f64) -> Result<(), TestCaseError> {
        let mut node = &self.model.layout.active_tab().root;
        for step in path {
            let Node::Split { children, .. } = node else {
                panic!("no split at {path:?}");
            };
            node = &children[*step];
        }
        let Node::Split { direction, .. } = node else {
            panic!("no split at {path:?}");
        };
        let direction = *direction;
        let mut drag = self.app.drag;
        self.screen.with(|| {
            drag.set(Some(Drag::Boundary {
                path: path.to_vec(),
                index,
                direction,
                start: SPAN.0,
                length: SPAN.1,
            }));
        });
        self.screen.render();
        let dom = &self.screen.dom;
        let overlay = match direction {
            Direction::Horizontal => "drag-overlay horizontal",
            Direction::Vertical => "drag-overlay vertical",
        };
        prop_assert_eq!(
            attribute(dom, self.one("workbench"), "class"),
            "workbench dragging".to_owned()
        );
        prop_assert_eq!(
            attribute(dom, self.one("drag-overlay"), "class"),
            overlay.to_owned()
        );

        // Only the coordinate along the split counts; the other is anywhere.
        let along = SPAN.0 + share * SPAN.1;
        let at = match direction {
            Direction::Horizontal => (along, 7.0),
            Direction::Vertical => (7.0, along),
        };
        self.screen
            .mouse_move(self.one("drag-overlay"), Some(MouseButton::Primary), at);
        self.model.layout.resize(path, index, share as f32);
        let layout = self.screen.with(|| self.app.layout.peek().clone());
        prop_assert_eq!(&layout, &self.model.layout);
        self.screen
            .mouse_up(self.one("drag-overlay"), MouseButton::Primary, at);
        Ok(())
    }

    /// While `pane` is held: every other pane of the tab offers itself as a
    /// target, and only once the pointer `moved` far enough to mean a drag.
    fn check_dragging(&self, pane: PaneId, moved: bool) -> Result<(), TestCaseError> {
        let dom = &self.screen.dom;
        let targets = if moved {
            self.model.panes().len() - 1
        } else {
            0
        };
        prop_assert_eq!(dom.by_class("drop-zones").len(), targets);
        prop_assert_eq!(dom.by_class("drop-zone").len(), targets * 5);
        prop_assert_eq!(dom.has_class(self.section(pane), "moving"), moved);
        prop_assert_eq!(dom.has_class(self.one("workbench"), "moving"), moved);
        // The layout is the user's until they let go.
        let layout = self.screen.with(|| self.app.layout.peek().clone());
        prop_assert_eq!(&layout, &self.model.layout);
        Ok(())
    }

    // ---- what should be on screen ------------------------------------------

    fn project_name(&self, id: &ProjectId) -> String {
        self.model
            .project(id)
            .map_or_else(|| id.to_string(), |(name, _)| (*name).to_owned())
    }

    /// What a pane's header should say.
    fn expected_header(&self, pane: PaneId) -> Header {
        let session = self.model.sessions.get(&pane);
        let reads = session.map(|session| session.state.reads_as());
        match self.model.agent(pane) {
            Some((project, agent)) => Header {
                project: some(self.project_name(project)),
                title: some(agent),
                session: optional(session.and_then(|session| session.title.as_deref())),
                dot: optional(reads.map(|(dot, _)| format!("dot {dot}")).as_deref()),
                status: optional(reads.map(|(dot, _)| format!("status {dot}")).as_deref()),
                words: optional(reads.map(|(_, words)| words)),
                unread: session.is_some_and(|session| session.unread),
            },
            None => Header {
                project: Vec::new(),
                title: some("New pane"),
                session: Vec::new(),
                dot: Vec::new(),
                status: Vec::new(),
                words: Vec::new(),
                unread: false,
            },
        }
    }

    fn shown_header(&self, pane: PaneId) -> Header {
        let dom = &self.screen.dom;
        let header = dom.find_in(self.section(pane), |dom, node| dom.tag(node) == "header")[0];
        Header {
            project: texts(dom, header, "crumb"),
            title: texts(dom, header, "title"),
            session: texts(dom, header, "session-title"),
            dot: classes(dom, header, "dot"),
            status: classes(dom, header, "status"),
            words: texts(dom, header, "status"),
            unread: !dom.by_class_in(header, "unread-badge").is_empty(),
        }
    }

    /// What each tab should say: it is named for its first pane, by what
    /// that pane's session is about if anything; it shows the state of its
    /// most pressing session; and it is marked when a pane of it has news,
    /// unless it is the tab the user is on.
    fn expected_tabs(&self) -> Vec<TabLook> {
        let layout = &self.model.layout;
        layout
            .tabs()
            .iter()
            .enumerate()
            .map(|(index, tab)| {
                let panes = tab.root.panes();
                let sessions: Vec<&Session> = panes
                    .iter()
                    .filter_map(|pane| self.model.sessions.get(pane))
                    .collect();
                let first = panes[0];
                let name = self
                    .model
                    .sessions
                    .get(&first)
                    .and_then(|session| session.title.clone())
                    .or_else(|| self.model.agent(first).map(|(_, agent)| agent.to_owned()))
                    .unwrap_or_else(|| "New pane".to_owned());
                let pressing = sessions
                    .iter()
                    .map(|session| session.state)
                    .min_by_key(|s| s.urgency());
                TabLook {
                    class: if index == layout.active() {
                        "tab active".to_owned()
                    } else if sessions.iter().any(|session| session.unread) {
                        "tab unread".to_owned()
                    } else {
                        "tab".to_owned()
                    },
                    name: some(name),
                    dot: optional(
                        pressing
                            .map(|state| format!("dot {}", state.reads_as().0))
                            .as_deref(),
                    ),
                    count: optional(
                        (panes.len() > 1)
                            .then(|| panes.len().to_string())
                            .as_deref(),
                    ),
                }
            })
            .collect()
    }

    fn shown_tabs(&self) -> Vec<TabLook> {
        let dom = &self.screen.dom;
        dom.by_class("tab")
            .into_iter()
            .map(|tab| TabLook {
                class: attribute(dom, tab, "class"),
                name: texts(dom, tab, "name"),
                dot: classes(dom, tab, "dot"),
                count: texts(dom, tab, "count"),
            })
            .collect()
    }

    /// What the sidebar should say of each project: its name, how many
    /// agents it has open, and, unless it is folded, what is wrong with
    /// its configuration and a row for each of those agents, in the order
    /// their panes are shown, tab after tab.
    fn expected_sidebar(&self) -> Vec<ProjectLook> {
        let layout = &self.model.layout;
        self.model
            .projects
            .iter()
            .map(|(id, (name, agents))| {
                let mut rows = Vec::new();
                for (tab, panes) in layout.tabs().iter().enumerate() {
                    for pane in panes.root.panes() {
                        let Some((project, agent)) = self.model.agent(pane) else {
                            continue;
                        };
                        if project != id {
                            continue;
                        }
                        let session = self.model.sessions.get(&pane);
                        let unread = session.is_some_and(|session| session.unread);
                        let (dot, words) = session.map_or(("idle", "not connected"), |session| {
                            session.state.reads_as()
                        });
                        let class = if layout.focused() == pane {
                            "row agent-row current focused"
                        } else {
                            match (layout.active() == tab, unread) {
                                (true, true) => "row agent-row current unread",
                                (true, false) => "row agent-row current",
                                (false, true) => "row agent-row unread",
                                (false, false) => "row agent-row",
                            }
                        };
                        rows.push(RowLook {
                            pane: pane.to_string(),
                            class: class.to_owned(),
                            name: some(agent),
                            session: optional(session.and_then(|s| s.title.as_deref())),
                            dot: some(format!("dot {dot}")),
                            words: some(words),
                            tab: optional(
                                (layout.tabs().len() > 1)
                                    .then(|| (tab + 1).to_string())
                                    .as_deref(),
                            ),
                        });
                    }
                }
                let open = !self.model.folded.contains(id);
                ProjectLook {
                    name: some(*name),
                    count: optional(
                        (!rows.is_empty())
                            .then(|| rows.len().to_string())
                            .as_deref(),
                    ),
                    problem: optional((open && agents.is_none()).then_some(BROKEN)),
                    rows: if open { rows } else { Vec::new() },
                }
            })
            .collect()
    }

    fn shown_sidebar(&self) -> Vec<ProjectLook> {
        let dom = &self.screen.dom;
        dom.by_class("project-group")
            .into_iter()
            .map(|group| {
                let head = dom.by_class_in(group, "project-row")[0];
                ProjectLook {
                    name: texts(dom, head, "name"),
                    count: texts(dom, head, "meta"),
                    problem: texts(dom, group, "config-error"),
                    rows: dom
                        .by_class_in(group, "agent-row")
                        .into_iter()
                        .map(|row| RowLook {
                            pane: attribute(dom, row, "data-pane"),
                            class: attribute(dom, row, "class"),
                            name: texts(dom, row, "name"),
                            session: texts(dom, row, "session-title"),
                            dot: classes(dom, row, "dot"),
                            words: texts(dom, row, "meta"),
                            tab: texts(dom, row, "tab-number"),
                        })
                        .collect(),
                }
            })
            .collect()
    }

    /// The palette's rows as (kind, name, detail, whether selected), and
    /// what its field holds; `None` while it is closed.
    fn shown_palette(&self) -> Option<PaletteLook> {
        let dom = &self.screen.dom;
        let palette = *dom.by_class("palette").first()?;
        let field = dom.find_in(palette, |dom, node| dom.tag(node) == "input")[0];
        let rows = dom
            .by_class_in(palette, "palette-item")
            .into_iter()
            .map(|row| {
                (
                    texts(dom, row, "kind").concat(),
                    texts(dom, row, "label").concat(),
                    texts(dom, row, "detail").concat(),
                    dom.has_class(row, "selected"),
                )
            })
            .collect();
        Some((attribute(dom, field, "value"), rows))
    }

    fn expected_palette(&self) -> Option<PaletteLook> {
        let open = self.model.palette.as_ref()?;
        let current = open.current();
        let rows = open
            .matches()
            .into_iter()
            .enumerate()
            .map(|(index, item)| {
                (
                    item.kind.to_owned(),
                    item.label,
                    item.detail,
                    index == current,
                )
            })
            .collect();
        Some((open.query.clone(), rows))
    }

    /// The app holds the state the model came to, and shows it.
    fn check(&self) -> Result<(), TestCaseError> {
        let (layout, palette, drag, sessions, projects) = self.screen.with(|| {
            let sessions: BTreeSet<PaneId> = self.app.sessions.peek().keys().copied().collect();
            let projects: Vec<ProjectId> = self
                .app
                .projects
                .peek()
                .iter()
                .map(|project| project.id.clone())
                .collect();
            (
                self.app.layout.peek().clone(),
                *self.app.palette.peek(),
                self.app.drag.peek().clone(),
                sessions,
                projects,
            )
        });
        prop_assert_eq!(&layout, &self.model.layout);
        let checked = layout.check();
        prop_assert!(checked.is_ok(), "{:?}", checked);
        prop_assert_eq!(palette, self.model.palette.is_some());
        // No gesture leaves a drag behind.
        prop_assert_eq!(drag, None);
        // A session lives exactly as long as its pane.
        let expected: BTreeSet<PaneId> = self.model.sessions.keys().copied().collect();
        prop_assert_eq!(sessions, expected);
        let expected: Vec<ProjectId> = self
            .model
            .projects
            .iter()
            .map(|(id, _)| id.clone())
            .collect();
        prop_assert_eq!(projects, expected);

        let dom = &self.screen.dom;
        prop_assert_eq!(
            attribute(dom, self.one("workbench"), "class"),
            "workbench".to_owned()
        );
        prop_assert!(dom.by_class("drop-zones").is_empty());

        prop_assert_eq!(self.shown_tabs(), self.expected_tabs());
        let shown = shown_shape(dom, only_child(dom, self.one("panes"))?)?;
        prop_assert_eq!(shown, expected_shape(&layout, &layout.active_tab().root));
        for pane in self.model.panes() {
            prop_assert_eq!(
                self.shown_header(pane),
                self.expected_header(pane),
                "pane {}",
                pane
            );
        }
        prop_assert_eq!(self.shown_sidebar(), self.expected_sidebar());
        prop_assert_eq!(self.shown_palette(), self.expected_palette());

        // What the screen came to through every change is what it shows
        // when it starts from the same state. A pane's body keeps what the
        // user did in it (the project its picker is on, a draft), which a
        // fresh start has not; so does the palette.
        let fresh = self.fresh();
        let chrome = |dom: &Dom| {
            dom.html_without(|dom, node| {
                dom.has_class(node, "pane-body")
                    || dom.has_class(node, "composer")
                    || dom.has_class(node, "palette-backdrop")
            })
        };
        prop_assert_eq!(chrome(dom), chrome(&fresh.dom));
        Ok(())
    }

    /// An app started in the state the model is in.
    fn fresh(&self) -> Screen {
        let mut projects = Projects::default();
        let mut configs = HashMap::new();
        let mut folds = Folds::default();
        // Ids go by the order projects were opened in, and closed ones
        // leave gaps; opening each in turn up to the last makes the same.
        let last = self
            .model
            .projects
            .iter()
            .filter_map(|(id, _)| id.to_string().trim_start_matches('p').parse::<usize>().ok())
            .max()
            .unwrap_or(0);
        for name in PROJECTS.iter().take(last) {
            let id = projects.add(&root_of(name));
            match self.model.project(&id) {
                Some((_, agents)) => {
                    configs.insert(
                        id.clone(),
                        match agents {
                            Some(agents) => Ok(config(agents)),
                            None => Err(BROKEN.to_owned()),
                        },
                    );
                    if self.model.folded.contains(&id) {
                        folds.toggle(Fold::new(&id, TreeNode::Project));
                    }
                }
                None => {
                    projects.remove(&id);
                }
            }
        }
        let mut screen = Screen::new(VirtualDom::new(Bench).with_root_context(Seed {
            layout: self.model.layout.clone(),
            platform: self.platform,
            projects,
            configs,
            folds,
        }));
        let mut sessions = screen.with(consume_context::<AppSignals>).sessions;
        screen.with(|| {
            for (pane, session) in &self.model.sessions {
                let transcript = session.state.transcript(session.title.as_deref());
                sessions
                    .write()
                    .insert(*pane, AgentSession::detached(transcript, session.unread));
            }
        });
        screen.render();
        screen
    }
}

fn platform() -> impl Strategy<Value = Platform> {
    prop_oneof![Just(Platform::Mac), Just(Platform::Other)]
}

/// A platform, the projects open on it, and what a user and their agents
/// do there.
fn session() -> impl Strategy<Value = (Platform, Vec<ProjectSpec>, Vec<Act>)> {
    platform().prop_flat_map(|platform| {
        (
            Just(platform),
            world(),
            prop::collection::vec(burst(platform), 0..40).prop_map(|bursts| bursts.concat()),
        )
    })
}

#[test]
fn starts_with_one_empty_focused_pane() -> Result<(), TestCaseError> {
    let run = Run::new(Platform::Other, &[]);
    run.check()?;
    let dom = &run.screen.dom;
    assert_eq!(dom.by_class("tab").len(), 1);
    let panes = dom.by_class("pane");
    assert_eq!(panes.len(), 1);
    assert!(dom.has_class(panes[0], "focused"));
    assert!(dom.text(panes[0]).contains("Open an agent"));
    Ok(())
}

proptest! {
    #[test]
    fn the_screen_shows_what_the_user_and_agents_made_of_it(
        (platform, world, acts) in session(),
    ) {
        let mut run = Run::new(platform, &world);
        run.check()?;
        for act in &acts {
            run.perform(act)?;
            run.check()?;
        }
    }
}
