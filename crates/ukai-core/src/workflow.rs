//! Workflows ([ayaito] flows) a relay runs in a gateway agent's workspace:
//! what may start one, what a run's records say about its nodes, and how a
//! node's agent answers a permission request nobody is there to see. Pure,
//! so the relay that runs them and the app that shows them agree, and so it
//! is tested here.
//!
//! The types are [`ukai_acp::workflows`]'s, since they cross the wire.
//!
//! [ayaito]: https://github.com/illumination-k/ayaito

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use ukai_acp::PermissionChoice;
pub use ukai_acp::workflows::{
    Access, AgentAnswer, AgentCall, DeclaredFlow, Launcher, Node, NodeKind, NodeState, Run,
    RunState, RunSummary, StartRequest, WorkflowListing,
};

/// Runs a relay keeps; older finished ones are dropped.
pub const RUNS_KEPT: usize = 30;

/// Bytes of a run's output and of what it printed that are shown: the end
/// of the log, where errors are, and the start of the output.
pub const TEXT_KEPT: usize = 16 * 1024;

/// Longest run id.
pub const RUN_ID_MAX: usize = 64;

/// What can be wrong with a request to start a run; [`problems`] lists them
/// all.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Problem {
    #[error(
        "name one of a declared flow (`flow`), a script in the checkout (`path`), or a script's source (`script`)"
    )]
    NoSource,
    #[error("name only one of `flow`, `path`, and `script`")]
    ManySources,
    #[error("the input is not JSON: {0}")]
    InvalidInput(String),
    #[error("run id `{0}` is not letters, digits, `-`, and `_`, at most {RUN_ID_MAX} characters")]
    InvalidRunId(String),
    #[error("the script is empty")]
    EmptyScript,
}

/// Everything wrong with `request`, before the checkout is looked at.
pub fn problems(request: &StartRequest) -> Vec<Problem> {
    let sources = [&request.flow, &request.path, &request.script]
        .into_iter()
        .filter(|source| source.is_some())
        .count();
    let source = match sources {
        0 => Some(Problem::NoSource),
        1 => None,
        _ => Some(Problem::ManySources),
    };
    let empty = request
        .script
        .as_ref()
        .is_some_and(|script| script.trim().is_empty())
        .then_some(Problem::EmptyScript);
    let input = (!request.input.trim().is_empty())
        .then(|| serde_json::from_str::<serde_json::Value>(&request.input).err())
        .flatten()
        .map(|error| Problem::InvalidInput(error.to_string()));
    let run = request
        .run
        .as_ref()
        .filter(|id| !is_run_id(id))
        .map(|id| Problem::InvalidRunId(id.clone()));
    source
        .into_iter()
        .chain(empty)
        .chain(input)
        .chain(run)
        .collect()
}

/// Whether `id` can name a run: it becomes a directory's name.
pub fn is_run_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= RUN_ID_MAX
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// What ayaito records about a run's nodes, a line each in its
/// `events.jsonl`. Usage is not kept: ACP agents report none per call.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    Start {
        node: String,
    },
    End {
        node: String,
        #[serde(default)]
        ms: u64,
    },
    Skip {
        node: String,
        #[serde(default)]
        reason: String,
    },
    Retry {
        node: String,
    },
    Error {
        node: String,
        #[serde(default)]
        error: String,
    },
    Stop {
        node: String,
        #[serde(default)]
        reason: String,
    },
    #[serde(other)]
    Other,
}

/// The events in `text`, one JSON object a line; a line that is none of
/// them (one being written, a kind added later) is left out.
pub fn events(text: &str) -> Vec<Event> {
    text.lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

/// How a run ended, as its `result.json` has it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Outcome {
    Done {
        #[serde(default)]
        output: serde_json::Value,
    },
    Stopped {
        #[serde(default)]
        node: String,
        #[serde(default)]
        reason: String,
    },
    Failed {
        #[serde(default)]
        error: String,
    },
    Cancelled,
}

/// A run's state and why, from how it ended (if it did) and whether its
/// process is `alive`. A run with neither was cut off with its relay.
pub fn run_state(outcome: Option<&Outcome>, alive: bool) -> (RunState, Option<String>) {
    match outcome {
        None if alive => (RunState::Running, None),
        None => (RunState::Interrupted, None),
        Some(Outcome::Done { .. }) => (RunState::Done, None),
        Some(Outcome::Stopped { node, reason }) => {
            (RunState::Stopped, Some(format!("{node}: {reason}")))
        }
        Some(Outcome::Failed { error }) => (RunState::Failed, Some(error.clone())),
        Some(Outcome::Cancelled) => (RunState::Cancelled, None),
    }
}

/// The session an agent call of a run went to, and the node that asked.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeSession {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
    pub session: String,
}

