//! What the MCP server tells the agent about workflows, and the tools that
//! start, read, and stop a run on the relay (`crate::workflows`).

use std::path::Path;
use std::time::Duration;

use serde_json::{Value, json};
use ukai_acp::workflows::{RelayError, RelayWorkflows};
use ukai_core::workflow::{self, Launcher, StartRequest, WorkflowListing};

pub(super) const RUN_WORKFLOW: &str = "run_workflow";
pub(super) const READ_WORKFLOW: &str = "read_workflow";
pub(super) const LIST_WORKFLOWS: &str = "list_workflows";
pub(super) const STOP_WORKFLOW: &str = "stop_workflow";

/// How long `read_workflow` waits at most for a run to end: under what an
/// agent lets a tool call take.
const WAIT_MAX: u64 = 50;

/// What the agent is told about workflows in the checkout at `root`.
pub(super) fn instructions(root: &Path) -> String {
    let mut text = format!(
        "\n\nWorkflows: when a task is better done as several agents than as one long turn — \
         the same question about many files, independent checks run side by side, a \
         check-and-fix loop, steps a later run should resume — write it as an ayaito flow \
         and start it with `{RUN_WORKFLOW}`. ayaito (https://github.com/illumination-k/ayaito) \
         builds typed graphs of nodes in TypeScript: `fnNode` (code), `agentNode` and \
         `refineNode` (an agent's validated answer), `Flow.start(name).then(node, {{ map, \
         use }}).build()`, `parallel`, `until`, `assign`, and the middleware `checkpoint` \
         and `retry`.\n\
         The script's default export is a node, or a function of `{{ runtime, input, runId \
         }}` returning one. Pass `runtime` to every agent node: its agents are sessions of \
         this workspace's agent, which the user sees listed under the run and can open. \
         `output` is a zod schema (objects only, no tuples); `access` is `\"read-only\"` \
         (the default here: what the agent asks permission for is refused) or `\"write\"` \
         (it is allowed). Nobody answers questions during a run, so a node's prompt must \
         say all it needs.\n\n\
         ```ts\n\
         import {{ agentNode, Flow, parallel }} from \"ayaito\";\n\
         import {{ z }} from \"zod\";\n\n\
         export default ({{ runtime }}) => {{\n\
         \x20 const check = (name: string, what: string) =>\n\
         \x20   agentNode(name, {{ prompt: (s: {{ dir: string }}) => `${{what}} in ${{s.dir}}`,\n\
         \x20     output: z.object({{ findings: z.array(z.string()) }}), access: \"read-only\" }}, runtime);\n\
         \x20 return Flow.start<{{ dir: string }}>(\"audit\")\n\
         \x20   .then(parallel(\"checks\", {{ errors: check(\"errors\", \"Find unhandled errors\"),\n\
         \x20     tests: check(\"tests\", \"Find untested code\") }}))\n\
         \x20   .build();\n\
         }};\n\
         ```\n\n\
         `{RUN_WORKFLOW}` takes the script as `script` (its source; it can import `ayaito` \
         and `zod` only), as `path` (a file of this checkout, which imports what the \
         checkout has installed), or a declared flow's name as `flow`, with `input` as the \
         flow's input. It returns at once with a run id; `{READ_WORKFLOW}` tells how the run \
         and each node are doing and, once it is over, its output — call it again until \
         the run has ended. `{STOP_WORKFLOW}` cancels a run. Starting a run again under its \
         id (`run`) resumes it: nodes behind `checkpoint` return what they saved.\n"
    );
    let declared = crate::workflows::declared(root);
    if declared.is_empty() {
        text.push_str(
            "This checkout declares no workflows. One the user should be able to start \
             themselves goes into `ukai.toml`:\n\n```toml\n[workflows.audit]\npath = \
             \"flows/audit.ts\"\ndescription = \"Audits a directory\"\n```\n",
        );
    } else {
        text.push_str("The workflows this checkout declares, which the user can start too:\n");
        for flow in declared {
            text.push_str(&format!("- `{}` ({})", flow.name, flow.path));
            if let Some(description) = flow.description {
                text.push_str(&format!(": {description}"));
            }
            text.push('\n');
        }
    }
    text
}

