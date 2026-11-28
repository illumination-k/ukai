//! Runs of ayaito flows in the workspaces agents work in, and the pane that
//! shows one on its graph.
//!
//! An agent starts a flow — a workflow of agents and code — in its checkout,
//! and ayaito saves the run there as it goes (`ukai_acp::flows`).
//! [`FollowFlows`] lists the runs of every workspace the app follows, this
//! machine's checkouts among them, for the sidebar; a run that starts where
//! an agent pane works opens beside that pane. A [`FlowPane`] reads its run
//! over and over while it goes on and lays what its nodes did over the
//! flow's graph (`ukai_core::flow`); from it the user stops the run, or
//! sends a message to one of its agents.
//!
//! Everything a run says — node names, what agents say, errors — was
//! written by an agent: it is only ever rendered as text.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use dioxus::prelude::*;
use ukai_acp::Flows;
use ukai_acp::flows::{FlowError, FlowListing, MAX_CHUNK, Order, RunChunk, RunState, RunSummary};
use ukai_config::AgentSpec;
use ukai_core::flow::{
    Drawing, Graph, NodeKind, NodeView, RunEvent, RunView, draw, new_runs, parse_events, run_mark,
    view,
};
use ukai_core::sidebar::{Fold, Node, pane_place};
use ukai_core::{Direction, PaneContent, PaneId, ProjectId, ReportSource};

use crate::follow::{Endpoints, Workspace, followed};
use crate::icons::{Icon, Svg};
use crate::layout::launch_target;
use crate::projects::{AppSignals, at_place};
use crate::sidebar::Head;
use crate::text_field::use_text_field;

/// How often the listings are read again.
const RELIST: Duration = Duration::from_secs(3);
/// How often a pane reads its run while it goes on, and once it has ended
/// (it may be resumed).
const READ_RUNNING: Duration = Duration::from_secs(1);
const READ_ENDED: Duration = Duration::from_secs(5);
/// How many runs of a workspace the sidebar lists, the ones written last.
const RUNS_SHOWN: usize = 20;

/// A checkout flows run in: a project's own on this machine, or a gateway
/// agent's workspace.
pub type Place = (ProjectId, ReportSource);

/// The last listing of a place's runs.
#[derive(Debug, Clone, PartialEq)]
pub enum Runs {
    Listed(FlowListing),
    /// The pod is not running: the agent has to be opened again first.
    NotRunning,
    Failed(String),
}

/// Every place's last listing, kept by [`FollowFlows`].
pub type FlowListings = Signal<HashMap<Place, Runs>>;

/// A run as named everywhere in the app.
pub type RunKey = (ProjectId, ReportSource, String);

/// What the app has read of a run so far.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RunRead {
    graph_text: String,
    /// The flow, or why its graph cannot be read; `None` before the first
    /// read.
    graph: Option<Result<Graph, String>>,
    events: Vec<RunEvent>,
    /// Where in the run's events to read on from.
    next: u64,
    state: RunState,
    result: Option<String>,
    /// Why the last read failed.
    problem: Option<String>,
}

impl RunRead {
    /// This with `chunk`, read from `after`, added.
    fn with(&self, after: u64, chunk: RunChunk) -> Self {
        let mut read = self.clone();
        if chunk.graph != read.graph_text || read.graph.is_none() {
            read.graph = Some(Graph::parse(&chunk.graph));
            read.graph_text = chunk.graph;
        }
        // The events were written anew: what was read is not theirs.
        if chunk.next < after {
            read.events.clear();
        }
        read.events.extend(parse_events(&chunk.events));
        read.next = chunk.next;
        read.state = chunk.state;
        read.result = chunk.result;
        read.problem = None;
        read
    }
}

/// What the open flow panes have read, by run: kept outside the panes so
/// that one mounted again (a tab shown after a while) starts from what it
/// had.
pub type FlowRuns = Signal<HashMap<RunKey, RunRead>>;

