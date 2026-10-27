//! Checks that serde cannot express: every agent a permission rule names has
//! to be declared, the report, review, and voice settings have to make
//! sense, and a project file holds nothing that only the user file may set.

use std::path::{Component, PathBuf};

use crate::layered::REPORT_DIRS_ENV;
use crate::schema::{Config, PRIORITIES, REVIEW_DESCRIPTION_MAX, REVIEW_TAG_MAX, Review};

/// One problem with a configuration. Validation collects all of them rather
/// than stopping at the first, so a user fixes the file in one pass.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Problem {
    #[error("{context} refers to unknown agent `{agent}`")]
    UnknownAgent { context: String, agent: String },
    #[error("agents.{0}: an agent id has no `/`, which names a gateway's agent")]
    AgentIdWithSlash(String),
    #[error("gateways.{0}: a gateway id is not empty and has no `/`")]
    InvalidGatewayId(String),
    #[error("gateways.{id}.url: `{url}` is not an http(s):// or ws(s):// URL")]
    InvalidGatewayUrl { id: String, url: String },
    #[error("reports.dirs: `{}` is not inside the project", .0.display())]
    ReportDirOutsideProject(PathBuf),
    #[error("{REPORT_DIRS_ENV}: `{}` is not inside the project", .0.display())]
    EnvReportDirOutsideProject(PathBuf),
    #[error("reports.globs: `{glob}`: {message}")]
    InvalidGlob { glob: String, message: String },
    #[error("keybindings: set them in the user config.toml, not in a project's ukai.toml")]
    KeybindingsInProject,
    #[error("voice: set it in the user config.toml, not in a project's ukai.toml")]
    VoiceInProject,
    #[error("voice.language: `{0}` is not an ISO 639-1 code such as `ja` or `en`")]
    InvalidVoiceLanguage(String),
    #[error(
        "review.tags: `{0}` is not lowercase letters, digits, and `-`, at most {REVIEW_TAG_MAX} characters"
    )]
    InvalidReviewTag(String),
    #[error("review.priorities: `{0}` is not one of p0, p1, p2, p3")]
    UnknownPriority(String),
    #[error("review.{0}: {1}")]
    ReviewDescription(String, DescriptionProblem),
}

/// What is wrong with a description in `[review]`, which the agent is told
/// as it is written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum DescriptionProblem {
    #[error("say what it means")]
    Empty,
    #[error("keep it to one line")]
    Multiline,
    #[error("keep it to {REVIEW_DESCRIPTION_MAX} characters")]
    Long,
    #[error("it has a character that does not show; remove it")]
    Invisible,
}

/// What a project's `ukai.toml` may not hold, whatever the user file says.
pub(crate) fn project_only(project: &Config) -> Vec<Problem> {
    let keybindings = (!project.keybindings.is_empty()).then_some(Problem::KeybindingsInProject);
    let voice = project.voice.is_some().then_some(Problem::VoiceInProject);
    keybindings.into_iter().chain(voice).collect()
}

