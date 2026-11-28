use rstest::rstest;

use super::*;

const GRAPH: &str = r#"{"nodes":[],"edges":[]}"#;

/// A checkout with its store beside it, and runs put there by hand.
struct Checkout {
    dir: tempfile::TempDir,
}

impl Checkout {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("a temporary directory: {e:?}"));
        std::fs::create_dir_all(dir.path().join("root"))
            .unwrap_or_else(|e| panic!("the checkout: {e:?}"));
        Self { dir }
    }

    fn root(&self) -> PathBuf {
        self.dir.path().join("root")
    }

    fn store(&self) -> Store {
        Store::at(self.dir.path().join("store"), &self.root())
    }

    fn put(&self, run: &str, file: &str, text: &str) {
        let folder = self.store().dir.join(run);
        std::fs::create_dir_all(&folder).unwrap_or_else(|e| panic!("the run's folder: {e:?}"));
        std::fs::write(folder.join(file), text)
            .unwrap_or_else(|e| panic!("the file is written: {e:?}"));
    }

    /// A stand-in for ayaito's command line in the checkout, which records
    /// its arguments and does what `script` says.
    #[cfg(unix)]
    fn install(&self, script: &str) {
        use std::os::unix::fs::PermissionsExt;
        let bin = self.root().join("node_modules").join(".bin");
        std::fs::create_dir_all(&bin).unwrap_or_else(|e| panic!("the bin directory: {e:?}"));
        let file = bin.join("ayaito");
        let args = self.dir.path().join("args");
        let text = format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\n{script}\n",
            args.display()
        );
        std::fs::write(&file, text).unwrap_or_else(|e| panic!("the script is written: {e:?}"));
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755))
            .unwrap_or_else(|e| panic!("it runs: {e:?}"));
    }

    #[cfg(unix)]
    fn args(&self) -> Vec<String> {
        let text = std::fs::read_to_string(self.dir.path().join("args")).unwrap_or_default();
        text.lines().map(str::to_owned).collect()
    }
}

#[rstest]
#[case("weekly-1a2b", true)]
#[case("a.b_c", true)]
#[case("", false)]
#[case(".hidden", false)]
#[case("-x", false)]
#[case("a/b", false)]
#[case("a b", false)]
#[case("..", false)]
fn a_run_id_names_a_folder(#[case] text: &str, #[case] valid: bool) {
    assert_eq!(run_id(text).is_some(), valid);
}

