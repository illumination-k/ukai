//! The sidebar's tree: each project, the workspaces its agents work in (this
//! machine's checkout, and the gateway workspaces), the agent panes open in
//! each, and which of its nodes the user has folded.
//!
//! A workspace is named by the [`ReportSource`] its reports and ports are read
//! from, so the agents, reports, and ports of one checkout land under one
//! node. Knowledge and schedules belong to the project's gateway agents
//! rather than to a workspace, so they sit beside the workspaces.

use serde::{Deserialize, Serialize};
use ukai_acp::{PaneWorkspace, WorkspaceScope};
use ukai_config::{AgentSpec, Config};

use crate::layout::{Layout, PaneContent, PaneId, ReportSource};
use crate::project::ProjectId;

/// The workspace a pane of `agent` works in: this machine's checkout
/// without a gateway workspace, else the agent's, with a shared one
/// normalized to no key (the gateway ignores it, so every pane sharing the
/// workspace names it alike).
pub fn place(agent: &str, workspace: Option<&PaneWorkspace>) -> ReportSource {
    match workspace {
        None => ReportSource::Project,
        Some(workspace) => ReportSource::Agent {
            agent: agent.to_owned(),
            workspace: (workspace.scope == WorkspaceScope::Pod).then(|| workspace.clone()),
        },
    }
}

/// An agent pane as the sidebar lists it under its workspace.
#[derive(Debug, Clone, PartialEq)]
pub struct OpenAgent {
    pub pane: PaneId,
    pub agent: String,
    /// Index of the tab holding the pane.
    pub tab: usize,
}

/// One workspace of a project and the agent panes working in it.
#[derive(Debug, Clone, PartialEq)]
pub struct Workspace {
    pub place: ReportSource,
    /// In tab and then pane order.
    pub agents: Vec<OpenAgent>,
}

/// What the sidebar shows of one project.
#[derive(Debug, Clone, PartialEq)]
pub struct Tree {
    /// This machine's checkout first, then the shared workspace of every
    /// gateway agent that shares one by default (it is there, and keeps its
    /// reports, with no pane open), then the workspaces of open panes not
    /// listed yet, in pane order.
    pub workspaces: Vec<Workspace>,
    /// The project's gateway agents, by name: knowledge and schedules exist
    /// only with one.
    pub gateways: Vec<String>,
}

impl Tree {
    /// Whether this machine's checkout is all there is, so the sidebar can
    /// leave out the workspace level.
    pub fn flat(&self) -> bool {
        matches!(self.workspaces.as_slice(), [only] if only.place.is_project())
    }

    pub fn agent_count(&self) -> usize {
        self.workspaces.iter().map(|w| w.agents.len()).sum()
    }
}

/// The tree of `project`, from the panes of `layout` and its configuration
/// (none when it did not load).
pub fn tree(layout: &Layout, project: &ProjectId, config: Option<&Config>) -> Tree {
    let mut workspaces = vec![Workspace {
        place: ReportSource::Project,
        agents: Vec::new(),
    }];
    let mut gateways = Vec::new();
    for (agent, spec) in config.into_iter().flat_map(|config| &config.agents) {
        if let AgentSpec::Gateway { workspace, .. } = spec {
            gateways.push(agent.clone());
            if *workspace == WorkspaceScope::Shared {
                workspaces.push(Workspace {
                    place: ReportSource::Agent {
                        agent: agent.clone(),
                        workspace: None,
                    },
                    agents: Vec::new(),
                });
            }
        }
    }
    for (tab, panes) in layout.tabs().iter().map(|t| t.root.panes()).enumerate() {
        for pane in panes {
            let Some(PaneContent::Agent {
                project: p,
                agent,
                workspace,
                ..
            }) = layout.content(pane)
            else {
                continue;
            };
            if p != project {
                continue;
            }
            let place = place(agent, workspace.as_ref());
            let open = OpenAgent {
                pane,
                agent: agent.clone(),
                tab,
            };
            match workspaces.iter_mut().find(|w| w.place == place) {
                Some(known) => known.agents.push(open),
                None => workspaces.push(Workspace {
                    place,
                    agents: vec![open],
                }),
            }
        }
    }
    Tree {
        workspaces,
        gateways,
    }
}

/// A node of the sidebar that folds.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Node {
    Project,
    Workspace { place: ReportSource },
    Reports { place: ReportSource },
    Ports { place: ReportSource },
    Knowledge,
    Schedules,
}

impl Node {
    /// Knowledge and schedules are looked at now and then rather than while
    /// working, so they start folded; their badges say when to look.
    fn open_by_default(&self) -> bool {
        !matches!(self, Self::Knowledge | Self::Schedules)
    }
}

