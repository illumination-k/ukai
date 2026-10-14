//! ukai desktop app.

mod agent_pane;
mod alert;
mod app;
mod icons;
mod layout;
mod palette;
mod persist;
mod projects;
mod report_pane;

use std::path::{Path, PathBuf};

use dioxus::desktop::{Config, LogicalSize, WindowBuilder};
use tracing_subscriber::EnvFilter;

use crate::projects::Paths;

/// Everything `main` resolves before the UI starts.
#[derive(Debug, Clone, PartialEq)]
pub struct Startup {
    pub paths: Paths,
    pub state: ukai_core::state::AppState,
    /// Project to open (and select) on top of the saved ones.
    pub open: Option<PathBuf>,
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    let env = |name: &str| std::env::var(name).ok();
    let paths = Paths {
        user_config: ukai_config::config_dir(env).map(|d| d.join(ukai_config::USER_FILE_NAME)),
        state_file: ukai_config::state_dir(env).map(|d| d.join("state.json")),
    };
    let state = persist::load(paths.state_file.as_deref());

    // `ukai PROJECT_DIR` opens that directory; with no argument, the nearest
    // ancestor of the working directory with a ukai.toml (if any).
    let open = std::env::args_os().nth(1).map(PathBuf::from).or_else(|| {
        let cwd = std::env::current_dir().ok()?;
        cwd.ancestors()
            .find(|dir| dir.join(ukai_config::FILE_NAME).is_file())
            .map(Path::to_owned)
    });

    let window = WindowBuilder::new()
        .with_title("ukai")
        .with_inner_size(LogicalSize::new(1280.0, 800.0));
    dioxus::LaunchBuilder::desktop()
        .with_cfg(Config::new().with_window(window).with_menu(None))
        .with_context(Startup { paths, state, open })
        .launch(app::App);
}
