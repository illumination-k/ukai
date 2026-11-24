//! The sidebar, in two columns under a dropdown of the open projects. The
//! first lists the selected project's workspaces — this machine's checkout,
//! and the gateway workspaces — each with its agent panes (click to focus),
//! and then the knowledge and schedules of its gateway agents. The second
//! column shows what is in one workspace — its changes, reports, and ports:
//! the focused pane's, following focus, or the one last clicked. What goes where is
//! [`ukai_core::sidebar`]'s; this draws it.

use std::collections::HashMap;

use dioxus::prelude::*;
use ukai_acp::WorkspaceScope;
use ukai_config::AgentSpec;
use ukai_core::keymap::Action;
use ukai_core::launch::{self, Choice, Group, Status};
use ukai_core::sidebar::{self, Fold, OpenAgent, Workspace};
use ukai_core::{PaneContent, ProjectId, ReportSource};

use crate::agent_pane::{agent_status, session_title};
use crate::alert::is_unread;
use crate::diff_pane::{diff_open, open_diff};
use crate::icons::{Icon, Svg};
use crate::knowledge::KnowledgeNode;
use crate::layout::{launch_agent, launch_target, open_agent_in};
use crate::metrics_pane::MetricsNode;
use crate::ports::PortGroup;
use crate::projects::{AppSignals, Paths, pick_and_add_project, reload_project, remove_project};
use crate::report_pane::ReportGroup;
use crate::schedules::ScheduleNode;
use crate::shortcuts::Shortcuts;
use crate::terminal_pane::open_terminal;

/// The workspace whose contents the second column shows.
type Opened = Signal<Option<(ProjectId, ReportSource)>>;

