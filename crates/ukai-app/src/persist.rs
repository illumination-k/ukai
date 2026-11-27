//! Saving the layout and project registry as they change.

use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use dioxus::prelude::*;
use tokio::sync::Notify;
use ukai_core::branch::SessionBranches;
use ukai_core::sidebar::{Folds, Hidden};
use ukai_core::state::{self, AppState};
use ukai_core::{Layout, Projects};

/// Coalesce bursts of changes (typing a split, resizing later) into one write.
const DEBOUNCE: Duration = Duration::from_millis(500);

/// Save `projects`, `layout`, `folds`, `sidebar_hidden`, and `branches` to `path` shortly
/// after one changes.
pub fn use_autosave(
    path: Option<PathBuf>,
    projects: Signal<Projects>,
    layout: Signal<Layout>,
    folds: Signal<Folds>,
    sidebar_hidden: Signal<Hidden>,
    branches: Signal<SessionBranches>,
) {
    let changed = use_hook(|| Rc::new(Notify::new()));
    use_effect({
        let changed = changed.clone();
        move || {
            // Reading them subscribes this effect to them. Nothing is cloned
            // here: dragging a boundary changes the layout with every move.
            projects.read();
            layout.read();
            folds.read();
            sidebar_hidden.read();
            branches.read();
            changed.notify_one();
        }
    });
    // One task writes, so a slow write is never overtaken by the next.
    use_future(move || {
        let changed = changed.clone();
        let path = path.clone();
        async move {
            let Some(path) = path else {
                return;
            };
            loop {
                changed.notified().await;
                while tokio::time::timeout(DEBOUNCE, changed.notified())
                    .await
                    .is_ok()
                {}
                let snapshot = AppState::new(
                    projects.peek().clone(),
                    layout.peek().clone(),
                    folds.peek().clone(),
                    *sidebar_hidden.peek(),
                    branches.peek().clone(),
                );
                // Off the UI thread: saving syncs the file to disk.
                let to = path.clone();
                match tokio::task::spawn_blocking(move || state::save(&to, &snapshot)).await {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => tracing::warn!(%error, "failed to save state"),
                    Err(error) => tracing::warn!(%error, "failed to save state"),
                }
            }
        }
    });
}

/// Load the saved state, moving an unreadable file aside rather than
/// failing startup.
pub fn load(path: Option<&std::path::Path>) -> AppState {
    let Some(path) = path else {
        return AppState::default();
    };
    match state::load(path) {
        Ok(Some(state)) => state,
        Ok(None) => AppState::default(),
        Err(error) => {
            tracing::warn!(%error, "ignoring saved state");
            match state::quarantine(path) {
                Ok(moved) => {
                    tracing::warn!(path = %moved.display(), "moved unreadable state aside")
                }
                Err(error) => tracing::warn!(%error, "could not move unreadable state aside"),
            }
            AppState::default()
        }
    }
}
