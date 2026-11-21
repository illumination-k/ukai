//! Application state that does not depend on the UI framework, so it can be
//! tested headless. `ukai-app` wraps these types in signals and renders them.

pub mod annotation;
pub mod attachment;
pub mod branch;
pub mod browser;
pub mod composer;
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
pub mod publication;
pub mod recall;
pub mod report;
pub mod review;
pub mod schedule;
pub mod secret;
pub mod shell;
pub mod sidebar;
pub mod slash;
pub mod state;
pub mod terminal;
mod transcript;

pub use layout::{
    Boundary, Direction, Frame, Frames, Layout, Node, PaneContent, PaneId, Placed, Recent,
    ReportSource, Side, Tab,
};
pub use project::{Project, ProjectId, Projects};
pub use transcript::{
    Alert, Connection, Entry, History, Queued, Sent, Status, Transcript, session_label,
    updated_label, usage_label,
};
