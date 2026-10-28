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
    let src = format!("[gateways.g]\nurl = \"https://gw.example\"\n{line}");
    let config = parse(&src);
    assert!(
        matches!(
            config.as_ref().ok().and_then(|c| c.agent("g/claude")),
            Some(AgentSpec::Gateway { workspace, .. }) if workspace == expected
        ),
        "{config:?}"
    );
}

#[rstest]
#[case::https("https://gw.example", "wss://gw.example/agents/claude/acp")]
#[case::http_with_slash("http://localhost:8080/", "ws://localhost:8080/agents/claude/acp")]
#[case::wss("wss://gw.example/base", "wss://gw.example/base/agents/claude/acp")]
fn a_gateway_agent_is_its_acp_under_the_root(#[case] root: &str, #[case] expected: &str) {
    let gateway = Gateway {
        url: root.into(),
        headers: [("Authorization".to_owned(), "Bearer t".to_owned())].into(),
        workspace: WorkspaceScope::Pod,
    };
    assert_eq!(
        gateway.agent("claude"),
        AgentSpec::Gateway {
            url: expected.into(),
            headers: gateway.headers.clone(),
            workspace: WorkspaceScope::Pod,
        }
    );
}

#[test]
fn a_gateways_agent_is_found_before_and_after_it_is_listed() {
    let mut config = parse(
        "[gateways.team]\nurl = \"https://gw.example\"\n\n[agents.local]\ntransport = \"stdio\"\ncommand = \"a\"",
    )
    .unwrap_or_else(|e| panic!("{e}"));
    assert!(config.agent("team/claude").is_some());
    assert!(config.agent("team/").is_none());
    assert!(config.agent("team/a/b").is_none());
    assert!(config.agent("other/claude").is_none());

    config.list_gateway("team", ["claude", "codex", "bad/id"]);
    config.list_gateway("unknown", ["x"]);
    assert_eq!(
        config.agents.keys().collect::<Vec<_>>(),
        ["local", "team/claude", "team/codex"]
    );
    assert_eq!(
        config.agents.get("team/codex"),
        config.agent("team/codex").as_ref()
    );

    // A listing replaces the one before it.
    config.list_gateway("team", ["opencode"]);
    assert_eq!(
        config.agents.keys().collect::<Vec<_>>(),
        ["local", "team/opencode"]
    );
}

#[rstest]
#[case::unknown_field("[project]\nnmae = \"x\"")]
#[case::unknown_transport("[agents.a]\ntransport = \"grpc\"\nurl = \"x\"")]
#[case::http_without_url("[agents.a]\ntransport = \"http\"")]
#[case::gateway_in_agents("[agents.a]\ntransport = \"gateway\"\nurl = \"ws://x/agents/a/acp\"")]
#[case::unknown_scope("[gateways.g]\nurl = \"https://x\"\nworkspace = \"tab\"")]
#[case::gateway_without_url("[gateways.g]\nheaders = {}")]
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
#[case::agent_of_unknown_gateway_in_permission(
    "[[permissions]]\nagent = \"team/claude\"\ndecision = \"allow\"",
    Problem::UnknownAgent { context: "permissions[0]".into(), agent: "team/claude".into() }
)]
#[case::agent_id_with_slash(
    "[agents.\"team/claude\"]\ntransport = \"stdio\"\ncommand = \"a\"",
    Problem::AgentIdWithSlash("team/claude".into())
)]
#[case::gateway_id_with_slash(
    "[gateways.\"a/b\"]\nurl = \"https://gw.example\"",
    Problem::InvalidGatewayId("a/b".into())
)]
#[case::empty_gateway_id(
    "[gateways.\"\"]\nurl = \"https://gw.example\"",
    Problem::InvalidGatewayId(String::new())
)]
#[case::gateway_url_without_scheme(
    "[gateways.g]\nurl = \"gw.example\"",
    Problem::InvalidGatewayUrl { id: "g".into(), url: "gw.example".into() }
)]
#[case::gateway_url_without_host(
    "[gateways.g]\nurl = \"https:///\"",
    Problem::InvalidGatewayUrl { id: "g".into(), url: "https:///".into() }
)]
#[case::absolute_report_dir(
    "[reports]\ndirs = [\"/tmp/reports\"]",
    Problem::ReportDirOutsideProject("/tmp/reports".into())
)]
#[case::parent_report_dir(
    "[reports]\ndirs = [\"reports/../../elsewhere\"]",
    Problem::ReportDirOutsideProject("reports/../../elsewhere".into())
)]
#[case::invalid_glob(
    "[reports]\nglobs = [\"**/*.{html\"]",
    Problem::InvalidGlob { glob: "**/*.{html".into(), message: "unclosed alternate group; missing '}' (maybe escape '{' with '[{]'?)".into() }
)]
fn reports_problem(#[case] src: &str, #[case] expected: Problem) {
    assert_eq!(problems(src), vec![expected]);
}

#[rstest]
#[case::bad_tag(
    "[review.tags]\nSecurity = \"auth\"",
    vec![Problem::InvalidReviewTag("Security".into())]
)]
#[case::unknown_priority(
    "[review.priorities]\np4 = \"never\"",
    vec![Problem::UnknownPriority("p4".into())]
)]
#[case::empty_description(
    "[review.tags]\nrisk = \"  \"",
    vec![Problem::ReviewDescription("tags.risk".into(), DescriptionProblem::Empty)]
)]
#[case::multiline_description(
    "[review.priorities]\np0 = \"\"\"\nbroken\nor unsafe\"\"\"",
    vec![Problem::ReviewDescription("priorities.p0".into(), DescriptionProblem::Multiline)]
)]
#[case::long_description(
    &format!("[review.tags]\nrisk = \"{}\"", "x".repeat(201)),
    vec![Problem::ReviewDescription("tags.risk".into(), DescriptionProblem::Long)]
)]
#[case::invisible_description(
    "[review.tags]\nrisk = \"risky\u{200b}\"",
    vec![Problem::ReviewDescription("tags.risk".into(), DescriptionProblem::Invisible)]
)]
fn review_problem(#[case] src: &str, #[case] expected: Vec<Problem>) {
    assert_eq!(problems(src), expected);
}

