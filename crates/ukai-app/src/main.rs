//! ukai desktop app.

mod agent_pane;
mod alert;
mod app;
mod clipboard;
mod icons;
mod layout;
mod markdown;
mod palette;
mod persist;
mod projects;
mod report_pane;
mod shortcuts;

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
        .with_cfg(Config::new().with_window(window).with_menu(menu()))
        .with_context(Startup { paths, state, open })
        .launch(app::App);
}

/// macOS routes cut, copy, paste, undo, and select all to the web view through
/// the Edit menu, so without one the clipboard keys do nothing. The menu
/// leaves out Close Window: cmd+w closes a pane. Elsewhere the web view
/// handles those keys itself and the window has no menu bar.
#[cfg(target_os = "macos")]
fn menu() -> Option<dioxus::desktop::muda::Menu> {
    use dioxus::desktop::muda::{Menu, PredefinedMenuItem, Submenu};

    let build = || -> Result<Menu, dioxus::desktop::muda::Error> {
        let app = Submenu::with_items(
            "ukai",
            true,
            &[
                &PredefinedMenuItem::about(None, None),
                &PredefinedMenuItem::separator(),
                &PredefinedMenuItem::hide(None),
                &PredefinedMenuItem::hide_others(None),
                &PredefinedMenuItem::show_all(None),
                &PredefinedMenuItem::separator(),
                &PredefinedMenuItem::quit(None),
            ],
        )?;
        let edit = Submenu::with_items(
            "Edit",
            true,
            &[
                &PredefinedMenuItem::undo(None),
                &PredefinedMenuItem::redo(None),
                &PredefinedMenuItem::separator(),
                &PredefinedMenuItem::cut(None),
                &PredefinedMenuItem::copy(None),
                &PredefinedMenuItem::paste(None),
                &PredefinedMenuItem::select_all(None),
            ],
        )?;
        let window = Submenu::with_items(
            "Window",
            true,
            &[
                &PredefinedMenuItem::minimize(None),
                &PredefinedMenuItem::fullscreen(None),
            ],
        )?;
        Menu::with_items(&[&app, &edit, &window])
    };
    build()
        .inspect_err(|error| tracing::warn!("no menu, so no clipboard keys: {error}"))
        .ok()
}

#[cfg(not(target_os = "macos"))]
fn menu() -> Option<dioxus::desktop::muda::Menu> {
    None
}
