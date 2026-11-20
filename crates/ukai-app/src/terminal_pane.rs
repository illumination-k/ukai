//! The terminal pane: the user's shell in a workspace's checkout — the
//! project root on this machine, or a gateway agent's in its pod
//! ([`ukai_acp::terminal`]) — drawn by xterm.js in the app's web view.
//!
//! The shell belongs to the pane, not to its view: switching tabs builds the
//! view again, and [`Terminals`] keeps the shell running and what it printed
//! ([`Backlog`]) to play to the next view. Closing the pane, or showing
//! something else in it, hangs up on the shell. After a restart of the app
//! the pane starts a new shell in the same place.
//!
//! xterm.js only ever writes the shell's bytes into its cells, so what a
//! program prints cannot become markup in the web view that holds the app's
//! IPC.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use dioxus::core::spawn_forever;
use dioxus::prelude::*;
use tokio::sync::mpsc;
use ukai_acp::terminal::{Size, Terminal, TerminalEvent, TerminalPlace};
use ukai_acp::{PaneWorkspace, WorkspaceScope};
use ukai_core::terminal::Backlog;
use ukai_core::{Layout, PaneContent, PaneId, ProjectId, ReportSource};

use crate::layout::launch_target;
use crate::projects::{AppSignals, project_context};

const SCRIPT: &str = include_str!("terminal_pane.js");

/// Every terminal pane's shell, by pane.
pub type Terminals = Signal<HashMap<PaneId, Shell>>;

/// A pane's shell.
#[derive(Clone)]
pub struct Shell {
    /// Where it runs; a pane showing another place gets a shell of its own.
    place: (ProjectId, ReportSource),
    /// `None` once the shell has ended.
    terminal: Option<Terminal>,
    screen: Arc<Mutex<Screen>>,
    pub state: ShellState,
}

impl PartialEq for Shell {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.screen, &other.screen) && self.state == other.state
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShellState {
    Running,
    Exited(Option<i32>),
    Failed(String),
}

/// What a shell printed, and the view it goes to now, if one shows it.
#[derive(Default)]
struct Screen {
    backlog: Backlog,
    view: Option<mpsc::UnboundedSender<Vec<u8>>>,
    size: Size,
}

fn lock(screen: &Mutex<Screen>) -> std::sync::MutexGuard<'_, Screen> {
    screen
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Open a terminal in `place` of `project` where [`launch_target`] puts it.
/// Each call opens another: a workspace can have many shells.
pub fn open_terminal(app: AppSignals, project: &ProjectId, place: &ReportSource) {
    if let Some(pane) = launch_target(app) {
        open_terminal_in(app, pane, project, place);
    }
}

/// Show a terminal in `place` of `project` in `pane`.
pub fn open_terminal_in(app: AppSignals, pane: PaneId, project: &ProjectId, place: &ReportSource) {
    let mut layout = app.layout;
    let mut layout = layout.write();
    layout.set_content(
        pane,
        PaneContent::Terminal {
            project: project.clone(),
            place: place.clone(),
        },
    );
    layout.focus(pane);
    let mut projects = app.projects;
    projects.write().select(project);
}

/// Where `place` of `project` runs a shell.
fn terminal_place(
    app: AppSignals,
    project: &ProjectId,
    place: &ReportSource,
) -> Result<TerminalPlace, String> {
    let context = project_context(&app.projects.peek(), &app.configs.peek(), project)?;
    let ReportSource::Agent { agent, workspace } = place else {
        return Ok(TerminalPlace::Local(context.root));
    };
    let spec = context
        .config
        .agent(agent)
        .ok_or_else(|| format!("agent `{agent}` is not declared anymore"))?;
    // A place names the shared workspace by leaving the key out.
    let workspace = workspace.clone().unwrap_or(PaneWorkspace {
        key: String::new(),
        scope: WorkspaceScope::Shared,
    });
    TerminalPlace::resolve(
        &spec,
        ukai_acp::system_lookup,
        Some(&workspace),
        &context.root,
    )
    .map_err(|error| error.to_string())?
    .ok_or_else(|| format!("agent `{agent}` names no gateway agent"))
}

