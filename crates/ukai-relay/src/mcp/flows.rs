//! The tools that run ayaito flows: workflows of agents and code as typed
//! graphs, which the agent writes and starts in its checkout.
//!
//! A run is a process of its own (`ukai_acp::flows::start`), so it goes on
//! when the turn ends, and ayaito saves what it does where the relay reads
//! it back for the user (`crate::flows`), who follows it on its graph in a
//! pane. Nothing tells the agent that a run ended: `read_flow` says how far
//! it is. Messages to a run's agents go through ayaito's command line.

use std::path::Path;

use serde_json::{Value, json};
use ukai_acp::flows::{FlowError, Order, RunState, StartRequest, Store};
use ukai_core::flow;

const RUN_FLOW: &str = "run_flow";
const READ_FLOW: &str = "read_flow";
const STEER_FLOW: &str = "steer_flow";
const STOP_FLOW: &str = "stop_flow";

/// What the agent reads of a result or a log: the rest is in the run's
/// folder.
const SHOWN: usize = 8000;

/// How many parts of a run's events one `read_flow` reads at most.
const MAX_READS: usize = 64;

/// What the agent is told about flows.
pub const INSTRUCTIONS: &str = "\n\nFlows: when work is better orchestrated across several agents \
     than done in this conversation — a plan whose tasks are fixed side by side, each in its own \
     git worktree, then tested; a check repeated until it passes — or when the user asks for an \
     ayaito flow or workflow, write it as an ayaito flow (a typed graph of agents and code) and \
     start it with `run_flow`. It runs in the background, in this checkout, and the user follows \
     it live on its graph in a ukai pane next to this conversation: which nodes run, what each \
     agent is doing, what it cost. Nothing tells you when a run ends: `read_flow` says how far \
     it is and, once it has ended, its result. `steer_flow` corrects an agent of a run while it \
     works, and `stop_flow` stops one, or the run. The user can do both from the pane too.";

/// The tools on offer.
pub fn tools() -> Vec<Value> {
    let run_id =
        json!({ "type": "string", "description": "The run's id, as `run_flow` answered it." });
    let node = "its node's name, name#item in a forEach (fix#2), or its session id";
    vec![
        json!({
            "name": RUN_FLOW,
            "description": format!("Runs a workflow of agents and code that you write, in the background, with ayaito (a typed graph engine). Use it when the user asks for an ayaito flow or workflow, or for work to be orchestrated across several agents. The call returns at once with a run id; the user sees the run on its graph in a ukai pane. No message reaches you when it ends: check with `{READ_FLOW}`, and do not wait for it in a loop of calls.\n\n{SCRIPT_GUIDE}"),
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "A short name for the run, as kebab-case words: the run id starts with it."
                    },
                    "script": {
                        "type": "string",
                        "description": "The TypeScript module whose default export is the built flow."
                    },
                    "input": { "description": "The flow's input, as JSON. Absent, the script's export named input." },
                    "run_id": {
                        "type": "string",
                        "description": "The id of an earlier run to resume from its checkpoints."
                    }
                }
            },
            "annotations": { "destructiveHint": false, "idempotentHint": false }
        }),
        json!({
            "name": READ_FLOW,
            "description": "Says how far a run of a flow is: its state (running, done, stopped, failed), each node with what it is doing or why it failed, and once the run has ended, its result. The whole of what its agents did is in events.jsonl in the run's folder.",
            "inputSchema": {
                "type": "object",
                "properties": { "run_id": run_id },
                "required": ["run_id"]
            },
            "annotations": { "readOnlyHint": true }
        }),
        json!({
            "name": STEER_FLOW,
            "description": "Sends a message to an agent of a run while it works: its turn is interrupted, it reads the message and goes on with its task. Use it to correct or redirect an agent that is going wrong, without stopping the run. `node` is the name of an agent node that is running now; in a forEach, where one node has an agent per item, `name#item` (fix#2) reaches the agent of that item and the bare name all of them; the id of an agent's session reaches that agent too. To see what the agents are doing, call `read_flow`, or read the end of events.jsonl in the run's folder: its session lines have each agent's session id, its activity lines what each agent said and the tools it called (with `item` inside a forEach), and a steer line appears when the agent got the message.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "run_id": run_id,
                    "node": { "type": "string", "description": format!("The agent to reach: {node}.") },
                    "message": { "type": "string", "description": "What to tell the agent." }
                },
                "required": ["run_id", "node", "message"]
            },
            "annotations": { "destructiveHint": false, "idempotentHint": false }
        }),
        json!({
            "name": STOP_FLOW,
            "description": format!("Stops a run of a flow. Its checkpoints stay, so the run can be resumed by passing its id to `{RUN_FLOW}`. With `node`, stops only the agent of that node, which must be running: it is interrupted and answers from what it has done so far, and the run goes on with that answer. Use it for an agent that has done enough or is stuck."),
            "inputSchema": {
                "type": "object",
                "properties": {
                    "run_id": run_id,
                    "node": { "type": "string", "description": format!("To stop only an agent: {node}.") }
                },
                "required": ["run_id"]
            },
            "annotations": { "destructiveHint": true, "idempotentHint": true }
        }),
    ]
}

