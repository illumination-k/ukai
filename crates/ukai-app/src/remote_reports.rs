//! Reports of the workspaces gateway agents work in.
//!
//! A gateway agent writes its reports in its pod, not here (see
//! [`ukai_acp::reports`]), so the app cannot scan a directory for them as it
//! does for a project. It asks the gateway instead: [`Follow`] keeps the
//! listing of every workspace an open agent pane uses, and the sidebar, the
//! palette, and a report pane read what it found rather than each polling on
//! their own.
//!
//! The unit is the *workspace*, not the pane: two panes sharing one
//! (`workspace = "shared"`) see one listing, and a report stays readable
//! after the pane that opened the agent is closed, as long as the pod is
//! still running.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use dioxus::prelude::*;
use ukai_acp::reports::{ReportsEndpoint, ReportsError};
use ukai_acp::{PaneWorkspace, WorkspaceScope};
use ukai_config::{AgentSpec, Config};
use ukai_core::{Layout, PaneContent, ProjectId, ReportSource};

use crate::projects::AppSignals;

/// How often the listings are read again while agent panes are open.
const RELIST: Duration = Duration::from_secs(3);

/// One workspace whose reports the app follows.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Workspace {
    pub project: ProjectId,
    pub agent: String,
    /// The pane's workspace; `None` when panes of this agent share one, so
    /// that they also share a listing.
    pub workspace: Option<PaneWorkspace>,
}

impl Workspace {
    /// The workspace a pane's agent works in, with a shared one normalized
    /// to `None`: the gateway ignores the key in that case, so keeping it
    /// would show the same reports once per pane.
    pub fn of(project: &ProjectId, agent: &str, workspace: Option<&PaneWorkspace>) -> Self {
        Self {
            project: project.clone(),
            agent: agent.to_owned(),
            workspace: workspace
                .filter(|workspace| workspace.scope == WorkspaceScope::Pod)
                .cloned(),
        }
    }

    /// What a report pane of this workspace records, so it finds the same
    /// workspace after a restart.
    pub fn source(&self) -> ReportSource {
        ReportSource::Agent {
            agent: self.agent.clone(),
            workspace: self.workspace.clone(),
        }
    }

    /// How the sidebar and the palette name it.
    pub fn label(&self) -> String {
        match &self.workspace {
            Some(_) => format!("{} · own pod", self.agent),
            None => self.agent.clone(),
        }
    }
}

/// The last listing of a workspace.
#[derive(Debug, Clone, PartialEq)]
pub enum Listing {
    /// Project-relative paths, newest first.
    Reports(Vec<PathBuf>),
    /// The pod is not running: the agent has to be opened again first.
    NotRunning,
    Failed(String),
}

/// Every workspace's last listing, kept by [`Follow`].
pub type Listings = Signal<HashMap<Workspace, Listing>>;

/// Where a workspace's reports are read from.
pub fn endpoint(
    configs: &HashMap<ProjectId, Result<Config, String>>,
    workspace: &Workspace,
) -> Result<ReportsEndpoint, String> {
    resolve(&declaration(configs, workspace)?, workspace)
}

/// How `workspace`'s agent is declared right now.
pub fn declaration(
    configs: &HashMap<ProjectId, Result<Config, String>>,
    workspace: &Workspace,
) -> Result<AgentSpec, String> {
    let config = configs
        .get(&workspace.project)
        .ok_or_else(|| "this project's configuration is not loaded".to_owned())?
        .as_ref()
        .map_err(Clone::clone)?;
    config
        .agents
        .get(&workspace.agent)
        .cloned()
        .ok_or_else(|| format!("agent `{}` is not declared anymore", workspace.agent))
}

fn resolve(spec: &AgentSpec, workspace: &Workspace) -> Result<ReportsEndpoint, String> {
    ReportsEndpoint::resolve(spec, ukai_acp::system_lookup, workspace.workspace.as_ref())
        .map_err(|error| error.to_string())?
        .ok_or_else(|| format!("agent `{}` serves no reports", workspace.agent))
}

/// The endpoints a poller such as [`Follow`] reads through, kept between
/// rounds so a listing reuses its connection instead of building a client —
/// and its TLS configuration — every few seconds. An agent whose
/// declaration changed, because the project was reloaded, is resolved again
/// with `resolve`.
pub struct Endpoints<E> {
    known: HashMap<Workspace, (AgentSpec, Result<E, String>)>,
    resolve: fn(&AgentSpec, &Workspace) -> Result<E, String>,
}

impl<E: Clone> Endpoints<E> {
    pub fn new(resolve: fn(&AgentSpec, &Workspace) -> Result<E, String>) -> Self {
        Self {
            known: HashMap::new(),
            resolve,
        }
    }

