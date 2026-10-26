//! Tabs and pane trees the user arranges at runtime, rendered from
//! [`ukai_core::Layout`]. Each pane belongs to a project, so one tab can show
//! agents of several projects side by side.

use std::path::PathBuf;
use std::rc::Rc;

use dioxus::html::input_data::MouseButton;
use dioxus::prelude::*;
use std::collections::HashMap;
use ukai_acp::{PaneWorkspace, WorkspaceScope};
use ukai_config::AgentSpec;
use ukai_core::keymap::Action;
use ukai_core::{
    Direction, Layout, Node, PaneContent, PaneId, ProjectId, Projects, ReportSource, Side,
    boundary_span,
};

use crate::agent_pane::{AgentView, HistoryMenu, Sessions, agent_status, connect};
use crate::alert::{is_unread, mark_read};
use crate::browser_pane::BrowserView;
use crate::diff_pane::DiffPane;
use crate::icons::{Icon, Svg};
use crate::knowledge::NoteView;
use crate::ports::{PortPrompt, open_browser};
use crate::projects::AppSignals;
use crate::report_pane::{ReportView, pick_report};
use crate::schedules::{ScheduleView, schedule_label};
use crate::shortcuts::Shortcuts;

/// The width of the divider between two children of a split, in CSS pixels;
/// matches `.divider` in `main.css`.
const DIVIDER: f64 = 1.0;

/// How far the pointer goes with a pane header held before it is a drag
/// rather than a click, in CSS pixels.
const DRAG_THRESHOLD: f64 = 4.0;

/// Something the user is dragging in the pane area.
#[derive(Debug, Clone, PartialEq)]
pub enum Drag {
    /// The boundary between children `index` and `index + 1` of the split at
    /// `path` in the active tab, which lie from `start` for `length` CSS
    /// pixels along `direction` in window coordinates.
    Boundary {
        path: Vec<usize>,
        index: usize,
        direction: Direction,
        start: f64,
        length: f64,
    },
    /// A pane held by its header, pressed at `from`; `moved` once the
    /// pointer went far enough to be a drag, when drop targets show.
    Pane {
        pane: PaneId,
        from: (f64, f64),
        moved: bool,
    },
}

impl Drag {
    /// Whether the drag draws over the panes, so their native views hide.
    pub fn covers(&self) -> bool {
        match self {
            Drag::Boundary { .. } => true,
            Drag::Pane { moved, .. } => *moved,
        }
    }

    /// The pane being moved, once it is a drag.
    fn moving(&self) -> Option<PaneId> {
        match self {
            Drag::Pane {
                pane, moved: true, ..
            } => Some(*pane),
            _ => None,
        }
    }
}

/// Follow the pointer of a drag: move the boundary being dragged, or turn a
/// held pane header into a drag once it has moved far enough. A pointer
/// released outside the window ends the drag at the next move.
fn drag_move(app: AppSignals, event: &MouseEvent) {
    let mut drag = app.drag;
    let Some(current) = drag.peek().clone() else {
        return;
    };
    if event.held_buttons().is_empty() {
        drag.set(None);
        return;
    }
    let point = event.client_coordinates();
    match current {
        Drag::Boundary {
            path,
            index,
            direction,
            start,
            length,
        } => {
            let at = match direction {
                Direction::Horizontal => point.x,
                Direction::Vertical => point.y,
            };
            if length > 0.0 {
                let share = ((at - start) / length) as f32;
                let mut layout = app.layout;
                layout.write().resize(&path, index, share);
            }
        }
        Drag::Pane {
            pane,
            from,
            moved: false,
        } => {
            if (point.x - from.0).hypot(point.y - from.1) >= DRAG_THRESHOLD {
                drag.set(Some(Drag::Pane {
                    pane,
                    from,
                    moved: true,
                }));
            }
        }
        Drag::Pane { .. } => {}
    }
}

