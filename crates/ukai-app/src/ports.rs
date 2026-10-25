//! Ports of the workspaces gateway agents work in, and opening browser panes
//! on them.
//!
//! Like reports ([`crate::remote_reports`]), what listens in a pod is asked of
//! the gateway, per workspace the app follows: [`FollowPorts`] keeps every
//! workspace's listing in one signal for the sidebar to read. The listing
//! also carries the previews the agent asked for; one that is new since the
//! app last looked opens in a pane beside the agent's, when one is open.

use std::collections::HashMap;
use std::time::Duration;

use dioxus::prelude::*;
use ukai_acp::PortsEndpoint;
use ukai_acp::ports::{PortListing, PortsError, Preview};
use ukai_config::{AgentSpec, Config};
use ukai_core::browser::{new_previews, parse_target};
use ukai_core::sidebar::{Fold, Node};
use ukai_core::{Direction, PaneContent, PaneId, ProjectId, ReportSource};

use crate::icons::{Icon, Svg};
use crate::layout::launch_target;
use crate::projects::AppSignals;
use crate::remote_reports::{Endpoints, Workspace, declaration, followed, workspaces};
use crate::sidebar::Head;

/// How often the listings are read again while agent panes are open.
const RELIST: Duration = Duration::from_secs(3);

/// The last listing of a workspace's ports.
#[derive(Debug, Clone, PartialEq)]
pub enum Ports {
    Listed(PortListing),
    /// The pod is not running: the agent has to be opened again first.
    NotRunning,
    Failed(String),
}

/// Every workspace's last listing, kept by [`FollowPorts`].
pub type PortListings = Signal<HashMap<Workspace, Ports>>;

/// Where a workspace's ports are listed and reached.
pub fn endpoint(
    configs: &HashMap<ProjectId, Result<Config, String>>,
    workspace: &Workspace,
) -> Result<PortsEndpoint, String> {
    resolve(&declaration(configs, workspace)?, workspace)
}

fn resolve(spec: &AgentSpec, workspace: &Workspace) -> Result<PortsEndpoint, String> {
    PortsEndpoint::resolve(spec, ukai_acp::system_lookup, workspace.workspace.as_ref())
        .map_err(|error| error.to_string())?
        .ok_or_else(|| format!("agent `{}` runs on this machine", workspace.agent))
}

/// Reads the port listings of the [`followed`] workspaces, over and over,
/// and opens the previews asked for meanwhile in those an agent pane works
/// in. Rendered once by the app; it draws nothing.
#[component]
pub fn FollowPorts() -> Element {
    let app = use_context::<AppSignals>();
    let mut listings = app.ports;
    use_future(move || async move {
        let mut endpoints = Endpoints::new(resolve);
        // The newest preview of each workspace the app knows of.
        let mut seen: HashMap<Workspace, u64> = HashMap::new();
        loop {
            let (wanted, open) = {
                let layout = app.layout.peek();
                let configs = app.configs.peek();
                (
                    followed(&layout, &configs, &app.folds.peek()),
                    workspaces(&layout, &configs),
                )
            };
            for workspace in &wanted {
                // Bound first, so the borrow of the configurations ends
                // before the request is awaited.
                let endpoint = endpoints.get(&app.configs.peek(), workspace);
                let ports = match endpoint {
                    Ok(endpoint) => match endpoint.list().await {
                        Ok(listing) => Ports::Listed(listing),
                        Err(PortsError::Gone) => Ports::NotRunning,
                        Err(error) => Ports::Failed(error.to_string()),
                    },
                    Err(problem) => Ports::Failed(problem),
                };
                if let Ports::Listed(listing) = &ports {
                    let (fresh, newest) =
                        new_previews(seen.get(workspace).copied(), &listing.previews);
                    seen.insert(workspace.clone(), newest);
                    if open.contains(workspace) {
                        for preview in fresh {
                            show_preview(app, workspace, &preview);
                        }
                    }
                }
                let changed = listings.peek().get(workspace) != Some(&ports);
                if changed {
                    listings.write().insert(workspace.clone(), ports);
                }
            }
            let stale = listings.peek().keys().any(|known| !wanted.contains(known));
            if stale {
                listings.write().retain(|known, _| wanted.contains(known));
            }
            seen.retain(|known, _| wanted.contains(known));
            endpoints.keep(&wanted);
            tokio::time::sleep(RELIST).await;
        }
    });
    rsx! {}
}

/// Show the page on `port` at `path` in `pane`.
pub fn open_browser(
    app: AppSignals,
    pane: PaneId,
    project: &ProjectId,
    port: u16,
    path: String,
    source: ReportSource,
) {
    let AppSignals {
        mut layout,
        mut projects,
        ..
    } = app;
    layout.write().set_content(
        pane,
        PaneContent::Browser {
            project: project.clone(),
            port,
            path,
            source,
        },
    );
    layout.write().focus(pane);
    projects.write().select(project);
}

