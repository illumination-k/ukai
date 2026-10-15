//! Tabs and pane trees the user arranges at runtime, rendered from
//! [`ukai_core::Layout`]. Each pane belongs to a project, so one tab can show
//! agents of several projects side by side.

use std::path::PathBuf;

use dioxus::prelude::*;
use std::collections::HashMap;
use ukai_acp::{PaneWorkspace, WorkspaceScope};
use ukai_config::AgentSpec;
use ukai_core::keymap::Action;
use ukai_core::{Direction, Layout, Node, PaneContent, PaneId, ProjectId, Projects, ReportSource};

use crate::agent_pane::{AgentView, HistoryMenu, Sessions, agent_status, connect};
use crate::alert::{is_unread, mark_read};
use crate::icons::{Icon, Svg};
use crate::projects::AppSignals;
use crate::report_pane::{ReportView, pick_report};
use crate::schedules::{ScheduleView, schedule_label};
use crate::shortcuts::Shortcuts;

/// Open `agent` of `project` in `pane` and start its session. A gateway
/// agent gets a workspace of the pane's own: `scope`, else the agent's default.
pub fn open_agent(
    app: AppSignals,
    pane: PaneId,
    project: &ProjectId,
    agent: &str,
    scope: Option<WorkspaceScope>,
) {
    let AppSignals {
        mut layout,
        mut projects,
        configs,
        ..
    } = app;
    let default_scope = match configs.peek().get(project) {
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

/// Open `agent` of `project` where [`launch_target`] puts it.
pub fn launch_agent(app: AppSignals, project: &ProjectId, agent: &str) {
    if let Some(pane) = launch_target(app) {
        open_agent(app, pane, project, agent, None);
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
        Some(PaneContent::Schedule {
            project, schedule, ..
        }) => (
            Some(project_name(project)),
            schedule_label(schedules, schedule.as_ref()),
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
    let root = app.layout.read().active_tab().root.clone();
    rsx! {
        div { class: "workbench",
            TabBar {}
            div { class: "panes",
                NodeView { node: root }
            }
        }
    }
}

#[component]
fn TabBar() -> Element {
    let app = use_context::<AppSignals>();
    let shortcuts = use_context::<Shortcuts>();
    let mut layout = app.layout;
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
                title: shortcuts.title("New tab", Action::NewTab),
                onclick: move |_| {
                    layout.write().new_tab();
                },
                Svg { icon: Icon::Plus }
            }
        }
    }
}

#[component]
fn NodeView(node: Node) -> Element {
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
            rsx! {
                div { class,
                    for (child, size) in children.into_iter().zip(sizes) {
                        div { class: "split-child", style: "flex: {size} 1 0",
                            NodeView { node: child }
                        }
                    }
                }
            }
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

    rsx! {
        section {
            class: if focused { "pane focused" } else { "pane" },
            onmousedown: move |_| {
                if layout.peek().focused() != pane {
                    layout.write().focus(pane);
                }
                mark_read(app.sessions, pane);
            },
            header {
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
                PaneContent::Report { project, path, source } => rsx! {
                    ReportView {
                        pane,
                        project,
                        path,
                        source,
                    }
                },
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
                        button {
                            class: "outline open-file",
                            title: "Show an HTML report or image from this project",
                            onclick: move |_| pick_report(app, Some(pane), project.clone()),
                            Svg { icon: Icon::Document }
                            "Open report or image…"
                        }
                    }
                }
            }
        }
    }
}

/// One way to open an agent in the picker. A gateway agent is offered once
/// per workspace scope, its default first.
struct Choice {
    agent: String,
    line: String,
    badge: &'static str,
    scope: Option<WorkspaceScope>,
}

fn choices((agent, spec): (&String, &AgentSpec)) -> Vec<Choice> {
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
