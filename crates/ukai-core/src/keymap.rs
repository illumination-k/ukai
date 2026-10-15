//! Keyboard shortcuts: the actions keys can be bound to, their defaults, and
//! the user's `[keybindings]` over them.
//!
//! A chord is written `mod+shift+d`: modifiers joined by `+`, then one key.
//! `mod` is cmd on macOS and ctrl elsewhere. The key is a character or a name
//! (`tab`, `enter`, `escape`, `space`, `up`, `pageup`, `f5`, ...).
//!
//! Most actions apply app-wide; `send` applies in the composer only (see
//! [`Scope`]). A chord without ctrl, alt, or cmd may not type a character,
//! which would take it away from the composer. App-wide, it may only be a
//! function key or escape, since enter and tab mean something in a text box.
//!
//! An action's chords in `[keybindings]` replace its defaults (`[]` unbinds
//! it). A chord the user binds is taken away from any default that had it.

use std::collections::{BTreeMap, BTreeSet};

use ukai_config::Keys;

/// What a shortcut does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Action {
    Palette,
    NewTab,
    ClosePane,
    SplitRight,
    SplitDown,
    NextTab,
    PrevTab,
    NextPane,
    PrevPane,
    /// The tab at this 1-based position (1 to 9).
    Tab(u8),
    /// Stop the focused pane's turn.
    Stop,
    /// Start or stop dictating into the focused pane's composer.
    Dictate,
    /// Send the composer's message (or the diff comment being written).
    Send,
}

/// Where an action's keys are listened for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Anywhere in the window.
    App,
    /// In the composer and the diff comment editor, before the app sees them.
    Composer,
}

impl Action {
    /// Every action, in the order hints and conflicts list them.
    pub fn all() -> impl Iterator<Item = Action> {
        [
            Action::Palette,
            Action::NewTab,
            Action::ClosePane,
            Action::SplitRight,
            Action::SplitDown,
            Action::NextTab,
            Action::PrevTab,
            Action::NextPane,
            Action::PrevPane,
        ]
        .into_iter()
        .chain((1..=9).map(Action::Tab))
        .chain([Action::Stop, Action::Dictate, Action::Send])
    }

    pub fn scope(self) -> Scope {
        match self {
            Action::Send => Scope::Composer,
            _ => Scope::App,
        }
    }

    /// The name `[keybindings]` uses.
    pub fn name(self) -> String {
        match self {
            Action::Palette => "palette".to_owned(),
            Action::NewTab => "new-tab".to_owned(),
            Action::ClosePane => "close-pane".to_owned(),
            Action::SplitRight => "split-right".to_owned(),
            Action::SplitDown => "split-down".to_owned(),
            Action::NextTab => "next-tab".to_owned(),
            Action::PrevTab => "prev-tab".to_owned(),
            Action::NextPane => "next-pane".to_owned(),
            Action::PrevPane => "prev-pane".to_owned(),
            Action::Tab(n) => format!("tab-{n}"),
            Action::Stop => "stop".to_owned(),
            Action::Dictate => "dictate".to_owned(),
            Action::Send => "send".to_owned(),
        }
    }

    pub fn from_name(name: &str) -> Option<Action> {
        Action::all().find(|action| action.name() == name)
    }

    fn defaults(self) -> Vec<String> {
        let chords: &[&str] = match self {
            Action::Palette => &["mod+k"],
            Action::NewTab => &["mod+t"],
            Action::ClosePane => &["mod+w"],
            Action::SplitRight => &["mod+d"],
            Action::SplitDown => &["mod+shift+d"],
            Action::NextTab => &["ctrl+tab", "mod+shift+]"],
            Action::PrevTab => &["ctrl+shift+tab", "mod+shift+["],
            Action::NextPane => &["mod+]"],
            Action::PrevPane => &["mod+["],
            Action::Stop => &["escape"],
            Action::Dictate => &["mod+shift+m"],
            Action::Send => &["enter"],
            Action::Tab(n) => return vec![format!("mod+{n}")],
        };
        chords.iter().map(|&chord| chord.to_owned()).collect()
    }
}

/// Where `mod` points and how hints are written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    Mac,
    Other,
}

impl Platform {
    pub fn current() -> Self {
        if cfg!(target_os = "macos") {
            Platform::Mac
        } else {
            Platform::Other
        }
    }
}

/// A key pressed with modifiers, as the DOM reports it: `key` is the
/// produced value (`"d"`, `"D"`, `"Tab"`, `"ArrowUp"`), `code` the physical
/// key (`"KeyD"`, `"Digit1"`, `"BracketRight"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Keystroke<'a> {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub meta: bool,
    pub key: &'a str,
    pub code: &'a str,
}