#[test]
fn reads_the_review_vocabulary() {
    let config = parse(
        "[review.tags]\nsecurity = \"auth and secrets\"\n\n[review.priorities]\np0 = \"breaks prod\"",
    );
    let review = config.map(|c| c.review).unwrap_or_default();
    assert_eq!(
        review.tags.get("security").map(String::as_str),
        Some("auth and secrets")
    );
    assert_eq!(
        review.priorities.get("p0").map(String::as_str),
        Some("breaks prod")
    );
}

#[rstest]
#[case::word("risk", true)]
#[case::dashed("needs-test", true)]
#[case::digit("p1-ish", true)]
#[case::upper("Risk", false)]
#[case::space("needs test", false)]
#[case::edge_dash("-risk", false)]
#[case::empty("", false)]
#[case::long(&"x".repeat(33), false)]
fn tells_a_tag(#[case] tag: &str, #[case] valid: bool) {
    assert_eq!(is_tag(tag), valid);
}

#[test]
fn a_permission_names_a_declared_gateways_agent() {
    let src = "[gateways.team]\nurl = \"https://gw.example\"\n\n[[permissions]]\nagent = \"team/claude\"\ndecision = \"allow\"";
    assert!(parse(src).is_ok());
}

#[test]
fn every_problem_is_reported() {
    let src = r#"
        [[permissions]]
        agent = "ghost"
        decision = "deny"

        [gateways."a/b"]
        url = "gw"

        [reports]
        dirs = ["ok", "../out"]
        globs = ["[", "**/*.html"]
    "#;
    assert_eq!(problems(src).len(), 5);
}

#[test]
fn reports_default_to_html_and_images_without_network() {
    let reports = Reports::default();
    assert!(reports.globs.contains(&"**/*.html".to_owned()));
    assert!(reports.globs.contains(&"**/*.png".to_owned()));
    assert!(!reports.allow_network);
    assert_eq!(
        parse("[reports]\ndirs = [\"r\"]")
            .ok()
            .map(|c| c.reports.globs),
        Some(reports.globs)
    );
}

#[rstest]
#[case::layouts("[layouts.default]\nroot = \"main\"")]
#[case::commands("[commands.review]\ntitle = \"x\"")]
fn layout_is_not_configurable(#[case] src: &str) {
    // Tabs and panes are arranged at runtime; the file only says what can be opened.
    assert!(matches!(parse(src), Err(ConfigError::Toml(_))));
}

#[rstest]
#[case::one("palette = \"mod+p\"", Keys::One("mod+p".into()), &["mod+p"])]
#[case::several(
    "next-tab = [\"ctrl+tab\", \"mod+shift+]\"]",
    Keys::Many(vec!["ctrl+tab".into(), "mod+shift+]".into()]),
    &["ctrl+tab", "mod+shift+]"]
)]
#[case::unbound("close-pane = []", Keys::Many(Vec::new()), &[])]
fn keybindings_take_one_chord_or_a_list(
    #[case] line: &str,
    #[case] expected: Keys,
    #[case] chords: &[&str],
) {
    let config = parse_unchecked(&format!("[keybindings]\n{line}"));
    let keys = config
        .ok()
        .and_then(|config| config.keybindings.into_values().next());
    assert_eq!(keys.as_ref(), Some(&expected));
    assert_eq!(expected.chords(), chords);
}

#[test]
fn keybindings_are_user_level_only() {
    assert_eq!(
        problems("[keybindings]\npalette = \"mod+p\""),
        vec![Problem::KeybindingsInProject]
    );
}

#[test]
fn voice_is_user_level_only() {
    assert_eq!(
        problems("[voice]\nmodel = \"ggml-base.bin\""),
        vec![Problem::VoiceInProject]
    );
}

#[rstest]
#[case::detected("", vec![])]
#[case::code("language = \"ja\"", vec![])]
#[case::name("language = \"Japanese\"", vec![Problem::InvalidVoiceLanguage("Japanese".into())])]
#[case::upper("language = \"JA\"", vec![Problem::InvalidVoiceLanguage("JA".into())])]
fn voice_language_is_a_code(#[case] line: &str, #[case] expected: Vec<Problem>) {
    let config = parse_unchecked(&format!("[voice]\nmodel = \"m.bin\"\n{line}"));
    let problems = config.map(|config| validate::validate(&config));
    assert_eq!(problems.ok(), Some(expected));
}

#[rstest]
#[case::relative("models/ggml-base.bin", Some("/cfg"), "/cfg/models/ggml-base.bin")]
#[case::absolute("/m/ggml-base.bin", Some("/cfg"), "/m/ggml-base.bin")]
#[case::no_config_dir("ggml-base.bin", None, "ggml-base.bin")]
fn voice_model_is_relative_to_the_config_dir(
    #[case] model: &str,
    #[case] dir: Option<&str>,
    #[case] expected: &str,
) {
    let voice = Voice {
        model: model.into(),
        language: None,
    };
    assert_eq!(
        voice.model_path(dir.map(std::path::Path::new)),
        std::path::PathBuf::from(expected)
    );
}
