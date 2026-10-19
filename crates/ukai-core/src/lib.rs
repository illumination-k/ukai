//! Application state that does not depend on the UI framework, so it can be
//! tested headless. `ukai-app` wraps these types in signals and renders them.

pub mod attachment;
pub mod browser;
pub mod config;
pub mod keymap;
pub mod knowledge;
mod layout;
pub mod markdown;
pub mod palette;
mod project;
pub mod report;
pub mod review;
pub mod schedule;
pub mod slash;
pub mod state;
mod transcript;

pub use layout::{Direction, Layout, Node, PaneContent, PaneId, ReportSource, Tab};
pub use project::{Project, ProjectId, Projects};
pub use transcript::{
    Alert, Entry, History, Queued, Status, Transcript, session_label, updated_label, usage_label,
};
