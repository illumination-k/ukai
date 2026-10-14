use rstest::rstest;

use super::*;

const REPO_CONFIG: &str = include_str!("../../../ukai.toml");

#[test]
fn repo_config_is_valid() {
    let config = parse(REPO_CONFIG);
    assert!(config.is_ok(), "{config:?}");
}

#[test]
fn empty_document_is_valid() {
    assert_eq!(parse("").ok(), Some(Config::default()));
}

#[test]
fn agent_transports_are_tagged() {
    let config = parse(
        r#"
        [agents.a]
        transport = "websocket"
        url = "wss://example.com/acp"

        [agents.b]
        transport = "stdio"
        command = "agent"
        "#,
    )
    .ok();
    let agents = config.map(|config| config.agents);
    assert_eq!(
        agents.as_ref().and_then(|agents| agents.get("a")),
        Some(&AgentSpec::Websocket {
            url: "wss://example.com/acp".to_owned(),
            headers: Default::default(),
        })
    );
    assert!(matches!(
        agents.as_ref().and_then(|agents| agents.get("b")),
        Some(AgentSpec::Stdio { command, .. }) if command == "agent"
    ));
}

#[rstest]
#[case::default_scope("", WorkspaceScope::Shared)]
#[case::pod("workspace = \"pod\"", WorkspaceScope::Pod)]
fn gateway_agents_have_a_workspace_scope(#[case] line: &str, #[case] expected: WorkspaceScope) {
    let src = format!(
        "[agents.g]\ntransport = \"gateway\"\nurl = \"ws://localhost:8080/agents/g/acp\"\n{line}"
    );
    let config = parse(&src);
    assert!(
        matches!(
            config.as_ref().ok().and_then(|c| c.agents.get("g")),
            Some(AgentSpec::Gateway { workspace, .. }) if *workspace == expected
        ),
        "{config:?}"
    );
}

#[rstest]
#[case::unknown_field("[project]\nnmae = \"x\"")]
#[case::unknown_transport("[agents.a]\ntransport = \"grpc\"\nurl = \"x\"")]
#[case::http_without_url("[agents.a]\ntransport = \"http\"")]
#[case::unknown_scope("[agents.a]\ntransport = \"gateway\"\nurl = \"ws://x\"\nworkspace = \"tab\"")]
fn rejects_malformed_toml(#[case] src: &str) {
    assert!(matches!(parse(src), Err(ConfigError::Toml(_))));
}

fn problems(src: &str) -> Vec<Problem> {
    match parse(src) {
        Err(ConfigError::Invalid(problems)) => problems,
        other => panic!("expected validation problems, got {other:?}"),
    }
}

#[rstest]
#[case::unknown_agent_in_permission(
    r#"
    [[permissions]]
    agent = "ghost"
    decision = "allow"
    "#,
    Problem::UnknownAgent { context: "permissions[0]".into(), agent: "ghost".into() }
)]
fn reports_problem(#[case] src: &str, #[case] expected: Problem) {
    assert_eq!(problems(src), vec![expected]);
}

#[rstest]
#[case::layouts("[layouts.default]\nroot = \"main\"")]
#[case::commands("[commands.review]\ntitle = \"x\"")]
fn layout_is_not_configurable(#[case] src: &str) {
    // Tabs and panes are arranged at runtime; the file only says what can be opened.
    assert!(matches!(parse(src), Err(ConfigError::Toml(_))));
}
