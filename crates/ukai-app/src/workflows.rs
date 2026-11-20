//! The workflows pane: the ayaito flows a gateway agent's workspace runs
//! ([`ukai_acp::workflows`]). It lists the flows the checkout declares,
//! which the user starts here with an input, and the runs so far — the
//! user's and the ones the agent started through the relay's MCP server.
//! A run shows its flow as an outline, each node with what became of it,
//! and the sessions its agent nodes worked in, which open in a pane like
//! any session of that workspace.

use std::time::Duration;

use dioxus::prelude::*;
use ukai_acp::workflows::{WorkflowsEndpoint, WorkflowsError};
use ukai_acp::{PaneWorkspace, WorkspaceScope};
use ukai_core::workflow::{
    self, Launcher, Node, NodeKind, NodeState, Run, RunState, RunSummary, WorkflowListing,
};
use ukai_core::{PaneContent, PaneId, ProjectId, ReportSource};

use crate::agent_pane::connect;
use crate::follow::poll;
use crate::icons::{Icon, Svg};
use crate::layout::launch_target;
use crate::projects::{AppSignals, project_context};
use crate::schedules::{local_now, when_unix};

/// How often an open pane reads the runs again.
const RELOAD: Duration = Duration::from_secs(3);

/// Show the workflows of `place` in `project`: focus the pane already
/// showing them, else open one where [`launch_target`] puts it.
pub fn open_workflows(app: AppSignals, project: &ProjectId, place: &ReportSource) {
    let mut layout = app.layout;
    let content = PaneContent::Workflows {
        project: project.clone(),
        place: place.clone(),
    };
    let shown = layout
        .peek()
        .contents()
        .find_map(|(pane, open)| (*open == content).then_some(pane));
    if let Some(pane) = shown {
        layout.write().focus(pane);
        return;
    }
    if let Some(pane) = launch_target(app) {
        let mut layout = layout.write();
        layout.set_content(pane, content);
        layout.focus(pane);
    }
    let mut projects = app.projects;
    projects.write().select(project);
}

/// Whether a pane shows the workflows of `place` in `project`.
pub fn workflows_open(
    layout: &ukai_core::Layout,
    project: &ProjectId,
    place: &ReportSource,
) -> bool {
    layout.contents().any(|(_, content)| {
        matches!(content, PaneContent::Workflows { project: p, place: q } if p == project && q == place)
    })
}

/// The workflows of the workspace `place` names in `project`.
fn endpoint_at(
    app: AppSignals,
    project: &ProjectId,
    place: &ReportSource,
) -> Result<WorkflowsEndpoint, String> {
    let ReportSource::Agent { agent, workspace } = place else {
        return Err("Workflows run in a gateway agent's workspace.".to_owned());
    };
    let context = project_context(&app.projects.peek(), &app.configs.peek(), project)?;
    let spec = context
        .config
        .agent(agent)
        .ok_or_else(|| format!("agent `{agent}` is not declared anymore"))?;
    // A place names the shared workspace by leaving the key out.
    let workspace = workspace.clone().unwrap_or(PaneWorkspace {
        key: String::new(),
        scope: WorkspaceScope::Shared,
    });
    WorkflowsEndpoint::resolve(&spec, ukai_acp::system_lookup, Some(&workspace))
        .map_err(|error| error.to_string())?
        .ok_or_else(|| format!("agent `{agent}` names no gateway agent"))
}

/// What a failed call says to the user.
fn explain(error: &WorkflowsError) -> String {
    match error {
        WorkflowsError::NotRunning => {
            "This workspace is not running. Open its agent to start it.".to_owned()
        }
        other => other.to_string(),
    }
}

/// Open `session` of the workspace `place` names in a pane.
fn open_session(app: AppSignals, project: &ProjectId, place: &ReportSource, session: &str) {
    let ReportSource::Agent { agent, workspace } = place else {
        return;
    };
    let Some(pane) = launch_target(app) else {
        return;
    };
    let mut layout = app.layout;
    {
        let mut layout = layout.write();
        layout.set_content(
            pane,
            PaneContent::Agent {
                project: project.clone(),
                agent: agent.clone(),
                session: Some(session.to_owned()),
                workspace: workspace.clone(),
            },
        );
        layout.focus(pane);
    }
    connect(app, pane, true);
}

