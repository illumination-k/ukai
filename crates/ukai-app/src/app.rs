use std::collections::HashMap;

use dioxus::desktop::tao::event::{Event, WindowEvent};
use dioxus::desktop::use_wry_event_handler;
use dioxus::prelude::*;
use ukai_core::ProjectId;
use ukai_core::keymap::Action;

use crate::Startup;
use crate::agent_pane::agent_status;
use crate::alert::{is_unread, mark_read};
use crate::icons::{Icon, Svg};
use crate::knowledge::KnowledgeList;
use crate::layout::{Workbench, launch_agent, transport_label};
use crate::palette::Palette;
use crate::persist::use_autosave;
use crate::ports::{FollowPorts, PortList};
use crate::projects::{
    AppSignals, OpenAgent, Paths, add_project, load_config, open_agents, pick_and_add_project,
    reload_project, remove_project,
};
use crate::remote_reports::Follow;
use crate::report_pane::ReportList;
use crate::schedules::ScheduleList;
use crate::shortcuts::{Shortcuts, on_keydown};
use crate::voice::Voice;

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
            reports: Signal::new(HashMap::new()),
            ports: Signal::new(HashMap::new()),
            palette: Signal::new(false),
            schedule_names: Signal::new(HashMap::new()),
            schedule_revision: Signal::new(0),
            knowledge_revision: Signal::new(0),
        }
    });
    use_hook(|| {
        if let Some(root) = &startup.open {
            add_project(&paths, app, root);
        }
    });
    use_autosave(paths.state_file.clone(), app.projects, app.layout);

    // Whatever pane the user is looking at has nothing unread: the focused
    // one, once focus moves to it or the window comes back to the front.
    use_effect(move || {
        let focused = app.layout.read().focused();
        mark_read(app.sessions, focused);
    });
    use_wry_event_handler(move |event, _| {
        if let Event::WindowEvent {
            event: WindowEvent::Focused(true),
            ..
        } = event
        {
            mark_read(app.sessions, app.layout.peek().focused());
        }
    });

    let shortcuts = use_context_provider(|| Shortcuts::load(&paths));
    use_context_provider(|| Voice::load(&paths));
    let palette = app.palette;

    rsx! {
        document::Stylesheet { href: MAIN_CSS }
        div {
            class: "shell",
            tabindex: "0",
            onkeydown: move |event| on_keydown(app, &shortcuts, &event),
            Sidebar {}
            Workbench {}
            Follow {}
            FollowPorts {}
            if palette() {
                Palette {}
            }
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
    let shortcuts = use_context::<Shortcuts>();
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
    let schedule_project = selected.clone();
    let schedule_project_name = selected_name.clone();
    let knowledge_project = selected.clone();
    let knowledge_project_name = selected_name.clone();
    let port_project = selected.clone();
    let port_project_name = selected_name.clone();
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
                                let class = match (pane == focused, is_unread(app.sessions, pane)) {
                                    (true, _) => "row agent-row focused",
                                    (false, true) => "row agent-row unread",
                                    (false, false) => "row agent-row",
                                };
                                rsx! {
                                    div {
                                        key: "{pane}",
                                        class,
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
                                        move |_| launch_agent(app, &project, &agent)
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
            if let (Some(project), Some(name)) = (schedule_project, schedule_project_name) {
                ScheduleList { key: "schedules-{project}", project, name }
            }
            if let (Some(project), Some(name)) = (knowledge_project, knowledge_project_name) {
                KnowledgeList { key: "knowledge-{project}", project, name }
            }
            if let (Some(project), Some(name)) = (report_project, report_project_name) {
                ReportList { key: "{project}", project, name }
            }
            if let (Some(project), Some(name)) = (port_project, port_project_name) {
                PortList { key: "ports-{project}", project, name }
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
