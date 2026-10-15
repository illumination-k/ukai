//! The command palette (`palette`, `mod+k` by default): one fuzzy search over open panes, the
//! agents every project can launch, their reports, and the layout commands.
//!
//! Report panes are native views above the DOM, so they hide while the
//! palette is open (see `report_pane`).

use std::path::PathBuf;

use dioxus::prelude::*;
use ukai_core::{Direction, PaneContent, PaneId, ProjectId, ReportSource, keymap, palette, report};

use crate::icons::{Icon, Svg};
use crate::layout::{close_pane, launch_agent, launch_target, open_report, pane_label};
use crate::projects::{AppSignals, Paths, pick_and_add_project, project_context};
use crate::remote_reports::Listing;
use crate::shortcuts::Shortcuts;

/// Rows shown at once; the query narrows the rest.
const LIMIT: usize = 50;

#[derive(Debug, Clone, PartialEq)]
enum Action {
    Focus(PaneId),
    Launch(ProjectId, String),
    Report(ProjectId, PathBuf, ReportSource),
    NewTab,
    Split(Direction),
    ClosePane,
    OpenProject,
}

#[derive(Debug, Clone, PartialEq)]
struct Item {
    kind: &'static str,
    label: String,
    detail: String,
    action: Action,
}

/// Everything the palette offers, gathered when it opens.
fn items(app: AppSignals, shortcuts: &Shortcuts) -> Vec<Item> {
    let layout = app.layout.peek();
    let projects = app.projects.peek();
    let configs = app.configs.peek();
    let mut items = Vec::new();
    for (tab, panes) in layout.tabs().iter().map(|t| t.root.panes()).enumerate() {
        for pane in panes {
            if pane == layout.focused()
                || matches!(layout.content(pane), Some(PaneContent::Empty) | None)
            {
                continue;
            }
            let (project, label) = pane_label(&layout, &projects, &app.schedule_names.peek(), pane);
            items.push(Item {
                kind: "Go to",
                label,
                detail: match project {
                    Some(project) => format!("{project} · tab {}", tab + 1),
                    None => format!("tab {}", tab + 1),
                },
                action: Action::Focus(pane),
            });
        }
    }
    for project in projects.iter() {
        if let Some(Ok(config)) = configs.get(&project.id) {
            for agent in config.agents.keys() {
                items.push(Item {
                    kind: "Open agent",
                    label: agent.clone(),
                    detail: project.name(),
                    action: Action::Launch(project.id.clone(), agent.clone()),
                });
            }
        }
    }
    for project in projects.iter() {
        if let Ok(context) = project_context(&projects, &configs, &project.id) {
            for path in report::list(&context.root, &context.config.reports) {
                items.push(Item {
                    kind: "Open report",
                    label: path.display().to_string(),
                    detail: project.name(),
                    action: Action::Report(project.id.clone(), path, ReportSource::Project),
                });
            }
        }
    }
    // What the gateway agents' workspaces last listed; `remote_reports`
    // keeps it, so the palette does not wait on the network to open.
    for (workspace, listing) in app.reports.peek().iter() {
        let Listing::Reports(paths) = listing else {
            continue;
        };
        let project = projects
            .get(&workspace.project)
            .map_or_else(|| workspace.project.to_string(), |project| project.name());
        for path in paths {
            items.push(Item {
                kind: "Open report",
                label: path.display().to_string(),
                detail: format!("{project} · {}", workspace.label()),
                action: Action::Report(workspace.project.clone(), path.clone(), workspace.source()),
            });
        }
    }
    let command = |label: &str, action, shortcut: Option<keymap::Action>| Item {
        kind: "Command",
        label: label.to_owned(),
        detail: shortcut
            .and_then(|shortcut| shortcuts.keymap.hint(shortcut))
            .unwrap_or_default(),
        action,
    };
    items.extend([
        command("New tab", Action::NewTab, Some(keymap::Action::NewTab)),
        command(
            "Split right",
            Action::Split(Direction::Horizontal),
            Some(keymap::Action::SplitRight),
        ),
        command(
            "Split down",
            Action::Split(Direction::Vertical),
            Some(keymap::Action::SplitDown),
        ),
        command(
            "Close pane",
            Action::ClosePane,
            Some(keymap::Action::ClosePane),
        ),
        command("Open project…", Action::OpenProject, None),
    ]);
    items
}

