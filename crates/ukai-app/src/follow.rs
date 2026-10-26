//! What the app's pollers share. Each follows gateway agents on its own
//! (reports, ports, schedules, knowledge), reading through [`Endpoints`] it
//! keeps between rounds, at the pace [`poll`] sets or its own.

use std::cell::RefCell;
use std::collections::HashMap;
use std::time::Duration;

use dioxus::prelude::*;
use tokio::time::Instant;
use ukai_acp::{PaneWorkspace, WorkspaceScope};
use ukai_config::{AgentSpec, Config};
use ukai_core::sidebar::{self, Fold, Folds, Node};
use ukai_core::{Layout, PaneContent, ProjectId, ReportSource};

use crate::projects::AppSignals;

/// How soon a poller notices that an edit asked for a fresh read.
const NOTICE: Duration = Duration::from_millis(300);

/// One gateway workspace the app follows.
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

    /// The gateway workspace `place` names in `project`; `None` for this
    /// machine's checkout.
    pub fn at(project: &ProjectId, place: &ReportSource) -> Option<Self> {
        match place {
            ReportSource::Project => None,
            ReportSource::Agent { agent, workspace } => Some(Self {
                project: project.clone(),
                agent: agent.clone(),
                workspace: workspace.clone(),
            }),
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
        .agent(&workspace.agent)
        .ok_or_else(|| format!("agent `{}` is not declared anymore", workspace.agent))
}

/// The endpoint of `workspace`'s agent as it is declared right now, through
/// `resolve`.
pub fn endpoint<E>(
    configs: &HashMap<ProjectId, Result<Config, String>>,
    workspace: &Workspace,
    resolve: fn(&AgentSpec, &Workspace) -> Result<E, String>,
) -> Result<E, String> {
    resolve(&declaration(configs, workspace)?, workspace)
}

/// The endpoints a poller reads through, kept between
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
            Some(config) if matches!(config.agent(agent), Some(AgentSpec::Gateway { .. }))
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

/// What the pollers read: the workspaces of the open agent panes, then the
/// others the sidebar shows under the projects it has open (a shared
/// workspace is there with no pane in it, and keeps its reports).
pub fn followed(
    layout: &Layout,
    configs: &HashMap<ProjectId, Result<Config, String>>,
    folds: &Folds,
) -> Vec<Workspace> {
    let mut found = workspaces(layout, configs);
    let mut projects: Vec<&ProjectId> = configs.keys().collect();
    projects.sort();
    for project in projects {
        if !folds.is_open(&Fold::new(project, Node::Project)) {
            continue;
        }
        let config = configs.get(project).and_then(|config| config.as_ref().ok());
        for shown in sidebar::tree(layout, project, config).workspaces {
            if let Some(workspace) = Workspace::at(project, &shown.place)
                && !found.contains(&workspace)
            {
                found.push(workspace);
            }
        }
    }
    found
}

/// The gateway agents of `config`, by name.
fn gateway_agents(config: &Config) -> Vec<String> {
    config
        .agents
        .iter()
        .filter(|(_, spec)| matches!(spec, AgentSpec::Gateway { .. }))
        .map(|(name, _)| name.clone())
        .collect()
}

/// Every project with a gateway agent: its id, name, and gateway agents.
fn gateways(app: AppSignals) -> Vec<(ProjectId, String, Vec<String>)> {
    let projects = app.projects.peek();
    let configs = app.configs.peek();
    projects
        .iter()
        .filter_map(|project| {
            let config = configs.get(&project.id)?.as_ref().ok()?;
            let agents = gateway_agents(config);
            (!agents.is_empty()).then(|| (project.id.clone(), project.name(), agents))
        })
        .collect()
}

/// Calls `read` now, every `every`, and soon after `revision` is bumped,
/// as an edit does when it wants a fresh read.
pub async fn poll<F: Future<Output = ()>>(
    revision: Signal<u64>,
    every: Duration,
    mut read: impl FnMut() -> F,
) {
    let mut last = None;
    let mut next = Instant::now();
    loop {
        let current = *revision.peek();
        if last != Some(current) || Instant::now() >= next {
            last = Some(current);
            next = Instant::now() + every;
            read().await;
        }
        tokio::time::sleep(NOTICE).await;
    }
}

/// Reads what `read` makes of each workspace in `wanted` into `listings`,
/// and drops what nothing follows anymore.
pub async fn relist<E: Clone, T: PartialEq + 'static>(
    app: AppSignals,
    mut listings: Signal<HashMap<Workspace, T>>,
    endpoints: &mut Endpoints<E>,
    wanted: &[Workspace],
    mut read: impl AsyncFnMut(&Workspace, Result<E, String>) -> T,
) {
    for workspace in wanted {
        // Bound first, so the borrow of the configurations ends before the
        // request is awaited.
        let endpoint = endpoints.get(&app.configs.peek(), workspace);
        let listing = read(workspace, endpoint).await;
        let changed = listings.peek().get(workspace) != Some(&listing);
        if changed {
            listings.write().insert(workspace.clone(), listing);
        }
    }
    let stale = listings.peek().keys().any(|known| !wanted.contains(known));
    if stale {
        listings.write().retain(|known, _| wanted.contains(known));
    }
    endpoints.keep(wanted);
}

