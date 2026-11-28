//! A run of an [ayaito](https://github.com/illumination-k/ayaito) flow, as a
//! pane shows it.
//!
//! ayaito runs a workflow of agents and code as a typed graph and saves each
//! run in a folder: `graph.json` (the flow, as its `toGraph` describes it)
//! and `events.jsonl` (what each node did: start, end, error, what its agent
//! says). This module reads both and says what to draw: [`view`] lays the
//! events over the graph, giving every node its state, and [`draw`] places
//! the graph's nodes and edges. Reading the files, and reaching the run of a
//! gateway workspace, is `ukai_acp::flows`.
//!
//! Both files are another program's, which changes between its releases:
//! what is not understood is skipped rather than refused — a node of an
//! unknown kind is drawn as a plain step, an event of an unknown type says
//! nothing.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};
use ukai_acp::flows::{RunState, RunSummary};

mod draw;
#[cfg(test)]
mod tests;

pub use draw::{Drawing, Line, Shape, draw};

/// ayaito's `toGraph`: the nodes of a flow, the groups they are in, and the
/// edges between them.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Graph {
    #[serde(default)]
    pub nodes: Vec<GraphNode>,
    #[serde(default)]
    pub edges: Vec<GraphEdge>,
}

impl Graph {
    /// `text` as a graph, or why it is none.
    pub fn parse(text: &str) -> Result<Self, String> {
        serde_json::from_str(text).map_err(|error| error.to_string())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GraphNode {
    /// Unique in the graph.
    pub id: String,
    pub kind: NodeKind,
    /// What to show.
    #[serde(default)]
    pub label: String,
    /// The `node` of the events this node emits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The group this node is in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    /// The state key the output is assigned to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assign: Option<String>,
    /// Runs in a git worktree.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub worktree: bool,
    /// The key of the output among those of a parallel node.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// How many items run at once (forEach); absent when all do.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub concurrency: Option<f64>,
    /// `false` when a failed node does not abort the others.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fail_fast: Option<bool>,
    /// How many times the body runs at most (until).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<f64>,
}

/// What a node of the graph is. `Input` and `Output` are the ends of the
/// graph, `Fn` is code, `Agent` an agent, `Choice` where the paths part; the
/// rest are groups, which hold their members through
/// [`GraphNode::parent`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum NodeKind {
    Input,
    Output,
    Fn,
    Agent,
    Choice,
    Flow,
    Parallel,
    ForEach,
    Until,
    /// A kind this ukai does not know: drawn as a step.
    #[serde(other)]
    Other,
}

impl NodeKind {
    /// Whether nodes of this kind hold others.
    pub fn is_group(self) -> bool {
        matches!(
            self,
            Self::Flow | Self::Parallel | Self::ForEach | Self::Until
        )
    }

    /// Whether a node of this kind does the run's work: what its progress
    /// counts.
    pub fn is_step(self) -> bool {
        matches!(self, Self::Fn | Self::Agent | Self::Other)
    }

