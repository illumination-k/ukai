use rstest::rstest;
use serde_json::{Value, json};

use super::*;

const A: ConnId = 1;
const B: ConnId = 2;
const SID: &str = "s1";

fn req(id: u64, method: &str, params: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
}

fn update(text: &str) -> Value {
    notification(
        "session/update",
        json!({
            "sessionId": SID,
            "update": { "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": text } }
        }),
    )
}

/// `outputs` without what is metered, which `says_what_a_turn_did_for_measuring_it` checks.
fn unmetered(outputs: Vec<Output>) -> Vec<Output> {
    outputs
        .into_iter()
        .filter(|output| !matches!(output, Output::Metered { .. }))
        .collect()
}

/// The id of the one message to the agent in `outputs`, besides what is
/// metered.
fn agent_id(outputs: &[Output]) -> u64 {
    let unmetered: Vec<&Output> = outputs
        .iter()
        .filter(|output| !matches!(output, Output::Metered { .. }))
        .collect();
    match unmetered.as_slice() {
        [Output::Agent(message)] => message["id"].as_u64().unwrap_or(u64::MAX),
        other => panic!("expected one message to the agent, got {other:?}"),
    }
}

fn client_messages(outputs: &[Output], conn: ConnId) -> Vec<Value> {
    outputs
        .iter()
        .filter_map(|output| match output {
            Output::Client(c, message) if *c == conn => Some(message.clone()),
            _ => None,
        })
        .collect()
}

/// A hub whose agent is initialized, with client `A` connected to session `SID`.
fn with_session() -> Hub {
    let (mut hub, init) = Hub::new(None, Vec::new());
    let init_id = agent_id(&init);
    hub.from_agent(result(
        json!(init_id),
        json!({ "protocolVersion": 1, "agentCapabilities": { "loadSession": false } }),
    ));
    hub.connect(A);
    hub.from_client(A, req(1, "initialize", json!({ "protocolVersion": 1 })));
    let sent = hub.from_client(
        A,
        req(2, "session/new", json!({ "cwd": "/w", "mcpServers": [] })),
    );
    let out = hub.from_agent(result(
        json!(agent_id(&sent)),
        json!({ "sessionId": SID, "modes": null }),
    ));
    assert_eq!(
        client_messages(&out, A),
        [result(json!(2), json!({ "sessionId": SID, "modes": null }))]
    );
    hub
}

fn prompt(hub: &mut Hub, conn: ConnId, id: u64, text: &str) -> u64 {
    agent_id(&hub.from_client(
        conn,
        req(
            id,
            "session/prompt",
            json!({ "sessionId": SID, "prompt": [{ "type": "text", "text": text }] }),
        ),
    ))
}

#[test]
fn answers_initialize_from_the_agents_with_load_session() {
    let (mut hub, init) = Hub::new(None, Vec::new());
    assert_eq!(init[0], Output::Agent(init_request(&init)));
    hub.connect(A);
    // Before the agent answers, a client's initialize waits.
    assert!(
        hub.from_client(A, req(7, "initialize", json!({})))
            .is_empty()
    );
    let out = hub.from_agent(result(
        json!(agent_id(&init)),
        json!({ "agentCapabilities": { "loadSession": false } }),
    ));
    assert_eq!(
        client_messages(&out, A),
        [result(
            json!(7),
            json!({ "agentCapabilities": { "loadSession": true } })
        )]
    );
    // Later clients are answered at once, and the agent is not asked again.
    hub.connect(B);
    let out = hub.from_client(B, req(1, "initialize", json!({})));
    assert_eq!(client_messages(&out, B).len(), 1);
}

