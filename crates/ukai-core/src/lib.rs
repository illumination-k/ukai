//! Application state that does not depend on the UI framework, so it can be
//! tested headless. `ukai-app` wraps these types in signals and renders them.

pub mod annotation;
pub mod attachment;
pub mod browser;
pub mod config;
pub mod diff;
pub mod keymap;
pub mod knowledge;
pub mod launch;
mod layout;
pub mod markdown;
pub mod mention;
pub mod palette;
mod project;
pub mod recall;
pub mod report;
pub mod review;
pub mod schedule;
pub mod secret;
pub mod shell;
pub mod sidebar;
pub mod slash;
pub mod state;
mod transcript;

pub use layout::{
    Direction, Layout, Node, PaneContent, PaneId, ReportSource, Side, Tab, boundary_span,
};
pub use project::{Project, ProjectId, Projects};
pub use transcript::{
    Alert, Entry, History, Queued, Status, Transcript, session_label, updated_label, usage_label,
};
