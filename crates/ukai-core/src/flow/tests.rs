use rstest::rstest;

use super::*;

/// in → prepare → select-topic → (until: done? ⇄ fix) → ship? → publish | skip → out
const GRAPH: &str = r#"{
  "nodes": [
    { "id": "in", "kind": "input", "label": "input" },
    { "id": "s0", "kind": "flow", "label": "weekly", "name": "weekly" },
    { "id": "n1", "kind": "fn", "label": "prepare", "name": "prepare", "parent": "s0" },
    { "id": "n2", "kind": "agent", "label": "select-topic", "name": "select-topic", "parent": "s0", "assign": "topic" },
    { "id": "s3", "kind": "until", "label": "green", "name": "green", "parent": "s0", "max": 3 },
    { "id": "c4", "kind": "choice", "label": "done?", "parent": "s3" },
    { "id": "n5", "kind": "agent", "label": "fix", "name": "fix", "parent": "s3", "worktree": true },
    { "id": "c6", "kind": "choice", "label": "ship?", "name": "ship?", "parent": "s0" },
    { "id": "n7", "kind": "fn", "label": "publish", "name": "publish", "parent": "s0" },
    { "id": "n8", "kind": "fn", "label": "skip", "name": "skip", "parent": "s0" },
    { "id": "out", "kind": "output", "label": "output" }
  ],
  "edges": [
    { "from": "in", "to": "n1" },
    { "from": "n1", "to": "n2", "map": true, "use": ["checkpoint", "retry"] },
    { "from": "n2", "to": "c4" },
    { "from": "c4", "to": "n5", "when": "no" },
    { "from": "n5", "to": "c4" },
    { "from": "c4", "to": "c6", "when": "yes" },
    { "from": "c6", "to": "n7", "when": "yes" },
    { "from": "c6", "to": "n8", "when": "no" },
    { "from": "n7", "to": "out" },
    { "from": "n8", "to": "out" }
  ]
}"#;

fn graph() -> Graph {
    Graph::parse(GRAPH).unwrap_or_else(|error| panic!("the graph parses: {error}"))
}

fn seen(events: &str) -> RunView {
    view(&graph(), &parse_events(events))
}

fn status(view: &RunView, id: &str) -> Status {
    view.node(id).unwrap_or_else(|| panic!("the node is shown")).status
}

#[test]
fn a_run_without_events_is_pending() {
    let view = seen("");
    assert_eq!(
        view.nodes
            .iter()
            .map(|node| node.id.as_str())
            .collect::<Vec<_>>(),
        ["s0", "n1", "n2", "s3", "n5", "c6", "n7", "n8"],
        "the ends of the graph and the loop's done? say nothing"
    );
    assert!(view.nodes.iter().all(|node| node.status == Status::Pending));
    assert_eq!((view.done, view.total), (0, 5));
    assert_eq!(view.summary(), "0/5");
    assert_eq!(view.node("n5").map(|node| node.depth), Some(2));
}