/// Open `agent` of `project` in `pane` and start its session. A gateway
/// agent gets a workspace of the pane's own: `scope`, else the agent's default.
pub fn open_agent(
    app: AppSignals,
    pane: PaneId,
    project: &ProjectId,
    agent: &str,
    scope: Option<WorkspaceScope>,
) {
    let default_scope = match app.configs.peek().get(project) {
        Some(Ok(config)) => match config.agents.get(agent) {
            Some(AgentSpec::Gateway { workspace, .. }) => Some(*workspace),
            _ => None,
        },
        _ => None,
    };
    let workspace = default_scope.map(|default_scope| PaneWorkspace {
        key: uuid::Uuid::new_v4().simple().to_string(),
        scope: scope.unwrap_or(default_scope),
    });
    open_agent_in(app, pane, project, agent, workspace);
}

/// Open `agent` of `project` in `pane`, working in `workspace` (`None` for
/// an agent that is not a gateway's), and start its session. Another pane
/// with the same workspace shares its pod.
pub fn open_agent_in(
    app: AppSignals,
    pane: PaneId,
    project: &ProjectId,
    agent: &str,
    workspace: Option<PaneWorkspace>,
) {
    let AppSignals {
        mut layout,
        mut projects,
        ..
    } = app;
    layout.write().set_content(
        pane,
        PaneContent::Agent {
            project: project.clone(),
            agent: agent.to_owned(),
            session: None,
            workspace,
        },
    );
    projects.write().select(project);
    connect(app, pane, false);
}

/// Where something launched from outside the pane tree opens: the focused
/// pane when it is empty, otherwise a new pane split to its right.
pub fn launch_target(app: AppSignals) -> Option<PaneId> {
    let mut layout = app.layout;
    let focused = layout.peek().focused();
    if layout.peek().content(focused) == Some(&PaneContent::Empty) {
        Some(focused)
    } else {
        layout.write().split(focused, Direction::Horizontal)
    }
}

/// Open `agent` of `project` where [`launch_target`] puts it, a gateway
/// agent in a workspace of `scope`, else of its default.
pub fn launch_agent(
    app: AppSignals,
    project: &ProjectId,
    agent: &str,
    scope: Option<WorkspaceScope>,
) {
    if let Some(pane) = launch_target(app) {
        open_agent(app, pane, project, agent, scope);
    }
}

/// Show the report at `path` in `pane`: a file of `project` on this machine,
/// or one of a gateway agent's workspace, depending on `source`.
pub fn open_report(
    app: AppSignals,
    pane: PaneId,
    project: &ProjectId,
    path: PathBuf,
    source: ReportSource,
) {
    let AppSignals {
        mut layout,
        mut projects,
        ..
    } = app;
    layout.write().set_content(
        pane,
        PaneContent::Report {
            project: project.clone(),
            path,
            source,
        },
    );
    layout.write().focus(pane);
    projects.write().select(project);
}

/// Close `pane` and end its session.
pub fn close_pane(app: AppSignals, pane: PaneId) {
    let AppSignals {
        mut layout,
        mut sessions,
        ..
    } = app;
    layout.write().close_pane(pane);
    sessions.write().remove(&pane);
}

fn close_tab(app: AppSignals, index: usize) {
    let AppSignals {
        mut layout,
        mut sessions,
        ..
    } = app;
    let removed = layout.write().close_tab(index);
    let mut sessions = sessions.write();
    for pane in removed {
        sessions.remove(&pane);
    }
}