#[component]
pub fn Sidebar() -> Element {
    let app = use_context::<AppSignals>();
    let paths = use_context::<Paths>();
    let shortcuts = use_context::<Shortcuts>();
    let mut projects_signal = app.projects;
    let mut launching = use_signal(|| false);
    let mut opened: Opened = use_signal(|| None);
    // The workspace of the focused pane: the second column follows it, and
    // the dropdown its project, whenever focus moves to another workspace.
    let focused_place = use_memo(move || {
        let layout = app.layout.read();
        layout
            .content(layout.focused())
            .and_then(sidebar::pane_place)
    });
    use_effect(move || {
        let Some((project, place)) = focused_place() else {
            return;
        };
        let mut projects = projects_signal.write();
        if projects.selected().map(|p| &p.id) != Some(&project) {
            projects.select(&project);
            launching.set(false);
        }
        opened.set(Some((project, place)));
    });
    let (projects, selected): (Vec<(ProjectId, String, String)>, Option<ProjectId>) = {
        let projects = app.projects.read();
        (
            projects
                .iter()
                .map(|project| {
                    (
                        project.id.clone(),
                        project.name(),
                        project.root.display().to_string(),
                    )
                })
                .collect(),
            projects.selected().map(|project| project.id.clone()),
        )
    };
    let pick = {
        let paths = paths.clone();
        move || {
            let paths = paths.clone();
            move |_| pick_and_add_project(paths.clone(), app)
        }
    };
    // The workspace the second column shows, while it is still one of the
    // selected project's.
    let shown = opened.read().clone().filter(|(project, place)| {
        Some(project) == selected.as_ref() && {
            let configs = app.configs.read();
            let config = configs.get(project).and_then(|config| config.as_ref().ok());
            sidebar::tree(&app.layout.read(), project, config)
                .workspaces
                .iter()
                .any(|workspace| workspace.place == *place)
        }
    });

    // Columns are hidden rather than unmounted, so they come back as they
    // were left.
    let (tree_shown, detail_shown) = app.sidebar_hidden.read().shown(shown.is_some());
    let class = match (tree_shown, detail_shown) {
        (false, false) => "sidebar hidden",
        (true, true) => "sidebar wide",
        _ => "sidebar",
    };

    rsx! {
        nav { class,
            div { class: "sidebar-header",
                if let Some(id) = selected.clone() {
                    ProjectSelect {
                        projects: projects.clone(),
                        selected: id.clone(),
                        onpick: move |chosen: ProjectId| {
                            projects_signal.write().select(&chosen);
                            launching.set(false);
                        },
                    }
                    button {
                        class: if launching() { "icon open" } else { "icon" },
                        title: "Open an agent…",
                        onclick: move |_| launching.toggle(),
                        Svg { icon: Icon::Plus }
                    }
                    button {
                        class: "icon",
                        title: "Reload configuration",
                        onclick: {
                            let id = id.clone();
                            let paths = paths.clone();
                            move |_| reload_project(&paths, app, &id)
                        },
                        Svg { icon: Icon::Reload }
                    }
                    button {
                        class: "icon",
                        title: "Close project (ends its sessions)",
                        onclick: {
                            let id = id.clone();
                            move |_| remove_project(app, &id)
                        },
                        Svg { icon: Icon::Close }
                    }
                } else {
                    span { class: "section-label", "Projects" }
                }
                button {
                    class: "icon",
                    title: "Open project…",
                    onclick: pick(),
                    Svg { icon: Icon::Folder }
                }
            }
            if projects.is_empty() {
                div { class: "sidebar-empty",
                    p { class: "muted", "Open a project folder to start." }
                    button { class: "outline", onclick: pick(),
                        Svg { icon: Icon::Folder }
                        "Open folder…"
                    }
                }
            }
            if let Some(id) = selected {
                div { class: "sidebar-columns",
                    div { class: "sidebar-column tree", hidden: !tree_shown,
                        ProjectPanes {
                            key: "{id}",
                            id: id.clone(),
                            launching,
                            opened,
                        }
                    }
                    if let Some((project, place)) = shown {
                        div {
                            class: "sidebar-column sidebar-detail tree",
                            hidden: !detail_shown,
                            WorkspaceDetail {
                                key: "{place_key(&place)}",
                                project,
                                place,
                                opened,
                            }
                        }
                    }
                }
            }
            div { class: "sidebar-footer",
                for (action, label) in [
                    (Action::Palette, "palette"),
                    (Action::NewTab, "tab"),
                    (Action::SplitRight, "split"),
                    (Action::ClosePane, "close"),
                ]
                {
                    if let Some(hint) = shortcuts.keymap.hint(action) {
                        span {
                            kbd { "{hint}" }
                            " {label}"
                        }
                    }
                }
                if !shortcuts.problems.is_empty() {
                    span {
                        class: "footer-warning",
                        title: shortcuts.problems.join("\n"),
                        "⚠ keybindings"
                    }
                }
            }
        }
    }
}