    /// The word for it in a class name.
    pub fn name(self) -> &'static str {
        match self {
            Self::Input => "input",
            Self::Output => "output",
            Self::Fn | Self::Other => "fn",
            Self::Agent => "agent",
            Self::Choice => "choice",
            Self::Flow => "flow",
            Self::Parallel => "parallel",
            Self::ForEach => "for-each",
            Self::Until => "until",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphEdge {
    pub from: String,
    pub to: String,
    /// When the edge is taken: the case of a branch, or the "yes" or "no" of
    /// a loop.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when: Option<String>,
    /// The names of the middleware that wraps the node the edge leads to.
    #[serde(default, rename = "use", skip_serializing_if = "Vec::is_empty")]
    pub wraps: Vec<String>,
}

/// A line of a run's `events.jsonl`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunEvent {
    #[serde(rename = "type")]
    pub kind: EventKind,
    pub node: String,
    /// Inside a forEach: the index of the item the event is for.
    #[serde(default)]
    pub item: Option<String>,
    #[serde(default)]
    pub ms: Option<f64>,
    #[serde(default)]
    pub attempt: Option<f64>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub usd: Option<f64>,
    #[serde(default)]
    pub tool: Option<String>,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub stopped: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum EventKind {
    Start,
    End,
    Skip,
    Retry,
    Error,
    Stop,
    WrapUp,
    Usage,
    Activity,
    Steer,
    Session,
    #[serde(other)]
    Other,
}

/// The events in `text`, the content of an `events.jsonl` or a part of it.
/// A line that is no event — one cut short by a write in progress, or of a
/// shape this ukai does not know — is left out.
pub fn parse_events(text: &str) -> Vec<RunEvent> {
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

/// How far a node is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Status {
    Pending,
    Running,
    Done,
    Failed,
    /// A checkpoint had its output, or it is on a branch not taken.
    Skipped,
    /// The run stopped here (`Stop`).
    Stopped,
}

impl Status {
    /// The word for it in a class name, and in what an agent reads.
    pub fn name(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Skipped => "skipped",
            Self::Stopped => "stopped",
        }
    }

    /// The mark before a node's label.
    pub fn mark(self) -> &'static str {
        match self {
            Self::Pending => "○",
            Self::Running => "●",
            Self::Done => "✓",
            Self::Failed => "✗",
            Self::Skipped => "↷",
            Self::Stopped => "■",
        }
    }
}

/// How deep in groups a node is shown at most.
const MAX_DEPTH: usize = 64;

/// As much of an activity as a node shows.
const ACTIVITY_CHARS: usize = 200;

/// What the events say about the nodes of one name.
#[derive(Debug, Clone, PartialEq)]
struct NodeState {
    status: Status,
    running: u32,
    runs: u32,
    skipped: bool,
    ms: Option<f64>,
    usd: f64,
    attempt: Option<f64>,
    error: Option<String>,
    /// The last thing its agent said or did, or was sent.
    activity: Option<String>,
}

impl NodeState {
    fn new() -> Self {
        Self {
            status: Status::Pending,
            running: 0,
            runs: 0,
            skipped: false,
            ms: None,
            usd: 0.0,
            attempt: None,
            error: None,
            activity: None,
        }
    }

    fn ended(&mut self, status: Status) {
        self.running = self.running.saturating_sub(1);
        self.status = if self.running > 0 {
            Status::Running
        } else {
            status
        };
    }
}

fn line(text: &str) -> String {
    let joined = text.split_whitespace().collect::<Vec<_>>().join(" ");
    joined.chars().take(ACTIVITY_CHARS).collect()
}

/// What the events say about each node, by its name.
fn fold(events: &[RunEvent]) -> HashMap<&str, NodeState> {
    let mut states: HashMap<&str, NodeState> = HashMap::new();
    for event in events {
        if event.kind == EventKind::Stop {
            // The flows around the node that stopped record nothing more:
            // the run is over.
            for state in states.values_mut().filter(|state| state.running > 0) {
                state.running = 0;
                state.status = Status::Stopped;
            }
            states
                .entry(&event.node)
                .or_insert_with(NodeState::new)
                .status = Status::Stopped;
            continue;
        }
        let state = states.entry(&event.node).or_insert_with(NodeState::new);
        // Which of a forEach's agents the line is about.
        let who = event
            .item
            .as_ref()
            .map(|item| format!("#{item} "))
            .unwrap_or_default();
        match event.kind {
            EventKind::Start => {
                state.running += 1;
                state.runs += 1;
                state.status = Status::Running;
                state.skipped = false;
                state.error = None;
                state.activity = None;
            }
            EventKind::Skip => state.skipped = true,
            EventKind::End => {
                state.ms = event.ms;
                let status = if state.skipped {
                    Status::Skipped
                } else {
                    Status::Done
                };
                state.ended(status);
            }
            EventKind::Error => {
                state.error.clone_from(&event.error);
                state.ended(Status::Failed);
            }
            EventKind::Retry => state.attempt = event.attempt,
            EventKind::Usage => state.usd += event.usd.unwrap_or(0.0),
            EventKind::Activity => {
                let text = event.text.as_deref().unwrap_or_default();
                state.activity = Some(line(&match &event.tool {
                    Some(tool) => format!("{who}{tool} {text}"),
                    None => format!("{who}{text}"),
                }));
            }
            EventKind::WrapUp => {
                if event.stopped == Some(true) {
                    state.activity = Some(format!("{who}← stop"));
                }
            }
            EventKind::Steer => {
                let message = event.message.as_deref().unwrap_or_default();
                state.activity = Some(line(&format!("{who}← {message}")));
            }
            EventKind::Stop | EventKind::Session | EventKind::Other => {}
        }
    }
    states
}

fn time(ms: f64) -> String {
    if ms < 1000.0 {
        format!("{ms:.0}ms")
    } else if ms < 60_000.0 {
        format!("{:.1}s", ms / 1000.0)
    } else {
        format!("{:.0}m", ms / 60_000.0)
    }
}

/// `usd` as a run's cost reads: three decimals under a dollar.
pub fn money(usd: f64) -> String {
    if usd < 1.0 {
        format!("${usd:.3}")
    } else {
        format!("${usd:.2}")
    }
}

/// A node of the graph with what the run's events say about it.
#[derive(Debug, Clone, PartialEq)]
pub struct NodeView {
    pub id: String,
    pub kind: NodeKind,
    /// How deep in groups it is.
    pub depth: usize,
    pub label: String,
    /// The case of the branch that leads here.
    pub when: Option<String>,
    pub status: Status,
    /// What wraps it and what it took, joined by ` · `.
    pub detail: String,
    /// Why it failed.
    pub error: Option<String>,
    /// While it runs: the last thing its agent said or did, or was sent.
    pub activity: Option<String>,
    /// For an agent that is running: the name a message reaches it by.
    pub reach: Option<String>,
}

/// A run on its graph.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RunView {
    /// Every node that says something about the run, in the graph's order:
    /// the ends of the graph and the "done?" of a loop are left out.
    pub nodes: Vec<NodeView>,
    /// How many steps are done or skipped, of how many.
    pub done: usize,
    pub total: usize,
    /// What the agents reported spending.
    pub usd: f64,
}