/// The dot class of a run's state.
fn run_dot(state: RunState) -> &'static str {
    match state {
        RunState::Running => "busy",
        RunState::Done | RunState::Stopped => "ready",
        RunState::Failed => "closed",
        RunState::Cancelled | RunState::Interrupted => "idle",
    }
}

/// The dot class of a node's state.
fn node_dot(state: NodeState) -> &'static str {
    match state {
        NodeState::Pending => "idle",
        NodeState::Running => "busy",
        NodeState::Done | NodeState::Skipped | NodeState::Stopped => "ready",
        NodeState::Failed => "closed",
    }
}

fn kind_label(kind: NodeKind) -> &'static str {
    match kind {
        NodeKind::Fn => "code",
        NodeKind::Agent => "agent",
        NodeKind::Flow => "flow",
        NodeKind::Parallel => "parallel",
        NodeKind::Until => "loop",
    }
}

/// What the pane last read of the listing.
#[derive(Debug, Clone, PartialEq)]
enum Listed {
    Loading,
    Loaded(WorkflowListing),
    Failed(String),
}

#[component]
pub fn WorkflowsPane(pane: PaneId, project: ProjectId, place: ReportSource) -> Element {
    let app = use_context::<AppSignals>();
    let mut listed = use_signal(|| Listed::Loading);
    // The run shown: the one clicked, else the newest.
    let mut chosen = use_signal(|| None::<String>);
    let mut detail = use_signal(|| None::<Result<Run, String>>);
    let mut input = use_signal(String::new);
    let mut problem = use_signal(|| None::<String>);
    let mut busy = use_signal(|| false);
    let mut revision = use_signal(|| 0_u64);
    let endpoint = use_memo({
        let project = project.clone();
        let place = place.clone();
        move || endpoint_at(app, &project, &place)
    });
    use_future(move || {
        poll(revision, RELOAD, move || async move {
            let endpoint = match endpoint.peek().clone() {
                Ok(endpoint) => endpoint,
                Err(why) => {
                    listed.set(Listed::Failed(why));
                    return;
                }
            };
            let now = match endpoint.list().await {
                Ok(listing) => Listed::Loaded(listing),
                Err(error) => Listed::Failed(explain(&error)),
            };
            let shown = chosen.peek().clone().or_else(|| match &now {
                Listed::Loaded(listing) => listing.runs.first().map(|run| run.id.clone()),
                _ => None,
            });
            if *listed.peek() != now {
                listed.set(now);
            }
            let read = match shown {
                Some(id) => Some(endpoint.get(&id).await.map_err(|error| explain(&error))),
                None => None,
            };
            if *detail.peek() != read {
                detail.set(read);
            }
        })
    });
    let mut refresh = move || *revision.write() += 1;

    // Start `flow` on the input, or resume `run` of it.
    let start = move |flow: String, run: Option<String>| {
        let Ok(endpoint) = endpoint.peek().clone() else {
            return;
        };
        busy.set(true);
        problem.set(None);
        spawn(async move {
            let text = input.peek().clone();
            match endpoint.start(&flow, &text, run.as_deref()).await {
                Ok(run) => chosen.set(Some(run.id)),
                Err(error) => problem.set(Some(explain(&error))),
            }
            busy.set(false);
            refresh();
        });
    };
    let stop = move |run: String| {
        let Ok(endpoint) = endpoint.peek().clone() else {
            return;
        };
        spawn(async move {
            if let Err(error) = endpoint.stop(&run).await {
                problem.set(Some(explain(&error)));
            }
            refresh();
        });
    };

    let agent = place.agent().unwrap_or_default().to_owned();
    let (flows, runs, failed) = match listed() {
        Listed::Loaded(listing) => (listing.flows, listing.runs, None),
        Listed::Failed(why) => (Vec::new(), Vec::new(), Some(why)),
        Listed::Loading => (Vec::new(), Vec::new(), None),
    };
    let loading = listed() == Listed::Loading;
    let shown = detail();
    let shown_id = match &shown {
        Some(Ok(run)) => Some(run.summary.id.clone()),
        _ => None,
    };
    let declared: Vec<String> = flows.iter().map(|flow| flow.name.clone()).collect();
    let now = local_now();

    rsx! {
        div { class: "pane-body schedule workflows",
            div { class: "schedule-panel",
                div { class: "schedule-head",
                    span { class: "workflows-title", "Workflows" }
                    span { class: "muted", "{agent}" }
                    span { class: "spacer" }
                    button {
                        class: "ghost icon",
                        title: "Read again",
                        onclick: move |_| refresh(),
                        Svg { icon: Icon::Reload, size: 13 }
                    }
                }
                p { class: "muted field-hint",
                    "ayaito flows run in this workspace. Each agent node is a session of {agent}, which you can open from its run."
                }
                if let Some(why) = failed {
                    pre { class: "config-error workflow-problem", "{why}" }
                } else if loading {
                    p { class: "muted hint", "Loading…" }
                } else if flows.is_empty() {
                    p { class: "muted hint",
                        "This checkout declares no workflows. Add one to its ukai.toml as [workflows.<name>] with a path, or ask the agent to write and run one."
                    }
                } else {
                    for flow in flows {
                        div { key: "{flow.name}", class: "workflow-flow",
                            div { class: "workflow-flow-text",
                                span { class: "name", "{flow.name}" }
                                span { class: "muted mono", "{flow.path}" }
                                if let Some(description) = flow.description.clone() {
                                    span { class: "muted", "{description}" }
                                }
                            }
                            button {
                                class: "outline small",
                                disabled: busy(),
                                onclick: {
                                    let name = flow.name.clone();
                                    let mut start = start;
                                    move |_| start(name.clone(), None)
                                },
                                "Run"
                            }
                        }
                    }
                    div { class: "field",
                        span { class: "field-label", "Input" }
                        textarea {
                            class: "mono",
                            rows: 3,
                            spellcheck: false,
                            placeholder: "{{ \"site\": \"example.com\" }}",
                            value: "{input}",
                            oninput: move |event| input.set(event.value()),
                        }
                        span { class: "muted field-hint",
                            "The flow's input as JSON. Leave it empty for a flow that takes none."
                        }
                    }
                }
                if let Some(why) = problem() {
                    pre { class: "config-error workflow-problem", "{why}" }
                }
            }
            if !loading {
                div { class: "schedule-panel",
                    span { class: "section-label", "Runs" }
                    if runs.is_empty() {
                        p { class: "muted hint", "No runs yet." }
                    }
                    for run in runs {
                        RunRow {
                            key: "{run.id}",
                            selected: shown_id.as_deref() == Some(run.id.as_str()),
                            when: when_unix(i64::try_from(run.started / 1000).unwrap_or_default(), now),
                            run: run.clone(),
                            onselect: move |id: String| {
                                chosen.set(Some(id));
                                refresh();
                            },
                        }
                    }
                }
            }
            match shown {
                Some(Ok(run)) => rsx! {
                    RunView {
                        key: "{run.summary.id}",
                        project: project.clone(),
                        place: place.clone(),
                        resumable: declared.contains(&run.summary.flow),
                        run: run.clone(),
                        busy: busy(),
                        onstop: move |id: String| stop(id),
                        onresume: {
                            let mut start = start;
                            move |(flow, id): (String, String)| start(flow, Some(id))
                        },
                    }
                },
                Some(Err(why)) => rsx! {
                    div { class: "schedule-panel",
                        pre { class: "config-error workflow-problem", "{why}" }
                    }
                },
                None => rsx! {},
            }
        }
    }
}

