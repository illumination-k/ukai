use rstest::rstest;

use super::*;

/// A gateway with one minimal agent, with `image` and any `extra` keys.
fn kubernetes(image: &str, extra: &str) -> String {
    format!(
        r#"
        [agents.k]
        image = "{image}"
        command = "claude-agent-acp"
        workspace = {{ git = "https://github.com/org/repo" }}
        {extra}
        "#
    )
}

const IMAGE: &str = "registry.example.com/agent:latest";

#[test]
fn kubernetes_agent_has_defaults() {
    let config = parse(&kubernetes(IMAGE, ""));
    let Ok(GatewayConfig { agents }) = config else {
        panic!("{config:?}");
    };
    let Some(agent) = agents.get("k") else {
        panic!("expected agent `k`");
    };
    assert_eq!(agent.home, "/home/ukai");
    assert_eq!(agent.run_as_user, 65534);
    assert_eq!(agent.idle_timeout, "30m");
    assert_eq!(agent.workspace.snapshot_interval, "5m");
    assert_eq!(agent.resources, Resources::default());
    assert_eq!(agent.workspace.git_ref, None);
}

#[rstest]
#[case::empty_image("", "", Problem::Empty { context: "agents.k.image".into() })]
#[case::bad_idle_timeout(
    IMAGE,
    "idle_timeout = \"30\"",
    Problem::InvalidDuration { context: "agents.k.idle_timeout".into(), value: "30".into() }
)]
#[case::zero_idle_timeout(
    IMAGE,
    "idle_timeout = \"0s\"",
    Problem::InvalidDuration { context: "agents.k.idle_timeout".into(), value: "0s".into() }
)]
#[case::bad_secret_ref(
    IMAGE,
    "secret_env = { TOKEN = \"just-a-name\" }",
    Problem::InvalidSecretRef { context: "agents.k.secret_env.TOKEN".into(), value: "just-a-name".into() }
)]
#[case::bad_mount_path(
    IMAGE,
    "mounts = [{ claim = \"home\", path = \".claude\" }]",
    Problem::InvalidMountPath { context: "agents.k.mounts[0]".into(), path: ".claude".into() }
)]
fn reports_kubernetes_problem(#[case] image: &str, #[case] extra: &str, #[case] expected: Problem) {
    assert_eq!(problems(&kubernetes(image, extra)), vec![expected]);
}

fn problems(src: &str) -> Vec<Problem> {
    match parse(src) {
        Err(ConfigError::Invalid(problems)) => problems,
        other => panic!("expected validation problems, got {other:?}"),
    }
}

#[test]
fn reports_every_kubernetes_problem() {
    let src = r#"
        [agents.k]
        image = ""
        command = ""
        idle_timeout = "soon"
        workspace = { git = "", snapshot_interval = "1d", github_token = "/key" }
    "#;
    assert_eq!(problems(src).len(), 6);
}

#[rstest]
#[case("45s", Some(45))]
#[case("30m", Some(1800))]
#[case("2h", Some(7200))]
#[case("30", None)]
#[case("m", None)]
#[case("1.5h", None)]
#[case("-1s", None)]
fn parses_durations(#[case] value: &str, #[case] seconds: Option<u64>) {
    assert_eq!(parse_duration(value).map(|d| d.as_secs()), seconds);
}

#[rstest]
#[case("token/GH_TOKEN", Some(("token", "GH_TOKEN")))]
#[case("token", None)]
#[case("/key", None)]
#[case("secret/", None)]
#[case("a/b/c", None)]
fn parses_secret_refs(#[case] value: &str, #[case] expected: Option<(&str, &str)>) {
    assert_eq!(parse_secret_ref(value), expected);
}