/// Start `pane`'s shell, `size` large, in place of any it had: the view the
/// old one went to, if any, is reset and goes on with the new one.
fn start(app: AppSignals, pane: PaneId, project: &ProjectId, place: &ReportSource, size: Size) {
    let mut terminals = app.terminals;
    let view = terminals
        .peek()
        .get(&pane)
        .and_then(|old| lock(&old.screen).view.take());
    if let Some(view) = &view {
        // ESC c: a full reset, so the new shell starts on a clean screen.
        let _ = view.send(b"\x1bc".to_vec());
    }
    let screen = Arc::new(Mutex::new(Screen {
        view,
        size,
        ..Screen::default()
    }));
    let key = (project.clone(), place.clone());
    let (terminal, state) = match terminal_place(app, project, place) {
        Ok(place) => {
            let (terminal, events) = ukai_acp::terminal::open(place, size);
            spawn_forever(follow(terminals, pane, screen.clone(), events));
            (Some(terminal), ShellState::Running)
        }
        Err(problem) => (None, ShellState::Failed(problem)),
    };
    terminals.write().insert(
        pane,
        Shell {
            place: key,
            terminal,
            screen,
            state,
        },
    );
}

/// Keep what the shell prints, pass it to the view, and note how it ends.
async fn follow(
    mut terminals: Terminals,
    pane: PaneId,
    screen: Arc<Mutex<Screen>>,
    mut events: mpsc::UnboundedReceiver<TerminalEvent>,
) {
    while let Some(event) = events.recv().await {
        let state = match event {
            TerminalEvent::Output(bytes) => {
                let mut screen = lock(&screen);
                screen.backlog.push(&bytes);
                if let Some(view) = &screen.view {
                    let _ = view.send(bytes);
                }
                continue;
            }
            TerminalEvent::Exited(code) => ShellState::Exited(code),
            TerminalEvent::Failed(why) => ShellState::Failed(why),
        };
        // Only if the pane still has this shell, not one started since.
        let mut terminals = terminals.write();
        if let Some(shell) = terminals.get_mut(&pane)
            && Arc::ptr_eq(&shell.screen, &screen)
        {
            shell.state = state;
            shell.terminal = None;
        }
        return;
    }
}

/// End the shells of panes that no longer show them: closed, or showing
/// something else now.
pub fn use_terminal_cleanup(app: AppSignals) {
    use_effect(move || {
        let layout = app.layout.read();
        let mut terminals = app.terminals;
        let stale = |pane: &PaneId, shell: &Shell| !shows(&layout, *pane, &shell.place);
        if terminals
            .peek()
            .iter()
            .any(|(pane, shell)| stale(pane, shell))
        {
            terminals.write().retain(|pane, shell| !stale(pane, shell));
        }
    });
}

fn shows(layout: &Layout, pane: PaneId, (project, place): &(ProjectId, ReportSource)) -> bool {
    matches!(
        layout.content(pane),
        Some(PaneContent::Terminal { project: p, place: q }) if p == project && q == place
    )
}

/// What the view sends.
#[derive(Debug, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum FromView {
    Resize {
        cols: u16,
        rows: u16,
    },
    /// Typed text.
    Input {
        data: String,
    },
    /// Bytes that are not text (some mouse reports), one per char.
    Binary {
        data: String,
    },
}

#[derive(serde::Serialize)]
#[serde(untagged)]
enum ToView {
    Data { data: String },
    Focus { focus: bool },
}