/// One parsed chord: modifiers and a key, lowercase, named as the DOM's
/// `key` values are (`tab`, `arrowup`, `" "`).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Chord {
    ctrl: bool,
    alt: bool,
    shift: bool,
    meta: bool,
    key: String,
}

const NAMED_KEYS: &[&str] = &[
    "tab",
    "enter",
    "escape",
    " ",
    "backspace",
    "delete",
    "insert",
    "home",
    "end",
    "pageup",
    "pagedown",
    "arrowup",
    "arrowdown",
    "arrowleft",
    "arrowright",
];

impl Chord {
    pub fn parse(src: &str, platform: Platform) -> Result<Chord, String> {
        let src = src.trim();
        let (modifiers, key) = match src.strip_suffix("++") {
            Some(modifiers) => (modifiers, "+"),
            None => src.rsplit_once('+').unwrap_or(("", src)),
        };
        let mut chord = Chord {
            ctrl: false,
            alt: false,
            shift: false,
            meta: false,
            key: String::new(),
        };
        for modifier in modifiers.split('+').filter(|m| !m.is_empty()) {
            match modifier.trim().to_lowercase().as_str() {
                "mod" if platform == Platform::Mac => chord.meta = true,
                "mod" => chord.ctrl = true,
                "cmd" | "command" | "meta" | "super" => chord.meta = true,
                "ctrl" | "control" => chord.ctrl = true,
                "alt" | "opt" | "option" => chord.alt = true,
                "shift" => chord.shift = true,
                other => return Err(format!("unknown modifier `{other}`")),
            }
        }
        let key = key.trim().to_lowercase();
        chord.key = match key.as_str() {
            "" => return Err("no key after the modifiers".to_owned()),
            "esc" => "escape".to_owned(),
            "return" => "enter".to_owned(),
            "space" => " ".to_owned(),
            "del" => "delete".to_owned(),
            "up" | "down" | "left" | "right" => format!("arrow{key}"),
            "pgup" => "pageup".to_owned(),
            "pgdn" => "pagedown".to_owned(),
            "plus" => "+".to_owned(),
            _ => key,
        };
        if chord.key.chars().count() != 1
            && !is_function_key(&chord.key)
            && !NAMED_KEYS.contains(&chord.key.as_str())
        {
            return Err(format!("unknown key `{}`", chord.key));
        }
        Ok(chord)
    }

    /// Whether an action of `scope` may use this chord; see the module docs.
    pub fn usable_in(&self, scope: Scope) -> Result<(), String> {
        let types_a_character = self.key == " " || self.key.chars().count() == 1;
        if self.ctrl || self.alt || self.meta || is_function_key(&self.key) {
            Ok(())
        } else if types_a_character {
            Err("needs ctrl, alt, or cmd: alone it would type a character".to_owned())
        } else if scope == Scope::App && self.key != "escape" {
            Err(
                "needs ctrl, alt, or cmd: alone only function keys and escape work app-wide"
                    .to_owned(),
            )
        } else {
            Ok(())
        }
    }

    pub fn matches(&self, stroke: &Keystroke) -> bool {
        (self.ctrl, self.alt, self.shift, self.meta)
            == (stroke.ctrl, stroke.alt, stroke.shift, stroke.meta)
            && (stroke.key.to_lowercase() == self.key
                || code_key(stroke.code).is_some_and(|key| key == self.key))
    }

    /// How a hint writes the chord: `⌘⇧D` on macOS, `Ctrl+Shift+D` elsewhere.
    pub fn hint(&self, platform: Platform) -> String {
        let key = match self.key.as_str() {
            " " => "Space".to_owned(),
            "arrowup" => "↑".to_owned(),
            "arrowdown" => "↓".to_owned(),
            "arrowleft" => "←".to_owned(),
            "arrowright" => "→".to_owned(),
            "escape" => "Esc".to_owned(),
            "pageup" => "PgUp".to_owned(),
            "pagedown" => "PgDn".to_owned(),
            key => {
                let mut chars = key.chars();
                chars
                    .next()
                    .map(|first| first.to_uppercase().chain(chars).collect())
                    .unwrap_or_default()
            }
        };
        match platform {
            Platform::Mac => {
                let mut out = String::new();
                for (on, symbol) in [
                    (self.ctrl, "⌃"),
                    (self.alt, "⌥"),
                    (self.shift, "⇧"),
                    (self.meta, "⌘"),
                ] {
                    if on {
                        out.push_str(symbol);
                    }
                }
                out + &key
            }
            Platform::Other => {
                let mut parts: Vec<&str> = [
                    (self.ctrl, "Ctrl"),
                    (self.alt, "Alt"),
                    (self.shift, "Shift"),
                    (self.meta, "Super"),
                ]
                .into_iter()
                .filter_map(|(on, name)| on.then_some(name))
                .collect();
                parts.push(&key);
                parts.join("+")
            }
        }
    }
}

