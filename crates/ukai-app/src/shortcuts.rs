//! Keyboard shortcuts: the keymap from the user's `[keybindings]` over the
//! defaults (`ukai_core::keymap`), read once at startup, and what each
//! action does.

use dioxus::prelude::*;
use ukai_core::Direction;
use ukai_core::keymap::{Action, Keymap, Keystroke, Platform, Scope};

use crate::agent_pane::stop_turn;
use crate::layout::close_pane;
use crate::projects::{AppSignals, Paths};

#[derive(Debug, Clone, PartialEq)]
pub struct Shortcuts {
    pub keymap: Keymap,
    /// What was wrong with `[keybindings]`; the rest still applies.
    pub problems: Vec<String>,
}

impl Shortcuts {
    pub fn load(paths: &Paths) -> Self {
        let platform = Platform::current();
        let (keymap, problems) = match ukai_config::load_user(paths.user_config.as_deref()) {
            Ok(config) => {
                let (keymap, problems) = Keymap::new(&config.keybindings, platform);
                (keymap, problems.iter().map(ToString::to_string).collect())
            }
            Err(error) => (Keymap::default_for(platform), vec![error.to_string()]),
        };
        for problem in &problems {
            tracing::warn!("{problem}");
        }
        Self { keymap, problems }
    }

    /// `label` with the action's chord, if it has one: `New tab (⌘T)`.
    pub fn title(&self, label: &str, action: Action) -> String {
        match self.keymap.hint(action) {
            Some(hint) => format!("{label} ({hint})"),
            None => label.to_owned(),
        }
    }

    /// The action of `scope` bound to the keys `event` pressed. Keys that
    /// go to an input method are nobody's.
    pub fn pressed(&self, scope: Scope, event: &KeyboardEvent) -> Option<Action> {
        if event.is_composing() {
            return None;
        }
        let modifiers = event.modifiers();
        let key = event.key().to_string();
        let code = event.code().to_string();
        self.keymap.action(
            scope,
            &Keystroke {
                ctrl: modifiers.ctrl(),
                alt: modifiers.alt(),
                shift: modifiers.shift(),
                meta: modifiers.meta(),
                key: &key,
                code: &code,
            },
        )
    }
}

/// Run the app-wide action bound to the pressed keys, if any.
pub fn on_keydown(app: AppSignals, shortcuts: &Shortcuts, event: &KeyboardEvent) {
    if let Some(action) = shortcuts.pressed(Scope::App, event) {
        event.prevent_default();
        perform(app, action);
    }
}

fn perform(app: AppSignals, action: Action) {
    let mut layout = app.layout;
    let mut palette = app.palette;
    let focused = layout.peek().focused();
    match action {
        Action::Palette => palette.toggle(),
        Action::NewTab => {
            layout.write().new_tab();
        }
        Action::ClosePane => close_pane(app, focused),
        Action::SplitRight => {
            layout.write().split(focused, Direction::Horizontal);
        }
        Action::SplitDown => {
            layout.write().split(focused, Direction::Vertical);
        }
        Action::NextTab => layout.write().step_tab(1),
        Action::PrevTab => layout.write().step_tab(-1),
        Action::NextPane => layout.write().step_focus(1),
        Action::PrevPane => layout.write().step_focus(-1),
        Action::Tab(n) => layout.write().select_tab(usize::from(n).saturating_sub(1)),
        // Escape also closes the palette; that one is not for the pane.
        Action::Stop if !palette() => stop_turn(app.sessions, focused),
        Action::Stop | Action::Send => {}
    }
}
