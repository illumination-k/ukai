use std::collections::HashMap;

use dioxus::prelude::*;
use ukai_core::{Direction, PaneContent, ProjectId};

use crate::Startup;
use crate::layout::{Workbench, close_pane, open_agent, transport_label};
use crate::persist::use_autosave;
use crate::projects::{
    AppSignals, Paths, add_project, agent_pane_counts, load_config, pick_and_add_project,
    reload_project, remove_project,
};

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

/// Projects, and the agents of the selected one. Clicking an agent opens it
/// in the focused pane when that pane is empty, otherwise in a new pane split
/// to its right.
#[component]
fn Sidebar() -> Element {
    let app = use_context::<AppSignals>();
    let paths = use_context::<Paths>();
    let mut projects_signal = app.projects;
    let counts = agent_pane_counts(&app.layout.read());
    let projects = app.projects.read();
    let selected = projects.selected().map(|p| p.id.clone());
    let rows: Vec<(ProjectId, String, String, usize)> = projects
        .iter()
        .map(|project| {
            (
                project.id.clone(),
                project.name(),
                project.root.display().to_string(),
                counts.get(&project.id).copied().unwrap_or(0),
            )
        })
        .collect();
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

    rsx! {
        nav { class: "sidebar",
            section {
                div { class: "section-title",
                    h2 { "Projects" }
                    button {
                        class: "icon",
                        title: "Open project…",
                        onclick: {
                            let paths = paths.clone();
                            move |_| pick_and_add_project(paths.clone(), app)
                        },
                        "+"
                    }
                }
                if rows.is_empty() {
                    p { class: "muted hint", "Open a project folder to start." }
                }
                ul {
                    for (id, name, root, count) in rows {
                        li {
                            key: "{id}",
                            class: if selected.as_ref() == Some(&id) { "selected" } else { "" },
                            title: "{root}",
                            onclick: {
                                let id = id.clone();
                                move |_| projects_signal.write().select(&id)
                            },
                            span { class: "name", "{name}" }
                            if count > 0 {
                                span { class: "badge", "{count}" }
                            }
                            button {
                                class: "icon",
                                title: "Reload ukai.toml",
                                onclick: {
                                    let id = id.clone();
                                    let paths = paths.clone();
                                    move |event: MouseEvent| {
                                        event.stop_propagation();
                                        reload_project(&paths, app, &id);
                                    }
                                },
                                "↻"
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
                                "×"
                            }
                        }
                    }
                }
            }
            if let Some(project) = selected {
                section {
                    h2 { "Agents" }
                    match agents {
                        Err(error) => rsx! {
                            pre { class: "notice-line error", "{error}" }
                        },
                        Ok(agents) => rsx! {
                            ul {
                                for (agent, transport) in agents {
                                    li {
                                        key: "{agent}",
                                        title: "Open in a pane",
                                        onclick: {
                                            let project = project.clone();
                                            let agent = agent.clone();
                                            move |_| open_in_focus(app, &project, &agent)
                                        },
                                        span { class: "name", "{agent}" }
                                        span { class: "badge", "{transport}" }
                                    }
                                }
                            }
                        },
                    }
                }
            }
        }
    }
}

fn open_in_focus(app: AppSignals, project: &ProjectId, agent: &str) {
    let mut layout = app.layout;
    let focused = layout.peek().focused();
    let pane = if layout.peek().content(focused) == Some(&PaneContent::Empty) {
        Some(focused)
    } else {
        layout.write().split(focused, Direction::Horizontal)
    };
    if let Some(pane) = pane {
        open_agent(app, pane, project, agent, None);
    }
}