fn explain(error: FlowError) -> String {
    match error {
        FlowError::NotRunning => {
            "The workspace is not running; open the agent to start it.".to_owned()
        }
        other => other.to_string(),
    }
}

/// Where the flows of a gateway workspace are read, for
/// [`crate::follow::Endpoints`].
fn resolve(spec: &AgentSpec, workspace: &Workspace) -> Result<Flows, String> {
    crate::follow::found(
        Flows::resolve(spec, ukai_acp::system_lookup, workspace.workspace.as_ref()),
        || format!("agent `{}` runs on this machine", workspace.agent),
    )
}

/// The flows of `place` in `project`.
fn flows_at(app: AppSignals, project: &ProjectId, place: &ReportSource) -> Result<Flows, String> {
    at_place(
        app,
        project,
        place,
        |root| Ok(Flows::Local(root)),
        |spec, workspace, _| Flows::resolve(spec, ukai_acp::system_lookup, Some(workspace)),
    )
}

async fn list(flows: Result<Flows, String>) -> Runs {
    match flows {
        Ok(flows) => match flows.list().await {
            Ok(listing) => Runs::Listed(listing),
            Err(FlowError::NotRunning) => Runs::NotRunning,
            Err(error) => Runs::Failed(error.to_string()),
        },
        Err(problem) => Runs::Failed(problem),
    }
}

/// Lists the runs of every place the app follows, over and over: this
/// machine's checkout of each open project, and the gateway workspaces the
/// other pollers follow. A run that starts where an agent pane works opens
/// in a pane beside it. Rendered once by the app; it draws nothing.
#[component]
pub fn FollowFlows() -> Element {
    let app = use_context::<AppSignals>();
    use_future(move || async move {
        let mut endpoints = Endpoints::new(resolve);
        // The runs of each place the app knows of.
        let mut seen: HashMap<Place, HashSet<String>> = HashMap::new();
        let mut listings = app.flows;
        let mut reads = app.flow_runs;
        loop {
            let (local, remote, working, shown) = {
                let layout = app.layout.peek();
                let configs = app.configs.peek();
                let projects = app.projects.peek();
                let local: Vec<(ProjectId, std::path::PathBuf)> = projects
                    .iter()
                    .map(|project| (project.id.clone(), project.root.clone()))
                    .collect();
                let remote = followed(&layout, &configs, projects.selected().map(|p| &p.id));
                // Where an agent pane works, and the runs panes show.
                let working: HashSet<Place> = layout
                    .contents()
                    .filter(|(_, content)| matches!(content, PaneContent::Agent { .. }))
                    .filter_map(|(_, content)| pane_place(content))
                    .collect();
                let shown: HashSet<RunKey> = layout
                    .contents()
                    .filter_map(|(_, content)| match content {
                        PaneContent::Flow {
                            project,
                            run,
                            place,
                        } => Some((project.clone(), place.clone(), run.clone())),
                        _ => None,
                    })
                    .collect();
                (local, remote, working, shown)
            };
            let mut wanted: Vec<(Place, Result<Flows, String>)> = local
                .into_iter()
                .map(|(project, root)| ((project, ReportSource::Project), Ok(Flows::Local(root))))
                .collect();
            for workspace in &remote {
                let flows = endpoints.get(&app.configs.peek(), workspace);
                wanted.push(((workspace.project.clone(), workspace.source()), flows));
            }
            endpoints.keep(&remote);
            let places: HashSet<Place> = wanted.iter().map(|(place, _)| place.clone()).collect();
            for (place, flows) in wanted {
                let runs = list(flows).await;
                if let Runs::Listed(listing) = &runs {
                    let (fresh, known) = new_runs(seen.get(&place), &listing.runs);
                    seen.insert(place.clone(), known);
                    if working.contains(&place) {
                        for run in fresh {
                            show_run(app, &place, &run);
                        }
                    }
                }
                if listings.peek().get(&place) != Some(&runs) {
                    listings.write().insert(place, runs);
                }
            }
            if listings.peek().keys().any(|place| !places.contains(place)) {
                listings.write().retain(|place, _| places.contains(place));
            }
            seen.retain(|place, _| places.contains(place));
            if reads.peek().keys().any(|run| !shown.contains(run)) {
                reads.write().retain(|run, _| shown.contains(run));
            }
            tokio::time::sleep(RELIST).await;
        }
    });
    rsx! {}
}