/// The open projects as a button naming the selected one and a menu of the
/// others, drawn like the app's other menus rather than as a native
/// `select`, whose popup the web view leaves to the platform.
#[component]
fn ProjectSelect(
    projects: Vec<(ProjectId, String, String)>,
    selected: ProjectId,
    onpick: EventHandler<ProjectId>,
) -> Element {
    let mut open = use_signal(|| false);
    let mut highlighted = use_signal(|| 0usize);
    let current = projects.iter().position(|(id, _, _)| *id == selected);
    let (name, root) = current
        .map(|at| (projects[at].1.clone(), projects[at].2.clone()))
        .unwrap_or_default();
    let mut pick = move |id: ProjectId| {
        open.set(false);
        onpick.call(id);
    };
    let on_key = {
        let ids: Vec<ProjectId> = projects.iter().map(|(id, _, _)| id.clone()).collect();
        move |event: KeyboardEvent| {
            let len = ids.len().max(1) as isize;
            let step = |delta: isize| (highlighted() as isize + delta).rem_euclid(len) as usize;
            match event.key() {
                Key::ArrowDown => highlighted.set(step(1)),
                Key::ArrowUp => highlighted.set(step(-1)),
                Key::Enter if !event.is_composing() => {
                    if let Some(id) = ids.get(highlighted()) {
                        pick(id.clone());
                    }
                }
                Key::Escape | Key::Tab => open.set(false),
                _ => return,
            }
            event.prevent_default();
            event.stop_propagation();
        }
    };
    rsx! {
        div { class: "project-select",
            button {
                class: if open() { "project-trigger open" } else { "project-trigger" },
                title: "{root}",
                aria_haspopup: "listbox",
                aria_expanded: open(),
                onclick: move |_| {
                    highlighted.set(current.unwrap_or(0));
                    open.toggle();
                },
                span { class: "row-icon",
                    Svg { icon: Icon::Folder, size: 12 }
                }
                span { class: "name", "{name}" }
                span { class: "caret",
                    Svg { icon: Icon::Chevron, size: 10 }
                }
            }
            if open() {
                div {
                    class: "config-backdrop",
                    onclick: move |_| open.set(false),
                }
                div {
                    class: "config-menu project-menu",
                    role: "listbox",
                    aria_label: "Projects",
                    tabindex: "-1",
                    onmounted: move |event| async move {
                        let _ = event.set_focus(true).await;
                    },
                    onkeydown: on_key,
                    for (at, (id, name, root)) in projects.into_iter().enumerate() {
                        div {
                            key: "{id}",
                            class: if at == highlighted() { "config-item highlighted" } else { "config-item" },
                            role: "option",
                            aria_selected: Some(at) == current,
                            title: "{root}",
                            onmouseenter: move |_| highlighted.set(at),
                            onclick: move |_| pick(id.clone()),
                            span { class: "check",
                                if Some(at) == current {
                                    Svg { icon: Icon::Check, size: 12 }
                                }
                            }
                            span { class: "label", "{name}" }
                            span { class: "description", "{root}" }
                        }
                    }
                }
            }
        }
    }
}

/// A node's row: a chevron, and a click anywhere else on it but its actions,
/// fold it. `marker` is the class of a dot that asks for attention while it
/// is folded, and `children` are its actions.
#[component]
pub fn Head(
    fold: Fold,
    depth: u8,
    label: String,
    #[props(default)] meta: String,
    #[props(default)] icon: Option<Icon>,
    #[props(default)] marker: Option<&'static str>,
    #[props(default)] title: String,
    #[props(default)] class: String,
    children: Element,
) -> Element {
    let app = use_context::<AppSignals>();
    let mut folds = app.folds;
    let open = folds.read().is_open(&fold);
    rsx! {
        div {
            class: "row tree-head {class}",
            style: "--depth: {depth}",
            title: if !title.is_empty() { "{title}" },
            aria_expanded: open,
            onclick: move |_| {
                folds.write().toggle(fold.clone());
            },
            span { class: if open { "fold open" } else { "fold" },
                Svg { icon: Icon::Chevron, size: 10 }
            }
            if let Some(icon) = icon {
                span { class: "row-icon",
                    Svg { icon, size: 12 }
                }
            }
            span { class: "name", "{label}" }
            if let Some(marker) = marker {
                span { class: "dot {marker}" }
            }
            if !meta.is_empty() {
                span { class: "meta", "{meta}" }
            }
            span { class: "row-actions", {children} }
        }
    }
}