impl RunView {
    pub fn node(&self, id: &str) -> Option<&NodeView> {
        self.nodes.iter().find(|node| node.id == id)
    }

    /// The run's progress in a few words.
    pub fn summary(&self) -> String {
        let steps = format!("{}/{}", self.done, self.total);
        if self.usd > 0.0 {
            format!("{steps} · {}", money(self.usd))
        } else {
            steps
        }
    }
}

/// The graph with what `events` say about each node. Nodes are matched to
/// events by name, so two nodes of one name show the same state.
pub fn view(graph: &Graph, events: &[RunEvent]) -> RunView {
    let states = fold(events);
    let by_id: HashMap<&str, &GraphNode> = graph
        .nodes
        .iter()
        .map(|node| (node.id.as_str(), node))
        .collect();

    let cases = branch_cases(graph, &by_id);

    let mut rows: Vec<(NodeView, Option<&str>, bool)> = Vec::new();
    let mut order: Vec<(&GraphNode, usize, bool)> = Vec::new();
    collect(graph, &states, &cases, None, 0, false, &mut order);
    for (node, depth, untaken_above) in order {
        let Some(name) = &node.name else { continue };
        let state = states.get(name.as_str());
        let from = cases.get(node.id.as_str());
        let untaken = untaken_above || not_taken(&states, from, state);
        let status = if untaken {
            Status::Skipped
        } else {
            state.map_or(Status::Pending, |state| state.status)
        };
        let detail = detail(graph, node, state, status, untaken && !untaken_above);
        let label = match &node.key {
            Some(key) if *key != node.label => format!("{key}: {}", node.label),
            _ => node.label.clone(),
        };
        let running = status == Status::Running;
        let is_step = node.kind.is_step() && !untaken;
        rows.push((
            NodeView {
                id: node.id.clone(),
                kind: node.kind,
                depth,
                label,
                when: from.map(|(when, _)| (*when).to_owned()),
                status,
                detail,
                error: state
                    .filter(|_| status == Status::Failed)
                    .and_then(|state| state.error.clone()),
                activity: state
                    .filter(|_| running)
                    .and_then(|state| state.activity.clone()),
                reach: (running && node.kind == NodeKind::Agent).then(|| name.clone()),
            },
            node.parent.as_deref(),
            is_step,
        ));
    }

    // `run` records nothing for the node it was given: a group without
    // events is as far as its members are.
    for index in (0..rows.len()).rev() {
        let (row, _, _) = &rows[index];
        let named = by_id
            .get(row.id.as_str())
            .and_then(|node| node.name.as_deref());
        if row.status != Status::Pending || named.is_some_and(|name| states.contains_key(name)) {
            continue;
        }
        let members: Vec<Status> = rows
            .iter()
            .filter(|(_, parent, _)| *parent == Some(row.id.as_str()))
            .map(|(member, _, _)| member.status)
            .collect();
        if members.is_empty() {
            continue;
        }
        rows[index].0.status = group_status(&members);
    }

    let usd = events
        .iter()
        .filter(|event| event.kind == EventKind::Usage)
        .filter_map(|event| event.usd)
        .sum();
    let steps = rows.iter().filter(|(_, _, is_step)| *is_step);
    let total = steps.clone().count();
    let done = steps
        .filter(|(row, _, _)| matches!(row.status, Status::Done | Status::Skipped))
        .count();
    RunView {
        nodes: rows.into_iter().map(|(row, _, _)| row).collect(),
        done,
        total,
        usd,
    }
}

