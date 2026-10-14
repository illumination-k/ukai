//! Projects the user has opened: a root directory under an opaque id.
//!
//! The id, not the path, is what panes refer to, so a project can later gain
//! worktrees or a remote location without rewriting every pane.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProjectId(pub(crate) String);

impl ProjectId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ProjectId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Project {
    pub id: ProjectId,
    pub root: PathBuf,
}

impl Project {
    /// The root directory's name.
    pub fn name(&self) -> String {
        self.root.file_name().map_or_else(
            || self.root.display().to_string(),
            |name| name.to_string_lossy().into_owned(),
        )
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Projects {
    list: Vec<Project>,
    selected: Option<ProjectId>,
    /// Ids are never reused, so a stale reference cannot hit a new project.
    next: u64,
}

impl Projects {
    pub fn iter(&self) -> impl Iterator<Item = &Project> {
        self.list.iter()
    }

    pub fn get(&self, id: &ProjectId) -> Option<&Project> {
        self.list.iter().find(|project| &project.id == id)
    }

    /// The project new panes open in by default.
    pub fn selected(&self) -> Option<&Project> {
        self.selected.as_ref().and_then(|id| self.get(id))
    }

    pub fn select(&mut self, id: &ProjectId) {
        if self.get(id).is_some() {
            self.selected = Some(id.clone());
        }
    }

    /// Register `root` (callers pass a canonical path) and select it. An
    /// already registered root keeps its id.
    pub fn add(&mut self, root: &Path) -> ProjectId {
        let id = match self.list.iter().find(|project| project.root == root) {
            Some(project) => project.id.clone(),
            None => {
                self.next += 1;
                let id = ProjectId(format!("p{}", self.next));
                self.list.push(Project {
                    id: id.clone(),
                    root: root.to_owned(),
                });
                id
            }
        };
        self.selected = Some(id.clone());
        id
    }

    pub fn remove(&mut self, id: &ProjectId) -> Option<Project> {
        let index = self.list.iter().position(|project| &project.id == id)?;
        let removed = self.list.remove(index);
        if self.selected.as_ref() == Some(id) {
            self.selected = self.list.first().map(|project| project.id.clone());
        }
        Some(removed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_dedupes_by_root_and_selects() {
        let mut projects = Projects::default();
        let a = projects.add(Path::new("/a"));
        let b = projects.add(Path::new("/b"));
        assert_ne!(a, b);
        assert_eq!(projects.selected().map(|p| &p.id), Some(&b));
        assert_eq!(projects.add(Path::new("/a")), a);
        assert_eq!(projects.iter().count(), 2);
        assert_eq!(projects.selected().map(|p| &p.id), Some(&a));
    }

    #[test]
    fn ids_are_not_reused_after_removal() {
        let mut projects = Projects::default();
        let a = projects.add(Path::new("/a"));
        projects.remove(&a);
        assert_eq!(projects.selected(), None);
        assert_ne!(projects.add(Path::new("/a")), a);
    }

    #[test]
    fn removing_the_selected_project_selects_another() {
        let mut projects = Projects::default();
        let a = projects.add(Path::new("/a"));
        let b = projects.add(Path::new("/b"));
        projects.remove(&b);
        assert_eq!(projects.selected().map(|p| &p.id), Some(&a));
    }
}