/// A pane's title: its project's name and what it shows.
pub fn pane_label(
    layout: &Layout,
    projects: &Projects,
    schedules: &HashMap<String, String>,
    pane: PaneId,
) -> (Option<String>, String) {
    let project_name = |id: &ProjectId| {
        projects
            .get(id)
            .map_or_else(|| id.to_string(), |p| p.name())
    };
    match layout.content(pane) {
        Some(PaneContent::Agent { project, agent, .. }) => {
            (Some(project_name(project)), agent.clone())
        }
        Some(PaneContent::Report { project, path, .. }) => {
            let file = path.file_name().map_or_else(
                || path.display().to_string(),
                |name| name.to_string_lossy().into_owned(),
            );
            (Some(project_name(project)), file)
        }
        Some(PaneContent::Browser {
            project,
            port,
            path,
            source,
        }) => (
            Some(project_name(project)),
            ukai_core::browser::label(source.agent().unwrap_or("localhost"), *port, path),
        ),
        Some(PaneContent::Schedule {
            project, schedule, ..
        }) => (
            Some(project_name(project)),
            schedule_label(schedules, schedule.as_ref()),
        ),
        Some(PaneContent::Note { project, note, .. }) => (
            Some(project_name(project)),
            note.clone().unwrap_or_else(|| "New note".to_owned()),
        ),
        Some(PaneContent::Diff { project, place }) => (
            Some(project_name(project)),
            match place.agent() {
                Some(agent) => format!("Changes · {agent}"),
                None => "Changes".to_owned(),
            },
        ),
        Some(PaneContent::Empty) | None => (None, "New pane".to_owned()),
    }
}

/// The state a tab shows: that of its most pressing agent pane.
fn tab_state(sessions: Sessions, panes: &[PaneId]) -> Option<&'static str> {
    const ORDER: [&str; 5] = ["attention", "closed", "busy", "ready", "idle"];
    panes
        .iter()
        .filter(|pane| sessions.peek().contains_key(pane))
        .map(|pane| agent_status(sessions, *pane).0)
        .min_by_key(|state| ORDER.iter().position(|s| s == state))
}

#[component]
pub fn Workbench() -> Element {
    let app = use_context::<AppSignals>();
    let mut drag = app.drag;
    let root = app.layout.read().active_tab().root.clone();
    let (class, overlay) = match &*drag.read() {
        None => ("workbench", None),
        Some(Drag::Boundary { direction, .. }) => (
            "workbench dragging",
            Some(match direction {
                Direction::Horizontal => "drag-overlay horizontal",
                Direction::Vertical => "drag-overlay vertical",
            }),
        ),
        Some(Drag::Pane { moved, .. }) => (
            if *moved {
                "workbench dragging moving"
            } else {
                "workbench"
            },
            None,
        ),
    };
    rsx! {
        div {
            class,
            onmousemove: move |event| drag_move(app, &event),
            // Drop targets handle the release first; whatever it was over,
            // the drag ends here.
            onmouseup: move |_| {
                if drag.peek().is_some() {
                    drag.set(None);
                }
            },
            TabBar {}
            div { class: "panes",
                NodeView { node: root, path: Vec::new() }
            }
            if let Some(overlay) = overlay {
                div { class: overlay }
            }
        }
    }
}