/// The node each case of a branch starts at, with the case and the name of
/// the branch: the edge leads to the first step inside the case.
fn branch_cases<'a>(
    graph: &'a Graph,
    by_id: &HashMap<&str, &'a GraphNode>,
) -> HashMap<&'a str, (&'a str, &'a str)> {
    let mut cases: HashMap<&str, (&str, &str)> = HashMap::new();
    for edge in &graph.edges {
        let Some(choice) = by_id.get(edge.from.as_str()) else {
            continue;
        };
        let (Some(when), Some(name)) = (&edge.when, &choice.name) else {
            continue;
        };
        let mut top = by_id.get(edge.to.as_str()).copied();
        // Bounded by the graph's size, should the parents form a cycle.
        for _ in 0..graph.nodes.len() {
            match top {
                Some(node) if node.parent.is_some() && node.parent != choice.parent => {
                    top = node
                        .parent
                        .as_deref()
                        .and_then(|parent| by_id.get(parent))
                        .copied();
                }
                _ => break,
            }
        }
        if let Some(top) = top {
            cases.insert(&top.id, (when, name));
        }
    }
    cases
}

/// What wraps `node` and what it took, joined by ` · `. `not_taken` is for
/// the case of a branch that chose another.
fn detail(
    graph: &Graph,
    node: &GraphNode,
    state: Option<&NodeState>,
    status: Status,
    not_taken: bool,
) -> String {
    let mut detail: Vec<String> = Vec::new();
    if let Some(key) = &node.assign {
        detail.push(format!("→ {key}"));
    }
    if node.worktree {
        detail.push("worktree".to_owned());
    }
    let all_finish = node.fail_fast == Some(false);
    match node.kind {
        NodeKind::ForEach => {
            let mut each = "each item".to_owned();
            if let Some(concurrency) = node.concurrency {
                each.push_str(&format!(", {concurrency} at a time"));
            }
            if all_finish {
                each.push_str(", all finish");
            }
            detail.push(each);
        }
        NodeKind::Until => {
            if let Some(max) = node.max {
                detail.push(format!("at most {max}"));
            }
        }
        NodeKind::Parallel => detail.push(
            if all_finish {
                "parallel, all finish"
            } else {
                "parallel"
            }
            .to_owned(),
        ),
        _ => {}
    }
    // What wraps the edges that lead here, each once, in the graph's order.
    let wraps = graph
        .edges
        .iter()
        .filter(|edge| edge.to == node.id)
        .flat_map(|edge| &edge.wraps);
    let before = detail.len();
    for wrap in wraps {
        if !detail[before..].contains(wrap) {
            detail.push(wrap.clone());
        }
    }
    if not_taken {
        detail.push("not taken".to_owned());
    }
    if let Some(state) = state {
        if state.runs > 1 {
            detail.push(format!("×{}", state.runs));
        }
        if let Some(attempt) = state.attempt
            && status == Status::Running
        {
            detail.push(format!("attempt {}", attempt + 1.0));
        }
        if let Some(ms) = state.ms
            && status != Status::Running
        {
            detail.push(time(ms));
        }
        if state.usd > 0.0 {
            detail.push(money(state.usd));
        }
    }
    detail.join(" · ")
}