/// What `list` finds for each gateway agent of every project, with the
/// project's id and name, through `endpoints`. Agents keep one shared
/// workspace for this.
pub async fn list_gateway_agents<E: Clone, T>(
    app: AppSignals,
    endpoints: &RefCell<Endpoints<E>>,
    list: impl AsyncFn(E) -> Result<T, String>,
) -> Vec<(ProjectId, String, Vec<(String, Result<T, String>)>)> {
    let mut fetched = Vec::new();
    let mut asked = Vec::new();
    for (project, name, agents) in gateways(app) {
        let mut listed = Vec::new();
        for agent in agents {
            let workspace = Workspace::of(&project, &agent, None);
            let endpoint = endpoints.borrow_mut().get(&app.configs.peek(), &workspace);
            let found = match endpoint {
                Ok(endpoint) => list(endpoint).await,
                Err(error) => Err(error),
            };
            listed.push((agent, found));
            asked.push(workspace);
        }
        fetched.push((project, name, listed));
    }
    endpoints.borrow_mut().keep(&asked);
    fetched
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use ukai_acp::ReportsEndpoint;
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
    fn follows_the_shared_workspaces_of_open_projects_too() {
        let mut projects = Projects::default();
        let open = projects.add(std::path::Path::new("/work/ukai"));
        let folded = projects.add(std::path::Path::new("/work/other"));
        let mut layout = Layout::default();
        let own = Some(PaneWorkspace {
            key: "c".to_owned(),
            scope: WorkspaceScope::Pod,
        });
        agent_pane(&mut layout, &folded, "claude", own.clone());
        let configs = HashMap::from([
            (open.clone(), config(&[("claude", gateway())])),
            (folded.clone(), config(&[("claude", gateway())])),
        ]);
        let mut folds = Folds::default();
        folds.toggle(Fold::new(&folded, Node::Project));

        // A folded project's shared workspace is not looked at, but a pane's
        // workspace is, wherever it is.
        assert_eq!(
            followed(&layout, &configs, &folds),
            [
                Workspace::of(&folded, "claude", own.as_ref()),
                Workspace::of(&open, "claude", None),
            ]
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
            endpoint(&configs, &workspace, crate::remote_reports::resolve)
                .map(|endpoint| endpoint.base().to_owned())
                .as_deref(),
            Ok("http://gw/agents/claude/reports")
        );
        let local = Workspace::of(&project, "local", None);
        assert!(endpoint(&configs, &local, crate::remote_reports::resolve).is_err());
        let unknown = Workspace::of(&project, "gone", None);
        assert!(endpoint(&configs, &unknown, crate::remote_reports::resolve).is_err());
    }

    #[test]
    fn keeps_an_endpoint_until_the_agent_is_declared_differently() {
        let mut projects = Projects::default();
        let project = projects.add(std::path::Path::new("/work/ukai"));
        let workspace = Workspace::of(&project, "claude", None);
        let mut configs = HashMap::from([(project.clone(), config(&[("claude", gateway())]))]);
        let mut endpoints = Endpoints::new(crate::remote_reports::resolve);
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