/// How long a run took or has been going.
fn run_took(run: &RunSummary) -> String {
    run.finished
        .map(|finished| workflow::took(finished.saturating_sub(run.started)))
        .unwrap_or_default()
}

#[component]
fn RunRow(
    run: RunSummary,
    selected: bool,
    when: String,
    onselect: EventHandler<String>,
) -> Element {
    let by = match run.by {
        Launcher::User => "",
        Launcher::Agent => " · by the agent",
    };
    let took = run_took(&run);
    let state = workflow::state_label(run.state);
    let dot = run_dot(run.state);
    let id = run.id.clone();
    rsx! {
        div {
            class: if selected { "workflow-run selected" } else { "workflow-run" },
            onclick: move |_| onselect.call(id.clone()),
            span { class: "dot {dot}" }
            span { class: "name", "{run.flow}" }
            span { class: "muted", "{state}{by}" }
            span { class: "spacer" }
            span { class: "muted", "{took}" }
            span { class: "run-time", "{when}" }
        }
    }
}

#[component]
fn RunView(
    project: ProjectId,
    place: ReportSource,
    run: Run,
    /// Whether the run's flow is one the user may start again.
    resumable: bool,
    busy: bool,
    onstop: EventHandler<String>,
    onresume: EventHandler<(String, String)>,
) -> Element {
    let app = use_context::<AppSignals>();
    let summary = run.summary.clone();
    let state = workflow::state_label(summary.state);
    let dot = run_dot(summary.state);
    let running = summary.state.is_running();
    let has_input = !matches!(run.input.trim(), "" | "null");
    rsx! {
        div { class: "schedule-panel",
            div { class: "schedule-head",
                span { class: "dot {dot}" }
                span { class: "workflows-title", "{summary.flow}" }
                span { class: "muted mono", "{summary.id}" }
                span { class: "spacer" }
                if running {
                    button {
                        class: "outline small danger",
                        onclick: {
                            let id = summary.id.clone();
                            move |_| onstop.call(id.clone())
                        },
                        "Stop"
                    }
                } else if resumable && summary.state != RunState::Done {
                    button {
                        class: "outline small",
                        disabled: busy,
                        title: "Run it again under the same id: nodes behind a checkpoint return what they saved",
                        onclick: {
                            let (flow, id) = (summary.flow.clone(), summary.id.clone());
                            move |_| onresume.call((flow.clone(), id.clone()))
                        },
                        "Resume"
                    }
                }
            }
            p { class: "muted field-hint",
                "{state}"
                if let Some(detail) = summary.detail.clone() {
                    " — {detail}"
                }
            }
            div { class: "workflow-nodes",
                for (index, node) in run.nodes.iter().cloned().enumerate() {
                    NodeRow {
                        key: "{index}-{node.name}",
                        node: node.clone(),
                        onopen: {
                            let project = project.clone();
                            let place = place.clone();
                            move |session: String| open_session(app, &project, &place, &session)
                        },
                    }
                }
            }
            if has_input {
                details { class: "run",
                    summary {
                        span { class: "run-outcome", "Input" }
                    }
                    pre { class: "run-output", "{run.input}" }
                }
            }
            if let Some(output) = run.output.clone() {
                details { class: "run", open: true,
                    summary {
                        span { class: "run-outcome", "Output" }
                    }
                    pre { class: "run-output", "{output}" }
                }
            }
            if !run.log.trim().is_empty() {
                details { class: "run", open: summary.state == RunState::Failed,
                    summary {
                        span { class: "run-outcome", "Printed" }
                    }
                    pre { class: "run-output", "{run.log}" }
                }
            }
        }
    }
}

