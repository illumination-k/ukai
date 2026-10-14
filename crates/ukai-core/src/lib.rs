//! Application state that does not depend on the UI framework, so it can be
//! tested headless. `ukai-app` wraps these types in signals and renders them.

mod layout;
mod project;
pub mod state;
mod transcript;

pub use layout::{Direction, Layout, Node, PaneContent, PaneId, Tab};
pub use project::{Project, ProjectId, Projects};
pub use transcript::{Entry, Status, Transcript};