/// How to write a flow, as ayaito's own tool for Claude Code tells it.
const SCRIPT_GUIDE: &str = r#"`script` is a TypeScript module (types are stripped, not checked) whose default export is a built flow. Annotate the input of every function. Example:

import { z } from "zod";
import { agentNode, assign, branch, checkpoint, exec, fnNode, Flow, forEach, parallel, retry, Stop, until, worktree } from "ayaito";
import { claudeRuntime } from "ayaito/claude";

const claude = claudeRuntime();
type In = { goal: string };
const plan = agentNode("plan", { prompt: (s: In) => `List the tasks for: ${s.goal}`, output: z.object({ tasks: z.array(z.string()) }), access: "read-only" }, claude);
const fix = agentNode("fix", { prompt: (task: string) => task, output: z.object({ summary: z.string() }), access: "write" }, claude);
const test = fnNode("test", async (fixes: unknown[], ctx) => ({ fixes, code: (await exec("npm", ["test"], { cwd: ctx.cwd })).code }));

export const input: In = { goal: "..." };
export default Flow.start<In>("fix-all")
  .then(assign("plan", plan), { use: [checkpoint] })               // state becomes In & { plan }
  .then(forEach("fix each", worktree(fix, { commit: (o) => o.summary }), { concurrency: 3, use: [checkpoint] }), { map: (s) => s.plan.tasks })
  .then(test)
  .build();

Parts: fnNode(name, async (input, ctx) => output) is code; agentNode(name, { prompt, output (a zod schema), access: "read-only" | "write", model?, system? }, runtime) is an agent whose answer is validated. .then(node, { map, use }): map turns the previous output into the node's input and is required when the types differ; use is middleware: checkpoint (skip on resume), retry({ attempts }), timeout(ms), budget({ usd }). assign(key, node) adds the output to the state. parallel(name, { key: node, ... }) runs nodes on one input and returns { key: output }; a failed node aborts the others, unless { failFast: false } is passed (all finish, then it throws an AggregateError). forEach(name, node, { concurrency, failFast }) runs a node per item of a list; failFast: false works as in parallel. branch(name, (input) => key, { key: node, ... }) picks one node. until(name, node, (output) => done, max) repeats a node whose input and output have one type. worktree(node, { commit: message }) gives an agent that writes its own git worktree and branch; its output is { output, branch, path, commit }, so agents can write side by side. throw new Stop(reason) ends the run early as a success. Node names must be unique: events are matched to nodes by name. Other runtimes: codexRuntime (ayaito/codex), opencodeRuntime (ayaito/opencode), acpRuntime (ayaito/acp).

The checkout needs `ayaito` installed (node_modules/.bin/ayaito, or on the PATH), `zod`, and the SDK of the runtime the flow's agents use; the flow is run by Node 22.18 or later.

To run a saved flow again from its checkpoints, pass its `run_id` (with a new `script` to change the flow, or without one to reuse it)."#;

fn text<'a>(arguments: &'a Value, key: &str) -> Option<&'a str> {
    arguments
        .get(key)
        .and_then(Value::as_str)
        .filter(|text| !text.trim().is_empty())
}

fn required<'a>(arguments: &'a Value, key: &str) -> Result<&'a str, String> {
    text(arguments, key).ok_or_else(|| format!("`{key}` is required."))
}

fn said(error: FlowError) -> String {
    error.to_string()
}

