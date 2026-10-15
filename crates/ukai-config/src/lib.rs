//! `ukai.toml`: the per-project declaration of the agents a pane can open,
//! layered over the user-level `config.toml` (see [`load_layered`]).
//!
//! [`parse`] turns a document into a [`Config`] and rejects it unless every
//! cross reference resolves; see [`Problem`] for what is checked.

mod interpolate;
mod layered;
mod schema;
mod validate;

use std::path::{Path, PathBuf};

pub use interpolate::{InterpolateError, interpolate};
pub use layered::{USER_FILE_NAME, config_dir, load_layered, load_user, merge, state_dir};
pub use schema::{
    AgentSpec, Config, Decision, Keys, PermissionRule, Project, Reports, Voice, WorkspaceScope,
};
pub use validate::Problem;

/// File name looked up at the project root.
pub const FILE_NAME: &str = "ukai.toml";

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("failed to read {path}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(transparent)]
    Toml(#[from] toml::de::Error),
    #[error("{path}: {source}")]
    TomlFile {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("invalid configuration:\n{}", list(.0))]
    Invalid(Vec<Problem>),
}

fn list(problems: &[Problem]) -> String {
    problems
        .iter()
        .map(|problem| format!("  - {problem}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Parse a document without cross-reference checks, for layers that are
/// only validated once merged.
fn parse_unchecked(src: &str) -> Result<Config, toml::de::Error> {
    toml::from_str(src)
}

/// Parse and validate a `ukai.toml` document.
pub fn parse(src: &str) -> Result<Config, ConfigError> {
    let config = parse_unchecked(src)?;
    let mut problems = validate::project_only(&config);
    problems.extend(validate::validate(&config));
    if problems.is_empty() {
        Ok(config)
    } else {
        Err(ConfigError::Invalid(problems))
    }
}

/// Read and validate the `ukai.toml` at `path`.
pub fn load(path: &Path) -> Result<Config, ConfigError> {
    let src = std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
        path: path.to_owned(),
        source,
    })?;
    parse(&src)
}

#[cfg(test)]
mod tests;
