//! Tabs and pane trees the user arranges at runtime, rendered from
//! [`ukai_core::Layout`]. Each pane belongs to a project, so one tab can show
//! agents of several projects side by side.

use dioxus::prelude::*;
use ukai_config::AgentSpec;
use ukai_core::{Direction, Layout, Node, PaneContent, PaneId, ProjectId, Projects};

use crate::agent_pane::{AgentView, agent_status, connect};
use crate::projects::AppSignals;

/// Open `agent` of `project` in `pane` and start its session.
pub fn open_agent(app: AppSignals, pane: PaneId, project: &ProjectId, agent: &str) {
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
        },
    );
    projects.write().select(project);
    connect(app, pane, false);
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

fn pane_label(layout: &Layout, projects: &Projects, pane: PaneId) -> String {
    let project_name = |id: &ProjectId| {
        projects
            .get(id)
            .map_or_else(|| id.to_string(), |p| p.name())
    };
    match layout.content(pane) {
        Some(PaneContent::Agent { project, agent, .. }) => {
            format!("{} / {agent}", project_name(project))
        }
        Some(PaneContent::Report { project, path }) => {
            let file = path.file_name().map_or_else(
                || path.display().to_string(),
                |name| name.to_string_lossy().into_owned(),
            );
            format!("{} / {file}", project_name(project))
        }
        Some(PaneContent::Empty) | None => "new pane".to_owned(),
    }
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
    let mut layout = app.layout;
    let (tabs, active) = {
        let current = layout.read();
        let projects = app.projects.read();
        let tabs: Vec<(usize, String)> = current
            .tabs()
            .iter()
            .enumerate()
            .map(|(index, tab)| {
                let panes = tab.root.panes();
                let first = panes
                    .first()
                    .map(|pane| pane_label(&current, &projects, *pane));
                let label = match (first, panes.len()) {
                    (Some(first), 1) => first,
                    (Some(first), n) => format!("{first} +{}", n - 1),
                    (None, _) => "tab".to_owned(),
                };
                (index, label)
            })
            .collect();
        (tabs, current.active())
    };

    rsx! {
        nav { class: "tabbar",
            for (index, label) in tabs {
                div {
                    key: "{index}",
                    class: if index == active { "tab active" } else { "tab" },
                    onclick: move |_| layout.write().select_tab(index),
                    span { class: "name", "{label}" }
                    button {
                        class: "icon",
                        title: "Close tab",
                        onclick: move |event| {
                            event.stop_propagation();
                            close_tab(app, index);
                        },
                        "×"
                    }
                }
            }
            button {
                class: "icon new-tab",
                title: "New tab (mod+t)",
                onclick: move |_| {
                    layout.write().new_tab();
                },
                "+"
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
    let mut layout = app.layout;
    let (content, focused, label) = {
        let current = layout.read();
        (
            current.content(pane).cloned().unwrap_or(PaneContent::Empty),
            current.focused() == pane,
            pane_label(&current, &app.projects.read(), pane),
        )
    };
    let (status, title) = match &content {
        PaneContent::Agent { .. } => {
            let (class, text, name) = agent_status(app.sessions, pane);
            let title = name.map_or_else(|| label.clone(), |name| format!("{label} · {name}"));
            (Some((class, text)), title)
        }
        _ => (None, label),
    };

    rsx! {
        section {
            class: if focused { "pane focused" } else { "pane" },
            onmousedown: move |_| {
                if layout.peek().focused() != pane {
                    layout.write().focus(pane);
                }
            },
            header {
                span { class: "name", "{title}" }
                if let Some((class, text)) = status {
                    span { class: "status {class}", "{text}" }
                }
                button {
                    class: "icon",
                    title: "Split right (mod+d)",
                    onclick: move |_| {
                        layout.write().split(pane, Direction::Horizontal);
                    },
                    "◫"
                }
                button {
                    class: "icon",
                    title: "Split down (mod+shift+d)",
                    onclick: move |_| {
                        layout.write().split(pane, Direction::Vertical);
                    },
                    "⊟"
                }
                button {
                    class: "icon",
                    title: "Close pane (mod+w)",
                    onclick: move |_| close_pane(app, pane),
                    "×"
                }
            }
            match content {
                PaneContent::Empty => rsx! {
                    Picker { pane }
                },
                PaneContent::Agent { session, .. } => rsx! {
                    AgentView { pane, has_saved_session: session.is_some() }
                },
                PaneContent::Report { path, .. } => rsx! {
                    div { class: "pane-body placeholder", "report: {path.display()}" }
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
    let agents: Result<Vec<(String, String, &'static str)>, String> = match &current {
        None => Ok(Vec::new()),
        Some(id) => match app.configs.read().get(id) {
            Some(Ok(config)) => Ok(config
                .agents
                .iter()
                .map(|(agent, spec)| (agent.clone(), launch_line(spec), transport_label(spec)))
                .collect()),
            Some(Err(error)) => Err(error.clone()),
            None => Ok(Vec::new()),
        },
    };

    rsx! {
        div { class: "pane-body picker",
            if projects.is_empty() {
                p { class: "muted", "Open a project from the sidebar to start." }
            } else {
                div { class: "project-chips",
                    for (id, name) in projects {
                        button {
                            key: "{id}",
                            class: if current.as_ref() == Some(&id) { "small primary" } else { "small" },
                            onclick: {
                                let id = id.clone();
                                move |_| chosen.set(Some(id.clone()))
                            },
                            "{name}"
                        }
                    }
                }
                match agents {
                    Err(error) => rsx! {
                        pre { class: "notice-line error", "{error}" }
                    },
                    Ok(agents) if agents.is_empty() => rsx! {
                        p { class: "muted",
                            "No agents for this project. Declare them in its ukai.toml or in ~/.config/ukai/config.toml."
                        }
                    },
                    Ok(agents) => rsx! {
                        for (agent, line, transport) in agents {
                            button {
                                key: "{agent}",
                                class: "choice",
                                onclick: {
                                    let project = current.clone();
                                    let agent = agent.clone();
                                    move |_| {
                                        if let Some(project) = &project {
                                            open_agent(app, pane, project, &agent);
                                        }
                                    }
                                },
                                span { class: "name", "{agent}" }
                                span { class: "badge", "{transport}" }
                                code { "{line}" }
                            }
                        }
                    },
                }
            }
        }
    }
}

pub fn transport_label(spec: &AgentSpec) -> &'static str {
    match spec {
        AgentSpec::Http { .. } => "http",
        AgentSpec::Websocket { .. } => "ws",
        AgentSpec::Stdio { .. } => "stdio",
    }
}

fn launch_line(spec: &AgentSpec) -> String {
    match spec {
        AgentSpec::Http { url, .. } | AgentSpec::Websocket { url, .. } => url.clone(),
        AgentSpec::Stdio { command, args, .. } => std::iter::once(command.as_str())
            .chain(args.iter().map(String::as_str))
            .collect::<Vec<_>>()
            .join(" "),
    }
}