fn is_function_key(key: &str) -> bool {
    key.strip_prefix('f')
        .and_then(|n| n.parse::<u8>().ok())
        .is_some_and(|n| (1..=24).contains(&n))
}

/// The key a physical key types without modifiers on a US layout, so a chord
/// still matches when shift or option changes what the key produces
/// (`⌥K` types `˚`, `⇧]` types `}`).
fn code_key(code: &str) -> Option<String> {
    if let Some(letter) = code.strip_prefix("Key") {
        return Some(letter.to_lowercase());
    }
    if let Some(digit) = code.strip_prefix("Digit") {
        return Some(digit.to_owned());
    }
    let key = match code {
        "BracketLeft" => "[",
        "BracketRight" => "]",
        "Comma" => ",",
        "Period" => ".",
        "Slash" => "/",
        "Backslash" => "\\",
        "Semicolon" => ";",
        "Quote" => "'",
        "Backquote" => "`",
        "Minus" => "-",
        "Equal" => "=",
        _ => return None,
    };
    Some(key.to_owned())
}

/// One problem with `[keybindings]`. All of them are collected; the rest of
/// the bindings still apply.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Problem {
    #[error("keybindings.{0}: no such action")]
    UnknownAction(String),
    #[error("keybindings.{action}: `{chord}`: {message}")]
    InvalidChord {
        action: String,
        chord: String,
        message: String,
    },
    #[error("keybindings: `{chord}` is bound to {}", .actions.join(" and "))]
    Conflict { chord: String, actions: Vec<String> },
}

/// Chords to actions, the user's over the defaults.
#[derive(Debug, Clone, PartialEq)]
pub struct Keymap {
    platform: Platform,
    bindings: Vec<(Chord, Action)>,
}

impl Keymap {
    pub fn new(keybindings: &BTreeMap<String, Keys>, platform: Platform) -> (Keymap, Vec<Problem>) {
        let mut problems = Vec::new();
        let mut user: BTreeMap<Action, Vec<(Chord, String)>> = BTreeMap::new();
        for (name, keys) in keybindings {
            let Some(action) = Action::from_name(name) else {
                problems.push(Problem::UnknownAction(name.clone()));
                continue;
            };
            let mut chords = Vec::new();
            for src in keys.chords() {
                let chord = Chord::parse(src, platform)
                    .and_then(|chord| chord.usable_in(action.scope()).map(|()| chord));
                match chord {
                    Ok(chord) => chords.push((chord, src.clone())),
                    Err(message) => problems.push(Problem::InvalidChord {
                        action: name.clone(),
                        chord: src.clone(),
                        message,
                    }),
                }
            }
            // `[]` unbinds; chords that are all invalid keep the defaults.
            if keys.chords().is_empty() || !chords.is_empty() {
                user.insert(action, chords);
            }
        }

        let mut claimed: BTreeMap<&Chord, (&str, Vec<Action>)> = BTreeMap::new();
        for (action, chords) in &user {
            for (chord, src) in chords {
                let (_, actions) = claimed.entry(chord).or_insert((src, Vec::new()));
                if !actions.contains(action) {
                    actions.push(*action);
                }
            }
        }
        problems.extend(
            claimed
                .values()
                .filter(|(_, actions)| actions.len() > 1)
                .map(|(src, actions)| Problem::Conflict {
                    chord: (*src).to_owned(),
                    actions: actions.iter().map(|action| action.name()).collect(),
                }),
        );

        let user_chords: BTreeSet<&Chord> = claimed.keys().copied().collect();
        let mut bindings: Vec<(Chord, Action)> = Vec::new();
        for action in Action::all() {
            match user.get(&action) {
                Some(chords) => {
                    bindings.extend(chords.iter().map(|(chord, _)| (chord.clone(), action)));
                }
                None => bindings.extend(
                    action
                        .defaults()
                        .iter()
                        .filter_map(|src| Chord::parse(src, platform).ok())
                        .filter(|chord| !user_chords.contains(chord))
                        .map(|chord| (chord, action)),
                ),
            }
        }
        (Keymap { platform, bindings }, problems)
    }

    /// The defaults alone.
    pub fn default_for(platform: Platform) -> Keymap {
        Keymap::new(&BTreeMap::new(), platform).0
    }

    /// The action of `scope` bound to the pressed keys.
    pub fn action(&self, scope: Scope, stroke: &Keystroke) -> Option<Action> {
        self.bindings
            .iter()
            .find(|(chord, action)| action.scope() == scope && chord.matches(stroke))
            .map(|(_, action)| *action)
    }

    /// The first chord bound to `action`, written for a hint.
    pub fn hint(&self, action: Action) -> Option<String> {
        self.bindings
            .iter()
            .find(|(_, bound)| *bound == action)
            .map(|(chord, _)| chord.hint(self.platform))
    }
}

#[cfg(test)]
mod tests;
