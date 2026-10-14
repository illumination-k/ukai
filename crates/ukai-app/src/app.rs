use std::collections::HashMap;

use dioxus::prelude::*;
use ukai_core::{Direction, ProjectId};

use crate::Startup;
use crate::agent_pane::agent_status;
use crate::icons::{Icon, Svg};
use crate::layout::{Workbench, close_pane, launch_target, open_agent, transport_label};
use crate::persist::use_autosave;
use crate::projects::{
    AppSignals, OpenAgent, Paths, add_project, load_config, open_agents, pick_and_add_project,
    reload_project, remove_project,
};
use crate::report_pane::ReportList;

const MAIN_CSS: Asset = asset!("/assets/main.css");

#[component]
pub fn App() -> Element {
    let startup = use_context::<Startup>();
    let paths = use_context_provider(|| startup.paths.clone());
    let app = use_context_provider(|| {
        let state = startup.state.clone();
        let configs = state
            .projects
            .iter()
            .map(|project| {
                (
                    project.id.clone(),
                    load_config(&startup.paths, &project.root),
                )
            })
            .collect::<HashMap<_, _>>();
        AppSignals {
            projects: Signal::new(state.projects),
            configs: Signal::new(configs),
            layout: Signal::new(state.layout),
            sessions: Signal::new(HashMap::new()),
        }
    });
    use_hook(|| {
        if let Some(root) = &startup.open {
            add_project(&paths, app, root);
        }
    });
    use_autosave(paths.state_file.clone(), app.projects, app.layout);

    let mut layout = app.layout;
    let shortcuts = move |event: KeyboardEvent| {
        let modifiers = event.modifiers();
        let primary = if cfg!(target_os = "macos") {
            modifiers.meta()
        } else {
            modifiers.ctrl()
        };
        let Key::Character(key) = event.key() else {
            return;
        };
        if !primary {
            return;
        }
        let focused = layout.peek().focused();
        match key.to_lowercase().as_str() {
            "t" => {
                layout.write().new_tab();
            }
            "d" => {
                let direction = if modifiers.shift() {
                    Direction::Vertical
                } else {
                    Direction::Horizontal
                };
                layout.write().split(focused, direction);
            }
            "w" => close_pane(app, focused),
            _ => return,
        }
        event.prevent_default();
    };

    rsx! {
        document::Stylesheet { href: MAIN_CSS }
        div { class: "shell", tabindex: "0", onkeydown: shortcuts,
            Sidebar {}
            Workbench {}
        }
    }
}

/// Projects, each with the agent panes open for it (click to focus), and the
/// agents the selected project can launch. Launching opens the agent in the
/// focused pane when that pane is empty, otherwise in a new pane split to its
/// right.
#[component]
fn Sidebar() -> Element {
    let app = use_context::<AppSignals>();
    let paths = use_context::<Paths>();
    let mut projects_signal = app.projects;
    let mut layout = app.layout;
    let (mut open, focused, many_tabs) = {
        let layout = app.layout.read();
        (
            open_agents(&layout),
            layout.focused(),
            layout.tabs().len() > 1,
        )
    };
    let projects = app.projects.read();
    let selected = projects.selected().map(|p| p.id.clone());
    let groups: Vec<(ProjectId, String, String, Vec<OpenAgent>)> = projects
        .iter()
        .map(|project| {
            (
                project.id.clone(),
                project.name(),
                project.root.display().to_string(),
                open.remove(&project.id).unwrap_or_default(),
            )
        })
        .collect();
    let selected_name = projects.selected().map(|p| p.name());
    let report_project = selected.clone();
    let report_project_name = selected_name.clone();
    drop(projects);
    let agents: Result<Vec<(String, &'static str)>, String> = match &selected {
        None => Ok(Vec::new()),
        Some(id) => match app.configs.read().get(id) {
            Some(Ok(config)) => Ok(config
                .agents
                .iter()
                .map(|(agent, spec)| (agent.clone(), transport_label(spec)))
                .collect()),
            Some(Err(error)) => Err(error.clone()),
            None => Ok(Vec::new()),
        },
    };
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
            if groups.is_empty() {
                div { class: "sidebar-empty",
                    p { class: "muted", "Open a project folder to start." }
                    button { class: "outline", onclick: pick(),
                        Svg { icon: Icon::Folder }
                        "Open folder…"
                    }
                }
            }
            div { class: "project-list",
                for (id, name, root, agents) in groups {
                    div { key: "{id}", class: "project-group",
                        div {
                            class: if selected.as_ref() == Some(&id) { "row project-row selected" } else { "row project-row" },
                            title: "{root}",
                            onclick: {
                                let id = id.clone();
                                move |_| projects_signal.write().select(&id)
                            },
                            span { class: "row-icon",
                                Svg { icon: Icon::Folder }
                            }
                            span { class: "name", "{name}" }
                            if !agents.is_empty() {
                                span { class: "count", "{agents.len()}" }
                            }
                            span { class: "row-actions",
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
                        }
                        for OpenAgent { pane, agent, tab } in agents {
                            {
                                let (state, label, _) = agent_status(app.sessions, pane);
                                rsx! {
                                    div {
                                        key: "{pane}",
                                        class: if pane == focused { "row agent-row focused" } else { "row agent-row" },
                                        title: "{agent} · {label}",
                                        onclick: move |_| layout.write().focus(pane),
                                        span { class: "dot {state}" }
                                        span { class: "name", "{agent}" }
                                        span { class: "meta", "{label}" }
                                        if many_tabs {
                                            span { class: "tab-number", title: "Tab {tab + 1}", "{tab + 1}" }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            if let (Some(project), Some(project_name)) = (selected, selected_name) {
                div { class: "sidebar-section",
                    div { class: "sidebar-header",
                        span { class: "section-label", "Launch · {project_name}" }
                    }
                    match agents {
                        Err(error) => rsx! {
                            pre { class: "config-error", "{error}" }
                        },
                        Ok(agents) if agents.is_empty() => rsx! {
                            p { class: "muted hint", "No agents declared in ukai.toml." }
                        },
                        Ok(agents) => rsx! {
                            for (agent, transport) in agents {
                                div {
                                    key: "{agent}",
                                    class: "row launch-row",
                                    title: "Open {agent} in a pane",
                                    onclick: {
                                        let project = project.clone();
                                        let agent = agent.clone();
                                        move |_| open_in_focus(app, &project, &agent)
                                    },
                                    span { class: "row-icon",
                                        Svg { icon: Icon::Plus, size: 12 }
                                    }
                                    span { class: "name", "{agent}" }
                                    span { class: "meta", "{transport}" }
                                }
                            }
                        },
                    }
                }
            }
            if let (Some(project), Some(name)) = (report_project, report_project_name) {
                ReportList { key: "{project}", project, name }
            }
            div { class: "sidebar-footer",
                span {
                    kbd { "{MOD_KEY}T" }
                    " tab"
                }
                span {
                    kbd { "{MOD_KEY}D" }
                    " split"
                }
                span {
                    kbd { "{MOD_KEY}W" }
                    " close"
                }
            }
        }
    }
}

/// How the primary modifier is written in hints.
pub const MOD_KEY: &str = if cfg!(target_os = "macos") {
    "⌘"
} else {
    "Ctrl+"
};

fn open_in_focus(app: AppSignals, project: &ProjectId, agent: &str) {
    if let Some(pane) = launch_target(app) {
        open_agent(app, pane, project, agent, None);
    }
}