    pub fn get(
        &mut self,
        configs: &HashMap<ProjectId, Result<Config, String>>,
        workspace: &Workspace,
    ) -> Result<E, String> {
        let spec = declaration(configs, workspace)?;
        let known = self
            .known
            .get(workspace)
            .is_some_and(|(declared, _)| *declared == spec);
        if !known {
            let endpoint = (self.resolve)(&spec, workspace);
            self.known.insert(workspace.clone(), (spec, endpoint));
        }
        match self.known.get(workspace) {
            Some((_, endpoint)) => endpoint.clone(),
            None => Err("this agent's workspace cannot be reached".to_owned()),
        }
    }

    pub fn keep(&mut self, wanted: &[Workspace]) {
        self.known.retain(|workspace, _| wanted.contains(workspace));
    }
}

/// The workspaces of the gateway agent panes that are open, in pane order
/// and without repeats.
pub fn workspaces(
    layout: &Layout,
    configs: &HashMap<ProjectId, Result<Config, String>>,
) -> Vec<Workspace> {
    let mut found: Vec<Workspace> = Vec::new();
    for (_, content) in layout.contents() {
        let PaneContent::Agent {
            project,
            agent,
            workspace,
            ..
        } = content
        else {
            continue;
        };
        let gateway = matches!(
            configs.get(project).and_then(|config| config.as_ref().ok()),
            Some(config) if matches!(config.agents.get(agent), Some(AgentSpec::Gateway { .. }))
        );
        if !gateway {
            continue;
        }
        let workspace = Workspace::of(project, agent, workspace.as_ref());
        if !found.contains(&workspace) {
            found.push(workspace);
        }
    }
    found
}

/// Reads the listings of the open agent panes' workspaces, over and over.
/// Rendered once by the app; it draws nothing.
#[component]
pub fn Follow() -> Element {
    let app = use_context::<AppSignals>();
    let mut listings = app.reports;
    use_future(move || async move {
        let mut endpoints = Endpoints::new(resolve);
        loop {
            let wanted = {
                let layout = app.layout.peek();
                let configs = app.configs.peek();
                workspaces(&layout, &configs)
            };
            for workspace in &wanted {
                let endpoint = endpoints.get(&app.configs.peek(), workspace);
                let listing = match endpoint {
                    Ok(endpoint) => read(&endpoint).await,
                    Err(problem) => Listing::Failed(problem),
                };
                let changed = listings.peek().get(workspace) != Some(&listing);
                if changed {
                    listings.write().insert(workspace.clone(), listing);
                }
            }
            // Drop what no open pane works in anymore.
            let stale = listings.peek().keys().any(|known| !wanted.contains(known));
            if stale {
                listings.write().retain(|known, _| wanted.contains(known));
            }
            endpoints.keep(&wanted);
            tokio::time::sleep(RELIST).await;
        }
    });
    rsx! {}
}

async fn read(endpoint: &ReportsEndpoint) -> Listing {
    match endpoint.list().await {
        Ok(entries) => {
            Listing::Reports(entries.into_iter().map(|e| PathBuf::from(e.path)).collect())
        }
        Err(ReportsError::Gone) => Listing::NotRunning,
        Err(error) => Listing::Failed(error.to_string()),
    }
}

