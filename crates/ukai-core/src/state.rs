//! What survives a restart: registered projects and the tab/pane layout,
//! including each agent pane's ACP session id so it can be continued.
//!
//! One versioned JSON document, written atomically. Transcripts are not
//! stored: agents that support `session/load` replay them.

use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::layout::{Layout, PaneContent};
use crate::project::Projects;

/// Bump on incompatible changes; older files are migrated on load, newer
/// ones are refused rather than misread.
pub const STATE_VERSION: u32 = 1;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AppState {
    pub version: u32,
    pub projects: Projects,
    pub layout: Layout,
}

#[derive(Debug, thiserror::Error)]
pub enum StateError {
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("{path}: not a valid state file: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error(
        "{path}: written by a newer ukai (state version {found}, this build reads {STATE_VERSION})"
    )]
    Newer { path: PathBuf, found: u32 },
    #[error("{path}: inconsistent layout: {reason}")]
    Inconsistent { path: PathBuf, reason: String },
}

impl AppState {
    pub fn new(projects: Projects, layout: Layout) -> Self {
        Self {
            version: STATE_VERSION,
            projects,
            layout,
        }
    }

    /// Empty every pane that refers to a project no longer registered.
    pub fn drop_orphans(&mut self) {
        let orphans: Vec<_> = self
            .layout
            .contents()
            .filter(|(_, content)| {
                content
                    .project()
                    .is_some_and(|project| self.projects.get(project).is_none())
            })
            .map(|(pane, _)| pane)
            .collect();
        for pane in orphans {
            self.layout.set_content(pane, PaneContent::Empty);
        }
    }
}

/// Read the state at `path`; `Ok(None)` when there is none yet.
pub fn load(path: &Path) -> Result<Option<AppState>, StateError> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(StateError::Io {
                path: path.to_owned(),
                source,
            });
        }
    };
    #[derive(Deserialize)]
    struct Version {
        version: u32,
    }
    let parse_error = |source| StateError::Parse {
        path: path.to_owned(),
        source,
    };
    let Version { version } = serde_json::from_slice(&bytes).map_err(parse_error)?;
    if version > STATE_VERSION {
        return Err(StateError::Newer {
            path: path.to_owned(),
            found: version,
        });
    }
    let mut state: AppState = serde_json::from_slice(&bytes).map_err(parse_error)?;
    state
        .layout
        .check()
        .map_err(|reason| StateError::Inconsistent {
            path: path.to_owned(),
            reason,
        })?;
    state.version = STATE_VERSION;
    state.drop_orphans();
    Ok(Some(state))
}

/// Move an unreadable state file aside so it is kept for inspection but no
/// longer blocks startup. Returns where it went.
pub fn quarantine(path: &Path) -> io::Result<PathBuf> {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".broken-{stamp}"));
    let target = path.with_file_name(name);
    std::fs::rename(path, &target)?;
    Ok(target)
}

/// Write `state` to `path` atomically: a temporary file in the same
/// directory is synced and renamed over the target, so a crash leaves either
/// the old or the new document, never a torn one.
pub fn save(path: &Path, state: &AppState) -> Result<(), StateError> {
    let io_error = |source| StateError::Io {
        path: path.to_owned(),
        source,
    };
    let json = serde_json::to_vec_pretty(state).map_err(|source| StateError::Parse {
        path: path.to_owned(),
        source,
    })?;
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir).map_err(io_error)?;
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".tmp");
    let tmp = path.with_file_name(name);
    let mut file = std::fs::File::create(&tmp).map_err(io_error)?;
    file.write_all(&json).map_err(io_error)?;
    file.sync_all().map_err(io_error)?;
    drop(file);
    std::fs::rename(&tmp, path).map_err(io_error)?;
    // Persist the rename itself; not every platform can open a directory.
    #[cfg(unix)]
    std::fs::File::open(dir)
        .and_then(|d| d.sync_all())
        .map_err(io_error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use rstest::{fixture, rstest};
    use tempfile::TempDir;

    use super::*;
    use crate::Direction;

    #[fixture]
    fn dir() -> TempDir {
        tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"))
    }

    fn sample() -> AppState {
        let mut projects = Projects::default();
        let project = projects.add(Path::new("/work/ukai"));
        let mut layout = Layout::default();
        let pane = layout.focused();
        layout.set_content(
            pane,
            PaneContent::Agent {
                project,
                agent: "claude".into(),
                session: None,
            },
        );
        layout.record_session(pane, "sess-1".into());
        layout.split(pane, Direction::Horizontal);
        AppState::new(projects, layout)
    }

    #[rstest]
    fn round_trips(dir: TempDir) {
        let path = dir.path().join("state.json");
        let state = sample();
        assert!(save(&path, &state).is_ok());
        assert_eq!(load(&path).ok().flatten(), Some(state));
        assert!(!dir.path().join("state.json.tmp").exists());
    }

    #[rstest]
    fn missing_file_is_no_state(dir: TempDir) {
        assert!(matches!(load(&dir.path().join("state.json")), Ok(None)));
    }

    #[rstest]
    #[case::garbage("{not json")]
    #[case::newer(r#"{"version": 999}"#)]
    fn refuses_unreadable_files(dir: TempDir, #[case] content: &str) {
        let path = dir.path().join("state.json");
        std::fs::write(&path, content).unwrap_or_else(|e| panic!("{e}"));
        assert!(load(&path).is_err());
        let moved = quarantine(&path).unwrap_or_else(|e| panic!("{e}"));
        assert!(!path.exists());
        assert_eq!(
            std::fs::read_to_string(moved).ok().as_deref(),
            Some(content)
        );
    }

    #[rstest]
    fn refuses_inconsistent_layouts(dir: TempDir) {
        let path = dir.path().join("state.json");
        let mut json = serde_json::to_value(sample()).unwrap_or_else(|e| panic!("{e}"));
        json["layout"]["focused"] = 42.into();
        std::fs::write(&path, json.to_string()).unwrap_or_else(|e| panic!("{e}"));
        assert!(matches!(load(&path), Err(StateError::Inconsistent { .. })));
    }

    #[test]
    fn panes_of_removed_projects_are_emptied() {
        let mut state = sample();
        let project = state
            .projects
            .iter()
            .next()
            .map(|p| p.id.clone())
            .unwrap_or_else(|| panic!("sample has a project"));
        state.projects.remove(&project);
        state.drop_orphans();
        assert!(
            state
                .layout
                .contents()
                .all(|(_, content)| *content == PaneContent::Empty)
        );
    }
}
