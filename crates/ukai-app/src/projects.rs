//! Projects as the app sees them: the registry from `ukai-core` plus each
//! project's loaded configuration (user `config.toml` layered with its
//! `ukai.toml`).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use dioxus::prelude::*;
use ukai_config::Config;
use ukai_core::sidebar::Folds;
use ukai_core::{Layout, PaneId, ProjectId, Projects};

use crate::agent_pane::Sessions;
use crate::knowledge::KnowledgeListings;
use crate::layout::Drag;
use crate::ports::PortListings;
use crate::remote_reports::Listings;
use crate::schedules::ScheduleListings;

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
    /// What each gateway workspace last listed, kept by
    /// [`crate::remote_reports::Follow`].
    pub reports: Listings,
    /// What each gateway workspace last listed of its ports, kept by
    /// [`crate::ports::FollowPorts`].
    pub ports: PortListings,
    /// What each project's gateway agents last listed of their schedules,
    /// kept by [`crate::schedules::FollowSchedules`].
    pub schedules: ScheduleListings,
    /// What each project's gateway agents last listed of their notes, kept
    /// by [`crate::knowledge::FollowKnowledge`].
    pub knowledge: KnowledgeListings,
    /// What of the sidebar's tree is folded.
    pub folds: Signal<Folds>,
    /// Whether the command palette is open.
    pub palette: Signal<bool>,
    /// A pane boundary or pane the user is dragging.
    pub drag: Signal<Option<Drag>>,
    /// The names of the gateway schedules the app has listed, for pane titles.
    pub schedule_names: Signal<HashMap<String, String>>,
    /// Bumped after a schedule changes, so its listings read it again.
    pub schedule_revision: Signal<u64>,
    /// Bumped after a note changes, so the knowledge listings read it again.
    pub knowledge_revision: Signal<u64>,
    /// The note panes with a draft, which opening another note leaves alone.
    pub editing_notes: Signal<HashSet<PaneId>>,
}

impl AppSignals {
    /// Whether something of the DOM is drawn over the panes, so the native
    /// views of reports and browser panes hide.
    pub fn covered(&self) -> bool {
        (self.palette)() || self.drag.read().as_ref().is_some_and(Drag::covers)
    }
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
        mut folds,
        ..
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
    folds.write().forget(id);
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
