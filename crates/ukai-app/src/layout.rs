//! Tabs and pane trees the user arranges at runtime, rendered from
//! [`ukai_core::Layout`]. Each pane belongs to a project, so one tab can show
//! agents of several projects side by side.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use dioxus::html::input_data::MouseButton;
use dioxus::prelude::*;
use std::collections::HashMap;
use ukai_acp::{PaneWorkspace, WorkspaceScope};
use ukai_config::AgentSpec;
use ukai_core::keymap::Action;
use ukai_core::launch::{self, Choice, Group, Status};
use ukai_core::sidebar::Column;
use ukai_core::{
    Boundary, Direction, Frame, Layout, PaneContent, PaneId, Placed, ProjectId, Projects, Recent,
    ReportSource, Side,
};

use crate::agent_pane::{AgentView, HistoryMenu, Sessions, agent_status, connect, session_title};
use crate::alert::{is_unread, mark_read};
use crate::browser_pane::BrowserView;
use crate::diff_pane::DiffPane;
use crate::icons::{Icon, Svg};
use crate::knowledge::NoteView;
use crate::ports::{PortPrompt, open_browser};
use crate::projects::AppSignals;
use crate::report_pane::{ReportView, delete_report, download_report, pick_report};
use crate::schedules::{ScheduleView, schedule_label};
use crate::secretary::{SecretaryPane, is_secretary};
use crate::shortcuts::Shortcuts;
use crate::terminal_pane::{TerminalPane, open_terminal_in};

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
        Some(Ok(config)) => match config.agent(agent) {
            Some(AgentSpec::Gateway { workspace, .. }) => Some(workspace),
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
        Some(PaneContent::Terminal { project, place }) => (
            Some(project_name(project)),
            match place.agent() {
                Some(agent) => format!("Terminal · {agent}"),
                None => "Terminal".to_owned(),
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
    let panes = use_kept_panes(app.layout);
    let (boundaries, split) = {
        let layout = app.layout.read();
        let frames = layout.active_tab().root.frames();
        (frames.boundaries, frames.panes.len() > 1)
    };
    // The panes of the tabs kept are placed in this one element rather than
    // nested in their splits, so that a split, a move, or a switch between
    // those tabs changes where a pane is drawn, or whether it shows, but
    // never remounts it: remounting rebuilds its transcript and reloads its
    // web view.
    let mut area = use_signal(|| None::<Rc<MountedData>>);
    let panes_class = if split { "panes split" } else { "panes" };
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
            div {
                class: panes_class,
                onmounted: move |event| area.set(Some(event.data())),
                PlacedPanes {
                    panes,
                    pane: |pane| rsx! {
                        PaneView { pane }
                    },
                }
                for boundary in boundaries {
                    Divider { boundary, area }
                }
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
        let tabs: Vec<_> = current
            .tabs()
            .iter()
            .enumerate()
            .map(|(index, tab)| {
                let panes = tab.root.panes();
                let label = panes.first().map_or_else(
                    || "New tab".to_owned(),
                    |pane| pane_label(&current, &projects, &schedules, *pane).1,
                );
                // An agent's tab reads as what its session is about.
                let (label, hint) = match panes
                    .first()
                    .and_then(|pane| session_title(app.sessions, *pane))
                {
                    Some(title) => (title.clone(), format!("{label} · {title}")),
                    None => (label.clone(), label),
                };
                let unread = panes.iter().any(|pane| is_unread(app.sessions, *pane));
                (
                    index,
                    label,
                    hint,
                    panes.len(),
                    tab_state(app.sessions, &panes),
                    unread,
                )
            })
            .collect();
        (tabs, current.active())
    };

    let mut sidebar_hidden = app.sidebar_hidden;

    rsx! {
        nav { class: "tabbar",
            for (column, icon, name) in [
                (Column::Tree, Icon::Sidebar, "workspaces"),
                (Column::Detail, Icon::SidebarDetail, "workspace detail"),
            ]
            {
                button {
                    key: "{name}",
                    class: if sidebar_hidden().is_hidden(column) { "icon sidebar-toggle" } else { "icon sidebar-toggle open" },
                    title: shortcuts
                        .title(
                            &if sidebar_hidden().is_hidden(column) {
                                format!("Show {name} column")
                            } else {
                                format!("Hide {name} column")
                            },
                            Action::ToggleColumn(column),
                        ),
                    onclick: move |_| sidebar_hidden.write().toggle(column),
                    Svg { icon }
                }
            }
            for (index, label, hint, count, state, unread) in tabs {
                div {
                    key: "{index}",
                    title: "{hint}",
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

/// How many tabs stay mounted: the one shown and those shown just before
/// it. A tab past them is dropped until it shows again, rebuilt then, so
/// that many tabs open do not keep all their transcripts and web views.
pub const KEPT_TABS: usize = 5;

/// The panes to mount: those of the [`KEPT_TABS`] tabs shown most recently.
pub fn use_kept_panes(layout: Signal<Layout>) -> Vec<Placed> {
    // Not a signal: noting the tab shown is bookkeeping for this render, and
    // noting it again changes nothing.
    let recent = use_hook(|| Rc::new(RefCell::new(Recent::default())));
    let layout = layout.read();
    let mut recent = recent.borrow_mut();
    recent.show(&layout);
    layout.placed_kept(&recent.kept(&layout, KEPT_TABS))
}

/// Each of `panes` ([`use_kept_panes`]: the kept tabs') drawn by `pane`, placed
/// at its frame. Keyed by pane id and kept in id order, so that a split, a
/// move, a resize, or a tab switch changes where a pane's element is, or
/// whether it shows, but never remounts what `pane` drew in it.
#[component]
pub fn PlacedPanes(panes: Vec<Placed>, pane: Callback<PaneId, Element>) -> Element {
    rsx! {
        for Placed { pane: id, frame, shown } in panes {
            PaneSlot { key: "{id}", frame, shown, {pane.call(id)} }
        }
    }
}

/// Whether the pane drawn in a slot is on the tab shown, provided to what
/// it draws: a native web view does not hide with its element, so it has to
/// be hidden itself.
#[derive(Clone, Copy)]
pub struct PaneShown(pub ReadSignal<bool>);

/// One pane's place: where it goes, hidden while its tab is not shown.
#[component]
fn PaneSlot(frame: Frame, shown: ReadSignal<bool>, children: Element) -> Element {
    use_context_provider(|| PaneShown(shown));
    rsx! {
        div {
            class: if shown() { "pane-slot" } else { "pane-slot hidden" },
            style: placement(frame),
            {children}
        }
    }
}

/// Where `frame` of the pane area is, as CSS.
fn placement(frame: Frame) -> String {
    let percent = |fraction: f64| fraction * 100.0;
    format!(
        "left: {}%; top: {}%; width: {}%; height: {}%;",
        percent(frame.x),
        percent(frame.y),
        percent(frame.width),
        percent(frame.height),
    )
}

/// A boundary between two children of a split: drag it to resize them,
/// double-click it to make them equal. `area` is the pane area it is laid
/// over.
#[component]
fn Divider(boundary: Boundary, area: Signal<Option<Rc<MountedData>>>) -> Element {
    let app = use_context::<AppSignals>();
    let mut drag = app.drag;
    let mut layout = app.layout;
    let class = match boundary.direction {
        Direction::Horizontal => "divider horizontal",
        Direction::Vertical => "divider vertical",
    };
    // Across the split only: its thickness is the stylesheet's.
    let line = boundary.line;
    let style = match boundary.direction {
        Direction::Horizontal => format!(
            "left: {}%; top: {}%; height: {}%;",
            line.x * 100.0,
            line.y * 100.0,
            line.height * 100.0,
        ),
        Direction::Vertical => format!(
            "left: {}%; top: {}%; width: {}%;",
            line.x * 100.0,
            line.y * 100.0,
            line.width * 100.0,
        ),
    };
    let Boundary {
        path,
        index,
        direction,
        span: (start, length),
        ..
    } = boundary;
    rsx! {
        div {
            class,
            style,
            onmousedown: {
                let path = path.clone();
                move |event: MouseEvent| {
                    if event.trigger_button() != Some(MouseButton::Primary) {
                        return;
                    }
                    // Keep the press from selecting text or focusing a pane.
                    event.prevent_default();
                    event.stop_propagation();
                    let Some(element) = area.peek().clone() else {
                        return;
                    };
                    let path = path.clone();
                    spawn(async move {
                        let Ok(rect) = element.get_client_rect().await else {
                            return;
                        };
                        let (origin, extent) = match direction {
                            Direction::Horizontal => (rect.origin.x, rect.size.width),
                            Direction::Vertical => (rect.origin.y, rect.size.height),
                        };
                        drag.set(
                            Some(Drag::Boundary {
                                path,
                                index,
                                direction,
                                start: origin + start * extent,
                                length: length * extent,
                            }),
                        );
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
    let session = session_title(app.sessions, pane);
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
                if let Some(session) = session {
                    span { class: "session-title", title: "{session}", "{session}" }
                }
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
                    if let PaneContent::Report { project, path, source } = &content {
                        button {
                            class: "icon",
                            title: "Save a copy of this report…",
                            onclick: {
                                let (project, path, source) = (project.clone(), path.clone(), source.clone());
                                move |_| download_report(app, &project, &path, &source)
                            },
                            Svg { icon: Icon::Download }
                        }
                        button {
                            class: "icon",
                            title: "Delete this report…",
                            onclick: {
                                let (project, path, source) = (project.clone(), path.clone(), source.clone());
                                move |_| delete_report(app, &project, &path, &source, || {})
                            },
                            Svg { icon: Icon::Trash }
                        }
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
                PaneContent::Agent {
                    session,
                    project,
                    agent,
                    ..
                } if is_secretary(app, &project, &agent) => rsx! {
                    SecretaryPane {
                        pane,
                        project,
                        agent,
                        has_saved_session: session.is_some(),
                    }
                },
                PaneContent::Agent { session, .. } => rsx! {
                    AgentView { pane, has_saved_session: session.is_some() }
                },
                PaneContent::Schedule { project, agent, schedule } => rsx! {
                    for (project, agent, schedule) in [(project, agent, schedule)] {
                        ScheduleView {
                            key: "{agent}-{schedule:?}",
                            pane,
                            project,
                            agent,
                            schedule,
                        }
                    }
                },
                PaneContent::Note { project, agent, note } => rsx! {
                    for (project, agent, note) in [(project, agent, note)] {
                        NoteView {
                            key: "{agent}-{note:?}",
                            pane,
                            project,
                            agent,
                            note,
                        }
                    }
                },
                PaneContent::Diff { project, place } => rsx! {
                    for (project, place) in [(project, place)] {
                        DiffPane {
                            key: "{project}-{place:?}",
                            pane,
                            project,
                            place,
                        }
                    }
                },
                PaneContent::Terminal { project, place } => rsx! {
                    for (project, place) in [(project, place)] {
                        TerminalPane {
                            key: "{project}-{place:?}",
                            pane,
                            project,
                            place,
                        }
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
    let groups: Result<Vec<Group>, String> = match &current {
        None => Ok(Vec::new()),
        Some(id) => match app.configs.read().get(id) {
            Some(Ok(config)) => Ok(launch::groups(config, app.gateways.read().get(id))),
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
                    match groups {
                        Err(error) => rsx! {
                            pre { class: "config-error", "{error}" }
                        },
                        Ok(groups) if groups.is_empty() => rsx! {
                            p { class: "muted",
                                "No agents for this project. Declare them, or a gateway, in its ukai.toml or in ~/.config/ukai/config.toml."
                            }
                        },
                        Ok(groups) => rsx! {
                            for Group { gateway, status, choices } in groups {
                                div { key: "{gateway:?}", class: "choice-group",
                                    if let Some(gateway) = &gateway {
                                        h3 { class: "choice-group-head",
                                            Svg { icon: Icon::Globe, size: 12 }
                                            "{gateway}"
                                        }
                                    }
                                    match status {
                                        Status::Loading => rsx! {
                                            p { class: "muted", "Asking the gateway for its agents…" }
                                        },
                                        Status::Failed(why) => rsx! {
                                            pre { class: "config-error", "{why}" }
                                        },
                                        Status::Ready if choices.is_empty() => rsx! {
                                            p { class: "muted", "The gateway serves no agents." }
                                        },
                                        Status::Ready => rsx! {
                                            div { class: "choices",
                                                for Choice { agent, name, line, badge, scope } in choices {
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
                                                            span { class: "name", "{name}" }
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
                                title: "Open a shell in this project's root",
                                onclick: {
                                    let project = project.clone();
                                    move |_| open_terminal_in(app, pane, &project, &ReportSource::Project)
                                },
                                Svg { icon: Icon::Terminal }
                                "Open a terminal"
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

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::HashMap;

    use dioxus::dioxus_core::NoOpMutations;
    use ukai_core::Side;

    use super::*;

    /// How many times each pane's content was mounted.
    type Mounts = Rc<RefCell<HashMap<PaneId, usize>>>;

    #[component]
    fn Probe(pane: PaneId) -> Element {
        let mounts = use_context::<Mounts>();
        use_hook(|| *mounts.borrow_mut().entry(pane).or_default() += 1);
        rsx! { "{pane}" }
    }

    #[component]
    fn Root() -> Element {
        let layout = use_context::<Signal<Layout>>();
        let panes = use_kept_panes(layout);
        rsx! {
            PlacedPanes {
                panes,
                pane: |pane| rsx! {
                    Probe { pane }
                },
            }
        }
    }

    /// Splitting, moving, resizing, and closing panes, switching tabs, and
    /// moving a pane to another tab leave every pane mounted once: a
    /// remount rebuilds an agent's transcript and reloads a report's web
    /// view.
    #[test]
    fn rearranging_panes_never_remounts_one() {
        let mounts = Mounts::default();
        let mut dom = VirtualDom::new(Root);
        let mut layout = dom.in_runtime(|| Signal::new_in_scope(Layout::default(), ScopeId::ROOT));
        dom.provide_root_context(mounts.clone());
        dom.provide_root_context(layout);
        dom.rebuild_in_place();

        let mut change = |dom: &mut VirtualDom, change: &dyn Fn(&mut Layout)| {
            dom.in_runtime(|| change(&mut layout.write()));
            settle(dom);
        };
        change(&mut dom, &|layout| {
            layout.split(0, Direction::Horizontal);
        });
        change(&mut dom, &|layout| {
            layout.split(1, Direction::Vertical);
        });
        change(&mut dom, &|layout| {
            layout.split(0, Direction::Vertical);
        });
        change(&mut dom, &|layout| {
            layout.resize(&[], 0, 0.3);
        });
        change(&mut dom, &|layout| {
            layout.move_pane(0, 2, Side::Right);
        });
        change(&mut dom, &|layout| {
            layout.swap_panes(1, 3);
        });
        change(&mut dom, &|layout| {
            layout.close_pane(2);
        });
        change(&mut dom, &|layout| {
            layout.new_tab();
        });
        change(&mut dom, &|layout| {
            layout.select_tab(0);
        });
        change(&mut dom, &|layout| {
            layout.move_to_tab(1, Some(1));
        });
        change(&mut dom, &|layout| {
            layout.select_tab(0);
        });

        let panes = dom.in_runtime(|| layout.peek().placed().len());
        assert_eq!(panes, 4);
        assert_eq!(
            *mounts.borrow(),
            HashMap::from([(0, 1), (1, 1), (2, 1), (3, 1), (4, 1)]),
        );
    }

    fn settle(dom: &mut VirtualDom) {
        dom.render_immediate(&mut NoOpMutations);
    }

    /// Only the [`KEPT_TABS`] tabs shown last stay mounted: going back to
    /// one of them remounts nothing, going back to an older one rebuilds it.
    #[test]
    fn only_the_tabs_shown_last_stay_mounted() {
        let mounts = Mounts::default();
        let mut dom = VirtualDom::new(Root);
        let mut layout = dom.in_runtime(|| Signal::new_in_scope(Layout::default(), ScopeId::ROOT));
        dom.provide_root_context(mounts.clone());
        dom.provide_root_context(layout);
        dom.rebuild_in_place();
        settle(&mut dom);

        // Tab `i` holds pane `i`.
        let tabs = KEPT_TABS + 2;
        for _ in 1..tabs {
            dom.in_runtime(|| layout.write().new_tab());
            settle(&mut dom);
        }
        let mut show = |dom: &mut VirtualDom, tab: usize| {
            dom.in_runtime(|| layout.write().select_tab(tab));
            settle(dom);
        };
        // Among the last kept: nothing is rebuilt.
        show(&mut dom, tabs - KEPT_TABS);
        assert!(mounts.borrow().values().all(|count| *count == 1));
        // The oldest was dropped, so it is rebuilt.
        show(&mut dom, 0);
        assert_eq!(mounts.borrow()[&0], 2);
        assert_eq!(mounts.borrow()[&(tabs as PaneId - 1)], 1);
    }
}