/// The first column: what can be launched (while asked for), the project's
/// workspaces with their agent panes, then its knowledge and schedules when
/// it has a gateway agent.
#[component]
fn ProjectPanes(id: ProjectId, launching: Signal<bool>, opened: Opened) -> Element {
    let app = use_context::<AppSignals>();
    let (tree, problem, launch) = {
        let configs = app.configs.read();
        let config = configs.get(&id);
        let tree = sidebar::tree(
            &app.layout.read(),
            &id,
            config.and_then(|config| config.as_ref().ok()),
        );
        let problem = config.and_then(|config| config.as_ref().err()).cloned();
        let launch: Vec<Group> = config
            .and_then(|config| config.as_ref().ok())
            .map(|config| launch::groups(config, app.gateways.read().get(&id)))
            .unwrap_or_default();
        (tree, problem, launch)
    };

    rsx! {
        if launching() {
            if launch.is_empty() {
                p { class: "muted hint tree-hint", style: "--depth: 0",
                    "No agents or gateways declared in ukai.toml."
                }
            }
            for Group { gateway, status, choices } in launch {
                // A gateway's agents sit one level under it.
                if let Some(gateway) = &gateway {
                    div {
                        key: "launch-gateway-{gateway}",
                        class: "row launch-row launch-gateway",
                        style: "--depth: 0",
                        span { class: "row-icon",
                            Svg { icon: Icon::Globe, size: 12 }
                        }
                        span { class: "name", "{gateway}" }
                        if status == Status::Loading {
                            span { class: "meta", "listing…" }
                        }
                    }
                    if let Status::Failed(why) = &status {
                        p {
                            class: "muted hint tree-hint",
                            style: "--depth: 1",
                            title: "{why}",
                            "Cannot list its agents."
                        }
                    }
                }
                for Choice { agent, name, badge, scope, line } in choices {
                    div {
                        key: "launch-{agent}-{badge}",
                        class: "row launch-row",
                        style: if gateway.is_some() { "--depth: 1" } else { "--depth: 0" },
                        title: "{line}",
                        onclick: {
                            let id = id.clone();
                            move |_| {
                                launching.set(false);
                                launch_agent(app, &id, &agent, scope);
                            }
                        },
                        span { class: "row-icon",
                            Svg { icon: Icon::Plus, size: 12 }
                        }
                        span { class: "name", "{name}" }
                        span { class: "meta", "{badge}" }
                    }
                }
            }
            div { class: "tree-divider" }
        }
        if let Some(problem) = problem {
            pre { class: "config-error tree-hint", style: "--depth: 0", "{problem}" }
        }
        for workspace in tree.workspaces {
            WorkspaceNode {
                key: "{place_key(&workspace.place)}",
                project: id.clone(),
                workspace,
                opened,
            }
        }
        if !tree.gateways.is_empty() {
            div { class: "tree-divider" }
            KnowledgeNode { project: id.clone(), depth: 0 }
            ScheduleNode {
                project: id.clone(),
                gateways: tree.gateways.clone(),
                depth: 0,
            }
            MetricsNode {
                project: id.clone(),
                gateways: tree.gateways.clone(),
                depth: 0,
            }
        }
    }
}

/// A stable key for a workspace among its project's.
fn place_key(place: &ReportSource) -> String {
    match place {
        ReportSource::Project => "local".to_owned(),
        ReportSource::Agent { agent, workspace } => match workspace {
            None => format!("{agent}/shared"),
            Some(workspace) => format!("{agent}/{}", workspace.key),
        },
    }
}

/// How the sidebar names a workspace: its icon, name, what kind it is, and a
/// tooltip.
fn place_label(
    place: &ReportSource,
    schedules: &HashMap<String, String>,
) -> (Icon, String, String, String) {
    match place {
        ReportSource::Project => (
            Icon::Monitor,
            "This machine".to_owned(),
            String::new(),
            "The project's checkout on this machine".to_owned(),
        ),
        ReportSource::Agent {
            agent,
            workspace: None,
        } => (
            Icon::Cloud,
            agent.clone(),
            "shared".to_owned(),
            format!("The workspace every pane of {agent} shares"),
        ),
        ReportSource::Agent {
            agent,
            workspace: Some(workspace),
        } => {
            let schedule = workspace
                .key
                .strip_prefix("schedule-")
                .map(|id| schedules.get(id).cloned().unwrap_or_else(|| id.to_owned()));
            let meta = match &schedule {
                Some(name) => format!("schedule · {name}"),
                None => format!("pod {}", workspace.key.get(..6).unwrap_or(&workspace.key)),
            };
            (
                Icon::Cloud,
                agent.clone(),
                meta,
                format!("A pod of its own, workspace {}", workspace.key),
            )
        }
    }
}