/// The pane already showing `run` of `place`.
fn flow_pane(layout: &ukai_core::Layout, place: &Place, run: &str) -> Option<PaneId> {
    layout.contents().find_map(|(pane, content)| match content {
        PaneContent::Flow {
            project,
            run: shown,
            place: at,
        } if *project == place.0 && *at == place.1 && shown == run => Some(pane),
        _ => None,
    })
}

fn content(place: &Place, run: &str) -> PaneContent {
    PaneContent::Flow {
        project: place.0.clone(),
        run: run.to_owned(),
        place: place.1.clone(),
    }
}

/// Show a run that just started: in a new pane beside the agent's, unless a
/// pane is on it already. The user's focus stays where it was; they may be
/// typing elsewhere.
fn show_run(app: AppSignals, place: &Place, run: &str) {
    let mut layout = app.layout;
    if flow_pane(&layout.peek(), place, run).is_some() {
        return;
    }
    let (focused, anchor) = {
        let current = layout.peek();
        let anchor = current
            .contents()
            .find_map(|(pane, content)| {
                (matches!(content, PaneContent::Agent { .. })
                    && pane_place(content).as_ref() == Some(place))
                .then_some(pane)
            })
            .unwrap_or_else(|| current.focused());
        (current.focused(), anchor)
    };
    let mut current = layout.write();
    if let Some(pane) = current.split(anchor, Direction::Horizontal) {
        current.set_content(pane, content(place, run));
        current.focus(focused);
    }
}

/// Open `run` of `place`: focus the pane already on it, else open it where
/// [`launch_target`] puts it.
pub fn open_flow(app: AppSignals, project: &ProjectId, place: &ReportSource, run: &str) {
    let mut layout = app.layout;
    let place = (project.clone(), place.clone());
    let shown = flow_pane(&layout.peek(), &place, run);
    if let Some(pane) = shown {
        layout.write().focus(pane);
    } else if let Some(pane) = launch_target(app) {
        let mut layout = layout.write();
        layout.set_content(pane, content(&place, run));
        layout.focus(pane);
    }
    let mut projects = app.projects;
    projects.write().select(project);
}

/// The runs the palette offers: every listed one, with where it is.
pub fn listed_runs(app: AppSignals) -> Vec<(ProjectId, ReportSource, RunSummary)> {
    let mut found = Vec::new();
    for ((project, place), runs) in app.flows.peek().iter() {
        if let Runs::Listed(listing) = runs {
            for run in listing.runs.iter().take(RUNS_SHOWN) {
                found.push((project.clone(), place.clone(), run.clone()));
            }
        }
    }
    found.sort_by_key(|found| std::cmp::Reverse(found.2.at));
    found
}

/// The flow runs of one workspace of a project in the sidebar. Nothing is
/// drawn where there are none: most checkouts never run a flow.
#[component]
pub fn FlowGroup(project: ProjectId, place: ReportSource, depth: u8) -> Element {
    let app = use_context::<AppSignals>();
    let listing = app
        .flows
        .read()
        .get(&(project.clone(), place.clone()))
        .cloned();
    let (runs, problem) = match listing {
        Some(Runs::Listed(listing)) if !listing.runs.is_empty() => (listing.runs, None),
        Some(Runs::Failed(problem)) => (Vec::new(), Some(problem)),
        _ => return rsx! {},
    };
    let fold = Fold::new(
        &project,
        Node::Flows {
            place: place.clone(),
        },
    );
    let open = app.folds.read().is_open(&fold);
    let going = runs
        .iter()
        .filter(|run| run.state == RunState::Running)
        .count();
    let meta = match (runs.len(), going) {
        (0, _) => String::new(),
        (all, 0) => all.to_string(),
        (all, going) => format!("{going} running · {all}"),
    };
    let more = runs.len().saturating_sub(RUNS_SHOWN);
    let inner = depth + 1;
    rsx! {
        Head {
            fold,
            depth,
            icon: Icon::Flow,
            label: "Flows",
            meta,
        }
        if open {
            if let Some(problem) = problem {
                pre {
                    class: "config-error tree-hint",
                    style: "--depth: {inner}",
                    "{problem}"
                }
            }
            for run in runs.into_iter().take(RUNS_SHOWN) {
                FlowRow {
                    key: "{run.id}",
                    project: project.clone(),
                    place: place.clone(),
                    run,
                    depth: inner,
                }
            }
            if more > 0 {
                p { class: "muted hint tree-hint", style: "--depth: {inner}", "{more} older" }
            }
        }
    }
}

