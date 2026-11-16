use std::collections::HashMap;

use dioxus::desktop::tao::event::{Event, WindowEvent};
use dioxus::desktop::use_wry_event_handler;
use dioxus::prelude::*;

use crate::Startup;
use crate::alert::mark_read;
use crate::gateways::list_gateways;
use crate::knowledge::FollowKnowledge;
use crate::layout::Workbench;
use crate::palette::Palette;
use crate::persist::use_autosave;
use crate::ports::FollowPorts;
use crate::projects::{AppSignals, add_project, load_config};
use crate::remote_reports::Follow;
use crate::schedules::FollowSchedules;
use crate::shortcuts::{Shortcuts, on_keydown};
use crate::sidebar::Sidebar;
use crate::voice::Voice;

const MAIN_CSS: Asset = asset!("/assets/main.css");

#[component]
pub fn App() -> Element {
    let startup = use_context::<Startup>();
    let paths = use_context_provider(|| startup.paths.clone());
    let app = use_context_provider(|| {
        let state = startup.state.clone();
        let configs = state
            .projects
            .iter()
            .map(|project| {
                (
                    project.id.clone(),
                    load_config(&startup.paths, &project.root),
                )
            })
            .collect::<HashMap<_, _>>();
        AppSignals::new(state.projects, configs, state.layout, state.sidebar)
    });
    use_hook(|| {
        let restored: Vec<_> = app.configs.peek().keys().cloned().collect();
        for project in restored {
            list_gateways(app, &project);
        }
        if let Some(root) = &startup.open {
            add_project(&paths, app, root);
        }
    });
    use_autosave(
        paths.state_file.clone(),
        app.projects,
        app.layout,
        app.folds,
    );

    use_wry_event_handler(move |event, _| {
        if let Event::WindowEvent {
            event: WindowEvent::Focused(true),
            ..
        } = event
        {
            mark_read(app.sessions, app.layout.peek().focused());
        }
    });

    use_context_provider(|| Shortcuts::load(&paths));
    use_context_provider(|| Voice::load(&paths));

    rsx! {
        document::Stylesheet { href: MAIN_CSS }
        Shell {}
    }
}

/// Everything the window shows, from the state and settings [`App`]
/// provides. Nothing here asks for the window, so tests mount it as it is.
#[component]
pub fn Shell() -> Element {
    let app = use_context::<AppSignals>();
    let shortcuts = use_context::<Shortcuts>();
    let palette = app.palette;

    // Whatever pane the user is looking at has nothing unread: the focused
    // one, once focus moves to it or the window comes back to the front.
    use_effect(move || {
        let focused = app.layout.read().focused();
        mark_read(app.sessions, focused);
    });

    rsx! {
        div {
            class: "shell",
            tabindex: "0",
            onkeydown: move |event| on_keydown(app, &shortcuts, &event),
            Sidebar {}
            Workbench {}
            Follow {}
            FollowPorts {}
            FollowSchedules {}
            FollowKnowledge {}
            if palette() {
                Palette {}
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod tests;
