use rstest::rstest;
use ukai_core::workflow::{NodeState, RunState};

use super::*;

fn workflows(root: &Path, home: &Path) -> Workflows {
    Workflows::new(root.to_owned(), home.to_owned(), 1, None)
}

/// A run as the runner and ayaito leave it.
fn keep(home: &Path, id: &str, started: u64, files: &[(&str, &str)]) {
    let dir = home.join("runs").join(id);
    std::fs::create_dir_all(&dir).unwrap_or_else(|e| panic!("{e}"));
    let meta = format!("{{\"flow\":\"weekly\",\"by\":\"agent\",\"started\":{started}}}");
    std::fs::write(dir.join("run.json"), meta).unwrap_or_else(|e| panic!("{e}"));
    for (name, text) in files {
        std::fs::write(dir.join(name), text).unwrap_or_else(|e| panic!("{e}"));
    }
}

#[test]
fn lists_declared_flows_and_runs_newest_first() {
    let (root, home) = (
        tempfile::tempdir().unwrap_or_else(|e| panic!("{e}")),
        tempfile::tempdir().unwrap_or_else(|e| panic!("{e}")),
    );
    std::fs::write(
        root.path().join("ukai.toml"),
        "[workflows.weekly]\npath = \"flows/weekly.ts\"\ndescription = \"Writes the article\"\n",
    )
    .unwrap_or_else(|e| panic!("{e}"));
    keep(
        home.path(),
        "old",
        1,
        &[("result.json", "{\"status\":\"cancelled\"}")],
    );
    keep(home.path(), "new", 2, &[]);
    let listing = workflows(root.path(), home.path()).listing();
    assert_eq!(
        listing.flows,
        vec![DeclaredFlow {
            name: "weekly".to_owned(),
            path: "flows/weekly.ts".to_owned(),
            description: Some("Writes the article".to_owned()),
        }]
    );
    let runs: Vec<_> = listing
        .runs
        .iter()
        .map(|run| (run.id.as_str(), run.state, run.by))
        .collect();
    // No process is known for `new`: it was cut off with an earlier relay.
    assert_eq!(
        runs,
        vec![
            ("new", RunState::Interrupted, Launcher::Agent),
            ("old", RunState::Cancelled, Launcher::Agent),
        ]
    );
    assert!(listing.runs[1].finished.is_some());
}

#[test]
fn a_run_shows_its_nodes_sessions_output_and_log() {
    let (root, home) = (
        tempfile::tempdir().unwrap_or_else(|e| panic!("{e}")),
        tempfile::tempdir().unwrap_or_else(|e| panic!("{e}")),
    );
    keep(
        home.path(),
        "r1",
        1,
        &[
            (
                "graph.json",
                "[{\"name\":\"weekly\",\"kind\":\"flow\",\"depth\":0},{\"name\":\"pick\",\"kind\":\"agent\",\"depth\":1,\"edge\":\"checkpoint\"}]",
            ),
            (
                "events.jsonl",
                "{\"type\":\"start\",\"node\":\"pick\",\"at\":\"t\"}\n{\"type\":\"end\",\"node\":\"pick\",\"at\":\"t\",\"ms\":7}\n",
            ),
            ("sessions.jsonl", "{\"node\":\"pick\",\"session\":\"s1\"}\n"),
            (
                "result.json",
                "{\"status\":\"done\",\"output\":{\"topic\":\"x\"}}",
            ),
            ("input.json", "{\"site\":\"a\"}"),
            ("log.txt", "hello\n"),
        ],
    );
    let run = workflows(root.path(), home.path())
        .detail("r1")
        .unwrap_or_else(|| panic!("nothing"));
    assert_eq!(run.summary.state, RunState::Done);
    assert_eq!(run.input, "{\"site\":\"a\"}");
    assert_eq!(run.output.as_deref(), Some("{\n  \"topic\": \"x\"\n}"));
    assert_eq!(run.log, "hello\n");
    assert_eq!(run.nodes.len(), 2);
    assert_eq!(run.nodes[0].state, NodeState::Done);
    let pick = &run.nodes[1];
    assert_eq!(
        (
            pick.state,
            pick.ms,
            pick.edge.as_deref(),
            pick.sessions.as_slice()
        ),
        (
            NodeState::Done,
            Some(7),
            Some("checkpoint"),
            ["s1".to_owned()].as_slice()
        )
    );
}