pub(super) fn tools() -> [Value; 4] {
    let run = json!({ "type": "string", "description": "The run's id." });
    [
        json!({
            "name": RUN_WORKFLOW,
            "description": "Start an ayaito flow in this workspace; its agent nodes run as sessions of this workspace's agent. Returns the run's id at once: follow it with read_workflow. Give exactly one of `script`, `path`, and `flow`.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "script": { "type": "string", "description": "The flow's TypeScript source. Its default export is an ayaito node, or a function ({ runtime, input, runId }) returning one." },
                    "path": { "type": "string", "description": "A script of this checkout, relative to its root." },
                    "flow": { "type": "string", "description": "A workflow this checkout's ukai.toml declares, by name." },
                    "input": { "description": "The flow's input: any JSON value." },
                    "run": { "type": "string", "description": "An earlier run's id, to resume it: letters, digits, `-`, and `_`." }
                }
            }
        }),
        json!({
            "name": READ_WORKFLOW,
            "description": "How a run and each of its nodes are doing, and its output and what it printed once it has ended. With `wait_seconds`, waits up to that long for the run to end first.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "run": run,
                    "wait_seconds": { "type": "integer", "minimum": 0, "maximum": WAIT_MAX, "description": "How long to wait for the run to end." }
                },
                "required": ["run"]
            },
            "annotations": { "readOnlyHint": true }
        }),
        json!({
            "name": LIST_WORKFLOWS,
            "description": "The workflows this checkout declares and the runs kept, newest first.",
            "inputSchema": { "type": "object", "properties": {} },
            "annotations": { "readOnlyHint": true }
        }),
        json!({
            "name": STOP_WORKFLOW,
            "description": "Cancel a run that is going.",
            "inputSchema": { "type": "object", "properties": { "run": run }, "required": ["run"] }
        }),
    ]
}

/// Answers `tool`, or `None` when it is not one of these.
pub(super) async fn call(
    relay: &RelayWorkflows,
    tool: &str,
    arguments: &Value,
) -> Option<Result<String, String>> {
    let text = |key: &str| {
        arguments
            .get(key)
            .and_then(Value::as_str)
            .map(str::to_owned)
    };
    let outcome = match tool {
        RUN_WORKFLOW => {
            let request = StartRequest {
                flow: text("flow"),
                path: text("path"),
                script: text("script"),
                input: arguments
                    .get("input")
                    .filter(|input| !input.is_null())
                    .map(Value::to_string)
                    .unwrap_or_default(),
                run: text("run"),
                by: Launcher::Agent,
            };
            relay.start(&request).await.map(|run| {
                format!(
                    "Started run {} of {}. The user sees it in ukai. Follow it with {READ_WORKFLOW}.",
                    run.id, run.flow
                )
            })
        }
        READ_WORKFLOW => {
            let wait = arguments
                .get("wait_seconds")
                .and_then(Value::as_u64)
                .unwrap_or(0)
                .min(WAIT_MAX);
            read(relay, &text("run").unwrap_or_default(), wait).await
        }
        LIST_WORKFLOWS => relay.list().await.map(|listing| describe(&listing)),
        STOP_WORKFLOW => relay
            .stop(&text("run").unwrap_or_default())
            .await
            .map(|run| format!("Run {} is {}.", run.id, workflow::state_label(run.state))),
        _ => return None,
    };
    Some(outcome.map_err(|error| match error {
        RelayError::NotFound => "no such run; list_workflows lists the runs kept".to_owned(),
        error => error.to_string(),
    }))
}

/// The run, once it has ended or `wait` seconds have passed.
async fn read(relay: &RelayWorkflows, run: &str, wait: u64) -> Result<String, RelayError> {
    let until = tokio::time::Instant::now() + Duration::from_secs(wait);
    loop {
        let detail = relay.get(run).await?;
        if !detail.summary.state.is_running() || tokio::time::Instant::now() >= until {
            return Ok(workflow::describe(&detail));
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

fn describe(listing: &WorkflowListing) -> String {
    let mut text = String::from("Declared workflows:\n");
    if listing.flows.is_empty() {
        text.push_str("(none)\n");
    }
    for flow in &listing.flows {
        text.push_str(&format!("- {} ({})", flow.name, flow.path));
        if let Some(description) = &flow.description {
            text.push_str(&format!(": {description}"));
        }
        text.push('\n');
    }
    text.push_str("\nRuns, newest first:\n");
    if listing.runs.is_empty() {
        text.push_str("(none)\n");
    }
    for run in &listing.runs {
        text.push_str(&format!(
            "- {} of {}: {}",
            run.id,
            run.flow,
            workflow::state_label(run.state)
        ));
        if let Some(detail) = &run.detail {
            text.push_str(&format!(" ({detail})"));
        }
        text.push('\n');
    }
    text
}