#[component]
fn FlowRow(project: ProjectId, place: ReportSource, run: RunSummary, depth: u8) -> Element {
    let app = use_context::<AppSignals>();
    let shown = flow_pane(
        &app.layout.read(),
        &(project.clone(), place.clone()),
        &run.id,
    )
    .is_some();
    let state = run.state.name();
    let mark = run_mark(run.state);
    let id = run.id.clone();
    rsx! {
        div {
            class: if shown { "row report-row flow-row open" } else { "row report-row flow-row" },
            style: "--depth: {depth}",
            title: "Show run {run.id} on its graph",
            onclick: move |_| open_flow(app, &project, &place, &id),
            span { class: "row-icon flow-mark {state}", "{mark}" }
            span { class: "name", "{run.id}" }
            span { class: "meta", "{state}" }
        }
    }
}

/// What came of something told to a run.
#[derive(Debug, Clone, PartialEq)]
enum Told {
    Sending,
    Said(String),
    Refused(String),
}

/// A run of a flow on its graph.
#[component]
pub fn FlowPane(pane: PaneId, project: ProjectId, run: String, place: ReportSource) -> Element {
    let app = use_context::<AppSignals>();
    let key: RunKey = (project.clone(), place.clone(), run.clone());
    let mut selected = use_signal(|| None::<String>);
    let mut confirming = use_signal(|| false);
    let mut told = use_signal(|| None::<Told>);

    use_future({
        let key = key.clone();
        move || {
            let key = key.clone();
            async move {
                let mut reads = app.flow_runs;
                let mut flows = None;
                loop {
                    let before = reads.peek().get(&key).cloned().unwrap_or_default();
                    if flows.is_none() {
                        flows = flows_at(app, &key.0, &key.1)
                            .map(Some)
                            .unwrap_or_else(|problem| {
                                let failed = RunRead {
                                    problem: Some(problem),
                                    ..before.clone()
                                };
                                if failed != before {
                                    reads.write().insert(key.clone(), failed);
                                }
                                None
                            });
                    }
                    let mut wait = READ_ENDED;
                    if let Some(reader) = &flows {
                        let after = before.next;
                        let read = match reader.read(&key.2, after).await {
                            Ok(chunk) => {
                                // As much as one read carries: more follows.
                                let more = chunk.events.len() >= MAX_CHUNK / 2;
                                let read = before.with(after, chunk);
                                wait = match (more, read.state) {
                                    (true, _) => Duration::ZERO,
                                    (false, RunState::Running) => READ_RUNNING,
                                    (false, _) => READ_ENDED,
                                };
                                read
                            }
                            Err(error) => RunRead {
                                problem: Some(explain(error)),
                                ..before.clone()
                            },
                        };
                        // Written only when it says something new, so a
                        // quiet run draws nothing.
                        if read != before || !reads.peek().contains_key(&key) {
                            reads.write().insert(key.clone(), read);
                        }
                    }
                    tokio::time::sleep(wait).await;
                }
            }
        }
    });

    let read = app.flow_runs.read().get(&key).cloned();
    let Some(read) = read else {
        return rsx! {
            div { class: "pane-body flow-pane",
                p { class: "muted hint", "Reading run {run}…" }
            }
        };
    };
    let graph = match &read.graph {
        Some(Ok(graph)) => Some(graph),
        _ => None,
    };
    let seen = graph
        .map(|graph| view(graph, &read.events))
        .unwrap_or_default();
    let drawing = graph.map(draw).unwrap_or_default();
    let running = read.state == RunState::Running;
    let state = read.state.name();
    let mark = run_mark(read.state);
    let summary = seen.summary();
    let chosen = selected().and_then(|id| seen.node(&id).cloned());

    // Tell the run something, and show what came of it.
    let send = {
        let key = key.clone();
        move |order: Order| {
            let key = key.clone();
            told.set(Some(Told::Sending));
            spawn(async move {
                let outcome = match flows_at(app, &key.0, &key.1) {
                    Ok(flows) => flows.order(&key.2, order).await.map_err(explain),
                    Err(problem) => Err(problem),
                };
                told.set(Some(match outcome {
                    Ok(ordered) => Told::Said(ordered.said),
                    Err(problem) => Told::Refused(problem),
                }));
            });
        }
    };

    rsx! {
        div { class: "pane-body flow-pane",
            div { class: "flow-toolbar",
                span { class: "flow-state {state}", "{mark} {state}" }
                span { class: "flow-run", title: "{run}", "{run}" }
                if seen.total > 0 {
                    span { class: "flow-summary", "{summary}" }
                }
                span { class: "spacer" }
                if running && confirming() {
                    span { class: "muted", "Stop the run? It can be resumed by its id." }
                    button {
                        class: "outline small danger",
                        onclick: {
                            let mut send = send.clone();
                            move |_| {
                                confirming.set(false);
                                send(Order::default());
                            }
                        },
                        "Stop"
                    }
                    button {
                        class: "outline small",
                        onclick: move |_| confirming.set(false),
                        "Cancel"
                    }
                } else if running {
                    button {
                        class: "outline small",
                        title: "Stop this run; its checkpoints stay",
                        onclick: move |_| confirming.set(true),
                        "Stop run"
                    }
                }
            }
            if let Some(problem) = &read.problem {
                pre { class: "config-error flow-problem", "{problem}" }
            }
            if let Some(Err(problem)) = &read.graph {
                pre { class: "config-error flow-problem", "This run's graph cannot be read: {problem}" }
            }
            match told() {
                Some(Told::Sending) => rsx! {
                    p { class: "muted hint flow-told", "Sending…" }
                },
                Some(Told::Said(said)) => rsx! {
                    p { class: "muted hint flow-told", onclick: move |_| told.set(None), "{said}" }
                },
                Some(Told::Refused(problem)) => rsx! {
                    pre { class: "config-error flow-problem", onclick: move |_| told.set(None), "{problem}" }
                },
                None => rsx! {},
            }
            div { class: "flow-canvas", onclick: move |_| selected.set(None),
                Canvas {
                    graph: graph.cloned().unwrap_or_default(),
                    drawing,
                    seen: seen.clone(),
                    selected,
                }
            }
            if let Some(node) = chosen {
                Detail {
                    key: "{node.id}",
                    node,
                    on_order: {
                        let mut send = send.clone();
                        move |order| send(order)
                    },
                    on_close: move |()| selected.set(None),
                }
            }
            if let Some(result) = &read.result {
                details { class: "flow-result",
                    summary { "Result" }
                    pre { "{result}" }
                }
            }
        }
    }
}