/// The browser pane already showing `port` of `source` in `project`.
fn browser_pane(
    layout: &ukai_core::Layout,
    project: &ProjectId,
    port: u16,
    source: &ReportSource,
) -> Option<PaneId> {
    layout.contents().find_map(|(pane, content)| match content {
        PaneContent::Browser {
            project: p,
            port: q,
            source: s,
            ..
        } if p == project && *q == port && s == source => Some(pane),
        _ => None,
    })
}

/// Show a preview the agent asked for: in the pane already on its port,
/// else in a new one beside the agent's. The user's focus stays where it
/// was; they may be typing elsewhere.
fn show_preview(app: AppSignals, workspace: &Workspace, preview: &Preview) {
    let mut layout = app.layout;
    let source = workspace.source();
    let content = PaneContent::Browser {
        project: workspace.project.clone(),
        port: preview.port,
        path: preview.path.clone(),
        source: source.clone(),
    };
    let existing = browser_pane(&layout.peek(), &workspace.project, preview.port, &source);
    if let Some(pane) = existing {
        layout.write().set_content(pane, content);
        return;
    }
    let (focused, anchor) = {
        let current = layout.peek();
        let anchor = current
            .contents()
            .find_map(|(pane, content)| match content {
                PaneContent::Agent {
                    project,
                    agent,
                    workspace: own,
                    ..
                } if Workspace::of(project, agent, own.as_ref()) == *workspace => Some(pane),
                _ => None,
            })
            .unwrap_or_else(|| current.focused());
        (current.focused(), anchor)
    };
    let mut current = layout.write();
    if let Some(pane) = current.split(anchor, Direction::Horizontal) {
        current.set_content(pane, content);
        current.focus(focused);
    }
}

/// Open `port` of `source` at `path`: focus the pane already on it, else
/// open it where [`launch_target`] puts it.
fn launch_browser(
    app: AppSignals,
    project: &ProjectId,
    port: u16,
    path: String,
    source: ReportSource,
) {
    let shown = browser_pane(&app.layout.peek(), project, port, &source);
    if let Some(pane) = shown {
        let mut layout = app.layout;
        layout.write().focus(pane);
    } else if let Some(pane) = launch_target(app) {
        open_browser(app, pane, project, port, path, source);
    }
}

/// One row of the sidebar: a port, what is known of it, and where a click
/// starts.
#[derive(Debug, Clone, PartialEq)]
struct PortRow {
    port: u16,
    meta: String,
    path: String,
}

/// The rows of a listing: every port something listens on, and every port a
/// preview was asked for, the latest preview naming it and saying where to
/// start.
fn rows(listing: &PortListing) -> Vec<PortRow> {
    let mut rows: Vec<PortRow> = listing
        .ports
        .iter()
        .map(|port| PortRow {
            port: port.port,
            meta: port.process.clone().unwrap_or_default(),
            path: "/".to_owned(),
        })
        .collect();
    for preview in &listing.previews {
        let index = match rows.iter().position(|row| row.port == preview.port) {
            Some(index) => index,
            None => {
                rows.push(PortRow {
                    port: preview.port,
                    meta: "not listening".to_owned(),
                    path: "/".to_owned(),
                });
                rows.len() - 1
            }
        };
        if let Some(title) = &preview.title {
            rows[index].meta = title.clone();
        }
        rows[index].path = preview.path.clone();
    }
    rows.sort_by_key(|row| row.port);
    rows
}

/// The browser panes of `project` on this machine's ports, as rows: these
/// are not listed by anyone, so the sidebar shows what is open.
fn local_rows(layout: &ukai_core::Layout, project: &ProjectId) -> Vec<PortRow> {
    let mut rows: Vec<PortRow> = layout
        .contents()
        .filter_map(|(_, content)| match content {
            PaneContent::Browser {
                project: p,
                port,
                path,
                source: ReportSource::Project,
            } if p == project => Some(PortRow {
                port: *port,
                meta: if path == "/" {
                    String::new()
                } else {
                    path.clone()
                },
                path: path.clone(),
            }),
            _ => None,
        })
        .collect();
    rows.sort_by_key(|row| row.port);
    rows.dedup_by_key(|row| row.port);
    rows
}

