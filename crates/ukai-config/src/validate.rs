//! Cross-reference checks that serde cannot express: every agent a
//! permission rule names has to be declared.

use crate::schema::Config;

/// One broken reference. Validation collects all of them rather than
/// stopping at the first, so a user fixes the file in one pass.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Problem {
    #[error("{context} refers to unknown agent `{agent}`")]
    UnknownAgent { context: String, agent: String },
}

pub(crate) fn validate(config: &Config) -> Vec<Problem> {
    config
        .permissions
        .iter()
        .enumerate()
        .filter_map(|(index, rule)| {
            let agent = rule.agent.as_ref()?;
            (!config.agents.contains_key(agent)).then(|| Problem::UnknownAgent {
                context: format!("permissions[{index}]"),
                agent: agent.clone(),
            })
        })
        .collect()
}