/// The graph as placed, with each node's state.
#[component]
fn Canvas(
    graph: Graph,
    drawing: Drawing,
    seen: RunView,
    selected: Signal<Option<String>>,
) -> Element {
    let mut selected = selected;
    let chosen = selected();
    rsx! {
        div {
            class: "flow-graph",
            style: "width: {drawing.width}px; height: {drawing.height}px",
            svg {
                class: "flow-lines",
                width: "{drawing.width}",
                height: "{drawing.height}",
                view_box: "0 0 {drawing.width} {drawing.height}",
                defs {
                    marker {
                        id: "flow-arrow",
                        view_box: "0 0 8 8",
                        ref_x: "7",
                        ref_y: "4",
                        marker_width: "7",
                        marker_height: "7",
                        orient: "auto",
                        path { d: "M0 0.5L7 4L0 7.5z" }
                    }
                }
                for line in drawing.lines.iter() {
                    path {
                        key: "{line.from}-{line.to}",
                        class: if line.back { "back" } else { "" },
                        d: "{line.path}",
                        marker_end: "url(#flow-arrow)",
                    }
                }
            }
            for shape in drawing.shapes.iter() {
                {
                    let node = seen.node(&shape.id);
                    let plain = graph.nodes.iter().find(|node| node.id == shape.id);
                    let status = node.map_or("none", |node| node.status.name());
                    let kind = shape.kind.name();
                    let group = if shape.group { "group" } else { "step" };
                    let picked = if chosen.as_deref() == Some(shape.id.as_str()) {
                        "selected"
                    } else {
                        ""
                    };
                    let label = node
                        .map(|node| node.label.clone())
                        .or_else(|| plain.map(|node| node.label.clone()))
                        .unwrap_or_default();
                    let id = shape.id.clone();
                    let can_select = node.is_some();
                    rsx! {
                        div {
                            key: "{shape.id}",
                            class: "flow-node {group} {kind} {status} {picked}",
                            style: "left: {shape.x}px; top: {shape.y}px; width: {shape.width}px; height: {shape.height}px",
                            onclick: move |event: MouseEvent| {
                                // A group is under its members: the click
                                // is for the innermost.
                                event.stop_propagation();
                                if can_select {
                                    selected.set(Some(id.clone()));
                                }
                            },
                            NodeBody {
                                label,
                                kind: shape.kind,
                                group: shape.group,
                                node: node.cloned(),
                            }
                        }
                    }
                }
            }
            for line in drawing.lines.iter() {
                if let Some(label) = &line.label {
                    span {
                        key: "{line.from}-{line.to}",
                        class: "flow-when",
                        style: "left: {line.label_x}px; top: {line.label_y}px",
                        "{label}"
                    }
                }
            }
        }
    }
}