#[test]
fn runs_are_listed_newest_first_with_their_state() {
    let checkout = Checkout::new();
    checkout.put("old", "graph.json", GRAPH);
    checkout.put("old", "result.json", r#"{"status":"done"}"#);
    checkout.put("stopped", "graph.json", GRAPH);
    checkout.put("stopped", "result.json", r#"{"status":"stopped"}"#);
    checkout.put("failed", "graph.json", GRAPH);
    checkout.put(
        "failed",
        "result.json",
        r#"{"status":"failed","error":"x"}"#,
    );
    checkout.put("going", "graph.json", GRAPH);
    // An attempt that started leaves its result empty until it ends.
    checkout.put("going", "result.json", "");
    checkout.put("no-graph", "events.jsonl", "");
    checkout.put(".hidden", "graph.json", GRAPH);
    std::fs::create_dir_all(checkout.root().join(".runs/legacy"))
        .unwrap_or_else(|e| panic!("the legacy store: {e:?}"));
    std::fs::write(checkout.root().join(".runs/legacy/graph.json"), GRAPH)
        .unwrap_or_else(|e| panic!("written: {e:?}"));

    let listing = checkout.store().list();
    let state = |id: &str| {
        listing
            .runs
            .iter()
            .find(|run| run.id == id)
            .map(|run| run.state)
    };
    assert_eq!(listing.runs.len(), 5);
    assert_eq!(state("old"), Some(RunState::Done));
    assert_eq!(state("stopped"), Some(RunState::Stopped));
    assert_eq!(state("failed"), Some(RunState::Failed));
    assert_eq!(state("going"), Some(RunState::Running));
    assert_eq!(state("legacy"), Some(RunState::Running));
    assert!(listing.runs.windows(2).all(|pair| pair[0].at >= pair[1].at));
}

#[test]
fn a_store_that_is_not_there_lists_nothing() {
    assert_eq!(Checkout::new().store().list(), FlowListing::default());
}

#[cfg(target_os = "linux")]
#[test]
fn a_run_whose_process_is_gone_has_failed() {
    let checkout = Checkout::new();
    checkout.put("run", "graph.json", GRAPH);
    checkout.put("run", "run.pid", &std::process::id().to_string());
    assert_eq!(checkout.store().list().runs[0].state, RunState::Running);
    // No process has an id this large.
    checkout.put("run", "run.pid", "4194999");
    assert_eq!(checkout.store().list().runs[0].state, RunState::Failed);
}

#[test]
fn a_run_is_read_in_whole_lines_from_an_offset() {
    let checkout = Checkout::new();
    checkout.put("run", "graph.json", GRAPH);
    let store = checkout.store();
    let empty = store
        .read("run", 0)
        .unwrap_or_else(|e| panic!("the run is read: {e:?}"));
    assert_eq!(
        (empty.graph.as_str(), empty.events.as_str(), empty.next),
        (GRAPH, "", 0)
    );
    assert_eq!(empty.state, RunState::Running);
    assert_eq!(empty.result, None);

    checkout.put("run", "events.jsonl", "{\"a\":1}\n{\"b\":2}\n{\"c\":");
    let first = store
        .read("run", 0)
        .unwrap_or_else(|e| panic!("the run is read: {e:?}"));
    assert_eq!(first.events, "{\"a\":1}\n{\"b\":2}\n");
    assert_eq!(first.next, 16);
    let none = store
        .read("run", first.next)
        .unwrap_or_else(|e| panic!("the run is read: {e:?}"));
    assert_eq!((none.events.as_str(), none.next), ("", 16));

    checkout.put("run", "events.jsonl", "{\"a\":1}\n{\"b\":2}\n{\"c\":3}\n");
    let rest = store
        .read("run", first.next)
        .unwrap_or_else(|e| panic!("the run is read: {e:?}"));
    assert_eq!(rest.events, "{\"c\":3}\n");

    // Written anew, shorter than what was read: from the start.
    checkout.put("run", "events.jsonl", "{}\n");
    let anew = store
        .read("run", rest.next)
        .unwrap_or_else(|e| panic!("the run is read: {e:?}"));
    assert_eq!((anew.events.as_str(), anew.next), ("{}\n", 3));

    checkout.put("run", "result.json", r#"{"status":"done","output":1}"#);
    let ended = store
        .read("run", 3)
        .unwrap_or_else(|e| panic!("the run is read: {e:?}"));
    assert_eq!(ended.state, RunState::Done);
    assert_eq!(
        ended.result.as_deref(),
        Some(r#"{"status":"done","output":1}"#)
    );
}

#[test]
fn long_events_are_read_in_parts() {
    let checkout = Checkout::new();
    checkout.put("run", "graph.json", GRAPH);
    let line = format!("{}\n", "x".repeat(1000));
    checkout.put("run", "events.jsonl", &line.repeat(1000));
    let store = checkout.store();
    let mut after = 0;
    let mut read = String::new();
    for _ in 0..10 {
        let chunk = store
            .read("run", after)
            .unwrap_or_else(|e| panic!("the run is read: {e:?}"));
        assert!(chunk.events.len() <= MAX_CHUNK);
        assert!(chunk.events.is_empty() || chunk.events.ends_with('\n'));
        read.push_str(&chunk.events);
        after = chunk.next;
    }
    assert_eq!(read.len(), 1_001_000);
}

#[test]
fn a_line_longer_than_two_parts_is_read_to_its_end() {
    let checkout = Checkout::new();
    checkout.put("run", "graph.json", GRAPH);
    let long = format!("{}\n", "x".repeat(2 * MAX_CHUNK + 10));
    checkout.put(
        "run",
        "events.jsonl",
        &format!("{long}next\n{}", "y".repeat(10)),
    );
    let store = checkout.store();
    let first = store
        .read("run", 0)
        .unwrap_or_else(|e| panic!("the run is read: {e:?}"));
    assert_eq!(
        (first.events.len(), first.next),
        (long.len(), long.len() as u64)
    );
    let rest = store
        .read("run", first.next)
        .unwrap_or_else(|e| panic!("the run is read: {e:?}"));
    assert_eq!(rest.events, "next\n");
}

#[test]
fn a_long_line_still_being_written_waits() {
    let checkout = Checkout::new();
    checkout.put("run", "graph.json", GRAPH);
    checkout.put("run", "events.jsonl", &"x".repeat(2 * MAX_CHUNK + 10));
    let chunk = checkout
        .store()
        .read("run", 0)
        .unwrap_or_else(|e| panic!("the run is read: {e:?}"));
    assert_eq!((chunk.events.as_str(), chunk.next), ("", 0));
}

#[rstest]
#[case(b"", 0)]
#[case(b"abc", 0)]
#[case(b"a\nb", 2)]
#[case(b"a\nb\n", 4)]
fn only_whole_lines_are_passed_on(#[case] bytes: &[u8], #[case] whole: usize) {
    assert_eq!(whole_lines(bytes), whole);
}

#[test]
fn a_line_longer_than_a_part_is_passed_whole() {
    let mut bytes = vec![b'x'; MAX_CHUNK + 10];
    bytes.push(b'\n');
    bytes.extend_from_slice(b"next\n");
    assert_eq!(whole_lines(&bytes), MAX_CHUNK + 11);
}

#[rstest]
#[case::not_an_id("../x", "is not a run id")]
#[case::unknown("nope", "there is no run")]
fn a_run_that_is_not_there_is_refused(#[case] run: &str, #[case] why: &str) {
    let error = Checkout::new()
        .store()
        .read(run, 0)
        .err()
        .unwrap_or_else(|| panic!("it is refused"));
    assert!(
        matches!(&error, FlowError::Refused(text) if text.contains(why)),
        "{error}"
    );
}

#[rstest]
#[case::steer(Some("fix"), Some("go on"), None)]
#[case::stop_agent(Some("fix#2"), None, None)]
#[case::stop_run(None, None, None)]
#[case::message_for_nobody(None, Some("hi"), Some("a message needs the node"))]
#[case::empty_message(Some("fix"), Some("  "), Some("the message is empty"))]
#[case::empty_node(Some(" "), None, Some("not a name"))]
#[case::node_of_lines(Some("a\nb"), None, Some("not a name"))]
fn an_order_is_checked(
    #[case] node: Option<&str>,
    #[case] message: Option<&str>,
    #[case] problem: Option<&str>,
) {
    let order = Order {
        node: node.map(str::to_owned),
        message: message.map(str::to_owned),
    };
    match (order.problem(), problem) {
        (None, None) => {}
        (Some(found), Some(expected)) => assert!(found.contains(expected), "{found}"),
        (found, expected) => panic!("{found:?} is not {expected:?}"),
    }
}

#[tokio::test]
async fn without_ayaito_nothing_is_ordered_or_started() {
    let checkout = Checkout::new();
    checkout.put("run", "graph.json", GRAPH);
    // Whatever is installed on this machine is not found.
    let path = std::env::var_os("PATH");
    if path.is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join("ayaito").is_file()))
    {
        return;
    }
    let error = order(
        &checkout.root(),
        &checkout.store(),
        "run",
        &Order::default(),
    )
    .await
    .err()
    .unwrap_or_else(|| panic!("it is refused"));
    assert!(
        matches!(&error, FlowError::Refused(why) if why.contains("not installed")),
        "{error}"
    );
    let request = StartRequest {
        script: Some("export default 1".to_owned()),
        ..StartRequest::default()
    };
    let error = start(&checkout.root(), &checkout.store(), &request)
        .err()
        .unwrap_or_else(|| panic!("it is refused"));
    assert!(
        matches!(&error, FlowError::Refused(why) if why.contains("not installed")),
        "{error}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn orders_go_through_ayaito() {
    let checkout = Checkout::new();
    checkout.put("run", "graph.json", GRAPH);
    checkout.install("echo sent");
    let store = checkout.store();
    let steer = Order {
        node: Some("fix".to_owned()),
        message: Some("leave the tests".to_owned()),
    };
    let ordered = order(&checkout.root(), &store, "run", &steer)
        .await
        .unwrap_or_else(|e| panic!("it is sent: {e:?}"));
    assert_eq!(ordered.said, "sent");
    let dir = store.dir.to_string_lossy().into_owned();
    assert_eq!(
        checkout.args(),
        [
            "steer",
            "--store",
            &dir,
            "--",
            "run",
            "fix",
            "leave the tests"
        ]
    );
    order(&checkout.root(), &store, "run", &Order::default())
        .await
        .unwrap_or_else(|e| panic!("it is sent: {e:?}"));
    assert_eq!(checkout.args(), ["stop", "--store", &dir, "--", "run"]);

    checkout.install("echo 'Run run has ended.' >&2; exit 1");
    let error = order(&checkout.root(), &store, "run", &Order::default())
        .await
        .err()
        .unwrap_or_else(|| panic!("ayaito refuses"));
    assert_eq!(error, FlowError::Refused("Run run has ended.".to_owned()));
    let error = order(&checkout.root(), &store, "nope", &Order::default())
        .await
        .err()
        .unwrap_or_else(|| panic!("there is no such run"));
    assert!(matches!(error, FlowError::Refused(_)));
}

#[cfg(unix)]
#[tokio::test]
async fn a_run_is_started_as_a_process_of_its_own() {
    let checkout = Checkout::new();
    // What ayaito does first: save the graph. Then it ends with a result.
    checkout.install(
        r#"mkdir -p "$6/$4"; echo '{"nodes":[],"edges":[]}' > "$6/$4/graph.json"; echo out; echo '{"status":"done"}' > "$6/$4/result.json""#,
    );
    let store = checkout.store();
    let request = StartRequest {
        name: Some("fix all!".to_owned()),
        script: Some("export default flow".to_owned()),
        input: Some(serde_json::json!({ "goal": "x" })),
        run: None,
    };
    let started =
        start(&checkout.root(), &store, &request).unwrap_or_else(|e| panic!("it starts: {e:?}"));
    assert!(started.run.starts_with("fix-all-"), "{}", started.run);
    assert_eq!(started.folder, store.dir.join(&started.run));
    assert_eq!(
        std::fs::read_to_string(started.folder.join("flow.ts"))
            .ok()
            .as_deref(),
        Some("export default flow")
    );
    assert_eq!(
        std::fs::read_to_string(started.folder.join("input.json"))
            .ok()
            .as_deref(),
        Some(r#"{"goal":"x"}"#)
    );
    for _ in 0..200 {
        if store
            .read(&started.run, 0)
            .is_ok_and(|run| run.state == RunState::Done)
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        store.read(&started.run, 0).map(|run| run.state),
        Ok(RunState::Done)
    );
    let flow = started
        .folder
        .join("flow.ts")
        .to_string_lossy()
        .into_owned();
    let input = started
        .folder
        .join("input.json")
        .to_string_lossy()
        .into_owned();
    let dir = store.dir.to_string_lossy().into_owned();
    assert_eq!(
        checkout.args(),
        [
            "run",
            &flow,
            "--run-id",
            &started.run,
            "--store",
            &dir,
            "--input-file",
            &input
        ]
    );
    assert_eq!(
        std::fs::read_to_string(started.folder.join("run.log"))
            .ok()
            .as_deref(),
        Some("out\n")
    );

    // The same run again, from its saved flow.
    let again = StartRequest {
        run: Some(started.run.clone()),
        ..StartRequest::default()
    };
    assert_eq!(
        start(&checkout.root(), &store, &again).map(|started| started.run),
        Ok(started.run)
    );
}

#[cfg(unix)]
#[rstest]
#[case::no_script(StartRequest::default(), "pass script")]
#[case::no_saved_flow(
    StartRequest { run: Some("gone".to_owned()), ..StartRequest::default() },
    "no saved flow"
)]
#[case::not_an_id(
    StartRequest { run: Some("../x".to_owned()), script: Some("x".to_owned()), ..StartRequest::default() },
    "not a run id"
)]
fn a_start_that_cannot_run_is_refused(#[case] request: StartRequest, #[case] why: &str) {
    let checkout = Checkout::new();
    checkout.install("exit 0");
    let error = start(&checkout.root(), &checkout.store(), &request)
        .err()
        .unwrap_or_else(|| panic!("it is refused"));
    assert!(
        matches!(&error, FlowError::Refused(text) if text.contains(why)),
        "{error}"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn a_run_that_is_going_is_not_started_again() {
    let checkout = Checkout::new();
    checkout.install("sleep 30");
    let store = checkout.store();
    let request = StartRequest {
        script: Some("x".to_owned()),
        run: Some("long".to_owned()),
        ..StartRequest::default()
    };
    start(&checkout.root(), &store, &request).unwrap_or_else(|e| panic!("it starts: {e:?}"));
    let error = start(&checkout.root(), &store, &request)
        .err()
        .unwrap_or_else(|| panic!("it is going"));
    assert!(
        matches!(&error, FlowError::Refused(why) if why.contains("still going")),
        "{error}"
    );
    let pid = std::fs::read_to_string(store.dir.join("long").join("run.pid"))
        .unwrap_or_else(|e| panic!("the pid: {e:?}"));
    let _ = std::process::Command::new("kill").arg(pid.trim()).status();
}

#[tokio::test]
async fn the_store_is_in_the_git_common_dir() {
    let checkout = Checkout::new();
    let root = checkout.root();
    let git = std::process::Command::new("git")
        .arg("init")
        .arg("-q")
        .current_dir(&root)
        .status();
    if !git.is_ok_and(|status| status.success()) {
        return;
    }
    let store = Store::of(&root).await;
    let expected = root
        .canonicalize()
        .unwrap_or_else(|e| panic!("the root: {e:?}"))
        .join(".git")
        .join("ayaito-runs");
    assert_eq!(
        store
            .dir
            .canonicalize()
            .ok()
            .or(Some(store.dir.clone()))
            .map(|dir| dir.ends_with(".git/ayaito-runs")),
        Some(true)
    );
    let _ = expected;
}

#[rstest]
#[case(RunState::Running)]
#[case(RunState::Done)]
#[case(RunState::Stopped)]
#[case(RunState::Failed)]
fn the_wire_keeps_a_run(#[case] state: RunState) {
    let chunk = RunChunk {
        graph: GRAPH.to_owned(),
        events: "{}\n".to_owned(),
        next: 3,
        state,
        result: Some("{}".to_owned()),
    };
    assert_eq!(RunChunk::from(gw::FlowRun::from(chunk.clone())), chunk);
    let listing = FlowListing {
        runs: vec![RunSummary {
            id: "a".to_owned(),
            state,
            at: 7,
        }],
    };
    assert_eq!(
        FlowListing::from(gw::FlowListing::from(listing.clone())),
        listing
    );
}