#[test]
fn events_are_laid_over_the_graph() {
    let view = seen(concat!(
        r#"{"type":"start","node":"weekly","at":"t"}"#,
        "\n",
        r#"{"type":"start","node":"prepare","at":"t"}"#,
        "\n",
        r#"{"type":"end","node":"prepare","at":"t","ms":12}"#,
        "\n",
        r#"{"type":"skip","node":"select-topic","at":"t","reason":"checkpoint"}"#,
        "\n",
        r#"{"type":"start","node":"select-topic","at":"t"}"#,
        "\n",
        r#"{"type":"skip","node":"select-topic","at":"t","reason":"checkpoint"}"#,
        "\n",
        r#"{"type":"end","node":"select-topic","at":"t","ms":1}"#,
        "\n",
        r#"{"type":"start","node":"fix","at":"t"}"#,
        "\n",
        r#"{"type":"usage","node":"fix","at":"t","usd":0.25,"models":{}}"#,
        "\n",
        r#"{"type":"end","node":"fix","at":"t","ms":61000}"#,
        "\n",
        r#"{"type":"start","node":"fix","at":"t"}"#,
        "\n",
        r#"{"type":"session","node":"fix","at":"t","sessionId":"s-1"}"#,
        "\n",
        r#"{"type":"retry","node":"fix","at":"t","attempt":1,"error":"x"}"#,
        "\n",
        r#"{"type":"activity","node":"fix","at":"t","tool":"Bash","text":"npm\n  test"}"#,
        "\n",
        r#"{"type":"mystery","node":"fix","at":"t"}"#,
        "\n",
        r#"{"type":"activity","node":"fix""#,
    ));
    assert_eq!(status(&view, "n1"), Status::Done);
    assert_eq!(
        view.node("n1").map(|node| node.detail.as_str()),
        Some("12ms")
    );
    assert_eq!(status(&view, "n2"), Status::Skipped);
    assert_eq!(
        view.node("n2").map(|node| node.detail.as_str()),
        Some("→ topic · checkpoint · retry · 1ms")
    );
    let fix = view.node("n5").unwrap_or_else(|| panic!("fix is shown"));
    assert_eq!(fix.status, Status::Running);
    assert_eq!(fix.detail, "worktree · ×2 · attempt 2 · $0.250");
    assert_eq!(fix.activity.as_deref(), Some("Bash npm test"));
    assert_eq!(fix.reach.as_deref(), Some("fix"));
    assert_eq!(
        status(&view, "s3"),
        Status::Running,
        "a group is as far as its members"
    );
    assert_eq!(
        view.node("s3").map(|node| node.detail.as_str()),
        Some("at most 3")
    );
    assert_eq!(status(&view, "s0"), Status::Running);
    assert_eq!((view.done, view.total), (2, 5));
    assert_eq!(view.summary(), "2/5 · $0.250");
}

#[test]
fn a_case_not_taken_is_skipped() {
    let view = seen(concat!(
        r#"{"type":"start","node":"ship?","at":"t"}"#,
        "\n",
        r#"{"type":"end","node":"ship?","at":"t","ms":1}"#,
        "\n",
        r#"{"type":"start","node":"publish","at":"t"}"#,
        "\n",
        r#"{"type":"error","node":"publish","at":"t","error":"no token"}"#,
        "\n",
    ));
    let publish = view.node("n7").unwrap_or_else(|| panic!("publish is shown"));
    assert_eq!(publish.status, Status::Failed);
    assert_eq!(publish.error.as_deref(), Some("no token"));
    assert_eq!(publish.when.as_deref(), Some("yes"));
    let skip = view.node("n8").unwrap_or_else(|| panic!("skip is shown"));
    assert_eq!(skip.status, Status::Skipped);
    assert_eq!(skip.detail, "not taken");
    assert_eq!(view.total, 4, "a step not taken is no step of this run");
}

#[test]
fn a_stop_ends_what_was_running() {
    let view = seen(concat!(
        r#"{"type":"start","node":"weekly","at":"t"}"#,
        "\n",
        r#"{"type":"start","node":"prepare","at":"t"}"#,
        "\n",
        r#"{"type":"stop","node":"prepare","at":"t","reason":"nothing to do"}"#,
        "\n",
    ));
    assert_eq!(status(&view, "n1"), Status::Stopped);
    assert_eq!(status(&view, "s0"), Status::Stopped);
}

#[test]
fn messages_sent_to_an_agent_show_as_its_activity() {
    let view = seen(concat!(
        r#"{"type":"start","node":"fix","at":"t","item":"2"}"#,
        "\n",
        r#"{"type":"steer","node":"fix","at":"t","item":"2","message":"leave the tests"}"#,
        "\n",
    ));
    assert_eq!(
        view.node("n5").and_then(|node| node.activity.as_deref()),
        Some("#2 ← leave the tests")
    );
    let view = seen(concat!(
        r#"{"type":"start","node":"fix","at":"t"}"#,
        "\n",
        r#"{"type":"wrapUp","node":"fix","at":"t","stopped":true}"#,
        "\n",
    ));
    assert_eq!(
        view.node("n5").and_then(|node| node.activity.as_deref()),
        Some("← stop")
    );
}