/// `graph` (the flow's nodes as the run drew them: name, kind, depth, edge)
/// with what `events` and `sessions` say happened to each. Nodes are found
/// by name, so one run several times (a loop's body, a retry) shows its
/// last state and how often it started. A node the graph does not have (the
/// run drew none) is added under the first. A node still running in a run
/// that is over did not finish. The first node is the flow itself, which
/// ayaito records nothing about: it follows `state`.
pub fn fold(
    graph: Vec<Node>,
    events: &[Event],
    sessions: &[NodeSession],
    state: RunState,
) -> Vec<Node> {
    let mut nodes = graph;
    let mut index: BTreeMap<String, usize> = BTreeMap::new();
    // The first of a name is the one found, and the flow itself is found
    // only when nothing inside it has its name.
    for (at, node) in nodes.iter().enumerate().skip(1) {
        index.entry(node.name.clone()).or_insert(at);
    }
    if let Some(root) = nodes.first() {
        index.entry(root.name.clone()).or_insert(0);
    }
    let mut at = |nodes: &mut Vec<Node>, name: &str| -> usize {
        *index.entry(name.to_owned()).or_insert_with(|| {
            nodes.push(Node {
                name: name.to_owned(),
                depth: 1,
                ..Node::default()
            });
            nodes.len() - 1
        })
    };
    for event in events {
        match event {
            Event::Start { node } => {
                let at = at(&mut nodes, node);
                nodes[at].state = NodeState::Running;
                nodes[at].runs += 1;
                nodes[at].detail = None;
            }
            Event::End { node, ms } => {
                let at = at(&mut nodes, node);
                // A checkpoint's skip is followed by the node's end.
                if nodes[at].state != NodeState::Skipped {
                    nodes[at].state = NodeState::Done;
                }
                nodes[at].ms = Some(*ms);
            }
            Event::Skip { node, reason } => {
                let at = at(&mut nodes, node);
                nodes[at].state = NodeState::Skipped;
                nodes[at].detail = Some(reason.clone());
            }
            Event::Retry { node } => {
                let at = at(&mut nodes, node);
                nodes[at].retries += 1;
            }
            Event::Error { node, error } => {
                let at = at(&mut nodes, node);
                nodes[at].state = NodeState::Failed;
                nodes[at].detail = Some(error.clone());
            }
            Event::Stop { node, reason } => {
                let at = at(&mut nodes, node);
                nodes[at].state = NodeState::Stopped;
                nodes[at].detail = Some(reason.clone());
            }
            Event::Other => {}
        }
    }
    for NodeSession { node, session } in sessions {
        let at = match node {
            Some(node) => at(&mut nodes, node),
            None => 0,
        };
        if let Some(node) = nodes.get_mut(at)
            && !node.sessions.contains(session)
        {
            node.sessions.push(session.clone());
        }
    }
    if !state.is_running() {
        for node in nodes.iter_mut().skip(1) {
            if node.state == NodeState::Running {
                node.state = NodeState::Failed;
                node.detail
                    .get_or_insert_with(|| "did not finish".to_owned());
            }
        }
    }
    if let Some(root) = nodes.first_mut()
        && root.runs == 0
    {
        root.state = match state {
            RunState::Running => NodeState::Running,
            RunState::Done => NodeState::Done,
            RunState::Stopped => NodeState::Stopped,
            RunState::Failed | RunState::Cancelled | RunState::Interrupted => NodeState::Failed,
        };
    }
    nodes
}

/// The option that answers a permission request of a node's agent, or
/// `None` to decline. A flow runs unattended, so nothing waits for the
/// user: a node that may write is allowed what it asks (once, when the
/// agent offers that), and a read-only one is refused, since an agent asks
/// before it edits or runs a command and not before it reads.
pub fn permit(access: Access, options: &[PermissionChoice]) -> Option<String> {
    let of = |kind: &str| {
        options
            .iter()
            .find(|option| option.kind == kind)
            .map(|option| option.id.clone())
    };
    match access {
        Access::Write => of("allow_once").or_else(|| of("allow_always")),
        Access::ReadOnly => of("reject_once"),
    }
}

/// `text` cut to its last [`TEXT_KEPT`] bytes, at a character.
pub fn tail(text: &str) -> &str {
    let mut from = text.len().saturating_sub(TEXT_KEPT);
    while !text.is_char_boundary(from) {
        from += 1;
    }
    &text[from..]
}

/// `text` cut to its first [`TEXT_KEPT`] bytes, at a character.
pub fn head(text: &str) -> &str {
    let mut to = text.len().min(TEXT_KEPT);
    while !text.is_char_boundary(to) {
        to -= 1;
    }
    &text[..to]
}

/// A word for a run's state.
pub fn state_label(state: RunState) -> &'static str {
    match state {
        RunState::Running => "running",
        RunState::Done => "done",
        RunState::Stopped => "stopped early",
        RunState::Failed => "failed",
        RunState::Cancelled => "cancelled",
        RunState::Interrupted => "interrupted",
    }
}

/// A word for a node's state.
pub fn node_label(state: NodeState) -> &'static str {
    match state {
        NodeState::Pending => "pending",
        NodeState::Running => "running",
        NodeState::Done => "done",
        NodeState::Skipped => "skipped",
        NodeState::Stopped => "stopped the run",
        NodeState::Failed => "failed",
    }
}

/// `run` as text, for the agent that asked about it: its state, each node
/// under its parent, and the output and what it printed.
pub fn describe(run: &Run) -> String {
    let summary = &run.summary;
    let mut text = format!(
        "Run {} of {}: {}",
        summary.id,
        summary.flow,
        state_label(summary.state)
    );
    if let Some(detail) = &summary.detail {
        text.push_str(&format!(" ({detail})"));
    }
    text.push('\n');
    for node in &run.nodes {
        text.push_str(&format!(
            "{}- {}: {}",
            "  ".repeat(node.depth as usize),
            node.name,
            node_label(node.state)
        ));
        if node.runs > 1 {
            text.push_str(&format!(", {} runs", node.runs));
        }
        if node.retries > 0 {
            text.push_str(&format!(", {} retries", node.retries));
        }
        if let Some(detail) = &node.detail {
            text.push_str(&format!(" ({detail})"));
        }
        text.push('\n');
    }
    if let Some(output) = &run.output {
        text.push_str(&format!("\nOutput:\n{output}\n"));
    }
    if !run.log.trim().is_empty() {
        text.push_str(&format!("\nPrinted:\n{}\n", run.log.trim_end()));
    }
    text
}

#[cfg(test)]
mod tests;