/// The ports of one workspace of a project in the sidebar: for a gateway
/// workspace, what listens in it; for this machine, the pages open, and a
/// way to open another.
#[component]
pub fn PortGroup(project: ProjectId, place: ReportSource, depth: u8) -> Element {
    let app = use_context::<AppSignals>();
    let mut prompting = use_signal(|| false);
    let local = place.is_project();
    let listing = match Workspace::at(&project, &place) {
        None => Some(Ports::Listed(PortListing::default())),
        Some(workspace) => app.ports.read().get(&workspace).cloned(),
    };
    let rows = match (&listing, local) {
        (_, true) => local_rows(&app.layout.read(), &project),
        (Some(Ports::Listed(listing)), false) => rows(listing),
        _ => Vec::new(),
    };
    let fold = Fold::new(
        &project,
        Node::Ports {
            place: place.clone(),
        },
    );
    let mut folds = app.folds;
    let open = folds.read().is_open(&fold);
    let count = if rows.is_empty() {
        String::new()
    } else {
        rows.len().to_string()
    };
    let inner = depth + 1;

    rsx! {
        Head {
            fold: fold.clone(),
            depth,
            icon: Icon::Globe,
            label: "Ports",
            meta: count,
            if local {
                button {
                    class: "icon",
                    title: "Open a port of this machine…",
                    onclick: move |event: MouseEvent| {
                        event.stop_propagation();
                        folds.write().open(fold.clone());
                        prompting.toggle();
                    },
                    Svg { icon: Icon::Plus }
                }
            }
        }
        if open {
            if prompting() {
                div { class: "tree-hint", style: "--depth: {inner}",
                    PortPrompt {
                        on_open: {
                            let project = project.clone();
                            move |(port, path): (u16, String)| {
                                prompting.set(false);
                                launch_browser(app, &project, port, path, ReportSource::Project);
                            }
                        },
                        on_cancel: move |()| prompting.set(false),
                    }
                }
            }
            match listing {
                None => rsx! {
                    p { class: "muted hint tree-hint", style: "--depth: {inner}", "Listing…" }
                },
                Some(Ports::NotRunning) => rsx! {
                    p { class: "muted hint tree-hint", style: "--depth: {inner}", "The workspace is not running." }
                },
                Some(Ports::Failed(problem)) => rsx! {
                    pre { class: "config-error tree-hint", style: "--depth: {inner}", "{problem}" }
                },
                Some(Ports::Listed(_)) if rows.is_empty() && !local => rsx! {
                    p { class: "muted hint tree-hint", style: "--depth: {inner}", "Nothing listens yet." }
                },
                Some(Ports::Listed(_)) => rsx! {
                    for row in rows {
                        PortRowView {
                            key: "{row.port}",
                            project: project.clone(),
                            row,
                            source: place.clone(),
                            depth: inner,
                        }
                    }
                },
            }
        }
    }
}

#[component]
fn PortRowView(project: ProjectId, row: PortRow, source: ReportSource, depth: u8) -> Element {
    let app = use_context::<AppSignals>();
    let shown = browser_pane(&app.layout.read(), &project, row.port, &source).is_some();
    let PortRow { port, meta, path } = row;
    rsx! {
        div {
            class: if shown { "row report-row open" } else { "row report-row" },
            style: "--depth: {depth}",
            title: "Open port {port} in a pane",
            onclick: move |_| launch_browser(app, &project, port, path.clone(), source.clone()),
            span { class: "row-icon",
                Svg { icon: Icon::Globe, size: 12 }
            }
            span { class: "name", "{port}" }
            span { class: "meta", "{meta}" }
        }
    }
}

/// Asks which port of this machine to open: `5173`, `localhost:3000/docs`.
#[component]
pub fn PortPrompt(on_open: EventHandler<(u16, String)>, on_cancel: EventHandler<()>) -> Element {
    let mut typed = use_signal(String::new);
    let mut invalid = use_signal(|| false);
    rsx! {
        div { class: "port-prompt",
            input {
                r#type: "text",
                placeholder: "5173 or localhost:3000/docs",
                spellcheck: false,
                autofocus: true,
                value: "{typed}",
                oninput: move |event| {
                    typed.set(event.value());
                    invalid.set(false);
                },
                onkeydown: move |event| match event.key() {
                    Key::Enter => {
                        match parse_target(&typed.peek()) {
                            Some(target) => on_open.call(target),
                            None => invalid.set(true),
                        }
                    }
                    Key::Escape => on_cancel.call(()),
                    _ => {}
                },
            }
            if invalid() {
                p { class: "muted hint", "Type a port, like 5173." }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use ukai_acp::ports::ListeningPort;

    use super::*;

    fn preview(port: u16, path: &str, title: Option<&str>, at: u64) -> Preview {
        Preview {
            port,
            path: path.into(),
            title: title.map(str::to_owned),
            at,
        }
    }

    #[test]
    fn lists_listening_ports_with_what_previews_say_of_them() {
        let listing = PortListing {
            ports: vec![
                ListeningPort {
                    port: 8080,
                    process: Some("python3".into()),
                },
                ListeningPort {
                    port: 5173,
                    process: Some("node".into()),
                },
            ],
            previews: vec![
                preview(5173, "/", Some("App"), 1),
                preview(5173, "/settings", None, 2),
                preview(6006, "/", Some("Storybook"), 3),
            ],
        };
        assert_eq!(
            rows(&listing),
            [
                PortRow {
                    port: 5173,
                    meta: "App".into(),
                    path: "/settings".into()
                },
                PortRow {
                    port: 6006,
                    meta: "Storybook".into(),
                    path: "/".into()
                },
                PortRow {
                    port: 8080,
                    meta: "python3".into(),
                    path: "/".into()
                },
            ]
        );
        assert!(rows(&PortListing::default()).is_empty());
    }
}
