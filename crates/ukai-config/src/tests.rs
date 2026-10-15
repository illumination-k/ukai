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

#[test]
fn every_problem_is_reported() {
    let src = r#"
        [[permissions]]
        agent = "ghost"
        decision = "deny"

        [reports]
        dirs = ["ok", "../out"]
        globs = ["[", "**/*.html"]
    "#;
    assert_eq!(problems(src).len(), 3);
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
