//! Application state that does not depend on the UI framework, so it can be
//! tested headless. `ukai-app` wraps these types in signals and renders them.

pub mod attachment;
pub mod config;
pub mod keymap;
mod layout;
pub mod markdown;
pub mod palette;
mod project;
pub mod report;
pub mod review;
pub mod slash;
pub mod state;
mod transcript;

pub use layout::{Direction, Layout, Node, PaneContent, PaneId, ReportSource, Tab};
pub use project::{Project, ProjectId, Projects};
pub use transcript::{Alert, Entry, Status, Steer, Transcript, usage_label};
