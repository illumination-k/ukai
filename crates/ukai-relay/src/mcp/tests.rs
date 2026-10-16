use std::fs;
use std::path::Path;

use rstest::rstest;
use serde_json::{Value, json};
use tempfile::TempDir;

use super::*;

fn checkout(config: Option<&str>) -> TempDir {
    let dir = TempDir::new().unwrap_or_else(|e| panic!("tempdir: {e}"));
    if let Some(config) = config {
        write(&dir, "ukai.toml", config);
    }
    dir
}

fn write(dir: &TempDir, path: &str, body: &str) {
    let path = dir.path().join(path);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap_or_else(|e| panic!("mkdir: {e}"));
    }
    fs::write(path, body).unwrap_or_else(|e| panic!("write: {e}"));
}

fn req(method: &str, params: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": 7, "method": method, "params": params })
}

fn call(root: &Path, method: &str, params: Value) -> Value {
    handle(root, &req(method, params)).unwrap_or_else(|| panic!("no reply to {method}"))
}

#[test]
fn server_entry_runs_the_relay_for_the_checkout() {
    assert_eq!(
        server_entry(Path::new("/relay/ukai-relay"), Path::new("/workspace")),
        json!({
            "name": "ukai",
            "command": "/relay/ukai-relay",
            "args": ["mcp", "--root", "/workspace"],
            "env": []
        })
    );
}

#[rstest]
#[case::known(json!("2025-03-26"), "2025-03-26")]
#[case::unknown(json!("1999-01-01"), PROTOCOL_VERSIONS[0])]
#[case::missing(Value::Null, PROTOCOL_VERSIONS[0])]
fn initialize_agrees_on_a_version_and_gives_instructions(
    #[case] requested: Value,
    #[case] expected: &str,
) {
    let dir = checkout(Some("[reports]\ndirs = [\"reports\"]\n"));
    let reply = call(
        dir.path(),
        "initialize",
        json!({ "protocolVersion": requested, "capabilities": {} }),
    );
    assert_eq!(reply["id"], json!(7));
    assert_eq!(reply["result"]["protocolVersion"], json!(expected));
    assert_eq!(reply["result"]["capabilities"], json!({ "tools": {} }));
    let instructions = reply["result"]["instructions"].as_str().unwrap_or_default();
    assert!(instructions.contains("`reports/`"), "{instructions}");
}

#[rstest]
#[case::no_config(None, "declares no report directories")]
#[case::no_dirs(Some("[project]\nname = \"x\"\n"), "declares no report directories")]
#[case::offline(Some("[reports]\ndirs = [\"out\"]\n"), "cannot reach the network")]
#[case::online(
    Some("[reports]\ndirs = [\"out\"]\nallow_network = true\n"),
    "may load from the network"
)]
#[case::unreadable(Some("[reports\n"), "cannot be read")]
fn instructions_follow_the_checkouts_report_settings(
    #[case] config: Option<&str>,
    #[case] expected: &str,
) {
    let dir = checkout(config);
    let text = instructions(dir.path());
    assert!(text.contains(expected), "{text}");
    assert!(text.contains("`list_reports`"), "{text}");
}

#[test]
fn lists_the_one_tool() {
    let dir = checkout(None);
    let reply = call(dir.path(), "tools/list", json!({}));
    let names: Vec<&str> = reply["result"]["tools"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|tool| tool["name"].as_str())
        .collect();
    assert_eq!(names, ["list_reports"]);
}

#[test]
fn list_reports_returns_what_the_sidebar_shows() {
    let dir = checkout(Some("[reports]\ndirs = [\"reports\"]\n"));
    write(&dir, "reports/summary.html", "<p>hi</p>");
    write(&dir, "reports/notes.txt", "not a report");
    let reply = call(dir.path(), "tools/call", json!({ "name": "list_reports" }));
    assert_eq!(
        reply["result"],
        json!({ "content": [{ "type": "text", "text": "reports/summary.html" }], "isError": false })
    );
}

#[rstest]
#[case::empty(Some("[reports]\ndirs = [\"reports\"]\n"), "No reports yet.", false)]
#[case::unreadable(Some("[reports\n"), "`ukai.toml` cannot be read", true)]
fn list_reports_says_why_nothing_is_listed(
    #[case] config: Option<&str>,
    #[case] expected: &str,
    #[case] failed: bool,
) {
    let dir = checkout(config);
    let reply = call(dir.path(), "tools/call", json!({ "name": "list_reports" }));
    let text = reply["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default();
    assert!(text.starts_with(expected), "{text}");
    assert_eq!(reply["result"]["isError"], json!(failed));
}

#[rstest]
#[case::unknown_tool("tools/call", json!({ "name": "rm_rf" }), -32602)]
#[case::unknown_method("resources/list", json!({}), -32601)]
fn refuses_what_it_does_not_offer(#[case] method: &str, #[case] params: Value, #[case] code: i64) {
    let dir = checkout(None);
    assert_eq!(
        call(dir.path(), method, params)["error"]["code"],
        json!(code)
    );
}

#[rstest]
#[case::notification(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))]
#[case::response(json!({ "jsonrpc": "2.0", "id": 1, "result": {} }))]
fn answers_nothing_that_wants_no_answer(#[case] message: Value) {
    let dir = checkout(None);
    assert_eq!(handle(dir.path(), &message), None);
}

#[test]
fn answers_ping() {
    let dir = checkout(None);
    assert_eq!(
        call(dir.path(), "ping", json!({})),
        json!({ "jsonrpc": "2.0", "id": 7, "result": {} })
    );
}
