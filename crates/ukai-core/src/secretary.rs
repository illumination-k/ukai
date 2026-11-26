//! What the secretary pane shows beside its conversation (see "秘書" in
//! `docs/design.md`): every session of the gateway's running workspaces,
//! those waiting on the user first, and what its sidebar row counts.

use ukai_acp::secretary::{RunningWorkspace, SessionSummary};

/// Where a session stands, in the order the overview shows them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Standing {
    /// The agent asks the user something (a permission) no one answered.
    Waiting,
    /// A turn is running.
    Running,
    /// Between turns.
    Finished,
    /// The workspace's relay did not say.
    Unreachable,
}

impl Standing {
    fn of(session: &SessionSummary) -> Self {
        if session.waiting {
            Self::Waiting
        } else if session.running {
            Self::Running
        } else {
            Self::Finished
        }
    }
}

/// One row of the overview: a session, or a workspace that did not answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    /// The agent's id in `gateway.toml`.
    pub agent: String,
    /// The pane key of a workspace of its own; `None` for the shared one.
    pub pane: Option<String>,
    pub standing: Standing,
    /// `None` for a workspace that did not answer.
    pub session: Option<SessionSummary>,
}

impl Item {
    /// What the row is called: the session's title, else its last prompt;
    /// a workspace that did not answer, by its place.
    pub fn title(&self) -> String {
        let Some(session) = self.session.as_ref() else {
            return self.place();
        };
        session
            .title
            .as_deref()
            .or(session.last_prompt.as_deref())
            .map(|text| {
                let text = ukai_acp::secretary::unmarked(text).map_or(text, |(_, prompt)| prompt);
                text.lines()
                    .map(str::trim)
                    .find(|line| !line.is_empty())
                    .unwrap_or_default()
                    .to_owned()
            })
            .filter(|title| !title.is_empty())
            .unwrap_or_else(|| "Untitled session".to_owned())
    }

    /// Which workspace it is, as the overview names it.
    pub fn place(&self) -> String {
        match &self.pane {
            Some(key) => format!("{} · {}", self.agent, short(key)),
            None => format!("{} · shared", self.agent),
        }
    }
}

/// A pane key as far as the user needs it to tell workspaces apart.
fn short(key: &str) -> &str {
    key.char_indices()
        .nth(8)
        .map_or(key, |(end, _)| &key[..end])
}

/// Every session of `workspaces` but the secretary's own, waiting ones
/// first, then running, finished, and the workspaces that did not answer;
/// in the gateway's order within each. A workspace with no session yet has
/// nothing to show.
pub fn overview(workspaces: &[RunningWorkspace], secretary: &str) -> Vec<Item> {
    let mut items: Vec<Item> = workspaces
        .iter()
        .filter(|workspace| workspace.agent != secretary)
        .flat_map(|workspace| {
            let item = |standing, session| Item {
                agent: workspace.agent.clone(),
                pane: workspace.pane.clone(),
                standing,
                session,
            };
            match &workspace.unreachable {
                Some(_) => vec![item(Standing::Unreachable, None)],
                None => workspace
                    .sessions
                    .iter()
                    .map(|session| item(Standing::of(session), Some(session.clone())))
                    .collect(),
            }
        })
        .collect();
    items.sort_by_key(|item| item.standing);
    items
}

/// How many of `items` stand at `standing`.
pub fn count(items: &[Item], standing: Standing) -> usize {
    items
        .iter()
        .filter(|item| item.standing == standing)
        .count()
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    fn session(id: &str, running: bool, waiting: bool) -> SessionSummary {
        SessionSummary {
            id: id.into(),
            running,
            waiting,
            ..SessionSummary::default()
        }
    }

    fn workspace(
        agent: &str,
        pane: Option<&str>,
        sessions: Vec<SessionSummary>,
    ) -> RunningWorkspace {
        RunningWorkspace {
            agent: agent.into(),
            pane: pane.map(str::to_owned),
            sessions,
            unreachable: None,
        }
    }

    #[test]
    fn waiting_comes_first_and_the_secretary_is_left_out() {
        let workspaces = [
            workspace(
                "cloud",
                None,
                vec![session("done", false, false), session("busy", true, false)],
            ),
            workspace("aide", None, vec![session("own", false, true)]),
            RunningWorkspace {
                unreachable: Some("its relay did not answer".into()),
                ..workspace("cloud", Some("k2"), Vec::new())
            },
            workspace("cloud", Some("k1"), vec![session("asks", true, true)]),
            workspace("cloud", Some("k3"), Vec::new()),
        ];
        let items = overview(&workspaces, "aide");
        let shown: Vec<_> = items
            .iter()
            .map(|item| {
                (
                    item.standing,
                    item.session.as_ref().map(|session| session.id.as_str()),
                )
            })
            .collect();
        assert_eq!(
            shown,
            [
                (Standing::Waiting, Some("asks")),
                (Standing::Running, Some("busy")),
                (Standing::Finished, Some("done")),
                (Standing::Unreachable, None),
            ]
        );
        assert_eq!(count(&items, Standing::Waiting), 1);
        assert_eq!(count(&items, Standing::Running), 1);
    }

    #[rstest]
    #[case::titled(Some("Fix CI"), Some("ignored"), "Fix CI")]
    #[case::first_line_of_the_prompt(None, Some("\n fix the build\nplease"), "fix the build")]
    #[case::secretarys_prompt(
        None,
        Some(
            "[ukai] This prompt is from `aide`, the user's secretary agent, not from the user.\n\nfix #123"
        ),
        "fix #123"
    )]
    #[case::nothing(None, None, "Untitled session")]
    fn an_item_is_titled(
        #[case] title: Option<&str>,
        #[case] prompt: Option<&str>,
        #[case] expected: &str,
    ) {
        let item = Item {
            agent: "cloud".into(),
            pane: None,
            standing: Standing::Finished,
            session: Some(SessionSummary {
                title: title.map(str::to_owned),
                last_prompt: prompt.map(str::to_owned),
                ..session("s", false, false)
            }),
        };
        assert_eq!(item.title(), expected);
    }

    #[test]
    fn a_workspace_that_did_not_answer_is_titled_by_its_place() {
        let item = Item {
            agent: "cloud".into(),
            pane: Some("k1".into()),
            standing: Standing::Unreachable,
            session: None,
        };
        assert_eq!(item.title(), "cloud · k1");
    }

    #[rstest]
    #[case::shared(None, "cloud · shared")]
    #[case::own(Some("0123456789abcdef"), "cloud · 01234567")]
    #[case::short_key(Some("k1"), "cloud · k1")]
    fn an_item_names_its_workspace(#[case] pane: Option<&str>, #[case] expected: &str) {
        let item = Item {
            agent: "cloud".into(),
            pane: pane.map(str::to_owned),
            standing: Standing::Finished,
            session: None,
        };
        assert_eq!(item.place(), expected);
    }
}
