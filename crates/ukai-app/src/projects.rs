//! Projects as the app sees them: the registry from `ukai-core` plus each
//! project's loaded configuration (user `config.toml` layered with its
//! `ukai.toml`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use dioxus::prelude::*;
use ukai_config::Config;
use ukai_core::{Layout, PaneContent, ProjectId, Projects};

use crate::agent_pane::Sessions;

/// Locations resolved at startup.
#[derive(Debug, Clone, PartialEq)]
pub struct Paths {
    /// `~/.config/ukai/config.toml`, shared by every project.
    pub user_config: Option<PathBuf>,
    /// `~/.local/state/ukai/state.json`.
    pub state_file: Option<PathBuf>,
}

/// Each project's configuration, or the error that kept it from loading.
pub type Configs = Signal<HashMap<ProjectId, Result<Config, String>>>;

/// The app-wide state, provided as one context so helpers take a single,
/// copyable argument.
#[derive(Clone, Copy, PartialEq)]
pub struct AppSignals {
    pub projects: Signal<Projects>,
    pub configs: Configs,
    pub layout: Signal<Layout>,
    pub sessions: Sessions,
}

/// What a session needs from its project.
pub struct ProjectContext {
    pub root: PathBuf,
    pub config: Config,
}

pub fn load_config(paths: &Paths, root: &Path) -> Result<Config, String> {
    ukai_config::load_layered(paths.user_config.as_deref(), root).map_err(|e| e.to_string())
}

pub fn project_context(
    projects: &Projects,
    configs: &HashMap<ProjectId, Result<Config, String>>,
    id: &ProjectId,
) -> Result<ProjectContext, String> {
    let project = projects
        .get(id)
        .ok_or_else(|| format!("project {id} is no longer open"))?;
    let config = configs
        .get(id)
        .cloned()
        .unwrap_or_else(|| Err(format!("configuration of {} is not loaded", project.name())))?;
    Ok(ProjectContext {
        root: project.root.clone(),
        config,
    })
}

/// Register `root` as a project (or select it if already registered) and
/// load its configuration.
pub fn add_project(paths: &Paths, app: AppSignals, root: &Path) {
    let AppSignals {
        mut projects,
        mut configs,
        ..
    } = app;
    let root = root.canonicalize().unwrap_or_else(|_| root.to_owned());
    let id = projects.write().add(&root);
    configs.write().insert(id, load_config(paths, &root));
}

/// Re-read a project's configuration files.
pub fn reload_project(paths: &Paths, app: AppSignals, id: &ProjectId) {
    let AppSignals {
        projects,
        mut configs,
        ..
    } = app;
    let Some(root) = projects.peek().get(id).map(|p| p.root.clone()) else {
        return;
    };
    configs
        .write()
        .insert(id.clone(), load_config(paths, &root));
}

/// Forget a project, closing its panes and ending their sessions.
pub fn remove_project(app: AppSignals, id: &ProjectId) {
    let AppSignals {
        mut projects,
        mut configs,
        mut layout,
        mut sessions,
    } = app;
    let panes: Vec<_> = layout
        .peek()
        .contents()
        .filter(|(_, content)| content.project() == Some(id))
        .map(|(pane, _)| pane)
        .collect();
    {
        let mut layout = layout.write();
        let mut sessions = sessions.write();
        for pane in panes {
            layout.close_pane(pane);
            sessions.remove(&pane);
        }
    }
    projects.write().remove(id);
    configs.write().remove(id);
}

/// Ask for a folder with the native dialog and open it as a project.
pub fn pick_and_add_project(paths: Paths, app: AppSignals) {
    spawn(async move {
        if let Some(folder) = rfd::AsyncFileDialog::new()
            .set_title("Open project")
            .pick_folder()
            .await
        {
            add_project(&paths, app, folder.path());
        }
    });
}

/// Number of agent panes open per project.
pub fn agent_pane_counts(layout: &Layout) -> HashMap<ProjectId, usize> {
    let mut counts = HashMap::new();
    for (_, content) in layout.contents() {
        if let PaneContent::Agent { project, .. } = content {
            *counts.entry(project.clone()).or_default() += 1;
        }
    }
    counts
}