#[component]
pub fn TerminalPane(pane: PaneId, project: ProjectId, place: ReportSource) -> Element {
    let app = use_context::<AppSignals>();
    // An id of this view's own: the view a split or move builds replaces
    // this one, and each disposes only its own xterm.js.
    let id = use_hook(|| {
        static VIEWS: AtomicU64 = AtomicU64::new(0);
        format!("terminal-{pane}-{}", VIEWS.fetch_add(1, Ordering::Relaxed))
    });
    let eval = use_hook({
        let id = id.clone();
        move || document::eval(&SCRIPT.replace("__ID__", &id))
    });
    use_future({
        let project = project.clone();
        let place = place.clone();
        move || {
            let project = project.clone();
            let place = place.clone();
            run_view(app, pane, project, place, eval)
        }
    });
    use_drop({
        let id = id.clone();
        move || {
            document::eval(&format!("window.ukaiTerminals?.['{id}']?.dispose()"));
        }
    });
    let focused = app.layout.read().focused() == pane;
    use_effect(use_reactive!(|focused| {
        if focused {
            let _ = eval.send(ToView::Focus { focus: true });
        }
    }));
    let state = app
        .terminals
        .read()
        .get(&pane)
        .map(|shell| shell.state.clone());
    let ended = match &state {
        Some(ShellState::Exited(Some(code))) => Some(format!("The shell exited with {code}.")),
        Some(ShellState::Exited(None)) => Some("The shell exited.".to_owned()),
        Some(ShellState::Failed(why)) => Some(why.clone()),
        Some(ShellState::Running) | None => None,
    };
    rsx! {
        div { class: "pane-body terminal-pane",
            if let Some(ended) = ended {
                div { class: "terminal-ended",
                    span { "{ended}" }
                    button {
                        class: "outline",
                        onclick: move |_| {
                            let size = app
                                .terminals
                                .peek()
                                .get(&pane)
                                .map(|shell| lock(&shell.screen).size)
                                .unwrap_or_default();
                            start(app, pane, &project, &place, size);
                        },
                        "Restart"
                    }
                }
            }
            div {
                id: "{id}",
                class: "terminal-host",
                // The keys of a focused terminal are the shell's: only those
                // with ⌘, which a terminal does not use, reach the app's
                // shortcuts. Elsewhere `mod` is ctrl, which the shell needs.
                onkeydown: move |event: KeyboardEvent| {
                    if !event.modifiers().meta() {
                        event.stop_propagation();
                    }
                },
            }
        }
    }
}

/// Carry keys and sizes from the view to the shell, and the shell's output
/// to the view, starting the shell at the view's size if it has none.
async fn run_view(
    app: AppSignals,
    pane: PaneId,
    project: ProjectId,
    place: ReportSource,
    eval: document::Eval,
) {
    let (view_tx, mut view_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let mut reading = eval;
    let from_view = async move {
        let mut attached = false;
        while let Ok(message) = reading.recv::<FromView>().await {
            let terminal = || {
                app.terminals
                    .peek()
                    .get(&pane)
                    .and_then(|shell| shell.terminal.clone())
            };
            match message {
                FromView::Resize { cols, rows } => {
                    let size = Size { cols, rows }.clamped();
                    let key = (project.clone(), place.clone());
                    let has_shell = app
                        .terminals
                        .peek()
                        .get(&pane)
                        .is_some_and(|shell| shell.place == key);
                    if !has_shell {
                        start(app, pane, &project, &place, size);
                    }
                    if let Some(shell) = app.terminals.peek().get(&pane) {
                        let mut screen = lock(&shell.screen);
                        screen.size = size;
                        if !attached {
                            // What the shell printed so far, then the rest
                            // as it comes: under the lock, so nothing falls
                            // between the two.
                            attached = true;
                            if !screen.backlog.bytes().is_empty() {
                                let _ = view_tx.send(screen.backlog.bytes().to_vec());
                            }
                            screen.view = Some(view_tx.clone());
                        }
                    }
                    if let Some(terminal) = terminal() {
                        terminal.resize(size);
                    }
                }
                FromView::Input { data } => {
                    if let Some(terminal) = terminal() {
                        terminal.write(data.into_bytes());
                    }
                }
                FromView::Binary { data } => {
                    if let Some(terminal) = terminal() {
                        // Each char is one byte, U+0000 to U+00FF.
                        let bytes = data
                            .chars()
                            .map(|c| u8::try_from(u32::from(c)).unwrap_or(b'?'))
                            .collect();
                        terminal.write(bytes);
                    }
                }
            }
        }
    };
    let to_view = async move {
        while let Some(bytes) = view_rx.recv().await {
            let data = base64::engine::general_purpose::STANDARD.encode(bytes);
            if eval.send(ToView::Data { data }).is_err() {
                break;
            }
        }
    };
    futures::future::join(from_view, to_view).await;
}