pub(crate) fn validate(config: &Config) -> Vec<Problem> {
    let unknown_agents = config
        .permissions
        .iter()
        .enumerate()
        .filter_map(|(index, rule)| {
            let agent = rule.agent.as_ref()?;
            config
                .agent(agent)
                .is_none()
                .then(|| Problem::UnknownAgent {
                    context: format!("permissions[{index}]"),
                    agent: agent.clone(),
                })
        });
    let slashed_agents = config
        .agents
        .keys()
        .filter(|id| id.contains('/'))
        .map(|id| Problem::AgentIdWithSlash(id.clone()));
    let bad_gateways = config.gateways.iter().flat_map(|(id, gateway)| {
        let bad_id =
            (id.is_empty() || id.contains('/')).then(|| Problem::InvalidGatewayId(id.clone()));
        let bad_url = (!is_gateway_url(&gateway.url)).then(|| Problem::InvalidGatewayUrl {
            id: id.clone(),
            url: gateway.url.clone(),
        });
        bad_id.into_iter().chain(bad_url)
    });
    let outside_dirs = config
        .reports
        .dirs
        .iter()
        .filter(|dir| !inside_project(dir))
        .map(|dir| Problem::ReportDirOutsideProject(dir.clone()));
    let bad_globs = config.reports.globs.iter().filter_map(|glob| {
        globset::Glob::new(glob)
            .err()
            .map(|error| Problem::InvalidGlob {
                glob: glob.clone(),
                message: error.kind().to_string(),
            })
    });
    let bad_language = config
        .voice
        .as_ref()
        .and_then(|voice| voice.language.as_ref())
        .filter(|language| {
            !(language.len() == 2 && language.chars().all(|c| c.is_ascii_lowercase()))
        })
        .map(|language| Problem::InvalidVoiceLanguage(language.clone()));
    unknown_agents
        .chain(slashed_agents)
        .chain(bad_gateways)
        .chain(outside_dirs)
        .chain(bad_globs)
        .chain(bad_language)
        .chain(review_problems(&config.review))
        .collect()
}

/// Whether `url` names a host over http(s) or ws(s).
fn is_gateway_url(url: &str) -> bool {
    url.split_once("://").is_some_and(|(scheme, rest)| {
        matches!(scheme, "http" | "https" | "ws" | "wss") && !rest.trim_end_matches('/').is_empty()
    })
}

fn review_problems(review: &Review) -> Vec<Problem> {
    let mut problems = Vec::new();
    for (tag, description) in &review.tags {
        if !is_tag(tag) {
            problems.push(Problem::InvalidReviewTag(tag.clone()));
        }
        problems.extend(
            description_problems(description)
                .map(|why| Problem::ReviewDescription(format!("tags.{tag}"), why)),
        );
    }
    for (priority, description) in &review.priorities {
        if !PRIORITIES.contains(&priority.as_str()) {
            problems.push(Problem::UnknownPriority(priority.clone()));
        }
        problems.extend(
            description_problems(description)
                .map(|why| Problem::ReviewDescription(format!("priorities.{priority}"), why)),
        );
    }
    problems
}

/// Whether `tag` is lowercase letters, digits, and `-` between them.
pub fn is_tag(tag: &str) -> bool {
    !tag.is_empty()
        && tag.len() <= REVIEW_TAG_MAX
        && !tag.starts_with('-')
        && !tag.ends_with('-')
        && tag
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

fn description_problems(description: &str) -> impl Iterator<Item = DescriptionProblem> {
    let text = description.trim();
    [
        (text.is_empty(), DescriptionProblem::Empty),
        (text.contains(['\n', '\r']), DescriptionProblem::Multiline),
        (
            text.chars().count() > REVIEW_DESCRIPTION_MAX,
            DescriptionProblem::Long,
        ),
        (
            text.chars()
                .any(|c| (c.is_control() && !matches!(c, '\n' | '\r')) || invisible(c)),
            DescriptionProblem::Invisible,
        ),
    ]
    .into_iter()
    .filter_map(|(wrong, problem)| wrong.then_some(problem))
}

/// Whether `c` renders as nothing or reorders what is around it, as
/// `ukai_core::knowledge` refuses in notes.
fn invisible(c: char) -> bool {
    matches!(
        u32::from(c),
        0x00AD
            | 0x061C
            | 0x180E
            | 0x200B..=0x200F
            | 0x202A..=0x202E
            | 0x2060..=0x2064
            | 0x2066..=0x206F
            | 0xFEFF
            | 0xFFF9..=0xFFFB
            | 0xE0000..=0xE007F
    )
}

/// Whether a report directory names a place inside the project: relative,
/// and without `..`.
pub(crate) fn inside_project(dir: &std::path::Path) -> bool {
    dir.components()
        .all(|c| matches!(c, Component::Normal(_) | Component::CurDir))
}
