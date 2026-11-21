//! A session publishing a report of a gateway agent's workspace: the agent
//! saying, through the relay's MCP server, that it wrote a report for the
//! user, so the user finds it under that session rather than among every
//! report the workspace's sessions ever wrote. What a valid publication is,
//! so the relay that sends one and `ukai-store` that keeps it agree. See
//! "セッションのレポート" in `docs/design.md`.
//!
//! The types are [`ukai_acp::reports`]'s, since they cross the wire.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

pub use ukai_acp::reports::{Publication, Published};

use crate::{Layout, PaneContent, ProjectId, ReportSource};

/// Longest session id. ACP leaves its shape to the agent; agents use UUIDs.
pub const SESSION_MAX: usize = 256;
/// Longest path, in bytes.
pub const PATH_MAX: usize = 1024;
/// Longest title, in characters: one line in the sidebar.
pub const TITLE_MAX: usize = 200;
/// Most publications one workspace keeps: as many as the reports the
/// gateway copies of it.
pub const WORKSPACE_MAX: usize = crate::report::MAX_TREE_FILES;

/// What can be wrong with a publication; [`problems`] lists them all.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Problem {
    #[error("the publication names no session")]
    EmptySession,
    #[error("a session id is at most {SESSION_MAX} characters, with no control characters")]
    InvalidSession,
    #[error("name the report to publish")]
    EmptyPath,
    #[error(
        "a report's path is relative to the checkout, like `reports/run.html`, at most {PATH_MAX} bytes"
    )]
    InvalidPath,
    #[error("the title is one line of at most {TITLE_MAX} characters")]
    InvalidTitle,
}

/// Everything wrong with `session` publishing `path` as `title`.
pub fn problems(session: &str, path: &str, title: Option<&str>) -> Vec<Problem> {
    let mut problems = Vec::new();
    if session.is_empty() {
        problems.push(Problem::EmptySession);
    } else if session.chars().count() > SESSION_MAX || session.chars().any(char::is_control) {
        problems.push(Problem::InvalidSession);
    }
    if path.is_empty() {
        problems.push(Problem::EmptyPath);
    } else if !is_report_path(path) {
        problems.push(Problem::InvalidPath);
    }
    if let Some(title) = title
        && (title.chars().count() > TITLE_MAX || title.chars().any(char::is_control))
    {
        problems.push(Problem::InvalidTitle);
    }
    problems
}

/// A title as kept: trimmed, and none when that leaves nothing.
pub fn title(title: Option<&str>) -> Option<String> {
    title
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .map(str::to_owned)
}

/// Which sessions published each report of a workspace's listing, as the
/// sidebar sorts them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Sessions(BTreeMap<PathBuf, Vec<Published>>);

impl Sessions {
    /// What `listing` says about who published its reports.
    pub fn of(listing: &ukai_acp::reports::Listing) -> Self {
        Self(
            listing
                .reports
                .iter()
                .filter(|entry| !entry.published.is_empty())
                .map(|entry| (PathBuf::from(&entry.path), entry.published.clone()))
                .collect(),
        )
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn published_by(&self, path: &Path, session: &str) -> bool {
        self.0
            .get(path)
            .is_some_and(|published| published.iter().any(|p| p.session == session))
    }

    /// The title the latest publication gave `path`.
    pub fn title(&self, path: &Path) -> Option<&str> {
        self.0.get(path)?.iter().find_map(|p| p.title.as_deref())
    }

    /// `paths` as the reports `session` published and the rest, each in
    /// the order given.
    pub fn split<'a>(
        &self,
        paths: &'a [PathBuf],
        session: &str,
    ) -> (Vec<&'a PathBuf>, Vec<&'a PathBuf>) {
        paths
            .iter()
            .partition(|path| self.published_by(path, session))
    }
}

/// The session of the focused pane, when it is an agent pane of `project`
/// working in `place`: whose reports the sidebar shows first.
pub fn focused_session(
    layout: &Layout,
    project: &ProjectId,
    place: &ReportSource,
) -> Option<String> {
    match layout.content(layout.focused())? {
        PaneContent::Agent {
            project: p,
            agent,
            session: Some(session),
            workspace,
        } if p == project && crate::sidebar::place(agent, workspace.as_ref()) == *place => {
            Some(session.clone())
        }
        _ => None,
    }
}

