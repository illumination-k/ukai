use rstest::rstest;

use super::*;

fn request() -> StartRequest {
    StartRequest {
        flow: Some("weekly".to_owned()),
        ..StartRequest::default()
    }
}

#[test]
fn a_declared_flow_without_input_may_start() {
    assert_eq!(problems(&request()), vec![]);
}

#[rstest]
#[case::no_source(StartRequest::default(), Problem::NoSource)]
#[case::two_sources(
    StartRequest { path: Some("a.ts".to_owned()), ..request() },
    Problem::ManySources
)]
#[case::empty_script(
    StartRequest { script: Some("  ".to_owned()), ..StartRequest::default() },
    Problem::EmptyScript
)]
#[case::bad_run(
    StartRequest { run: Some("../x".to_owned()), ..request() },
    Problem::InvalidRunId("../x".to_owned())
)]
fn tells_a_problem(#[case] request: StartRequest, #[case] expected: Problem) {
    assert_eq!(problems(&request), vec![expected]);
}

#[test]
fn input_must_be_json() {
    let request = StartRequest {
        input: "{site:".to_owned(),
        ..request()
    };
    assert!(matches!(
        problems(&request).as_slice(),
        [Problem::InvalidInput(_)]
    ));
}

#[test]
fn every_problem_is_reported() {
    let request = StartRequest {
        input: "nope".to_owned(),
        run: Some("a b".to_owned()),
        ..StartRequest::default()
    };
    assert_eq!(problems(&request).len(), 3);
}

#[rstest]
#[case::plain("weekly-2026_11", true)]
#[case::empty("", false)]
#[case::slash("a/b", false)]
#[case::dot("..", false)]
#[case::long(&"x".repeat(65), false)]
fn tells_a_run_id(#[case] id: &str, #[case] valid: bool) {
    assert_eq!(is_run_id(id), valid);
}

#[test]
fn reads_events_and_leaves_out_what_it_does_not_know() {
    let text = concat!(
        "{\"type\":\"start\",\"node\":\"a\",\"at\":\"t\"}\n",
        "{\"type\":\"usage\",\"node\":\"a\",\"at\":\"t\",\"models\":{}}\n",
        "{\"type\":\"end\",\"node\":\"a\",\"at\":\"t\",\"ms\":12}\n",
        "{\"type\":\"sta",
    );
    assert_eq!(
        events(text),
        vec![
            Event::Start {
                node: "a".to_owned()
            },
            Event::Other,
            Event::End {
                node: "a".to_owned(),
                ms: 12
            },
        ]
    );
}

#[rstest]
#[case::running(None, true, RunState::Running, None)]
#[case::interrupted(None, false, RunState::Interrupted, None)]
#[case::done(
    Some(Outcome::Done { output: serde_json::Value::Null }),
    false,
    RunState::Done,
    None
)]
#[case::stopped(
    Some(Outcome::Stopped { node: "pick".to_owned(), reason: "nothing new".to_owned() }),
    false,
    RunState::Stopped,
    Some("pick: nothing new")
)]
#[case::failed(
    Some(Outcome::Failed { error: "boom".to_owned() }),
    true,
    RunState::Failed,
    Some("boom")
)]
#[case::cancelled(Some(Outcome::Cancelled), false, RunState::Cancelled, None)]
fn tells_a_runs_state(
    #[case] outcome: Option<Outcome>,
    #[case] alive: bool,
    #[case] state: RunState,
    #[case] detail: Option<&str>,
) {
    assert_eq!(
        run_state(outcome.as_ref(), alive),
        (state, detail.map(str::to_owned))
    );
}

#[test]
fn reads_an_outcome() {
    let outcome: Outcome =
        serde_json::from_str("{\"status\":\"done\",\"output\":{\"topic\":\"x\"}}").unwrap();
    assert_eq!(
        outcome,
        Outcome::Done {
            output: serde_json::json!({"topic": "x"})
        }
    );
}

fn node(name: &str, depth: u32) -> Node {
    Node {
        name: name.to_owned(),
        depth,
        ..Node::default()
    }
}

fn start(node: &str) -> Event {
    Event::Start {
        node: node.to_owned(),
    }
}

fn end(node: &str, ms: u64) -> Event {
    Event::End {
        node: node.to_owned(),
        ms,
    }
}

fn states(nodes: &[Node]) -> Vec<(&str, NodeState)> {
    nodes
        .iter()
        .map(|node| (node.name.as_str(), node.state))
        .collect()
}

#[test]
fn folds_events_onto_the_graph() {
    let graph = vec![node("flow", 0), node("a", 1), node("b", 1), node("c", 1)];
    let events = [start("a"), end("a", 5), start("b")];
    let nodes = fold(graph, &events, &[], RunState::Running);
    assert_eq!(
        states(&nodes),
        vec![
            ("flow", NodeState::Running),
            ("a", NodeState::Done),
            ("b", NodeState::Running),
            ("c", NodeState::Pending),
        ]
    );
    assert_eq!(nodes[1].ms, Some(5));
}

#[test]
fn a_node_run_again_counts_its_runs_and_retries() {
    let events = [
        start("a"),
        Event::Retry {
            node: "a".to_owned(),
        },
        Event::Error {
            node: "a".to_owned(),
            error: "boom".to_owned(),
        },
        start("a"),
        end("a", 1),
    ];
    let nodes = fold(
        vec![node("flow", 0), node("a", 1)],
        &events,
        &[],
        RunState::Done,
    );
    assert_eq!(
        (
            nodes[1].state,
            nodes[1].runs,
            nodes[1].retries,
            &nodes[1].detail
        ),
        (NodeState::Done, 2, 1, &None)
    );
    assert_eq!(nodes[0].state, NodeState::Done);
}

