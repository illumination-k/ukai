//! The user-level `config.toml` layered under each project's `ukai.toml`,
//! and where ukai keeps its files.
//!
//! The user file holds what every project shares — typically cloud agents —
//! and a project file adds to or overrides it by agent id.

use std::path::{Path, PathBuf};

use crate::schema::Config;
use crate::{ConfigError, FILE_NAME, parse_unchecked, validate};

/// Name of the user-level file inside [`config_dir`].
pub const USER_FILE_NAME: &str = "config.toml";

/// `$UKAI_CONFIG_DIR`, else `$XDG_CONFIG_HOME/ukai`, else `~/.config/ukai`.
pub fn config_dir(env: impl Fn(&str) -> Option<String>) -> Option<PathBuf> {
    dir(&env, "UKAI_CONFIG_DIR", "XDG_CONFIG_HOME", ".config")
}

/// `$UKAI_STATE_DIR`, else `$XDG_STATE_HOME/ukai`, else `~/.local/state/ukai`.
pub fn state_dir(env: impl Fn(&str) -> Option<String>) -> Option<PathBuf> {
    dir(&env, "UKAI_STATE_DIR", "XDG_STATE_HOME", ".local/state")
}

fn dir(
    env: &impl Fn(&str) -> Option<String>,
    own: &str,
    xdg: &str,
    home_relative: &str,
) -> Option<PathBuf> {
    let non_empty = |name: &str| env(name).filter(|value| !value.is_empty());
    if let Some(dir) = non_empty(own) {
        return Some(PathBuf::from(dir));
    }
    if let Some(base) = non_empty(xdg) {
        return Some(PathBuf::from(base).join("ukai"));
    }
    non_empty("HOME").map(|home| PathBuf::from(home).join(home_relative).join("ukai"))
}

/// Overlay `project` on `user`: agents are replaced by id, project
/// permission rules are consulted before the user's, project, report, and
/// review settings come from the project, and keybindings and voice from the
/// user.
pub fn merge(user: Config, project: Config) -> Config {
    let mut agents = user.agents;
    agents.extend(project.agents);
    let mut permissions = project.permissions;
    permissions.extend(user.permissions);
    Config {
        project: project.project,
        agents,
        permissions,
        reports: project.reports,
        review: project.review,
        keybindings: user.keybindings,
        voice: user.voice,
    }
}

/// The environment variable naming report directories on top of
/// `[reports] dirs`, separated as in `PATH` and relative to the project
/// root: how a pod's image or a gateway agent's environment adds reports to
/// a checkout whose `ukai.toml` declares none.
pub const REPORT_DIRS_ENV: &str = "UKAI_REPORT_DIRS";

/// Load the configuration for the project at `root`: the user file (if any)
/// overlaid with the project's `ukai.toml` (if any), with the report
/// directories of [`REPORT_DIRS_ENV`] added, validated together.
pub fn load_layered(user_file: Option<&Path>, root: &Path) -> Result<Config, ConfigError> {
    load_layered_with(user_file, root, |name| std::env::var(name).ok())
}

/// [`load_layered`], reading the environment through `env`.
pub fn load_layered_with(
    user_file: Option<&Path>,
    root: &Path,
    env: impl Fn(&str) -> Option<String>,
) -> Result<Config, ConfigError> {
    let user = read_optional(user_file)?;
    let project = read_optional(Some(&root.join(FILE_NAME)))?;
    let mut problems = validate::project_only(&project);
    let mut merged = merge(user, project);
    problems.extend(validate::validate(&merged));
    if let Some(value) = env(REPORT_DIRS_ENV) {
        for dir in std::env::split_paths(&value) {
            if dir.as_os_str().is_empty() || merged.reports.dirs.contains(&dir) {
                continue;
            }
            if validate::inside_project(&dir) {
                merged.reports.dirs.push(dir);
            } else {
                problems.push(crate::Problem::EnvReportDirOutsideProject(dir));
            }
        }
    }
    if problems.is_empty() {
        Ok(merged)
    } else {
        Err(ConfigError::Invalid(problems))
    }
}

/// The user file alone (empty if absent), for what applies to the whole app
/// rather than to a project: `[keybindings]` and `[voice]`. Agents it names are checked
/// once it is layered under a project, not here.
pub fn load_user(user_file: Option<&Path>) -> Result<Config, ConfigError> {
    read_optional(user_file)
}

