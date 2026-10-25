//! The sidebar: a tree of the open projects. Under each, the workspaces its
//! agents work in — this machine's checkout, and the gateway workspaces —
//! each with its agent panes (click to focus), reports, and ports; beside
//! them, the knowledge and schedules of the project's gateway agents. What
//! goes where is [`ukai_core::sidebar`]'s; this draws it.

use std::collections::HashMap;

use dioxus::prelude::*;
use ukai_acp::WorkspaceScope;
use ukai_config::AgentSpec;
use ukai_core::keymap::Action;
use ukai_core::sidebar::{self, Fold, Node, OpenAgent, Workspace};
use ukai_core::{ProjectId, ReportSource};

use crate::agent_pane::agent_status;
use crate::alert::is_unread;
use crate::icons::{Icon, Svg};
use crate::knowledge::KnowledgeNode;
use crate::layout::{Choice, choices, launch_agent, launch_target, open_agent_in};
use crate::ports::PortGroup;
use crate::projects::{AppSignals, Paths, pick_and_add_project, reload_project, remove_project};
use crate::report_pane::ReportGroup;
use crate::schedules::ScheduleNode;
use crate::shortcuts::Shortcuts;

#[component]
pub fn Sidebar() -> Element {
    let app = use_context::<AppSignals>();
    let paths = use_context::<Paths>();
    let shortcuts = use_context::<Shortcuts>();
    let projects: Vec<(ProjectId, String, String)> = app
        .projects
        .read()
        .iter()
        .map(|project| {
            (
                project.id.clone(),
                project.name(),
                project.root.display().to_string(),
            )
        })
        .collect();
    let pick = {
        let paths = paths.clone();
        move || {
            let paths = paths.clone();
            move |_| pick_and_add_project(paths.clone(), app)
        }
    };

    rsx! {
        nav { class: "sidebar",
            div { class: "sidebar-header",
                span { class: "section-label", "Projects" }
                button {
                    class: "icon",
                    title: "Open project…",
                    onclick: pick(),
                    Svg { icon: Icon::Plus }
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
            div { class: "tree",
                for (id, name, root) in projects {
                    ProjectNode {
                        key: "{id}",
                        id,
                        name,
                        root,
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
    #[props(default)] on_toggle: Option<EventHandler<()>>,
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
                if let Some(on_toggle) = on_toggle {
                    on_toggle.call(());
                }
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

/// One project: its workspaces, then its knowledge and schedules when it has
/// a gateway agent. A project with only this machine's checkout lists its
/// agents, reports, and ports right under it.
#[component]
fn ProjectNode(id: ProjectId, name: String, root: String) -> Element {
    let app = use_context::<AppSignals>();
    let paths = use_context::<Paths>();
    let mut launching = use_signal(|| false);
    let mut projects = app.projects;
    let (tree, problem, launch) = {
        let configs = app.configs.read();
        let config = configs.get(&id);
        let tree = sidebar::tree(
            &app.layout.read(),
            &id,
            config.and_then(|config| config.as_ref().ok()),
        );
        let problem = config.and_then(|config| config.as_ref().err()).cloned();
        let launch: Vec<Choice> = config
            .and_then(|config| config.as_ref().ok())
            .map(|config| config.agents.iter().flat_map(choices).collect())
            .unwrap_or_default();
        (tree, problem, launch)
    };
    let fold = Fold::new(&id, Node::Project);
    let mut folds = app.folds;
    let open = folds.read().is_open(&fold);
    let count = match tree.agent_count() {
        0 => String::new(),
        n => n.to_string(),
    };
    let flat = tree.flat();
    let launch_fold = fold.clone();

    rsx! {
        div { class: "project-group",
            Head {
                fold: fold.clone(),
                depth: 0,
                icon: Icon::Folder,
                label: name,
                meta: count,
                title: root,
                class: "project-row",
                on_toggle: {
                    let id = id.clone();
                    move |()| projects.write().select(&id)
                },
                button {
                    class: if launching() { "icon open" } else { "icon" },
                    title: "Open an agent…",
                    onclick: move |event: MouseEvent| {
                        event.stop_propagation();
                        folds.write().open(launch_fold.clone());
                        launching.toggle();
                    },
                    Svg { icon: Icon::Plus }
                }
                button {
                    class: "icon",
                    title: "Reload configuration",
                    onclick: {
                        let id = id.clone();
                        let paths = paths.clone();
                        move |event: MouseEvent| {
                            event.stop_propagation();
                            reload_project(&paths, app, &id);
                        }
                    },
                    Svg { icon: Icon::Reload }
                }
                button {
                    class: "icon",
                    title: "Close project (ends its sessions)",
                    onclick: {
                        let id = id.clone();
                        move |event: MouseEvent| {
                            event.stop_propagation();
                            remove_project(app, &id);
                        }
                    },
                    Svg { icon: Icon::Close }
                }
            }
            if open {
                if launching() {
                    if launch.is_empty() {
                        p {
                            class: "muted hint tree-hint",
                            style: "--depth: 1",
                            "No agents declared in ukai.toml."
                        }
                    }
                    for Choice { agent, badge, scope, line } in launch {
                        div {
                            key: "launch-{agent}-{badge}",
                            class: "row launch-row",
                            style: "--depth: 1",
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
                            span { class: "name", "{agent}" }
                            span { class: "meta", "{badge}" }
                        }
                    }
                }
                if let Some(problem) = problem {
                    pre { class: "config-error tree-hint", style: "--depth: 1", "{problem}" }
                }
                for workspace in tree.workspaces {
                    WorkspaceNode {
                        key: "{place_key(&workspace.place)}",
                        project: id.clone(),
                        workspace,
                        depth: 1,
                        flat,
                    }
                }
                if !tree.gateways.is_empty() {
                    KnowledgeNode { project: id.clone(), depth: 1 }
                    ScheduleNode {
                        project: id.clone(),
                        gateways: tree.gateways.clone(),
                        depth: 1,
                    }
                }
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

/// A workspace: its agent panes, reports, and ports. With `flat`, it is the
/// project's only one, so it has no row of its own.
#[component]
fn WorkspaceNode(project: ProjectId, workspace: Workspace, depth: u8, flat: bool) -> Element {
    let app = use_context::<AppSignals>();
    let mut choosing = use_signal(|| false);
    let Workspace { place, agents } = workspace;
    let fold = Fold::new(
        &project,
        Node::Workspace {
            place: place.clone(),
        },
    );
    let mut folds = app.folds;
    let open = flat || folds.read().is_open(&fold);
    let inner = if flat { depth } else { depth + 1 };
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
    let new_session = {
        let project = project.clone();
        let place = place.clone();
        let local = local.clone();
        let fold = fold.clone();
        move |event: MouseEvent| {
            event.stop_propagation();
            folds.write().open(fold.clone());
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
        if !flat {
            Head {
                fold: fold.clone(),
                depth,
                icon,
                label,
                meta,
                title,
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
        if open {
            if choosing() {
                for agent in local {
                    div {
                        key: "launch-{agent}",
                        class: "row launch-row",
                        style: "--depth: {inner}",
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
                AgentRow { key: "{agent.pane}", agent, depth: inner }
            }
            ReportGroup {
                project: project.clone(),
                place: place.clone(),
                depth: inner,
            }
            PortGroup {
                project: project.clone(),
                place: place.clone(),
                depth: inner,
            }
        }
    }
}

/// An open agent pane, with its state: click to focus it.
#[component]
fn AgentRow(agent: OpenAgent, depth: u8) -> Element {
    let app = use_context::<AppSignals>();
    let mut layout = app.layout;
    let OpenAgent { pane, agent, tab } = agent;
    let (focused, many_tabs) = {
        let layout = layout.read();
        (layout.focused() == pane, layout.tabs().len() > 1)
    };
    let (state, label, _) = agent_status(app.sessions, pane);
    let class = match (focused, is_unread(app.sessions, pane)) {
        (true, _) => "row agent-row focused",
        (false, true) => "row agent-row unread",
        (false, false) => "row agent-row",
    };
    rsx! {
        div {
            class,
            style: "--depth: {depth}",
            title: "{agent} · {label}",
            onclick: move |_| layout.write().focus(pane),
            span { class: "dot {state}" }
            span { class: "name", "{agent}" }
            span { class: "meta", "{label}" }
            span { class: "unread-mark", title: "Unread" }
            if many_tabs {
                span { class: "tab-number", title: "Tab {tab + 1}", "{tab + 1}" }
            }
        }
    }
}