#[test]
fn a_checkpoint_skip_stays_skipped_past_its_end() {
    let events = [
        start("a"),
        Event::Skip {
            node: "a".to_owned(),
            reason: "checkpoint a.123".to_owned(),
        },
        end("a", 0),
    ];
    let nodes = fold(
        vec![node("flow", 0), node("a", 1)],
        &events,
        &[],
        RunState::Done,
    );
    assert_eq!(nodes[1].state, NodeState::Skipped);
    assert_eq!(nodes[1].detail.as_deref(), Some("checkpoint a.123"));
}

#[test]
fn a_node_running_in_a_run_that_is_over_did_not_finish() {
    let nodes = fold(
        vec![node("flow", 0), node("a", 1)],
        &[start("a")],
        &[],
        RunState::Cancelled,
    );
    assert_eq!(
        states(&nodes),
        vec![("flow", NodeState::Failed), ("a", NodeState::Failed)]
    );
    assert_eq!(nodes[1].detail.as_deref(), Some("did not finish"));
}

#[test]
fn a_node_the_graph_lacks_is_added() {
    let nodes = fold(vec![node("flow", 0)], &[start("x")], &[], RunState::Running);
    assert_eq!(
        nodes[1],
        Node {
            state: NodeState::Running,
            runs: 1,
            ..node("x", 1)
        }
    );
}

#[test]
fn the_stopping_node_says_why() {
    let events = [
        start("pick"),
        Event::Stop {
            node: "pick".to_owned(),
            reason: "nothing new".to_owned(),
        },
    ];
    let nodes = fold(
        vec![node("flow", 0), node("pick", 1)],
        &events,
        &[],
        RunState::Stopped,
    );
    assert_eq!(
        states(&nodes),
        vec![("flow", NodeState::Stopped), ("pick", NodeState::Stopped)]
    );
}

#[test]
fn sessions_go_to_their_nodes_once_and_the_rest_to_the_flow() {
    let session = |node: Option<&str>, session: &str| NodeSession {
        node: node.map(str::to_owned),
        session: session.to_owned(),
    };
    let sessions = [
        session(Some("a"), "s1"),
        session(Some("a"), "s1"),
        session(Some("a"), "s2"),
        session(None, "s3"),
    ];
    let nodes = fold(
        vec![node("flow", 0), node("a", 1)],
        &[],
        &sessions,
        RunState::Running,
    );
    assert_eq!(nodes[1].sessions, vec!["s1", "s2"]);
    assert_eq!(nodes[0].sessions, vec!["s3"]);
}

#[test]
fn a_node_named_as_its_flow_is_the_one_inside() {
    let nodes = fold(
        vec![node("fix", 0), node("fix", 1)],
        &[start("fix"), end("fix", 3)],
        &[],
        RunState::Running,
    );
    assert_eq!(
        (nodes[0].state, nodes[1].state),
        (NodeState::Running, NodeState::Done)
    );
}

fn choice(id: &str, kind: &str) -> PermissionChoice {
    PermissionChoice {
        id: id.to_owned(),
        name: id.to_owned(),
        kind: kind.to_owned(),
    }
}

#[rstest]
#[case::write_once(Access::Write, &[("always", "allow_always"), ("once", "allow_once"), ("no", "reject_once")], Some("once"))]
#[case::write_always(Access::Write, &[("always", "allow_always"), ("no", "reject_once")], Some("always"))]
#[case::write_no_allow(Access::Write, &[("no", "reject_once")], None)]
#[case::read_only_rejects(Access::ReadOnly, &[("once", "allow_once"), ("no", "reject_once")], Some("no"))]
#[case::read_only_declines(Access::ReadOnly, &[("once", "allow_once")], None)]
fn answers_a_permission_request(
    #[case] access: Access,
    #[case] options: &[(&str, &str)],
    #[case] expected: Option<&str>,
) {
    let options: Vec<_> = options.iter().map(|(id, kind)| choice(id, kind)).collect();
    assert_eq!(permit(access, &options), expected.map(str::to_owned));
}

#[test]
fn cuts_text_at_a_character() {
    let text = "あ".repeat(TEXT_KEPT);
    assert!(tail(&text).len() <= TEXT_KEPT);
    assert!(head(&text).len() <= TEXT_KEPT);
    assert_eq!(tail("short"), "short");
    assert_eq!(head("short"), "short");
}

#[test]
fn describes_a_run() {
    let run = Run {
        summary: RunSummary {
            id: "r1".to_owned(),
            flow: "weekly".to_owned(),
            by: Launcher::Agent,
            started: 0,
            finished: Some(1),
            state: RunState::Failed,
            detail: Some("boom".to_owned()),
        },
        input: String::new(),
        nodes: vec![
            Node {
                state: NodeState::Failed,
                ..node("weekly", 0)
            },
            Node {
                state: NodeState::Failed,
                runs: 2,
                retries: 1,
                detail: Some("boom".to_owned()),
                ..node("write", 1)
            },
        ],
        output: None,
        log: "trace\n".to_owned(),
    };
    assert_eq!(
        describe(&run),
        "Run r1 of weekly: failed (boom)\n- weekly: failed\n  - write: failed, 2 runs, 1 retries (boom)\n\nPrinted:\ntrace\n"
    );
}