fn read_optional(path: Option<&Path>) -> Result<Config, ConfigError> {
    let Some(path) = path else {
        return Ok(Config::default());
    };
    match std::fs::read_to_string(path) {
        Ok(src) => parse_unchecked(&src).map_err(|source| ConfigError::TomlFile {
            path: path.to_owned(),
            source,
        }),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
        Err(source) => Err(ConfigError::Io {
            path: path.to_owned(),
            source,
        }),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use rstest::rstest;

    use super::*;
    use crate::{AgentSpec, Problem};

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        move |name| map.get(name).cloned()
    }

    #[rstest]
    #[case::own_override(&[("UKAI_CONFIG_DIR", "/x"), ("XDG_CONFIG_HOME", "/y"), ("HOME", "/h")], Some("/x"))]
    #[case::xdg(&[("XDG_CONFIG_HOME", "/y"), ("HOME", "/h")], Some("/y/ukai"))]
    #[case::home(&[("HOME", "/h")], Some("/h/.config/ukai"))]
    #[case::empty_values_ignored(&[("UKAI_CONFIG_DIR", ""), ("HOME", "/h")], Some("/h/.config/ukai"))]
    #[case::nothing(&[], None)]
    fn resolves_config_dir(#[case] vars: &[(&str, &str)], #[case] expected: Option<&str>) {
        assert_eq!(config_dir(env(vars)), expected.map(PathBuf::from));
    }

    #[test]
    fn resolves_state_dir_under_home() {
        assert_eq!(
            state_dir(env(&[("HOME", "/h")])),
            Some(PathBuf::from("/h/.local/state/ukai"))
        );
    }

    fn config(src: &str) -> Config {
        parse_unchecked(src).unwrap_or_else(|e| panic!("{e}"))
    }

    #[test]
    fn project_agents_override_user_agents_by_id() {
        let user = config(
            r#"
            [agents.cloud]
            transport = "http"
            url = "https://user.example/acp"
            [agents.shared]
            transport = "stdio"
            command = "shared"
            "#,
        );
        let project = config(
            r#"
            [agents.cloud]
            transport = "http"
            url = "https://project.example/acp"
            "#,
        );
        let merged = merge(user, project);
        assert_eq!(
            merged.agents.keys().collect::<Vec<_>>(),
            ["cloud", "shared"]
        );
        assert!(matches!(
            merged.agents.get("cloud"),
            Some(AgentSpec::Http { url, .. }) if url == "https://project.example/acp"
        ));
    }

    #[test]
    fn layered_validation_sees_agents_from_both_files() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let user_file = dir.path().join("config.toml");
        std::fs::write(
            &user_file,
            "[agents.cloud]\ntransport = \"http\"\nurl = \"https://x/acp\"\n",
        )
        .unwrap_or_else(|e| panic!("{e}"));
        let root = dir.path().join("project");
        std::fs::create_dir(&root).unwrap_or_else(|e| panic!("{e}"));
        std::fs::write(
            root.join(FILE_NAME),
            "[[permissions]]\nagent = \"cloud\"\ndecision = \"allow\"\n",
        )
        .unwrap_or_else(|e| panic!("{e}"));

        let merged = load_layered(Some(&user_file), &root);
        assert!(merged.is_ok(), "{merged:?}");
    }

    #[test]
    fn keybindings_come_from_the_user_file() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let user_file = dir.path().join("config.toml");
        std::fs::write(&user_file, "[keybindings]\npalette = \"mod+p\"\n")
            .unwrap_or_else(|e| panic!("{e}"));
        let root = dir.path().join("project");
        std::fs::create_dir(&root).unwrap_or_else(|e| panic!("{e}"));

        let user = load_user(Some(&user_file)).map(|c| c.keybindings.len());
        assert_eq!(user.ok(), Some(1));
        let merged = load_layered(Some(&user_file), &root).map(|c| c.keybindings.len());
        assert_eq!(merged.ok(), Some(1));

        std::fs::write(root.join(FILE_NAME), "[keybindings]\npalette = \"mod+o\"\n")
            .unwrap_or_else(|e| panic!("{e}"));
        assert!(matches!(
            load_layered(Some(&user_file), &root),
            Err(ConfigError::Invalid(problems)) if problems == [Problem::KeybindingsInProject]
        ));
    }

    #[rstest]
    #[case::adds("out:build/reports", &["reports", "out", "build/reports"])]
    #[case::skips_repeats_and_empty("reports::out:", &["reports", "out"])]
    #[case::unset_keeps_the_file(None, &["reports"])]
    fn report_dirs_come_from_the_environment_too(
        #[case] value: impl Into<Option<&'static str>>,
        #[case] expected: &[&str],
    ) {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        std::fs::write(
            dir.path().join(FILE_NAME),
            "[reports]\ndirs = [\"reports\"]\n",
        )
        .unwrap_or_else(|e| panic!("{e}"));
        let value = value.into();
        let vars: Vec<(&str, &str)> = value.map(|v| (REPORT_DIRS_ENV, v)).into_iter().collect();
        let merged =
            load_layered_with(None, dir.path(), env(&vars)).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(
            merged.reports.dirs,
            expected.iter().map(PathBuf::from).collect::<Vec<_>>()
        );
    }

    #[test]
    fn report_dirs_from_the_environment_stay_inside_the_project() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let merged = load_layered_with(
            None,
            dir.path(),
            env(&[(REPORT_DIRS_ENV, "reports:../elsewhere:/etc")]),
        );
        assert!(
            matches!(
                &merged,
                Err(ConfigError::Invalid(problems)) if *problems == [
                    Problem::EnvReportDirOutsideProject(PathBuf::from("../elsewhere")),
                    Problem::EnvReportDirOutsideProject(PathBuf::from("/etc")),
                ]
            ),
            "{merged:?}"
        );
    }

    #[test]
    fn missing_files_are_empty_layers() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let merged = load_layered(Some(&dir.path().join("none.toml")), dir.path());
        assert_eq!(merged.ok(), Some(Config::default()));
    }
}