/// How far a group without events of its own is, from its members.
fn group_status(members: &[Status]) -> Status {
    let has = |status| members.contains(&status);
    if has(Status::Running) {
        Status::Running
    } else if has(Status::Failed) {
        Status::Failed
    } else if has(Status::Stopped) {
        Status::Stopped
    } else if !has(Status::Pending) {
        Status::Done
    } else if has(Status::Done) || has(Status::Skipped) {
        Status::Running
    } else {
        Status::Pending
    }
}

/// Whether the branch that leads to a node chose, and not this case.
fn not_taken(
    states: &HashMap<&str, NodeState>,
    from: Option<&(&str, &str)>,
    state: Option<&NodeState>,
) -> bool {
    let Some((_, choice)) = from else {
        return false;
    };
    state.is_none()
        && states
            .get(choice)
            .is_some_and(|choice| choice.status == Status::Done)
}

/// The members of `parent` and theirs, depth first in the graph's order,
/// each with its depth and whether it is under a case not taken.
fn collect<'a>(
    graph: &'a Graph,
    states: &HashMap<&str, NodeState>,
    cases: &HashMap<&str, (&str, &str)>,
    parent: Option<&str>,
    depth: usize,
    untaken: bool,
    order: &mut Vec<(&'a GraphNode, usize, bool)>,
) {
    // Someone else's file: what is nested deeper than a flow is written is
    // not followed.
    if depth > MAX_DEPTH {
        return;
    }
    for node in &graph.nodes {
        if node.parent.as_deref() != parent
            || matches!(node.kind, NodeKind::Input | NodeKind::Output)
            || node.name.is_none()
        {
            continue;
        }
        // No node comes twice, so two that share an id, each the other's
        // parent, cannot be followed in circles.
        if order.len() >= graph.nodes.len() {
            return;
        }
        order.push((node, depth, untaken));
        if node.kind.is_group() {
            let state = node.name.as_deref().and_then(|name| states.get(name));
            let below = untaken || not_taken(states, cases.get(node.id.as_str()), state);
            collect(
                graph,
                states,
                cases,
                Some(&node.id),
                depth + 1,
                below,
                order,
            );
        }
    }
}

/// The run as text, one node a line, for an agent to read.
pub fn outline(view: &RunView) -> String {
    let mut lines = Vec::new();
    for node in &view.nodes {
        let indent = "  ".repeat(node.depth);
        let when = node
            .when
            .as_ref()
            .map(|when| format!("[{when}] "))
            .unwrap_or_default();
        let mut text = format!("{indent}{when}{} {}", node.status.mark(), node.label);
        text.push_str(&format!(" ({})", node.status.name()));
        if !node.detail.is_empty() {
            text.push_str(&format!(" · {}", node.detail));
        }
        lines.push(text);
        for extra in [&node.error, &node.activity].into_iter().flatten() {
            lines.push(format!("{}  {extra}", "  ".repeat(node.depth)));
        }
    }
    lines.join("\n")
}

/// The runs of a listing that started since the app last looked: those
/// going on whose id is not in `seen`, with the ids now known. With no
/// `seen` — a checkout listed for the first time — none count as new: they
/// were started before the app was looking, and opening them all on a
/// restart would bury the layout. A run that has ended opens only when
/// asked for.
pub fn new_runs(
    seen: Option<&HashSet<String>>,
    runs: &[RunSummary],
) -> (Vec<String>, HashSet<String>) {
    let known: HashSet<String> = runs.iter().map(|run| run.id.clone()).collect();
    let fresh = match seen {
        None => Vec::new(),
        Some(seen) => runs
            .iter()
            .filter(|run| run.state == RunState::Running && !seen.contains(&run.id))
            .map(|run| run.id.clone())
            .collect(),
    };
    (fresh, known)
}

/// The mark of a run in a list, as a node's is.
pub fn run_mark(state: RunState) -> &'static str {
    match state {
        RunState::Running => Status::Running.mark(),
        RunState::Done => Status::Done.mark(),
        RunState::Stopped => Status::Stopped.mark(),
        RunState::Failed => Status::Failed.mark(),
    }
}