/// What a node's box says.
#[component]
fn NodeBody(label: String, kind: NodeKind, group: bool, node: Option<NodeView>) -> Element {
    let Some(node) = node else {
        return rsx! {
            div { class: "flow-label",
                span { class: "name", "{label}" }
            }
        };
    };
    let mark = node.status.mark();
    let third = node.error.clone().or_else(|| node.activity.clone());
    rsx! {
        div { class: "flow-label", title: "{label}",
            span { class: "mark", "{mark}" }
            span { class: "name", "{label}" }
            if group && !node.detail.is_empty() {
                span { class: "detail", "{node.detail}" }
            }
        }
        if !group && kind != NodeKind::Choice {
            div { class: "flow-detail-line", title: "{node.detail}", "{node.detail}" }
            if let Some(third) = third {
                div { class: "flow-activity", "{third}" }
            }
        }
    }
}

/// Everything about the selected node, and for an agent at work, a way to
/// reach it.
#[component]
fn Detail(node: NodeView, on_order: EventHandler<Order>, on_close: EventHandler<()>) -> Element {
    let mut typed = use_signal(String::new);
    let field = use_text_field(move || typed.cloned());
    let status = node.status.name();
    let mark = node.status.mark();
    let kind = node.kind.name();
    let reach = node.reach.clone();
    let mut send = {
        let reach = reach.clone();
        move || {
            let message = typed.peek().trim().to_owned();
            if let (Some(node), false) = (reach.clone(), message.is_empty()) {
                on_order.call(Order {
                    node: Some(node),
                    message: Some(message),
                });
                typed.set(String::new());
            }
        }
    };
    rsx! {
        div { class: "flow-detail",
            div { class: "flow-detail-head",
                span { class: "flow-state {status}", "{mark} {status}" }
                span { class: "name", "{node.label}" }
                span { class: "meta", "{kind}" }
                if let Some(when) = &node.when {
                    span { class: "meta", "when {when}" }
                }
                span { class: "spacer" }
                button {
                    class: "icon",
                    title: "Close",
                    onclick: move |_| on_close.call(()),
                    Svg { icon: Icon::Close }
                }
            }
            if !node.detail.is_empty() {
                p { class: "muted", "{node.detail}" }
            }
            if let Some(error) = &node.error {
                pre { class: "config-error", "{error}" }
            }
            if let Some(activity) = &node.activity {
                p { class: "flow-activity-full", "{activity}" }
            }
            if let Some(reach) = reach {
                div { class: "flow-steer",
                    input {
                        r#type: "text",
                        placeholder: "Tell this agent something; it reads it and goes on",
                        spellcheck: false,
                        id: field.id(),
                        onmounted: move |_| field.mounted(),
                        oninput: move |event| {
                            field.typed(&event.value());
                            typed.set(event.value());
                        },
                        onkeydown: {
                            let mut send = send.clone();
                            move |event: KeyboardEvent| {
                                // Keys typed here are not the app's.
                                event.stop_propagation();
                                if event.key() == Key::Enter && !event.is_composing() {
                                    send();
                                }
                            }
                        },
                    }
                    button {
                        class: "outline small",
                        disabled: typed.read().trim().is_empty(),
                        onclick: move |_| send(),
                        "Send"
                    }
                    button {
                        class: "outline small",
                        title: "The agent answers from what it has done so far, and the run goes on",
                        onclick: move |_| {
                            on_order
                                .call(Order {
                                    node: Some(reach.clone()),
                                    message: None,
                                });
                        },
                        "Stop this agent"
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(events: &str, next: u64, state: RunState) -> RunChunk {
        RunChunk {
            graph: r#"{"nodes":[{"id":"a","kind":"fn","label":"a","name":"a"}],"edges":[]}"#
                .to_owned(),
            events: events.to_owned(),
            next,
            state,
            result: None,
        }
    }

    #[test]
    fn reads_add_up() {
        let start = "{\"type\":\"start\",\"node\":\"a\"}\n";
        let end = "{\"type\":\"end\",\"node\":\"a\",\"ms\":3}\n";
        let first = RunRead::default().with(0, chunk(start, 28, RunState::Running));
        assert_eq!(first.events.len(), 1);
        assert!(matches!(first.graph, Some(Ok(_))));
        let second = first.with(28, chunk(end, 60, RunState::Done));
        assert_eq!((second.events.len(), second.next), (2, 60));
        assert_eq!(second.state, RunState::Done);
        // Nothing new reads the same, so nothing is drawn again.
        assert_eq!(second.with(60, chunk("", 60, RunState::Done)), second);
    }

    #[test]
    fn events_written_anew_replace_what_was_read() {
        let start = "{\"type\":\"start\",\"node\":\"a\"}\n";
        let read = RunRead::default().with(0, chunk(&start.repeat(3), 84, RunState::Running));
        let anew = read.with(84, chunk(start, 28, RunState::Running));
        assert_eq!((anew.events.len(), anew.next), (1, 28));
    }

    #[test]
    fn a_failed_read_is_forgotten_by_the_next() {
        let failed = RunRead {
            problem: Some("unreachable".to_owned()),
            ..RunRead::default()
        };
        assert_eq!(
            failed.with(0, chunk("", 0, RunState::Running)).problem,
            None
        );
    }

    #[test]
    fn a_graph_that_cannot_be_read_says_why() {
        let mut bad = chunk("", 0, RunState::Running);
        bad.graph = "nope".to_owned();
        assert!(matches!(
            RunRead::default().with(0, bad).graph,
            Some(Err(_))
        ));
    }

    #[test]
    fn a_workspace_that_is_not_running_says_what_to_do() {
        assert!(explain(FlowError::NotRunning).contains("open the agent"));
        assert_eq!(explain(FlowError::Refused("no run".to_owned())), "no run");
    }
}
