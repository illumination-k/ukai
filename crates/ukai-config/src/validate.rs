//! Checks that serde cannot express: every agent a permission rule names has
//! to be declared, and the report settings have to make sense.

use std::path::{Component, PathBuf};

use crate::schema::Config;

/// One problem with a configuration. Validation collects all of them rather
/// than stopping at the first, so a user fixes the file in one pass.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Problem {
    #[error("{context} refers to unknown agent `{agent}`")]
    UnknownAgent { context: String, agent: String },
    #[error("reports.dirs: `{}` is not inside the project", .0.display())]
    ReportDirOutsideProject(PathBuf),
    #[error("reports.globs: `{glob}`: {message}")]
    InvalidGlob { glob: String, message: String },
}

pub(crate) fn validate(config: &Config) -> Vec<Problem> {
    let unknown_agents = config
        .permissions
        .iter()
        .enumerate()
        .filter_map(|(index, rule)| {
            let agent = rule.agent.as_ref()?;
            (!config.agents.contains_key(agent)).then(|| Problem::UnknownAgent {
                context: format!("permissions[{index}]"),
                agent: agent.clone(),
            })
        });
    let outside_dirs = config
        .reports
        .dirs
        .iter()
        .filter(|dir| {
            !dir.components()
                .all(|c| matches!(c, Component::Normal(_) | Component::CurDir))
        })
        .map(|dir| Problem::ReportDirOutsideProject(dir.clone()));
    let bad_globs = config.reports.globs.iter().filter_map(|glob| {
        globset::Glob::new(glob)
            .err()
            .map(|error| Problem::InvalidGlob {
                glob: glob.clone(),
                message: error.kind().to_string(),
            })
    });
    unknown_agents
        .chain(outside_dirs)
        .chain(bad_globs)
        .collect()
}