/// The workspaces of `project` with what each last listed, in the order
/// [`workspaces`] found them.
pub fn of_project(app: AppSignals, project: &ProjectId) -> Vec<(Workspace, Listing)> {
    let wanted = {
        let layout = app.layout.read();
        let configs = app.configs.read();
        workspaces(&layout, &configs)
    };
    let listings = app.reports.read();
    wanted
        .into_iter()
        .filter(|workspace| workspace.project == *project)
        .map(|workspace| {
            let listing = listings
                .get(&workspace)
                .cloned()
                .unwrap_or(Listing::Reports(Vec::new()));
            (workspace, listing)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use ukai_core::{Direction, Projects};

    use super::*;

    fn config(agents: &[(&str, AgentSpec)]) -> Result<Config, String> {
        Ok(Config {
            agents: agents
                .iter()
                .map(|(id, spec)| ((*id).to_owned(), spec.clone()))
                .collect(),
            ..Config::default()
        })
    }

    fn gateway() -> AgentSpec {
        AgentSpec::Gateway {
            url: "ws://gw/agents/claude/acp".into(),
            headers: BTreeMap::new(),
            workspace: WorkspaceScope::Shared,
        }
    }

    fn stdio() -> AgentSpec {
        AgentSpec::Stdio {
            command: "agent".into(),
            args: Vec::new(),
            env: BTreeMap::new(),
        }
    }

    fn agent_pane(
        layout: &mut Layout,
        project: &ProjectId,
        agent: &str,
        workspace: Option<PaneWorkspace>,
    ) {
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
        layout.split(pane, Direction::Horizontal);
    }

    #[test]
    fn follows_one_workspace_per_gateway_agent_and_pod() {
        let mut projects = Projects::default();
        let project = projects.add(std::path::Path::new("/work/ukai"));
        let mut layout = Layout::default();
        let own = |key: &str| {
            Some(PaneWorkspace {
                key: key.to_owned(),
                scope: WorkspaceScope::Pod,
            })
        };
        let shared = |key: &str| {
            Some(PaneWorkspace {
                key: key.to_owned(),
                scope: WorkspaceScope::Shared,
            })
        };
        // Two panes sharing a workspace, two with pods of their own, and a
        // local agent that has no gateway to ask.
        agent_pane(&mut layout, &project, "claude", shared("a"));
        agent_pane(&mut layout, &project, "claude", shared("b"));
        agent_pane(&mut layout, &project, "claude", own("c"));
        agent_pane(&mut layout, &project, "claude", own("d"));
        agent_pane(&mut layout, &project, "local", None);
        let configs = HashMap::from([(
            project.clone(),
            config(&[("claude", gateway()), ("local", stdio())]),
        )]);

        let found = workspaces(&layout, &configs);
        assert_eq!(found.len(), 3, "{found:?}");
        assert_eq!(found[0], Workspace::of(&project, "claude", None));
        assert_eq!(
            found[1..],
            [
                Workspace::of(&project, "claude", own("c").as_ref()),
                Workspace::of(&project, "claude", own("d").as_ref()),
            ]
        );
        assert_eq!(found[0].label(), "claude");
        assert_eq!(found[1].label(), "claude · own pod");
        assert_eq!(
            found[1].source(),
            ReportSource::Agent {
                agent: "claude".into(),
                workspace: own("c")
            }
        );
    }

    #[test]
    fn follows_nothing_without_a_gateway_agent() {
        let mut projects = Projects::default();
        let project = projects.add(std::path::Path::new("/work/ukai"));
        let mut layout = Layout::default();
        agent_pane(&mut layout, &project, "local", None);
        let configs = HashMap::from([(project, config(&[("local", stdio())]))]);
        assert!(workspaces(&layout, &configs).is_empty());
    }

    #[test]
    fn resolves_the_endpoint_of_a_gateway_workspace() {
        let mut projects = Projects::default();
        let project = projects.add(std::path::Path::new("/work/ukai"));
        let configs = HashMap::from([(
            project.clone(),
            config(&[("claude", gateway()), ("local", stdio())]),
        )]);
        let workspace = Workspace::of(&project, "claude", None);
        assert_eq!(
            endpoint(&configs, &workspace)
                .map(|endpoint| endpoint.base().to_owned())
                .as_deref(),
            Ok("http://gw/agents/claude/reports")
        );
        let local = Workspace::of(&project, "local", None);
        assert!(endpoint(&configs, &local).is_err());
        let unknown = Workspace::of(&project, "gone", None);
        assert!(endpoint(&configs, &unknown).is_err());
    }

    #[test]
    fn keeps_an_endpoint_until_the_agent_is_declared_differently() {
        let mut projects = Projects::default();
        let project = projects.add(std::path::Path::new("/work/ukai"));
        let workspace = Workspace::of(&project, "claude", None);
        let mut configs = HashMap::from([(project.clone(), config(&[("claude", gateway())]))]);
        let mut endpoints = Endpoints::new(resolve);
        let first = endpoints.get(&configs, &workspace);
        assert_eq!(
            first.as_ref().map(ReportsEndpoint::base).ok(),
            Some("http://gw/agents/claude/reports")
        );
        // The same declaration keeps the same endpoint, connection and all.
        assert_eq!(endpoints.get(&configs, &workspace).ok(), first.clone().ok());

        let moved = AgentSpec::Gateway {
            url: "ws://elsewhere/agents/claude/acp".into(),
            headers: BTreeMap::new(),
            workspace: WorkspaceScope::Shared,
        };
        configs.insert(project, config(&[("claude", moved)]));
        assert_eq!(
            endpoints
                .get(&configs, &workspace)
                .as_ref()
                .map(ReportsEndpoint::base)
                .ok(),
            Some("http://elsewhere/agents/claude/reports")
        );

        // Nothing is kept for a workspace no pane works in.
        endpoints.keep(&[]);
        assert!(endpoints.known.is_empty());
    }
}