fn perform(app: AppSignals, paths: &Paths, action: Action) {
    let mut layout = app.layout;
    let focused = layout.peek().focused();
    match action {
        Action::Focus(pane) => layout.write().focus(pane),
        Action::Launch(project, agent) => launch_agent(app, &project, &agent),
        Action::Report(project, path, source) => {
            if let Some(pane) = launch_target(app) {
                open_report(app, pane, &project, path, source);
            }
        }
        Action::NewTab => {
            layout.write().new_tab();
        }
        Action::Split(direction) => {
            layout.write().split(focused, direction);
        }
        Action::ClosePane => close_pane(app, focused),
        Action::OpenProject => pick_and_add_project(paths.clone(), app),
    }
}

#[component]
pub fn Palette() -> Element {
    let app = use_context::<AppSignals>();
    let paths = use_context::<Paths>();
    let shortcuts = use_context::<Shortcuts>();
    let all = use_hook(|| items(app, &shortcuts));
    let mut query = use_signal(String::new);
    let mut selected = use_signal(|| 0_usize);
    let mut open = app.palette;

    let matches: Vec<Item> = palette::filter(
        &query.read(),
        all.iter().map(|item| {
            (
                format!("{} {} {}", item.kind, item.label, item.detail),
                item.clone(),
            )
        }),
    )
    .into_iter()
    .take(LIMIT)
    .collect();
    let current = selected().min(matches.len().saturating_sub(1));

    let mut close = move || {
        open.set(false);
        // Give the keyboard back to the app's shortcuts.
        document::eval("document.querySelector('.shell')?.focus()");
    };
    let run = {
        let paths = paths.clone();
        move |action: Action| {
            close();
            perform(app, &paths, action);
        }
    };
    let on_key = {
        let matches = matches.clone();
        let mut run = run.clone();
        move |event: KeyboardEvent| {
            let mut step = |delta: isize| {
                let len = matches.len().max(1);
                let next = (current as isize + delta).rem_euclid(len as isize);
                selected.set(next as usize);
                document::eval(
                    "requestAnimationFrame(() => document.querySelector('.palette-item.selected')?.scrollIntoView({ block: 'nearest' }))",
                );
            };
            match event.key() {
                Key::ArrowDown => step(1),
                Key::ArrowUp => step(-1),
                Key::Escape => close(),
                Key::Enter if !event.is_composing() => {
                    if let Some(item) = matches.get(current) {
                        run(item.action.clone());
                    }
                }
                _ => return,
            }
            event.prevent_default();
            event.stop_propagation();
        }
    };

    rsx! {
        div { class: "palette-backdrop", onclick: move |_| close(),
            div {
                class: "palette",
                onclick: move |event| event.stop_propagation(),
                div { class: "palette-input",
                    Svg { icon: Icon::Search }
                    input {
                        r#type: "text",
                        placeholder: "Go to a pane, open an agent or report, run a command",
                        value: "{query}",
                        spellcheck: "false",
                        onmounted: move |event| async move {
                            let _ = event.set_focus(true).await;
                        },
                        oninput: move |event| {
                            query.set(event.value());
                            selected.set(0);
                        },
                        onkeydown: on_key,
                    }
                }
                div { class: "palette-list",
                    if matches.is_empty() {
                        div { class: "palette-empty muted", "No matches" }
                    }
                    for (index, item) in matches.into_iter().enumerate() {
                        div {
                            key: "{index}-{item.kind}-{item.label}-{item.detail}",
                            class: if index == current { "palette-item selected" } else { "palette-item" },
                            onmouseenter: move |_| selected.set(index),
                            onclick: {
                                let mut run = run.clone();
                                let action = item.action.clone();
                                move |_| run(action.clone())
                            },
                            span { class: "kind", "{item.kind}" }
                            span { class: "label", "{item.label}" }
                            span { class: "detail", "{item.detail}" }
                        }
                    }
                }
            }
        }
    }
}