#[component]
fn NodeRow(node: Node, onopen: EventHandler<String>) -> Element {
    let dot = node_dot(node.state);
    let kind = kind_label(node.kind);
    let state = workflow::node_label(node.state);
    let mut facts = vec![state.to_owned()];
    if node.runs > 1 {
        facts.push(format!("{} runs", node.runs));
    }
    if node.retries > 0 {
        facts.push(format!("{} retries", node.retries));
    }
    if let Some(ms) = node.ms.filter(|_| node.state != NodeState::Running) {
        facts.push(workflow::took(ms));
    }
    let facts = facts.join(" · ");
    // A session is one client's at a time: opening a node's while its turn
    // runs would take it from the flow.
    let running = node.state == NodeState::Running;
    let many = node.sessions.len() > 1;
    rsx! {
        div { class: "workflow-node", style: "--depth: {node.depth}",
            div { class: "workflow-node-line",
                span { class: "dot {dot}" }
                span { class: "name", "{node.name}" }
                span { class: "workflow-kind {kind}", "{kind}" }
                if let Some(edge) = node.edge.clone() {
                    span { class: "muted", "{edge}" }
                }
                span { class: "spacer" }
                span { class: "muted", "{facts}" }
                for (index, session) in node.sessions.iter().cloned().enumerate() {
                    button {
                        key: "{session}",
                        class: "outline small",
                        disabled: running,
                        title: if running { "Open it once the node has finished: a session answers one client at a time" } else { "Open this node's session in a pane" },
                        onclick: move |_| onopen.call(session.clone()),
                        if many {
                            "Session {index + 1}"
                        } else {
                            "Session"
                        }
                    }
                }
            }
            if let Some(detail) = node.detail.clone() {
                p { class: "workflow-node-detail muted", "{detail}" }
            }
        }
    }
}