/// A node of one project's tree.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Fold {
    pub project: ProjectId,
    pub node: Node,
}

impl Fold {
    pub fn new(project: &ProjectId, node: Node) -> Self {
        Self {
            project: project.clone(),
            node,
        }
    }
}

/// Which nodes the user opened or folded, as the nodes whose state differs
/// from their default, so a node never touched follows the default.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Folds {
    toggled: Vec<Fold>,
}

impl Folds {
    pub fn is_empty(&self) -> bool {
        self.toggled.is_empty()
    }

    pub fn is_open(&self, fold: &Fold) -> bool {
        fold.node.open_by_default() != self.toggled.contains(fold)
    }

    pub fn toggle(&mut self, fold: Fold) {
        match self.toggled.iter().position(|known| *known == fold) {
            Some(index) => {
                self.toggled.remove(index);
            }
            None => self.toggled.push(fold),
        }
    }

    /// Show `fold` open, as when something in it is launched.
    pub fn open(&mut self, fold: Fold) {
        if !self.is_open(&fold) {
            self.toggle(fold);
        }
    }

    /// Forget what was folded of `project`, once it is closed.
    pub fn forget(&mut self, project: &ProjectId) {
        self.toggled.retain(|fold| fold.project != *project);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::Path;

    use rstest::rstest;

    use super::*;
    use crate::{Direction, Projects};

    fn gateway(workspace: WorkspaceScope) -> AgentSpec {
        AgentSpec::Gateway {
            url: "ws://gw/agents/claude/acp".into(),
            headers: BTreeMap::new(),
            workspace,
        }
    }

    fn stdio() -> AgentSpec {
        AgentSpec::Stdio {
            command: "agent".into(),
            args: Vec::new(),
            env: BTreeMap::new(),
        }
    }

    fn config(agents: &[(&str, AgentSpec)]) -> Config {
        Config {
            agents: agents
                .iter()
                .map(|(id, spec)| ((*id).to_owned(), spec.clone()))
                .collect(),
            ..Config::default()
        }
    }

    fn pod(key: &str) -> Option<PaneWorkspace> {
        Some(PaneWorkspace {
            key: key.to_owned(),
            scope: WorkspaceScope::Pod,
        })
    }

    fn shared(key: &str) -> Option<PaneWorkspace> {
        Some(PaneWorkspace {
            key: key.to_owned(),
            scope: WorkspaceScope::Shared,
        })
    }

    /// Fill the focused pane with an agent and split a new one off it.
    fn agent_pane(
        layout: &mut Layout,
        project: &ProjectId,
        agent: &str,
        workspace: Option<PaneWorkspace>,
    ) -> PaneId {
        let pane = layout.focused();
        layout.set_content(
            pane,
            PaneContent::Agent {
                project: project.clone(),
                agent: agent.to_owned(),
                session: None,
                workspace,
            },
        );
        if let Some(next) = layout.split(pane, Direction::Horizontal) {
            layout.focus(next);
        }
        pane
    }

    fn places(tree: &Tree) -> Vec<(ReportSource, Vec<PaneId>)> {
        tree.workspaces
            .iter()
            .map(|w| (w.place.clone(), w.agents.iter().map(|a| a.pane).collect()))
            .collect()
    }

    fn agent_place(agent: &str, workspace: Option<PaneWorkspace>) -> ReportSource {
        ReportSource::Agent {
            agent: agent.to_owned(),
            workspace,
        }
    }

    #[test]
    fn groups_agent_panes_by_the_workspace_they_work_in() {
        let mut projects = Projects::default();
        let project = projects.add(Path::new("/work/ukai"));
        let other = projects.add(Path::new("/work/other"));
        let mut layout = Layout::default();
        let local = agent_pane(&mut layout, &project, "local", None);
        let a = agent_pane(&mut layout, &project, "cloud", shared("a"));
        let c = agent_pane(&mut layout, &project, "cloud", pod("c"));
        let b = agent_pane(&mut layout, &project, "cloud", shared("b"));
        let d = agent_pane(&mut layout, &project, "pods", pod("d"));
        let e = agent_pane(&mut layout, &project, "pods", shared("e"));
        agent_pane(&mut layout, &other, "local", None);
        let config = config(&[
            ("cloud", gateway(WorkspaceScope::Shared)),
            ("local", stdio()),
            ("pods", gateway(WorkspaceScope::Pod)),
        ]);

        let tree = tree(&layout, &project, Some(&config));
        assert_eq!(
            places(&tree),
            [
                (ReportSource::Project, vec![local]),
                // Shared by default, so listed first; both shared panes are in it.
                (agent_place("cloud", None), vec![a, b]),
                (agent_place("cloud", pod("c")), vec![c]),
                (agent_place("pods", pod("d")), vec![d]),
                // A pod-by-default agent's shared workspace shows once a pane is in it.
                (agent_place("pods", None), vec![e]),
            ]
        );
        assert_eq!(tree.gateways, ["cloud", "pods"]);
        assert_eq!(tree.agent_count(), 6);
        assert!(!tree.flat());
    }

    #[test]
    fn lists_a_shared_workspace_with_no_pane_in_it() {
        let mut projects = Projects::default();
        let project = projects.add(Path::new("/work/ukai"));
        let config = config(&[
            ("cloud", gateway(WorkspaceScope::Shared)),
            ("pods", gateway(WorkspaceScope::Pod)),
        ]);
        let tree = tree(&Layout::default(), &project, Some(&config));
        assert_eq!(
            places(&tree),
            [
                (ReportSource::Project, vec![]),
                (agent_place("cloud", None), vec![]),
            ]
        );
    }

    #[rstest]
    #[case::no_config(None)]
    #[case::local_agents_only(Some(config(&[("local", stdio())])))]
    fn is_flat_with_only_this_machine(#[case] config: Option<Config>) {
        let mut projects = Projects::default();
        let project = projects.add(Path::new("/work/ukai"));
        let mut layout = Layout::default();
        let pane = agent_pane(&mut layout, &project, "local", None);
        let tree = tree(&layout, &project, config.as_ref());
        assert!(tree.flat());
        assert!(tree.gateways.is_empty());
        assert_eq!(places(&tree), [(ReportSource::Project, vec![pane])]);
    }

    #[test]
    fn a_gateway_pane_is_not_flat_even_with_its_agent_gone() {
        // The pane's workspace says where it works, whatever the
        // configuration says now.
        let mut projects = Projects::default();
        let project = projects.add(Path::new("/work/ukai"));
        let mut layout = Layout::default();
        agent_pane(&mut layout, &project, "cloud", pod("c"));
        assert!(!tree(&layout, &project, None).flat());
    }

    #[test]
    fn places_a_shared_workspace_under_no_key() {
        assert_eq!(place("cloud", None), ReportSource::Project);
        assert_eq!(
            place("cloud", shared("a").as_ref()),
            agent_place("cloud", None)
        );
        assert_eq!(
            place("cloud", pod("c").as_ref()),
            agent_place("cloud", pod("c"))
        );
    }

    #[rstest]
    #[case::project(Node::Project, true)]
    #[case::workspace(Node::Workspace { place: ReportSource::Project }, true)]
    #[case::reports(Node::Reports { place: ReportSource::Project }, true)]
    #[case::ports(Node::Ports { place: ReportSource::Project }, true)]
    #[case::knowledge(Node::Knowledge, false)]
    #[case::schedules(Node::Schedules, false)]
    fn toggles_a_node_from_its_default(#[case] node: Node, #[case] open: bool) {
        let mut projects = Projects::default();
        let project = projects.add(Path::new("/work/ukai"));
        let fold = Fold::new(&project, node);
        let mut folds = Folds::default();
        assert_eq!(folds.is_open(&fold), open);
        folds.toggle(fold.clone());
        assert_eq!(folds.is_open(&fold), !open);
        folds.toggle(fold.clone());
        assert_eq!(folds.is_open(&fold), open);
        assert!(
            folds.is_empty(),
            "back to the default leaves nothing to save"
        );
        folds.open(fold.clone());
        folds.open(fold.clone());
        assert!(folds.is_open(&fold));
    }

    #[test]
    fn forgets_the_folds_of_a_closed_project() {
        let mut projects = Projects::default();
        let a = projects.add(Path::new("/work/a"));
        let b = projects.add(Path::new("/work/b"));
        let mut folds = Folds::default();
        folds.toggle(Fold::new(&a, Node::Project));
        folds.toggle(Fold::new(&b, Node::Knowledge));
        folds.forget(&a);
        assert!(folds.is_open(&Fold::new(&a, Node::Project)));
        assert!(folds.is_open(&Fold::new(&b, Node::Knowledge)));
    }

    #[test]
    fn folds_round_trip_through_json() {
        let mut projects = Projects::default();
        let project = projects.add(Path::new("/work/ukai"));
        let mut folds = Folds::default();
        folds.toggle(Fold::new(
            &project,
            Node::Reports {
                place: agent_place("cloud", pod("c")),
            },
        ));
        folds.toggle(Fold::new(&project, Node::Schedules));
        let json = serde_json::to_string(&folds).unwrap_or_default();
        let back: Result<Folds, _> = serde_json::from_str(&json);
        assert_eq!(back.ok(), Some(folds));
    }
}
