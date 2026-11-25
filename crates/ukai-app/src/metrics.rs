//! Where each session's turn metrics go: `metrics/` under the state
//! directory, one JSON Lines file per session (`ukai_core::metrics`).

use std::path::PathBuf;
use std::sync::OnceLock;

use ukai_core::metrics::{self, TurnRecord};

static DIR: OnceLock<Option<PathBuf>> = OnceLock::new();

/// Set once at startup; with `None`, nothing is recorded.
pub fn init(dir: Option<PathBuf>) {
    let _ = DIR.set(dir);
}

/// Append `record` to its session's file, off the UI thread.
pub fn record(record: TurnRecord) {
    let Some(dir) = DIR.get().cloned().flatten() else {
        return;
    };
    std::thread::spawn(move || {
        if let Err(error) = metrics::append(&dir, &record) {
            tracing::warn!(%error, dir = %dir.display(), "cannot record the turn's metrics");
        }
    });
}
