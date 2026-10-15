//! `ukai-relay mcp`: a stdio MCP server the relay adds to every session it
//! sets up (see [`crate::hub`]), so the agent learns what ukai does with its
//! work without anything written into the checkout.
//!
//! An agent behind the relay has no idea a person is watching it through
//! ukai, or that an HTML page it writes under `reports.dirs` shows up in that
//! person's sidebar. MCP is the one channel every ACP agent takes from its
//! client, and a server's `instructions` go into the agent's context, so they
//! carry that: what a report is, where this checkout keeps them, and what a
//! report can load. The `list_reports` tool shows what the user can open
//! right now.
//!
//! The protocol is newline-delimited JSON-RPC on stdin/stdout; [`handle`]
//! answers one message and does no I/O beyond reading the checkout.

use std::io::{self, BufRead, Write};
use std::path::Path;

use serde_json::{Value, json};
use ukai_config::Reports;
use ukai_core::report::ReportTree;

/// The server's name in `mcpServers`; agents prefix its tools with it.
pub const SERVER_NAME: &str = "ukai";

/// The subcommand of `ukai-relay` that runs the server.
pub const SUBCOMMAND: &str = "mcp";

const LIST_REPORTS: &str = "list_reports";

/// MCP revisions this server can speak; what it uses (tools, instructions,
/// text content) is the same in all of them.
const PROTOCOL_VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

/// The ACP `mcpServers` entry that starts this server for the checkout at
/// `root`, with `program` being the relay binary.
pub fn server_entry(program: &Path, root: &Path) -> Value {
    json!({
        "name": SERVER_NAME,
        "command": program.to_string_lossy(),
        "args": [SUBCOMMAND, "--root", root.to_string_lossy()],
        "env": []
    })
}

/// Serve MCP on stdin/stdout for the checkout at `root` until stdin closes.
pub fn serve(root: &Path) -> io::Result<()> {
    let stdin = io::stdin().lock();
    let mut stdout = io::stdout().lock();
    for line in stdin.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<Value>(&line) {
            Ok(message) => handle(root, &message),
            Err(_) => Some(error(Value::Null, -32700, "parse error")),
        };
        if let Some(reply) = reply {
            serde_json::to_writer(&mut stdout, &reply)?;
            stdout.write_all(b"\n")?;
            stdout.flush()?;
        }
    }
    Ok(())
}

/// The answer to one message from the agent: `None` for notifications and
/// responses, which want none.
pub fn handle(root: &Path, message: &Value) -> Option<Value> {
    let id = message.get("id").filter(|id| !id.is_null())?.clone();
    let method = message.get("method").and_then(Value::as_str)?;
    Some(match method {
        "initialize" => {
            let requested = message
                .pointer("/params/protocolVersion")
                .and_then(Value::as_str);
            let version = PROTOCOL_VERSIONS
                .iter()
                .find(|v| Some(**v) == requested)
                .unwrap_or(&PROTOCOL_VERSIONS[0]);
            result(
                id,
                json!({
                    "protocolVersion": version,
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": SERVER_NAME, "version": env!("CARGO_PKG_VERSION") },
                    "instructions": instructions(root)
                }),
            )
        }
        "ping" => result(id, json!({})),
        "tools/list" => result(
            id,
            json!({
                "tools": [{
                    "name": LIST_REPORTS,
                    "description": "List the reports the user can open in ukai right now, newest first, as paths relative to the checkout.",
                    "inputSchema": { "type": "object", "properties": {} },
                    "annotations": { "readOnlyHint": true }
                }]
            }),
        ),
        "tools/call" => match message.pointer("/params/name").and_then(Value::as_str) {
            Some(LIST_REPORTS) => {
                let (text, failed) = match list_reports(root) {
                    Ok(text) => (text, false),
                    Err(text) => (text, true),
                };
                result(
                    id,
                    json!({ "content": [{ "type": "text", "text": text }], "isError": failed }),
                )
            }
            _ => error(id, -32602, "unknown tool"),
        },
        _ => error(id, -32601, "method not found"),
    })
}

/// What the agent is told about ukai, from the checkout's `ukai.toml` as it
/// is now.
pub fn instructions(root: &Path) -> String {
    let mut text = String::from(
        "The user is following this session in ukai, a desktop client for ACP agents. \
         They see the conversation and its tool calls, not the files on this machine.\n\n\
         Reports: HTML pages and images written into this checkout's report directories \
         appear in the user's ukai sidebar, newest first, and open in a pane that reloads \
         whenever the file changes. When something reads better as a page than as chat — \
         findings, tables, diagrams, plots, a review — write it as a report and tell the \
         user its path.\n\n",
    );
    match ukai_config::load_layered(None, root) {
        Ok(config) if config.reports.dirs.is_empty() => text.push_str(
            "This checkout declares no report directories, so nothing written here shows up \
             as a report. If the user wants reports, add to `ukai.toml` at the checkout root \
             (it takes effect immediately):\n\n```toml\n[reports]\ndirs = [\"reports\"]\n```\n",
        ),
        Ok(config) => text.push_str(&report_rules(&config.reports)),
        Err(error) => text.push_str(&format!(
            "`ukai.toml` in this checkout cannot be read, so no reports are listed until it \
             is fixed: {error}\n"
        )),
    }
    text.push_str(&format!(
        "\nThe `{LIST_REPORTS}` tool lists the reports the user can open right now."
    ));
    text
}

fn report_rules(reports: &Reports) -> String {
    let list = |items: Vec<String>| items.join(", ");
    let dirs = list(
        reports
            .dirs
            .iter()
            .map(|dir| format!("`{}/`", dir.display()))
            .collect(),
    );
    let globs = list(reports.globs.iter().map(|g| format!("`{g}`")).collect());
    let network = if reports.allow_network {
        "Reports may load from the network (CDN scripts, web fonts, remote images)."
    } else {
        "Reports cannot reach the network: inline scripts and styles, or keep them as files \
         in the report directory; CDN scripts, web fonts and remote images do not load. \
         `http(s)` links still open in the user's browser."
    };
    format!(
        "Report directories: {dirs}. Files there matching {globs} are listed.\n\
         - A report loads only files inside its report directory: refer to stylesheets, \
         scripts and images relatively (`./plot.png`, `../assets/style.css`) and keep them \
         in there.\n\
         - {network}\n\
         - A report's scripts must not declare a top-level `ipc` with `const` or `let`; the \
         viewer already defines it.\n"
    )
}

fn list_reports(root: &Path) -> Result<String, String> {
    let config = ukai_config::load_layered(None, root)
        .map_err(|error| format!("`ukai.toml` cannot be read: {error}"))?;
    let tree = ReportTree::open(root, &config.reports).map_err(|error| error.to_string())?;
    let reports = tree.list();
    if reports.is_empty() {
        return Ok("No reports yet.".to_owned());
    }
    Ok(reports
        .iter()
        .map(|listed| listed.path.to_string_lossy())
        .collect::<Vec<_>>()
        .join("\n"))
}

fn result(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn error(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

#[cfg(test)]
mod tests;