/// Whether `path` names a file inside the checkout by plain names alone:
/// relative, with no `.`, `..`, or empty part, and `/` between parts.
fn is_report_path(path: &str) -> bool {
    path.len() <= PATH_MAX
        && !path.contains('\\')
        && !path.chars().any(char::is_control)
        && path.split('/').all(|part| !part.is_empty())
        && Path::new(path)
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case::fine("s1", "reports/run.html", Some("Run"), vec![])]
    #[case::untitled("s1", "reports/run.html", None, vec![])]
    #[case::no_session("", "reports/run.html", None, vec![Problem::EmptySession])]
    #[case::long_session(&"s".repeat(SESSION_MAX + 1), "a.html", None, vec![Problem::InvalidSession])]
    #[case::control_in_session("s\n1", "a.html", None, vec![Problem::InvalidSession])]
    #[case::no_path("s1", "", None, vec![Problem::EmptyPath])]
    #[case::absolute("s1", "/etc/passwd", None, vec![Problem::InvalidPath])]
    #[case::parent("s1", "reports/../secret.html", None, vec![Problem::InvalidPath])]
    #[case::current("s1", "./reports/a.html", None, vec![Problem::InvalidPath])]
    #[case::empty_part("s1", "reports//a.html", None, vec![Problem::InvalidPath])]
    #[case::trailing_slash("s1", "reports/", None, vec![Problem::InvalidPath])]
    #[case::backslash("s1", "reports\\a.html", None, vec![Problem::InvalidPath])]
    #[case::long_path("s1", &"a/".repeat(PATH_MAX), None, vec![Problem::InvalidPath])]
    #[case::long_title("s1", "a.html", Some(&*"t".repeat(TITLE_MAX + 1)), vec![Problem::InvalidTitle])]
    #[case::multiline_title("s1", "a.html", Some("a\nb"), vec![Problem::InvalidTitle])]
    #[case::everything("", "../a", Some("a\nb"), vec![Problem::EmptySession, Problem::InvalidPath, Problem::InvalidTitle])]
    fn finds_every_problem(
        #[case] session: &str,
        #[case] path: &str,
        #[case] title: Option<&str>,
        #[case] expected: Vec<Problem>,
    ) {
        assert_eq!(problems(session, path, title), expected);
    }

    fn entry(path: &str, sessions: &[(&str, u64)]) -> ukai_acp::reports::Entry {
        ukai_acp::reports::Entry {
            path: path.into(),
            modified: None,
            published: sessions
                .iter()
                .map(|(session, at)| Published {
                    session: (*session).into(),
                    title: Some(format!("{path} by {session}")),
                    at: *at,
                })
                .collect(),
        }
    }

    #[test]
    fn splits_a_sessions_reports_from_the_rest() {
        let listing = ukai_acp::reports::Listing {
            reports: vec![
                entry("reports/c.html", &[("s2", 3)]),
                entry("reports/b.html", &[]),
                entry("reports/a.html", &[("s2", 2), ("s1", 1)]),
            ],
            synced: None,
        };
        let sessions = Sessions::of(&listing);
        let paths: Vec<PathBuf> = listing
            .reports
            .iter()
            .map(|e| PathBuf::from(&e.path))
            .collect();
        let (mine, rest) = sessions.split(&paths, "s1");
        assert_eq!(mine, [&PathBuf::from("reports/a.html")]);
        assert_eq!(
            rest,
            [
                &PathBuf::from("reports/c.html"),
                &PathBuf::from("reports/b.html")
            ]
        );
        let (mine, rest) = sessions.split(&paths, "s2");
        assert_eq!(mine.len(), 2);
        assert_eq!(rest, [&PathBuf::from("reports/b.html")]);
        assert_eq!(
            sessions.title(Path::new("reports/a.html")),
            Some("reports/a.html by s2")
        );
        assert_eq!(sessions.title(Path::new("reports/b.html")), None);
        // A new session has published nothing yet.
        let (mine, rest) = sessions.split(&paths, "s3");
        assert!(mine.is_empty());
        assert_eq!(rest.len(), 3);
    }

    #[test]
    fn the_focused_agent_pane_names_its_workspaces_session() {
        let project = ProjectId("p".into());
        let shared = ukai_acp::PaneWorkspace {
            key: "a1".into(),
            scope: ukai_acp::WorkspaceScope::Shared,
        };
        let mut layout = Layout::default();
        let pane = layout.focused();
        layout.set_content(
            pane,
            PaneContent::Agent {
                project: project.clone(),
                agent: "cloud".into(),
                session: Some("s1".into()),
                workspace: Some(shared.clone()),
            },
        );
        let shared = crate::sidebar::place("cloud", Some(&shared));
        assert_eq!(
            focused_session(&layout, &project, &shared).as_deref(),
            Some("s1")
        );
        assert_eq!(
            focused_session(&layout, &project, &ReportSource::Project),
            None
        );
        assert_eq!(
            focused_session(&layout, &ProjectId("q".into()), &shared),
            None
        );
        layout.set_content(pane, PaneContent::Empty);
        assert_eq!(focused_session(&layout, &project, &shared), None);
    }

    #[rstest]
    #[case::kept(Some(" Run "), Some("Run"))]
    #[case::blank(Some("  "), None)]
    #[case::none(None, None)]
    fn keeps_a_trimmed_title(#[case] given: Option<&str>, #[case] kept: Option<&str>) {
        assert_eq!(title(given).as_deref(), kept);
    }
}
