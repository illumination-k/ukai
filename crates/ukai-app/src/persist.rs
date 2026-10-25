//! Saving the layout and project registry as they change.

use std::path::PathBuf;
use std::time::Duration;

use dioxus::prelude::*;
use ukai_core::sidebar::Folds;
use ukai_core::state::{self, AppState};
use ukai_core::{Layout, Projects};

/// Coalesce bursts of changes (typing a split, resizing later) into one write.
const DEBOUNCE: Duration = Duration::from_millis(500);

/// Save `projects`, `layout`, and `folds` to `path` shortly after one changes.
pub fn use_autosave(
    path: Option<PathBuf>,
    projects: Signal<Projects>,
    layout: Signal<Layout>,
    folds: Signal<Folds>,
) {
    let mut generation = use_signal(|| 0_u64);
    use_effect(move || {
        // Reading them subscribes this effect to them.
        let snapshot = AppState::new(
            projects.read().clone(),
            layout.read().clone(),
            folds.read().clone(),
        );
        let Some(path) = path.clone() else {
            return;
        };
        let current = *generation.peek() + 1;
        generation.set(current);
        spawn(async move {
            tokio::time::sleep(DEBOUNCE).await;
            if *generation.peek() != current {
                return;
            }
            if let Err(error) = state::save(&path, &snapshot) {
                tracing::warn!(%error, "failed to save state");
            }
        });
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