#[test]
fn a_run_that_drew_nothing_is_its_flow_alone() {
    let (root, home) = (
        tempfile::tempdir().unwrap_or_else(|e| panic!("{e}")),
        tempfile::tempdir().unwrap_or_else(|e| panic!("{e}")),
    );
    keep(
        home.path(),
        "r1",
        1,
        &[(
            "result.json",
            "{\"status\":\"failed\",\"error\":\"ayaito is not installed\"}",
        )],
    );
    let run = workflows(root.path(), home.path())
        .detail("r1")
        .unwrap_or_else(|| panic!("nothing"));
    assert_eq!(
        run.summary.detail.as_deref(),
        Some("ayaito is not installed")
    );
    assert_eq!(
        run.nodes
            .iter()
            .map(|n| (n.name.as_str(), n.state))
            .collect::<Vec<_>>(),
        vec![("weekly", NodeState::Failed)]
    );
}

#[rstest]
#[case::unknown("nope")]
#[case::outside("../r1")]
fn no_such_run(#[case] id: &str) {
    let (root, home) = (
        tempfile::tempdir().unwrap_or_else(|e| panic!("{e}")),
        tempfile::tempdir().unwrap_or_else(|e| panic!("{e}")),
    );
    keep(home.path(), "r1", 1, &[]);
    assert_eq!(workflows(root.path(), home.path()).detail(id), None);
}

fn refusal(root: &Path, request: StartRequest) -> String {
    let home = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    workflows(root, home.path())
        .start(&request)
        .err()
        .unwrap_or_default()
        .join("\n")
}

#[test]
fn refuses_a_flow_the_checkout_does_not_declare() {
    let root = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    std::fs::write(
        root.path().join("ukai.toml"),
        "[workflows.weekly]\npath = \"w.ts\"\n",
    )
    .unwrap_or_else(|e| panic!("{e}"));
    let request = StartRequest {
        flow: Some("daily".to_owned()),
        ..StartRequest::default()
    };
    assert_eq!(
        refusal(root.path(), request),
        "the checkout's ukai.toml declares no workflow `daily` (it declares: weekly)"
    );
}

#[rstest]
#[case::missing("flows/none.ts")]
#[case::outside("../flow.ts")]
#[case::absolute("/etc/passwd")]
fn refuses_a_path_that_is_no_file_of_the_checkout(#[case] path: &str) {
    let root = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let request = StartRequest {
        path: Some(path.to_owned()),
        ..StartRequest::default()
    };
    assert_eq!(
        refusal(root.path(), request),
        format!("`{path}` is not a file in the checkout")
    );
}

#[test]
fn every_problem_of_a_request_is_told() {
    let root = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
    let request = StartRequest {
        input: "{".to_owned(),
        ..StartRequest::default()
    };
    assert_eq!(refusal(root.path(), request).lines().count(), 2);
}

#[tokio::test]
async fn a_turn_needs_a_running_run() {
    let (root, home) = (
        tempfile::tempdir().unwrap_or_else(|e| panic!("{e}")),
        tempfile::tempdir().unwrap_or_else(|e| panic!("{e}")),
    );
    let call = AgentCall {
        run: "r1".to_owned(),
        node: None,
        prompt: "hi".to_owned(),
        model: None,
        access: workflow::Access::ReadOnly,
        session: None,
    };
    assert_eq!(
        workflows(root.path(), home.path()).turn(call).await,
        Err("no run r1 is running".to_owned())
    );
}

fn models() -> Vec<ConfigOption> {
    let choice = |value: &str, name: &str| ukai_acp::ConfigChoice {
        value: value.to_owned(),
        name: name.to_owned(),
        description: None,
        group: None,
    };
    vec![ConfigOption {
        id: "model".to_owned(),
        name: "Model".to_owned(),
        description: None,
        category: Some("model".to_owned()),
        value: ConfigValue::Select {
            current: "opus".to_owned(),
            choices: vec![choice("opus", "Opus"), choice("haiku", "Haiku")],
        },
    }]
}

#[rstest]
#[case::by_value("haiku", Ok(Some(("model".to_owned(), "haiku".to_owned()))))]
#[case::by_name("Haiku", Ok(Some(("model".to_owned(), "haiku".to_owned()))))]
#[case::current("opus", Ok(None))]
#[case::unknown("gpt", Err("the agent has no model `gpt` (it has: opus, haiku)".to_owned()))]
fn chooses_a_model(
    #[case] model: &str,
    #[case] expected: Result<Option<(String, String)>, String>,
) {
    assert_eq!(model_choice(&models(), model), expected);
}

#[test]
fn an_agent_without_models_runs_with_its_own() {
    assert_eq!(model_choice(&[], "haiku"), Ok(None));
}

#[test]
fn the_answer_is_what_is_said_after_the_last_tool_call() {
    let mut answer = Answer::default();
    answer.say("Let me look. ");
    answer.tool();
    answer.say("It is ");
    answer.say("42.");
    assert_eq!(answer.text(), "It is 42.");
}

#[test]
fn an_answer_ending_on_a_tool_call_is_everything_said() {
    let mut answer = Answer::default();
    answer.say("Done.");
    answer.tool();
    assert_eq!(answer.text(), "Done.");
}