/// A workspace in the first column: a row that opens it in the second, and
/// its agent panes under it.
#[component]
fn WorkspaceNode(project: ProjectId, workspace: Workspace, opened: Opened) -> Element {
    let app = use_context::<AppSignals>();
    let mut choosing = use_signal(|| false);
    let mut opened = opened;
    let Workspace { place, agents } = workspace;
    // The agents a new session here can be: this machine's, or the one
    // whose workspace this is.
    let local: Vec<String> = match app.configs.read().get(&project) {
        Some(Ok(config)) => config
            .agents
            .iter()
            .filter(|(_, spec)| !matches!(spec, AgentSpec::Gateway { .. }))
            .map(|(agent, _)| agent.clone())
            .collect(),
        _ => Vec::new(),
    };
    let (icon, label, meta, title) = place_label(&place, &app.schedule_names.read());
    let selected = opened
        .read()
        .as_ref()
        .is_some_and(|(p, known)| *p == project && *known == place);
    let new_session = {
        let project = project.clone();
        let place = place.clone();
        let local = local.clone();
        move |event: MouseEvent| {
            event.stop_propagation();
            match &place {
                ReportSource::Project => match local.as_slice() {
                    [only] => launch_agent(app, &project, only, None),
                    _ => choosing.toggle(),
                },
                ReportSource::Agent {
                    agent,
                    workspace: None,
                } => launch_agent(app, &project, agent, Some(WorkspaceScope::Shared)),
                ReportSource::Agent {
                    agent,
                    workspace: Some(workspace),
                } => {
                    if let Some(pane) = launch_target(app) {
                        open_agent_in(app, pane, &project, agent, Some(workspace.clone()));
                    }
                }
            }
        }
    };
    // This machine with no agent of its own has nothing to start here.
    let can_start = !place.is_project() || !local.is_empty();

    rsx! {
        div {
            class: if selected { "row tree-head workspace-row selected" } else { "row tree-head workspace-row" },
            style: "--depth: 0",
            title: "{title}",
            onclick: {
                let project = project.clone();
                let place = place.clone();
                move |_| {
                    let next = (project.clone(), place.clone());
                    let mut opened = opened.write();
                    *opened = if opened.as_ref() == Some(&next) { None } else { Some(next) };
                }
            },
            span { class: "row-icon",
                Svg { icon, size: 12 }
            }
            span { class: "name", "{label}" }
            if !meta.is_empty() {
                span { class: "meta", "{meta}" }
            }
            span { class: "fold-hint",
                Svg { icon: Icon::Chevron, size: 10 }
            }
            span { class: "row-actions",
                if can_start {
                    button {
                        class: "icon",
                        title: "New session here",
                        onclick: new_session,
                        Svg { icon: Icon::Plus }
                    }
                }
            }
        }
        if choosing() {
            for agent in local {
                div {
                    key: "launch-{agent}",
                    class: "row launch-row",
                    style: "--depth: 1",
                    onclick: {
                        let project = project.clone();
                        move |_| {
                            choosing.set(false);
                            launch_agent(app, &project, &agent, None);
                        }
                    },
                    span { class: "row-icon",
                        Svg { icon: Icon::Plus, size: 12 }
                    }
                    span { class: "name", "{agent}" }
                }
            }
        }
        for agent in agents {
            AgentRow {
                key: "{agent.pane}",
                agent,
                depth: 1,
                on_open: {
                    let project = project.clone();
                    let place = place.clone();
                    move |()| opened.set(Some((project.clone(), place.clone())))
                },
            }
        }
    }
}