#[component]
fn TabBar() -> Element {
    let app = use_context::<AppSignals>();
    let shortcuts = use_context::<Shortcuts>();
    let mut layout = app.layout;
    let moving = app.drag.read().as_ref().and_then(Drag::moving);
    let (tabs, active) = {
        let current = layout.read();
        let projects = app.projects.read();
        let schedules = app.schedule_names.read();
        let tabs: Vec<(usize, String, usize, Option<&'static str>, bool)> = current
            .tabs()
            .iter()
            .enumerate()
            .map(|(index, tab)| {
                let panes = tab.root.panes();
                let label = panes.first().map_or_else(
                    || "New tab".to_owned(),
                    |pane| pane_label(&current, &projects, &schedules, *pane).1,
                );
                let unread = panes.iter().any(|pane| is_unread(app.sessions, *pane));
                (
                    index,
                    label,
                    panes.len(),
                    tab_state(app.sessions, &panes),
                    unread,
                )
            })
            .collect();
        (tabs, current.active())
    };

    rsx! {
        nav { class: "tabbar",
            for (index, label, count, state, unread) in tabs {
                div {
                    key: "{index}",
                    class: match (index == active, unread) {
                        (true, _) => "tab active",
                        (false, true) => "tab unread",
                        (false, false) => "tab",
                    },
                    onclick: move |_| layout.write().select_tab(index),
                    onmouseup: move |_| {
                        if let Some(pane) = moving {
                            layout.write().move_to_tab(pane, Some(index));
                        }
                    },
                    if let Some(state) = state {
                        span { class: "dot {state}" }
                    }
                    span { class: "name", "{label}" }
                    if count > 1 {
                        span { class: "count", "{count}" }
                    }
                    button {
                        class: "icon close",
                        title: "Close tab",
                        onclick: move |event| {
                            event.stop_propagation();
                            close_tab(app, index);
                        },
                        Svg { icon: Icon::Close, size: 12 }
                    }
                }
            }
            button {
                class: "icon new-tab",
                title: if moving.is_some() { "Move to a new tab".to_owned() } else { shortcuts.title("New tab", Action::NewTab) },
                onclick: move |_| {
                    layout.write().new_tab();
                },
                onmouseup: move |_| {
                    if let Some(pane) = moving {
                        layout.write().move_to_tab(pane, None);
                    }
                },
                Svg { icon: Icon::Plus }
            }
        }
    }
}

/// A node of the active tab's tree at `path`, child indices from its root.
#[component]
fn NodeView(node: Node, path: Vec<usize>) -> Element {
    let mut host = use_signal(|| None::<Rc<MountedData>>);
    match node {
        Node::Pane(pane) => rsx! {
            PaneView { key: "{pane}", pane }
        },
        Node::Split {
            direction,
            children,
            sizes,
        } => {
            let class = match direction {
                Direction::Horizontal => "split horizontal",
                Direction::Vertical => "split vertical",
            };
            let last = children.len() - 1;
            rsx! {
                div {
                    class,
                    onmounted: move |event| host.set(Some(event.data())),
                    for (index, (child, size)) in children.into_iter().zip(sizes.iter().copied()).enumerate() {
                        div { class: "split-child", style: "flex: {size} 1 0",
                            NodeView {
                                node: child,
                                path: path.iter().copied().chain([index]).collect::<Vec<_>>(),
                            }
                        }
                        if index < last {
                            Divider {
                                path: path.clone(),
                                index,
                                direction,
                                sizes: sizes.clone(),
                                host,
                            }
                        }
                    }
                }
            }
        }
    }
}

/// The boundary after child `index` of the split at `path`: drag it to
/// resize the two children beside it, double-click it to make them equal.
#[component]
fn Divider(
    path: Vec<usize>,
    index: usize,
    direction: Direction,
    sizes: Vec<f32>,
    host: Signal<Option<Rc<MountedData>>>,
) -> Element {
    let app = use_context::<AppSignals>();
    let mut drag = app.drag;
    let mut layout = app.layout;
    rsx! {
        div {
            class: "divider",
            onmousedown: {
                let path = path.clone();
                move |event: MouseEvent| {
                    if event.trigger_button() != Some(MouseButton::Primary) {
                        return;
                    }
                    // Keep the press from selecting text or focusing a pane.
                    event.prevent_default();
                    event.stop_propagation();
                    let Some(element) = host.peek().clone() else {
                        return;
                    };
                    let path = path.clone();
                    let sizes = sizes.clone();
                    spawn(async move {
                        let Ok(rect) = element.get_client_rect().await else {
                            return;
                        };
                        let (origin, extent) = match direction {
                            Direction::Horizontal => (rect.origin.x, rect.size.width),
                            Direction::Vertical => (rect.origin.y, rect.size.height),
                        };
                        if let Some((offset, length)) = boundary_span(
                            &sizes,
                            index,
                            extent,
                            DIVIDER,
                        ) {
                            drag.set(
                                Some(Drag::Boundary {
                                    path,
                                    index,
                                    direction,
                                    start: origin + offset,
                                    length,
                                }),
                            );
                        }
                    });
                }
            },
            ondoubleclick: move |_| {
                layout.write().resize(&path, index, 0.5);
            },
        }
    }
}

#[component]
fn PaneView(pane: PaneId) -> Element {
    let app = use_context::<AppSignals>();
    let shortcuts = use_context::<Shortcuts>();
    let mut layout = app.layout;
    let (content, focused, (project, item)) = {
        let current = layout.read();
        (
            current.content(pane).cloned().unwrap_or(PaneContent::Empty),
            current.focused() == pane,
            pane_label(
                &current,
                &app.projects.read(),
                &app.schedule_names.read(),
                pane,
            ),
        )
    };
    let status = match &content {
        PaneContent::Agent { .. } => Some(agent_status(app.sessions, pane)),
        _ => None,
    };
    let unread = is_unread(app.sessions, pane);
    let is_agent = status.is_some();
    let mut drag = app.drag;
    let moving = drag.read().as_ref().and_then(Drag::moving);
    let class = match (focused, moving == Some(pane)) {
        (_, true) => "pane moving",
        (true, false) => "pane focused",
        (false, false) => "pane",
    };

    rsx! {
        section {
            class,
            onmousedown: move |_| {
                if layout.peek().focused() != pane {
                    layout.write().focus(pane);
                }
                mark_read(app.sessions, pane);
            },
            if let Some(moving) = moving.filter(|moving| *moving != pane) {
                DropZones { pane: moving, target: pane }
            }
            header {
                title: "Drag to move this pane",
                onmousedown: move |event: MouseEvent| {
                    if event.trigger_button() == Some(MouseButton::Primary) {
                        // Keep a drag from selecting the text it passes over.
                        event.prevent_default();
                        let point = event.client_coordinates();
                        drag.set(
                            Some(Drag::Pane {
                                pane,
                                from: (point.x, point.y),
                                moved: false,
                            }),
                        );
                    }
                },
                if let Some((state, _, _)) = &status {
                    span { class: "dot {state}" }
                }
                if let Some(project) = project {
                    span { class: "crumb", "{project}" }
                    span { class: "crumb-sep", "/" }
                }
                span { class: "title", "{item}" }
                if let Some((state, label, name)) = status {
                    span { class: "status {state}", "{label}" }
                    if let Some(name) = name {
                        span { class: "agent-name", "{name}" }
                    }
                }
                if unread {
                    span { class: "unread-badge", "new" }
                }
                span { class: "spacer" }
                span { class: "pane-tools",
                    if is_agent {
                        HistoryMenu { pane }
                    }
                    button {
                        class: "icon",
                        title: shortcuts.title("Split right", Action::SplitRight),
                        onclick: move |_| {
                            layout.write().split(pane, Direction::Horizontal);
                        },
                        Svg { icon: Icon::SplitRight }
                    }
                    button {
                        class: "icon",
                        title: shortcuts.title("Split down", Action::SplitDown),
                        onclick: move |_| {
                            layout.write().split(pane, Direction::Vertical);
                        },
                        Svg { icon: Icon::SplitDown }
                    }
                    button {
                        class: "icon",
                        title: shortcuts.title("Close pane", Action::ClosePane),
                        onclick: move |_| close_pane(app, pane),
                        Svg { icon: Icon::Close }
                    }
                }
            }
            match content {
                PaneContent::Empty => rsx! {
                    Picker { pane }
                },
                PaneContent::Agent { session, .. } => rsx! {
                    AgentView { pane, has_saved_session: session.is_some() }
                },
                PaneContent::Schedule { project, agent, schedule } => rsx! {
                    ScheduleView {
                        key: "{agent}-{schedule:?}",
                        pane,
                        project,
                        agent,
                        schedule,
                    }
                },
                PaneContent::Note { project, agent, note } => rsx! {
                    NoteView {
                        key: "{agent}-{note:?}",
                        pane,
                        project,
                        agent,
                        note,
                    }
                },
                PaneContent::Diff { project, place } => rsx! {
                    DiffPane {
                        key: "{project}-{place:?}",
                        pane,
                        project,
                        place,
                    }
                },
                PaneContent::Report { project, path, source } => rsx! {
                    ReportView {
                        pane,
                        project,
                        path,
                        source,
                    }
                },
                PaneContent::Browser { project, port, path, source } => rsx! {
                    BrowserView {
                        pane,
                        project,
                        port,
                        path,
                        source,
                    }
                },
            }
        }
    }
}

/// Where a dragged pane can go on `target`: beside it on one of its four
/// sides, or in its place (the two swap). Each edge strip previews the half
/// the pane would take.
#[component]
fn DropZones(pane: PaneId, target: PaneId) -> Element {
    let app = use_context::<AppSignals>();
    let mut layout = app.layout;
    let mut over = use_signal(|| None::<Option<Side>>);
    let zones = [
        ("left", Some(Side::Left)),
        ("right", Some(Side::Right)),
        ("top", Some(Side::Top)),
        ("bottom", Some(Side::Bottom)),
        ("center", None),
    ];
    let preview = over().map(|side| match side {
        Some(Side::Left) => "drop-preview left",
        Some(Side::Right) => "drop-preview right",
        Some(Side::Top) => "drop-preview top",
        Some(Side::Bottom) => "drop-preview bottom",
        None => "drop-preview center",
    });
    rsx! {
        div { class: "drop-zones",
            if let Some(preview) = preview {
                div { class: preview }
            }
            for (name, side) in zones {
                div {
                    key: "{name}",
                    class: "drop-zone {name}",
                    onmouseenter: move |_| over.set(Some(side)),
                    onmouseleave: move |_| {
                        if over.peek().as_ref() == Some(&side) {
                            over.set(None);
                        }
                    },
                    onmouseup: move |_| {
                        let mut layout = layout.write();
                        match side {
                            Some(side) => layout.move_pane(pane, target, side),
                            None => layout.swap_panes(pane, target),
                        };
                    },
                }
            }
        }
    }
}

/// What an empty pane offers: a project, then any agent its configuration
/// declares, shown with the command or endpoint it starts.
#[component]
fn Picker(pane: PaneId) -> Element {
    let app = use_context::<AppSignals>();
    let mut chosen = use_signal(|| app.projects.peek().selected().map(|p| p.id.clone()));
    let mut prompting = use_signal(|| false);
    let projects: Vec<(ProjectId, String)> = app
        .projects
        .read()
        .iter()
        .map(|project| (project.id.clone(), project.name()))
        .collect();
    let current = chosen
        .read()
        .clone()
        .filter(|id| projects.iter().any(|(p, _)| p == id))
        .or_else(|| projects.first().map(|(id, _)| id.clone()));
    let agents: Result<Vec<Choice>, String> = match &current {
        None => Ok(Vec::new()),
        Some(id) => match app.configs.read().get(id) {
            Some(Ok(config)) => Ok(config.agents.iter().flat_map(choices).collect()),
            Some(Err(error)) => Err(error.clone()),
            None => Ok(Vec::new()),
        },
    };

    rsx! {
        div { class: "pane-body picker",
            div { class: "picker-panel",
                h2 { "Open an agent" }
                if projects.is_empty() {
                    p { class: "muted", "Open a project from the sidebar to start." }
                } else {
                    if projects.len() > 1 {
                        div { class: "segmented",
                            for (id, name) in projects {
                                button {
                                    key: "{id}",
                                    class: if current.as_ref() == Some(&id) { "active" } else { "" },
                                    onclick: {
                                        let id = id.clone();
                                        move |_| chosen.set(Some(id.clone()))
                                    },
                                    "{name}"
                                }
                            }
                        }
                    } else if let Some((_, name)) = projects.first() {
                        p { class: "muted", "in {name}" }
                    }
                    match agents {
                        Err(error) => rsx! {
                            pre { class: "config-error", "{error}" }
                        },
                        Ok(agents) if agents.is_empty() => rsx! {
                            p { class: "muted",
                                "No agents for this project. Declare them in its ukai.toml or in ~/.config/ukai/config.toml."
                            }
                        },
                        Ok(agents) => rsx! {
                            div { class: "choices",
                                for Choice { agent, line, badge, scope } in agents {
                                    button {
                                        key: "{agent}-{badge}",
                                        class: "choice",
                                        onclick: {
                                            let project = current.clone();
                                            let agent = agent.clone();
                                            move |_| {
                                                if let Some(project) = &project {
                                                    open_agent(app, pane, project, &agent, scope);
                                                }
                                            }
                                        },
                                        span { class: "choice-head",
                                            span { class: "name", "{agent}" }
                                            span { class: "badge", "{badge}" }
                                        }
                                        code { "{line}" }
                                        span { class: "chevron",
                                            Svg { icon: Icon::Chevron }
                                        }
                                    }
                                }
                            }
                        },
                    }
                    if let Some(project) = current {
                        div { class: "open-other",
                            button {
                                class: "outline open-file",
                                title: "Show an HTML report or image from this project",
                                onclick: {
                                    let project = project.clone();
                                    move |_| pick_report(app, Some(pane), project.clone())
                                },
                                Svg { icon: Icon::Document }
                                "Open report or image…"
                            }
                            button {
                                class: "outline open-file",
                                title: "Show a page served on a port of this machine",
                                onclick: move |_| prompting.toggle(),
                                Svg { icon: Icon::Globe }
                                "Open a local port…"
                            }
                        }
                        if prompting() {
                            PortPrompt {
                                on_open: {
                                    let project = project.clone();
                                    move |(port, path): (u16, String)| {
                                        open_browser(app, pane, &project, port, path, ReportSource::Project);
                                    }
                                },
                                on_cancel: move |()| prompting.set(false),
                            }
                        }
                    }
                }
            }
        }
    }
}

/// One way to open an agent in the picker. A gateway agent is offered once
/// per workspace scope, its default first.
pub struct Choice {
    pub agent: String,
    pub line: String,
    pub badge: &'static str,
    pub scope: Option<WorkspaceScope>,
}

pub fn choices((agent, spec): (&String, &AgentSpec)) -> Vec<Choice> {
    let choice = |badge, scope| Choice {
        agent: agent.clone(),
        line: launch_line(spec),
        badge,
        scope,
    };
    match spec {
        AgentSpec::Gateway { workspace, .. } => {
            let other = match workspace {
                WorkspaceScope::Shared => WorkspaceScope::Pod,
                WorkspaceScope::Pod => WorkspaceScope::Shared,
            };
            [*workspace, other]
                .into_iter()
                .map(|scope| choice(scope_label(scope), Some(scope)))
                .collect()
        }
        _ => vec![choice(transport_label(spec), None)],
    }
}

fn scope_label(scope: WorkspaceScope) -> &'static str {
    match scope {
        WorkspaceScope::Shared => "gateway · shared",
        WorkspaceScope::Pod => "gateway · own pod",
    }
}

pub fn transport_label(spec: &AgentSpec) -> &'static str {
    match spec {
        AgentSpec::Http { .. } => "http",
        AgentSpec::Websocket { .. } => "ws",
        AgentSpec::Gateway { .. } => "gateway",
        AgentSpec::Stdio { .. } => "stdio",
    }
}

fn launch_line(spec: &AgentSpec) -> String {
    match spec {
        AgentSpec::Http { url, .. }
        | AgentSpec::Websocket { url, .. }
        | AgentSpec::Gateway { url, .. } => url.clone(),
        AgentSpec::Stdio { command, args, .. } => std::iter::once(command.as_str())
            .chain(args.iter().map(String::as_str))
            .collect::<Vec<_>>()
            .join(" "),
    }
}