#[test]
fn the_outline_reads_as_text() {
    let view = seen(concat!(
        r#"{"type":"start","node":"prepare","at":"t"}"#,
        "\n",
        r#"{"type":"error","node":"prepare","at":"t","error":"boom"}"#,
        "\n",
    ));
    let outline = outline(&view);
    assert!(
        outline.contains("  ✗ prepare (failed)\n    boom"),
        "{outline}"
    );
    assert!(outline.contains("  [yes] ○ publish (pending)"), "{outline}");
}

#[rstest]
#[case::not_json("nope")]
#[case::no_nodes_array(r#"{"nodes": 3}"#)]
fn what_is_no_graph_is_refused(#[case] text: &str) {
    assert!(Graph::parse(text).is_err());
}

#[test]
fn an_unknown_kind_is_a_step() {
    let graph = Graph::parse(
        r#"{"nodes":[{"id":"a","kind":"quantum","label":"a","name":"a"}],"edges":[]}"#,
    ).unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(graph.nodes[0].kind, NodeKind::Other);
    assert_eq!(view(&graph, &[]).total, 1);
}

#[rstest]
#[case(0.004, "$0.004")]
#[case(1.5, "$1.50")]
fn money_reads_as_dollars(#[case] usd: f64, #[case] text: &str) {
    assert_eq!(money(usd), text);
}

fn shape<'a>(drawing: &'a Drawing, id: &str) -> &'a Shape {
    drawing
        .shapes
        .iter()
        .find(|shape| shape.id == id)
        .unwrap_or_else(|| panic!("the node is placed"))
}

fn inside(inner: &Shape, outer: &Shape) -> bool {
    inner.x >= outer.x
        && inner.y >= outer.y
        && inner.x + inner.width <= outer.x + outer.width
        && inner.y + inner.height <= outer.y + outer.height
}

fn overlap(a: &Shape, b: &Shape) -> bool {
    a.x < b.x + b.width && b.x < a.x + a.width && a.y < b.y + b.height && b.y < a.y + a.height
}

#[test]
fn every_node_is_placed_inside_its_group() {
    let graph = graph();
    let drawing = draw(&graph);
    assert_eq!(drawing.shapes.len(), graph.nodes.len());
    assert_eq!(drawing.lines.len(), graph.edges.len());
    for node in &graph.nodes {
        let placed = shape(&drawing, &node.id);
        assert!(placed.x >= 0 && placed.y >= 0);
        assert!(
            placed.x + placed.width <= drawing.width,
            "{} fits the width",
            node.id
        );
        assert!(
            placed.y + placed.height <= drawing.height,
            "{} fits the height",
            node.id
        );
        if let Some(parent) = &node.parent {
            assert!(
                inside(placed, shape(&drawing, parent)),
                "{} is in {parent}",
                node.id
            );
        }
    }
    for a in &graph.nodes {
        for b in &graph.nodes {
            if a.id != b.id && a.parent == b.parent {
                assert!(
                    !overlap(shape(&drawing, &a.id), shape(&drawing, &b.id)),
                    "{} and {}",
                    a.id,
                    b.id
                );
            }
        }
    }
    assert!(shape(&drawing, "s0").group);
    assert!(!shape(&drawing, "n1").group);
}