/// The second column: what is in one workspace — its changes, a terminal,
/// its reports, and ports.
#[component]
fn WorkspaceDetail(project: ProjectId, place: ReportSource, opened: Opened) -> Element {
    let app = use_context::<AppSignals>();
    let mut opened = opened;
    let (icon, label, meta, title) = place_label(&place, &app.schedule_names.read());
    rsx! {
        div { class: "detail-header", title: "{title}",
            span { class: "row-icon",
                Svg { icon, size: 12 }
            }
            span { class: "name", "{label}" }
            if !meta.is_empty() {
                span { class: "meta", "{meta}" }
            }
            button {
                class: "icon",
                title: "Close",
                onclick: move |_| opened.set(None),
                Svg { icon: Icon::Close }
            }
        }
        ChangesRow { project: project.clone(), place: place.clone(), depth: 0 }
        TerminalRow { project: project.clone(), place: place.clone(), depth: 0 }
        ReportGroup { project: project.clone(), place: place.clone(), depth: 0 }
        PortGroup { project, place, depth: 0 }
    }
}

/// What changed in a workspace's checkout: click to see it in a diff pane.
#[component]
fn ChangesRow(project: ProjectId, place: ReportSource, depth: u8) -> Element {
    let app = use_context::<AppSignals>();
    let open = diff_open(&app.layout.read(), &project, &place);
    rsx! {
        div {
            class: if open { "row report-row open" } else { "row report-row" },
            style: "--depth: {depth}",
            title: "Everything that changed in this checkout",
            onclick: move |_| open_diff(app, &project, &place),
            span { class: "row-icon",
                Svg { icon: Icon::Diff, size: 12 }
            }
            span { class: "name", "Changes" }
        }
    }
}

/// A shell in a workspace's checkout: click to open another terminal pane.
#[component]
fn TerminalRow(project: ProjectId, place: ReportSource, depth: u8) -> Element {
    let app = use_context::<AppSignals>();
    let open = app.layout.read().contents().any(|(_, content)| {
        matches!(content, PaneContent::Terminal { project: p, place: q } if *p == project && *q == place)
    });
    rsx! {
        div {
            class: if open { "row report-row open" } else { "row report-row" },
            style: "--depth: {depth}",
            title: "Open a shell in this checkout",
            onclick: move |_| open_terminal(app, &project, &place),
            span { class: "row-icon",
                Svg { icon: Icon::Terminal, size: 12 }
            }
            span { class: "name", "Terminal" }
        }
    }
}

/// An open agent pane, with its state: click to focus it and open its
/// workspace beside.
#[component]
fn AgentRow(agent: OpenAgent, depth: u8, on_open: EventHandler<()>) -> Element {
    let app = use_context::<AppSignals>();
    let mut layout = app.layout;
    let OpenAgent { pane, agent, tab } = agent;
    let (focused, current, many_tabs) = {
        let layout = layout.read();
        (
            layout.focused() == pane,
            layout.active() == tab,
            layout.tabs().len() > 1,
        )
    };
    let (state, label, _) = agent_status(app.sessions, pane);
    let session = session_title(app.sessions, pane);
    let hint = match &session {
        Some(session) => format!("{agent} · {session} · {label}"),
        None => format!("{agent} · {label}"),
    };
    // Rows in the active tab are marked as the tab bar marks it, so the two
    // read as one; the focused pane's row is also filled.
    let class = match (focused, current, is_unread(app.sessions, pane)) {
        (true, _, _) => "row agent-row current focused",
        (false, true, true) => "row agent-row current unread",
        (false, true, false) => "row agent-row current",
        (false, false, true) => "row agent-row unread",
        (false, false, false) => "row agent-row",
    };
    rsx! {
        div {
            class,
            style: "--depth: {depth}",
            title: "{hint}",
            onclick: move |_| {
                layout.write().focus(pane);
                on_open.call(());
            },
            span { class: "dot {state}" }
            span { class: "name", "{agent}" }
            if let Some(session) = session {
                span { class: "session-title", "{session}" }
            }
            span { class: "meta", "{label}" }
            span { class: "unread-mark", title: "Unread" }
            if many_tabs {
                span { class: "tab-number", title: "Tab {tab + 1}", "{tab + 1}" }
            }
        }
    }
}