fn init_request(init: &[Output]) -> Value {
    match init {
        [Output::Agent(message)] => {
            assert_eq!(message["method"], "initialize");
            assert_eq!(
                message["params"]["clientCapabilities"]["fs"]["readTextFile"],
                false
            );
            message.clone()
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn forwards_a_turn_and_maps_ids_back() {
    let mut hub = with_session();
    let turn = prompt(&mut hub, A, 3, "hi");
    assert!(hub.busy());
    assert_eq!(
        client_messages(&hub.from_agent(update("hello")), A),
        [update("hello")]
    );
    let out = hub.from_agent(result(json!(turn), json!({ "stopReason": "end_turn" })));
    assert_eq!(
        client_messages(&out, A),
        [result(json!(3), json!({ "stopReason": "end_turn" }))]
    );
    assert!(out.contains(&Output::TurnEnded(SID.into())));
    hub.disconnect(A);
    assert!(!hub.busy());
}

#[test]
fn a_turn_finishes_without_a_client_and_replays_on_load() {
    let mut hub = with_session();
    let turn = prompt(&mut hub, A, 3, "do it");
    hub.from_agent(update("start"));
    hub.disconnect(A);
    // The agent keeps working with nobody connected.
    assert!(hub.busy());
    assert!(unmetered(hub.from_agent(update("while away"))).is_empty());
    assert_eq!(
        unmetered(hub.from_agent(result(json!(turn), json!({ "stopReason": "end_turn" })))),
        [Output::TurnEnded(SID.into())]
    );
    assert!(!hub.busy());

    hub.connect(B);
    let out = hub.from_client(
        B,
        req(1, "session/load", json!({ "sessionId": SID, "cwd": "/w" })),
    );
    let messages = client_messages(&out, B);
    let texts: Vec<&str> = messages
        .iter()
        .filter_map(|m| {
            m.pointer("/params/update/content/text")
                .and_then(Value::as_str)
        })
        .collect();
    assert_eq!(texts, ["do it", "start", "while away"]);
    assert_eq!(
        messages[0].pointer("/params/update/sessionUpdate"),
        Some(&json!("user_message_chunk"))
    );
    // The load answer comes last, without the session id `session/new` returned.
    assert_eq!(
        messages.last(),
        Some(&result(json!(1), json!({ "modes": null })))
    );
}

fn steer(hub: &mut Hub, id: u64, text: &str) -> Vec<Output> {
    hub.from_client(
        A,
        req(
            id,
            ukai_acp::STEER_METHOD,
            json!({ "sessionId": SID, "prompt": [{ "type": "text", "text": text }] }),
        ),
    )
}

fn replayed_texts(hub: &mut Hub) -> Vec<String> {
    hub.disconnect(A);
    hub.connect(B);
    let out = hub.from_client(B, req(1, "session/load", json!({ "sessionId": SID })));
    client_messages(&out, B)
        .iter()
        .filter_map(|m| {
            m.pointer("/params/update/content/text")?
                .as_str()
                .map(str::to_owned)
        })
        .collect()
}

#[test]
fn a_steer_into_a_running_turn_is_replayed() {
    let mut hub = with_session();
    let turn = prompt(&mut hub, A, 3, "do it");
    let steered = agent_id(&steer(&mut hub, 4, "left instead"));
    hub.from_agent(result(json!(steered), json!({ "outcome": "injected" })));
    hub.from_agent(result(json!(turn), json!({ "stopReason": "end_turn" })));
    assert_eq!(replayed_texts(&mut hub), ["do it", "left instead"]);
}

#[test]
fn a_steer_without_a_turn_is_not_recorded() {
    // The agent hands it back, and the client sends it as a prompt.
    let mut hub = with_session();
    let steered = agent_id(&steer(&mut hub, 3, "hello"));
    let out = hub.from_agent(result(
        json!(steered),
        json!({ "outcome": "promptRequired" }),
    ));
    assert_eq!(
        client_messages(&out, A),
        [result(json!(3), json!({ "outcome": "promptRequired" }))]
    );
    assert!(replayed_texts(&mut hub).is_empty());
}

#[test]
fn a_client_attaching_mid_turn_hears_it_running_and_ending() {
    let mut hub = with_session();
    let turn = prompt(&mut hub, A, 3, "long job");
    hub.disconnect(A);
    hub.connect(B);
    let out = hub.from_client(B, req(1, "session/load", json!({ "sessionId": SID })));
    assert_eq!(
        client_messages(&out, B).last(),
        Some(&turn_state(SID, "running", None, None))
    );
    assert_eq!(
        client_messages(&hub.from_agent(update("more")), B),
        [update("more")]
    );
    let out = hub.from_agent(result(json!(turn), json!({ "stopReason": "end_turn" })));
    assert_eq!(
        client_messages(&out, B),
        [turn_state(SID, "idle", Some(json!("end_turn")), None)]
    );
}

#[test]
fn a_client_attaching_mid_turn_hears_the_tokens_it_took() {
    let mut hub = with_session();
    let turn = prompt(&mut hub, A, 3, "long job");
    hub.disconnect(A);
    hub.connect(B);
    hub.from_client(B, req(1, "session/load", json!({ "sessionId": SID })));
    let usage = json!({ "totalTokens": 30, "inputTokens": 20, "outputTokens": 10 });
    let out = hub.from_agent(result(
        json!(turn),
        json!({ "stopReason": "end_turn", "usage": usage }),
    ));
    assert_eq!(
        client_messages(&out, B),
        [turn_state(
            SID,
            "idle",
            Some(json!("end_turn")),
            Some(usage)
        )]
    );
}

#[test]
fn resume_attaches_without_replay() {
    let mut hub = with_session();
    hub.from_agent(update("earlier"));
    hub.disconnect(A);
    hub.connect(B);
    let out = hub.from_client(B, req(1, "session/resume", json!({ "sessionId": SID })));
    assert_eq!(
        client_messages(&out, B),
        [result(json!(1), json!({ "modes": null }))]
    );
}

fn permission(id: u64) -> Value {
    req(
        id,
        "session/request_permission",
        json!({ "sessionId": SID, "options": [] }),
    )
}

#[test]
fn permission_requests_wait_for_a_client() {
    let mut hub = with_session();
    prompt(&mut hub, A, 3, "rm");
    hub.disconnect(A);
    assert!(unmetered(hub.from_agent(permission(99))).is_empty());

    hub.connect(B);
    let out = hub.from_client(B, req(1, "session/load", json!({ "sessionId": SID })));
    let asked = client_messages(&out, B)
        .into_iter()
        .find(|m| m["method"] == "session/request_permission");
    let Some(asked) = asked else {
        panic!("the pending permission request was not sent again");
    };
    let answer = json!({ "jsonrpc": "2.0", "id": asked["id"], "result": { "outcome": { "outcome": "selected", "optionId": "allow" } } });
    match hub.from_client(B, answer).as_slice() {
        [Output::Agent(message)] => {
            assert_eq!(message["id"], 99);
            assert_eq!(message["result"]["outcome"]["optionId"], "allow");
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_permission_sent_to_a_departed_client_goes_to_the_next() {
    let mut hub = with_session();
    prompt(&mut hub, A, 3, "rm");
    let first = client_messages(&hub.from_agent(permission(99)), A);
    hub.disconnect(A);
    hub.connect(B);
    let out = hub.from_client(B, req(1, "session/load", json!({ "sessionId": SID })));
    assert!(
        client_messages(&out, B)
            .iter()
            .any(|m| m["method"] == "session/request_permission")
    );
    // A late answer under the old id reaches nobody.
    let late = json!({ "jsonrpc": "2.0", "id": first[0]["id"], "result": {} });
    assert!(hub.from_client(A, late).is_empty());
}

#[test]
fn loads_a_session_the_relay_does_not_know_through_the_agent() {
    let (mut hub, init) = Hub::new(None, Vec::new());
    hub.from_agent(result(json!(agent_id(&init)), json!({})));
    hub.connect(A);
    let sent = hub.from_client(A, req(1, "session/load", json!({ "sessionId": SID })));
    let load = agent_id(&sent);
    // The agent's replay reaches the client and is recorded.
    assert_eq!(
        client_messages(&hub.from_agent(update("from disk")), A),
        [update("from disk")]
    );
    hub.from_agent(result(json!(load), json!({})));
    hub.disconnect(A);

    hub.connect(B);
    let out = hub.from_client(B, req(1, "session/load", json!({ "sessionId": SID })));
    assert_eq!(client_messages(&out, B)[0], update("from disk"));
}

#[test]
fn a_failed_load_forgets_the_session() {
    let (mut hub, init) = Hub::new(None, Vec::new());
    hub.from_agent(result(json!(agent_id(&init)), json!({})));
    hub.connect(A);
    let load = agent_id(&hub.from_client(A, req(1, "session/load", json!({ "sessionId": SID }))));
    let failed = error(json!(load), -32002, "no such session");
    assert_eq!(
        client_messages(&hub.from_agent(failed), A),
        [error(json!(1), -32002, "no such session")]
    );
    // A second try goes to the agent again rather than replaying nothing.
    assert!(matches!(
        hub.from_client(A, req(2, "session/load", json!({ "sessionId": SID })))
            .as_slice(),
        [Output::Agent(_)]
    ));
}

#[test]
fn a_client_whose_session_was_taken_over_still_gets_its_answer() {
    let mut hub = with_session();
    let turn = prompt(&mut hub, A, 3, "long job");
    hub.connect(B);
    hub.from_client(B, req(1, "session/load", json!({ "sessionId": SID })));
    let out = hub.from_agent(result(json!(turn), json!({ "stopReason": "end_turn" })));
    assert_eq!(
        client_messages(&out, A),
        [result(json!(3), json!({ "stopReason": "end_turn" }))]
    );
    assert_eq!(
        client_messages(&out, B),
        [turn_state(SID, "idle", Some(json!("end_turn")), None)]
    );
}

#[test]
fn a_new_attachment_takes_the_session_over() {
    let mut hub = with_session();
    hub.connect(B);
    hub.from_client(B, req(1, "session/load", json!({ "sessionId": SID })));
    let out = hub.from_agent(update("x"));
    assert!(client_messages(&out, A).is_empty());
    assert_eq!(client_messages(&out, B), [update("x")]);
}

#[test]
fn session_setup_runs_in_the_relays_directory() {
    let (mut hub, init) = Hub::new(Some("/workspace/repo".into()), Vec::new());
    hub.from_agent(result(json!(agent_id(&init)), json!({})));
    hub.connect(A);
    // `session/list` too, so it lists the sessions in this workspace.
    for method in ["session/new", "session/load", "session/list"] {
        match hub
            .from_client(
                A,
                req(
                    1,
                    method,
                    json!({ "sessionId": SID, "cwd": "/Users/me/repo" }),
                ),
            )
            .as_slice()
        {
            [Output::Agent(message)] => assert_eq!(message["params"]["cwd"], "/workspace/repo"),
            other => panic!("{other:?}"),
        }
    }
}

fn relay_server(name: &str) -> Value {
    json!({ "name": name, "command": "/relay/ukai-relay", "args": ["mcp"], "env": [] })
}

#[rstest]
#[case::new("session/new", json!({ "cwd": "/w", "mcpServers": [] }))]
#[case::load("session/load", json!({ "sessionId": SID, "cwd": "/w", "mcpServers": [] }))]
#[case::resume("session/resume", json!({ "sessionId": SID, "cwd": "/w" }))]
fn session_setup_gets_the_relays_mcp_servers(#[case] method: &str, #[case] params: Value) {
    let (mut hub, init) = Hub::new(None, vec![relay_server("ukai")]);
    hub.from_agent(result(json!(agent_id(&init)), json!({})));
    hub.connect(A);
    match hub.from_client(A, req(1, method, params)).as_slice() {
        [Output::Agent(message)] => {
            let mut expected = relay_server("ukai");
            expected["env"] = json!([{ "name": SESSION_KEY_ENV, "value": "k1" }]);
            assert_eq!(message["params"]["mcpServers"], json!([expected]));
        }
        other => panic!("{other:?}"),
    }
}

/// A hub appending `ROLE` to the system prompt, whose agent answered
/// `initialize` with `caps` as its `sessionCapabilities`.
fn with_system_prompt(caps: Value) -> Hub {
    let (mut hub, init) = Hub::new(None, Vec::new());
    hub.append_system_prompt(ROLE.to_owned());
    hub.from_agent(result(
        json!(agent_id(&init)),
        json!({ "agentCapabilities": { "sessionCapabilities": caps } }),
    ));
    hub.connect(A);
    hub
}

const ROLE: &str = "You are the secretary.";

fn system_prompt_sent(outputs: &[Output]) -> Value {
    match outputs {
        [Output::Agent(message)] => message["params"]["systemPrompt"].clone(),
        other => panic!("{other:?}"),
    }
}

#[rstest]
#[case::new("session/new", json!({ "cwd": "/w", "mcpServers": [] }))]
#[case::load("session/load", json!({ "sessionId": SID, "cwd": "/w", "mcpServers": [] }))]
#[case::resume("session/resume", json!({ "sessionId": SID, "cwd": "/w" }))]
fn session_setup_appends_the_relays_system_prompt(#[case] method: &str, #[case] params: Value) {
    let mut hub = with_system_prompt(json!({ "systemPrompt": {} }));
    assert_eq!(
        system_prompt_sent(&hub.from_client(A, req(1, method, params))),
        json!({ "mode": "append", "content": [{ "type": "text", "text": ROLE }] })
    );
}

#[rstest]
#[case::append("append")]
#[case::replace("override")]
fn the_relays_system_prompt_follows_the_clients(#[case] mode: &str) {
    let mut hub = with_system_prompt(json!({ "systemPrompt": { "mode": { "override": {} } } }));
    let theirs = json!({ "type": "text", "text": "Theirs." });
    let sent = hub.from_client(
        A,
        req(
            1,
            "session/new",
            json!({ "cwd": "/w", "systemPrompt": { "mode": mode, "content": [theirs.clone()] } }),
        ),
    );
    assert_eq!(
        system_prompt_sent(&sent),
        json!({ "mode": mode, "content": [theirs, { "type": "text", "text": ROLE }] })
    );
}

#[rstest]
#[case::no_capability(json!({}))]
#[case::null_capability(json!({ "systemPrompt": null }))]
fn an_agent_without_system_prompts_is_sent_none(#[case] caps: Value) {
    let mut hub = with_system_prompt(caps);
    let sent = hub.from_client(A, req(1, "session/new", json!({ "cwd": "/w" })));
    assert_eq!(system_prompt_sent(&sent), Value::Null);
}

#[test]
fn other_requests_get_no_system_prompt() {
    let mut hub = with_system_prompt(json!({ "systemPrompt": {} }));
    let sent = hub.from_client(A, req(1, "session/list", json!({})));
    assert_eq!(system_prompt_sent(&sent), Value::Null);
}

/// The key a setup sent to the agent gave the relay's servers.
fn key_of(outputs: &[Output]) -> String {
    match outputs {
        [Output::Agent(message)] => message["params"]["mcpServers"][0]["env"][0]["value"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
        other => panic!("{other:?}"),
    }
}

#[test]
fn each_setup_gives_the_relays_servers_a_key_and_names_its_session() {
    let (mut hub, init) = Hub::new(None, vec![relay_server("ukai")]);
    hub.from_agent(result(json!(agent_id(&init)), json!({})));
    hub.connect(A);
    let created = hub.from_client(A, req(1, "session/new", json!({ "cwd": "/w" })));
    let new_key = key_of(&created);
    assert_eq!(
        created.first().and_then(|o| match o {
            Output::Agent(m) => Some(m["params"]["mcpServers"][0]["env"][0]["name"].clone()),
            _ => None,
        }),
        Some(json!(SESSION_KEY_ENV))
    );
    let answered = hub.from_agent(result(
        json!(agent_id(&created)),
        json!({ "sessionId": SID }),
    ));
    assert!(answered.contains(&Output::SessionKey {
        key: new_key.clone(),
        session: SID.into()
    }));
    // The relay restarted under a client that loads another session.
    let loaded = hub.from_client(
        A,
        req(2, "session/load", json!({ "sessionId": "s2", "cwd": "/w" })),
    );
    let load_key = key_of(&loaded);
    assert_ne!(load_key, new_key);
    let answered = hub.from_agent(result(json!(agent_id(&loaded)), json!({})));
    assert!(answered.contains(&Output::SessionKey {
        key: load_key,
        session: "s2".into()
    }));
}

#[test]
fn a_failed_setup_names_no_session() {
    let (mut hub, init) = Hub::new(None, vec![relay_server("ukai")]);
    hub.from_agent(result(json!(agent_id(&init)), json!({})));
    hub.connect(A);
    let created = hub.from_client(A, req(1, "session/new", json!({ "cwd": "/w" })));
    let failed = hub.from_agent(error(json!(agent_id(&created)), -32603, "no"));
    assert!(
        !failed
            .iter()
            .any(|output| matches!(output, Output::SessionKey { .. })),
        "{failed:?}"
    );
}

#[test]
fn the_relays_mcp_servers_join_the_clients_and_replace_a_namesake() {
    let (mut hub, init) = Hub::new(None, vec![relay_server("ukai")]);
    hub.from_agent(result(json!(agent_id(&init)), json!({})));
    hub.connect(A);
    let theirs =
        json!({ "type": "http", "name": "docs", "url": "https://docs.example", "headers": [] });
    let sent = hub.from_client(
        A,
        req(
            1,
            "session/new",
            json!({
                "cwd": "/w",
                "mcpServers": [theirs.clone(), { "name": "ukai", "command": "elsewhere", "args": [], "env": [] }]
            }),
        ),
    );
    match sent.as_slice() {
        [Output::Agent(message)] => {
            let mut ours = relay_server("ukai");
            ours["env"] = json!([{ "name": SESSION_KEY_ENV, "value": "k1" }]);
            assert_eq!(message["params"]["mcpServers"], json!([theirs, ours]));
        }
        other => panic!("{other:?}"),
    }
}

#[rstest]
#[case::list("session/list")]
#[case::prompt("session/prompt")]
fn other_requests_get_no_mcp_servers(#[case] method: &str) {
    let (mut hub, init) = Hub::new(None, vec![relay_server("ukai")]);
    hub.from_agent(result(json!(agent_id(&init)), json!({})));
    hub.connect(A);
    match hub
        .from_client(A, req(1, method, json!({ "sessionId": SID, "prompt": [] })))
        .as_slice()
    {
        [Output::Agent(message)] => assert!(message["params"].get("mcpServers").is_none()),
        other => panic!("{other:?}"),
    }
}

#[test]
fn agent_requests_outside_a_session_are_refused() {
    let (mut hub, _) = Hub::new(None, Vec::new());
    match hub
        .from_agent(req(
            5,
            "fs/read_text_file",
            json!({ "path": "/etc/passwd" }),
        ))
        .as_slice()
    {
        [Output::Agent(message)] => assert_eq!(message["error"]["code"], -32601),
        other => panic!("{other:?}"),
    }
}

fn model_option(current: &str) -> Value {
    json!([{
        "id": "model", "name": "Model", "category": "model", "type": "select",
        "currentValue": current,
        "options": [{ "value": "a", "name": "A" }, { "value": "b", "name": "B" }]
    }])
}

#[test]
fn a_load_answers_with_the_settings_changed_since_setup() {
    let mut hub = with_session();
    let sent = hub.from_client(
        A,
        req(
            3,
            "session/set_config_option",
            json!({ "sessionId": SID, "configId": "model", "value": "b" }),
        ),
    );
    hub.from_agent(result(
        json!(agent_id(&sent)),
        json!({ "configOptions": model_option("b") }),
    ));
    let load = |hub: &mut Hub, conn| {
        hub.disconnect(A);
        hub.connect(conn);
        let out = hub.from_client(conn, req(1, "session/resume", json!({ "sessionId": SID })));
        client_messages(&out, conn)
            .last()
            .and_then(|answer| answer.pointer("/result/configOptions").cloned())
    };
    assert_eq!(load(&mut hub, B), Some(model_option("b")));

    // The agent may also change them on its own.
    hub.from_agent(notification(
        "session/update",
        json!({
            "sessionId": SID,
            "update": { "sessionUpdate": "config_option_update", "configOptions": model_option("a") }
        }),
    ));
    assert_eq!(load(&mut hub, A), Some(model_option("a")));
}

fn unattached_prompt_id(outputs: &[Output]) -> u64 {
    outputs
        .iter()
        .find_map(|output| match output {
            Output::Agent(message) if message["method"] == "session/prompt" => {
                message["id"].as_u64()
            }
            _ => None,
        })
        .unwrap_or(u64::MAX)
}

#[test]
fn lists_sessions_with_their_state() {
    let mut hub = with_session();
    let turn = prompt(&mut hub, A, 3, "fix the build");
    hub.from_agent(update("Fixed."));
    let [summary] = hub
        .sessions()
        .try_into()
        .unwrap_or_else(|s| panic!("{s:?}"));
    assert!(summary.running);
    assert_eq!(summary.last_prompt.as_deref(), Some("fix the build"));
    assert_eq!(summary.last_reply.as_deref(), Some("Fixed."));

    hub.from_agent(permission(7));
    assert!(hub.sessions()[0].waiting);
    hub.from_agent(result(json!(turn), json!({ "stopReason": "end_turn" })));
    let summary = &hub.sessions()[0];
    assert!(!summary.running);
    assert_eq!(summary.stop_reason.as_deref(), Some("end_turn"));
}

#[test]
fn reads_the_end_of_a_transcript() {
    let mut hub = with_session();
    prompt(&mut hub, A, 3, "hello");
    hub.from_agent(update("hi"));
    let texts: Vec<_> = hub
        .transcript(SID, 1)
        .unwrap_or_default()
        .into_iter()
        .map(|entry| entry.text)
        .collect();
    assert_eq!(texts, ["hi"]);
    assert_eq!(hub.transcript("nope", 1), None);
}

#[test]
fn an_unattached_prompt_runs_a_turn_the_attached_client_follows() {
    let mut hub = with_session();
    let outputs = hub
        .prompt_unattached(SID, "from the secretary")
        .unwrap_or_default();
    let turn = unattached_prompt_id(&outputs);
    // The client attached hears the prompt and the turn starting...
    let heard = client_messages(&outputs, A);
    assert_eq!(
        heard[0]["params"]["update"]["sessionUpdate"],
        "user_message_chunk"
    );
    assert_eq!(heard[1], turn_state(SID, "running", None, None));
    assert!(hub.sessions()[0].running);
    // ...a second prompt is refused while it runs...
    assert_eq!(
        hub.prompt_unattached(SID, "again"),
        Err(PromptRefused::Running)
    );
    // ...and the turn ending, though it did not ask.
    let ended = hub.from_agent(result(json!(turn), json!({ "stopReason": "end_turn" })));
    assert_eq!(
        client_messages(&ended, A),
        [turn_state(SID, "idle", Some(json!("end_turn")), None)]
    );
    assert_eq!(
        replayed_texts(&mut hub),
        ["from the secretary"],
        "the prompt is in the history"
    );
}

#[test]
fn an_unattached_prompt_needs_a_known_session() {
    let mut hub = with_session();
    assert_eq!(
        hub.prompt_unattached("nope", "hi"),
        Err(PromptRefused::Unknown)
    );
}

#[test]
fn an_unattached_start_sets_up_a_session_and_prompts_it() {
    let mut hub = with_session();
    let sent = hub.start_unattached(9, "start on #12".to_owned());
    let setup = agent_id(&sent);
    let out = hub.from_agent(result(json!(setup), json!({ "sessionId": "s2" })));
    assert_eq!(
        out[0],
        Output::Started {
            ticket: 9,
            outcome: Ok("s2".to_owned())
        }
    );
    let turn = unattached_prompt_id(&out);
    assert_ne!(turn, u64::MAX, "the prompt goes to the agent: {out:?}");
    assert!(client_messages(&out, A).is_empty(), "no client is told");
    assert!(hub.sessions().iter().any(|s| s.id == "s2" && s.running));
}

#[test]
fn an_unattached_start_says_why_it_failed() {
    let mut hub = with_session();
    let setup = agent_id(&hub.start_unattached(9, "hi".to_owned()));
    let out = hub.from_agent(error(json!(setup), -32603, "no room"));
    assert_eq!(
        out,
        [Output::Started {
            ticket: 9,
            outcome: Err("no room".to_owned())
        }]
    );
}

#[test]
fn an_unattached_start_waits_for_the_agent() {
    let (mut hub, _) = Hub::new(None, Vec::new());
    assert!(matches!(
        hub.start_unattached(1, "hi".to_owned()).as_slice(),
        [Output::Started {
            outcome: Err(_),
            ..
        }]
    ));
}

/// What the hub says happened in `outputs`, for measuring turns.
fn metered(outputs: &[Output]) -> Vec<SessionEvent> {
    outputs
        .iter()
        .filter_map(|output| match output {
            Output::Metered { session, event } if session == SID => Some(event.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn says_what_a_turn_did_for_measuring_it() {
    let mut hub = with_session();
    let out = hub.from_client(
        A,
        req(
            3,
            "session/prompt",
            json!({ "sessionId": SID, "prompt": [{ "type": "text", "text": "go" }] }),
        ),
    );
    assert_eq!(metered(&out), [SessionEvent::TurnStarted]);
    let turn = agent_id(&out);
    assert_eq!(
        metered(&hub.from_agent(update("hi"))),
        [SessionEvent::AgentMessage { text: "hi".into() }]
    );
    let asked = hub.from_agent(json!({
        "jsonrpc": "2.0", "id": 90, "method": "session/request_permission",
        "params": { "sessionId": SID, "toolCall": { "toolCallId": "t" }, "options": [] }
    }));
    assert!(matches!(
        metered(&asked).as_slice(),
        [SessionEvent::PermissionRequested { .. }]
    ));
    let usage = json!({ "totalTokens": 3, "inputTokens": 2, "outputTokens": 1 });
    let ended = hub.from_agent(result(
        json!(turn),
        json!({ "stopReason": "end_turn", "usage": usage }),
    ));
    assert_eq!(
        metered(&ended),
        [SessionEvent::TurnEnded {
            stop_reason: "end_turn".into(),
            tokens: Some(ukai_acp::TokenUsage {
                total: 3,
                input: 2,
                output: 1,
                ..ukai_acp::TokenUsage::default()
            }),
        }]
    );
}