#[test]
fn a_node_is_below_what_leads_to_it() {
    let drawing = draw(&graph());
    let below = |a: &str, b: &str| {
        let (a, b) = (shape(&drawing, a), shape(&drawing, b));
        assert!(b.y >= a.y + a.height, "{} is below {}", b.id, a.id);
    };
    below("in", "s0");
    below("n1", "n2");
    below("n2", "s3");
    below("s3", "c6");
    below("c6", "n7");
    below("s0", "out");
    // The cases of a branch are side by side.
    assert_eq!(shape(&drawing, "n7").y, shape(&drawing, "n8").y);
    // So are a loop's check and its body, the body to the right.
    let (check, body) = (shape(&drawing, "c4"), shape(&drawing, "n5"));
    assert_eq!(check.y, body.y);
    assert!(body.x >= check.x + check.width);
}

#[test]
fn edges_carry_their_case_and_loops_lead_back() {
    let drawing = draw(&graph());
    let line = |from: &str, to: &str| {
        drawing
            .lines
            .iter()
            .find(|line| line.from == from && line.to == to)
            .unwrap_or_else(|| panic!("the edge is drawn"))
    };
    assert_eq!(line("c6", "n8").label.as_deref(), Some("no"));
    assert!(line("n5", "c4").back);
    assert!(!line("c4", "n5").back);
    assert!(line("in", "n1").path.starts_with("M "));
}

#[rstest]
#[case::empty(r#"{"nodes":[],"edges":[]}"#, 0)]
#[case::groups_holding_each_other(
    r#"{"nodes":[{"id":"a","kind":"flow","label":"a","parent":"b"},{"id":"b","kind":"flow","label":"b","parent":"a"},{"id":"c","kind":"fn","label":"c"}],"edges":[{"from":"a","to":"c"},{"from":"c","to":"nowhere"}]}"#,
    1
)]
#[case::in_itself(
    r#"{"nodes":[{"id":"a","kind":"flow","label":"a","parent":"a"}],"edges":[]}"#,
    1
)]
#[case::group_not_there(
    r#"{"nodes":[{"id":"a","kind":"fn","label":"a","parent":"gone"}],"edges":[]}"#,
    1
)]
fn a_graph_that_is_no_tree_is_drawn_as_far_as_it_goes(#[case] text: &str, #[case] placed: usize) {
    let graph = Graph::parse(text).unwrap_or_else(|error| panic!("{error}"));
    let drawing = draw(&graph);
    assert_eq!(drawing.shapes.len(), placed);
    assert!(drawing.lines.is_empty());
    let _ = view(&graph, &[]);
}

#[test]
fn deep_nesting_ends_as_a_step() {
    let mut nodes = Vec::new();
    for at in 0..200 {
        let parent = if at == 0 {
            String::new()
        } else {
            format!(r#","parent":"g{}""#, at - 1)
        };
        nodes.push(format!(
            r#"{{"id":"g{at}","kind":"flow","label":"g","name":"g{at}"{parent}}}"#
        ));
    }
    let graph = Graph::parse(&format!(r#"{{"nodes":[{}],"edges":[]}}"#, nodes.join(",")))
        .unwrap_or_else(|| panic!("it parses"));
    let drawing = draw(&graph);
    assert!(drawing.shapes.len() < 200);
    assert!(!drawing.shapes.is_empty());
    let _ = view(&graph, &[]);
}

fn summary(id: &str, state: RunState) -> RunSummary {
    RunSummary {
        id: id.to_owned(),
        state,
        at: 0,
    }
}

#[test]
fn only_runs_that_start_while_looking_are_new() {
    let runs = [
        summary("a", RunState::Running),
        summary("b", RunState::Done),
    ];
    let (fresh, known) = new_runs(None, &runs);
    assert!(fresh.is_empty(), "nothing is new on the first look");
    assert_eq!(known.len(), 2);

    let later = [
        summary("c", RunState::Running),
        summary("d", RunState::Failed),
        summary("a", RunState::Running),
    ];
    let (fresh, known) = new_runs(Some(&known), &later);
    assert_eq!(fresh, ["c"], "an ended run does not open by itself");
    let (fresh, _) = new_runs(Some(&known), &later);
    assert!(fresh.is_empty(), "a run is new once");
}