fn cut(text: &str) -> String {
    if text.len() <= SHOWN {
        return text.to_owned();
    }
    let mut end = SHOWN;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n… (cut; the whole is in the file)", &text[..end])
}

fn tail(text: &str) -> String {
    if text.len() <= SHOWN {
        return text.to_owned();
    }
    let mut start = text.len() - SHOWN;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    format!("…\n{}", &text[start..])
}

/// The outcome of a flow tool in the checkout at `root`, or `None` when
/// `tool` is not one.
pub async fn call(root: &Path, tool: &str, arguments: &Value) -> Option<Result<String, String>> {
    Some(match tool {
        RUN_FLOW => run(root, arguments).await,
        READ_FLOW => read(root, arguments).await,
        STEER_FLOW => order(root, arguments, true).await,
        STOP_FLOW => order(root, arguments, false).await,
        _ => return None,
    })
}

async fn run(root: &Path, arguments: &Value) -> Result<String, String> {
    let request = StartRequest {
        name: text(arguments, "name").map(str::to_owned),
        script: text(arguments, "script").map(str::to_owned),
        input: arguments
            .get("input")
            .filter(|input| !input.is_null())
            .cloned(),
        run: text(arguments, "run_id").map(str::to_owned),
    };
    let store = Store::of(root).await;
    let root = root.to_owned();
    let started =
        tokio::task::spawn_blocking(move || ukai_acp::flows::start(&root, &store, &request))
            .await
            .map_err(|error| error.to_string())?
            .map_err(said)?;
    Ok(format!(
        "Started run {} in the background ({}: flow.ts, run.log, events.jsonl, and result.json \
         once it ends). The user sees it live on its graph in a ukai pane; tell them it is \
         running. No message reaches you when it ends: `{READ_FLOW}` says how far it is.",
        started.run,
        started.folder.display()
    ))
}

async fn read(root: &Path, arguments: &Value) -> Result<String, String> {
    let run = required(arguments, "run_id")?.to_owned();
    let store = Store::of(root).await;
    let (first, events, folder) = tokio::task::spawn_blocking(move || {
        let first = store.read(&run, 0)?;
        let mut events = first.events.clone();
        let mut next = first.next;
        for _ in 1..MAX_READS {
            let more = store.read(&run, next)?;
            if more.events.is_empty() {
                break;
            }
            events.push_str(&more.events);
            next = more.next;
        }
        let folder = store.folder(&run)?;
        // The state and result as they are after the events were read.
        Ok::<_, FlowError>((store.read(&run, next)?, events, folder))
    })
    .await
    .map_err(|error| error.to_string())?
    .map_err(said)?;
    let mut text = format!("Run is {}.", first.state.name());
    match flow::Graph::parse(&first.graph) {
        Ok(graph) => {
            let view = flow::view(&graph, &flow::parse_events(&events));
            text.push_str(&format!(
                " Steps done: {}.\n\n{}",
                view.summary(),
                flow::outline(&view)
            ));
        }
        Err(error) => text.push_str(&format!(" Its graph cannot be read: {error}")),
    }
    match (&first.result, first.state) {
        (Some(result), _) => text.push_str(&format!(
            "\n\nResult ({}):\n{}",
            folder.join("result.json").display(),
            cut(result)
        )),
        (None, RunState::Failed) => {
            let log = folder.join("run.log");
            let said = std::fs::read_to_string(&log).unwrap_or_default();
            text.push_str(&format!(
                "\n\nIt ended without a result. Its log ({}):\n{}",
                log.display(),
                if said.trim().is_empty() {
                    "(empty)".to_owned()
                } else {
                    tail(&said)
                }
            ));
        }
        (None, _) => {}
    }
    Ok(text)
}

async fn order(root: &Path, arguments: &Value, steer: bool) -> Result<String, String> {
    let run = required(arguments, "run_id")?;
    let sent = if steer {
        Order {
            node: Some(required(arguments, "node")?.to_owned()),
            message: Some(required(arguments, "message")?.to_owned()),
        }
    } else {
        Order {
            node: text(arguments, "node").map(str::to_owned),
            message: None,
        }
    };
    let store = Store::of(root).await;
    ukai_acp::flows::order(root, &store, run, &sent)
        .await
        .map(|ordered| ordered.said)
        .map_err(said)
}
