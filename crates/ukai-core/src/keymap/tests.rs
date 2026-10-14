use rstest::rstest;

use super::*;

fn stroke<'a>(modifiers: &str, key: &'a str, code: &'a str) -> Keystroke<'a> {
    Keystroke {
        ctrl: modifiers.contains('c'),
        alt: modifiers.contains('a'),
        shift: modifiers.contains('s'),
        meta: modifiers.contains('m'),
        key,
        code,
    }
}

fn keymap(src: &str, platform: Platform) -> (Keymap, Vec<Problem>) {
    let keybindings = toml_keybindings(src).unwrap_or_else(|error| panic!("{error}"));
    Keymap::new(&keybindings, platform)
}

/// `[keybindings]` as the user file would hold it.
fn toml_keybindings(src: &str) -> Result<BTreeMap<String, Keys>, String> {
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    let file = dir.path().join("config.toml");
    std::fs::write(&file, format!("[keybindings]\n{src}")).map_err(|e| e.to_string())?;
    ukai_config::load_user(Some(&file))
        .map(|config| config.keybindings)
        .map_err(|e| e.to_string())
}

#[rstest]
#[case::mod_on_mac("mod+k", Platform::Mac, stroke("m", "k", "KeyK"))]
#[case::mod_elsewhere("mod+k", Platform::Other, stroke("c", "k", "KeyK"))]
#[case::shifted_letter("mod+shift+d", Platform::Mac, stroke("ms", "D", "KeyD"))]
#[case::option_changes_the_character("alt+k", Platform::Mac, stroke("a", "˚", "KeyK"))]
#[case::shift_changes_the_symbol("mod+shift+]", Platform::Mac, stroke("ms", "}", "BracketRight"))]
#[case::digit("mod+1", Platform::Other, stroke("c", "1", "Digit1"))]
#[case::named_key("ctrl+tab", Platform::Mac, stroke("c", "Tab", "Tab"))]
#[case::alias("ctrl+up", Platform::Other, stroke("c", "ArrowUp", "ArrowUp"))]
#[case::space("ctrl+space", Platform::Other, stroke("c", " ", "Space"))]
#[case::plus_key("ctrl++", Platform::Other, stroke("c", "+", "Equal"))]
#[case::function_key_alone("f5", Platform::Mac, stroke("", "F5", "F5"))]
#[case::spelled_out("Command+Option+P", Platform::Mac, stroke("ma", "π", "KeyP"))]
fn chord_matches(#[case] src: &str, #[case] platform: Platform, #[case] pressed: Keystroke) {
    let chord = Chord::parse(src, platform);
    assert!(
        chord.as_ref().is_ok_and(|chord| chord.matches(&pressed)),
        "{chord:?}"
    );
}

#[rstest]
#[case::missing_shift("mod+shift+d", stroke("m", "d", "KeyD"))]
#[case::extra_modifier("mod+k", stroke("ma", "k", "KeyK"))]
#[case::other_key("mod+k", stroke("m", "j", "KeyJ"))]
fn chord_does_not_match(#[case] src: &str, #[case] pressed: Keystroke) {
    let chord = Chord::parse(src, Platform::Mac);
    assert!(
        chord.as_ref().is_ok_and(|chord| !chord.matches(&pressed)),
        "{chord:?}"
    );
}

#[rstest]
#[case::unknown_modifier("hyper+k", "unknown modifier `hyper`")]
#[case::unknown_key("mod+enterr", "unknown key `enterr`")]
#[case::no_key("mod+", "no key after the modifiers")]
fn chord_is_rejected(#[case] src: &str, #[case] message: &str) {
    assert_eq!(Chord::parse(src, Platform::Mac), Err(message.to_owned()));
}

#[rstest]
#[case::modified_letter("mod+k", Scope::App, Ok(()))]
#[case::function_key("f5", Scope::App, Ok(()))]
#[case::escape_app_wide("escape", Scope::App, Ok(()))]
#[case::enter_in_composer("enter", Scope::Composer, Ok(()))]
#[case::shift_enter_in_composer("shift+enter", Scope::Composer, Ok(()))]
#[case::enter_app_wide(
    "enter",
    Scope::App,
    Err("needs ctrl, alt, or cmd: alone only function keys and escape work app-wide")
)]
#[case::shift_tab_app_wide(
    "shift+tab",
    Scope::App,
    Err("needs ctrl, alt, or cmd: alone only function keys and escape work app-wide")
)]
#[case::bare_letter(
    "k",
    Scope::Composer,
    Err("needs ctrl, alt, or cmd: alone it would type a character")
)]
#[case::shifted_letter(
    "shift+k",
    Scope::App,
    Err("needs ctrl, alt, or cmd: alone it would type a character")
)]
#[case::space(
    "space",
    Scope::Composer,
    Err("needs ctrl, alt, or cmd: alone it would type a character")
)]
fn chord_usable_in(#[case] src: &str, #[case] scope: Scope, #[case] expected: Result<(), &str>) {
    assert_eq!(
        Chord::parse(src, Platform::Mac).and_then(|chord| chord.usable_in(scope)),
        expected.map_err(str::to_owned)
    );
}

#[rstest]
#[case::mac("mod+shift+d", Platform::Mac, "⇧⌘D")]
#[case::other("mod+shift+d", Platform::Other, "Ctrl+Shift+D")]
#[case::named("ctrl+alt+up", Platform::Other, "Ctrl+Alt+↑")]
#[case::tab("ctrl+tab", Platform::Mac, "⌃Tab")]
fn chord_hint(#[case] src: &str, #[case] platform: Platform, #[case] expected: &str) {
    assert_eq!(
        Chord::parse(src, platform).map(|chord| chord.hint(platform)),
        Ok(expected.to_owned())
    );
}

#[rstest]
fn defaults_parse_and_do_not_conflict(
    #[values(Platform::Mac, Platform::Other)] platform: Platform,
) {
    let mut seen = BTreeSet::new();
    for action in Action::all() {
        assert_eq!(Action::from_name(&action.name()), Some(action));
        for src in action.defaults() {
            let chord = Chord::parse(&src, platform)
                .and_then(|chord| chord.usable_in(action.scope()).map(|()| chord));
            assert!(chord.is_ok(), "{action:?}: {src}: {chord:?}");
            assert!(seen.insert(chord.ok()), "{src} is a default twice");
        }
    }
}

#[test]
fn defaults_resolve_the_usual_shortcuts() {
    let keymap = Keymap::default_for(Platform::Mac);
    assert_eq!(
        keymap.action(Scope::App, &stroke("m", "k", "KeyK")),
        Some(Action::Palette)
    );
    assert_eq!(
        keymap.action(Scope::App, &stroke("ms", "D", "KeyD")),
        Some(Action::SplitDown)
    );
    assert_eq!(
        keymap.action(Scope::App, &stroke("m", "3", "Digit3")),
        Some(Action::Tab(3))
    );
    assert_eq!(keymap.action(Scope::App, &stroke("m", "c", "KeyC")), None);
    assert_eq!(keymap.hint(Action::Palette).as_deref(), Some("⌘K"));
}

#[test]
fn scopes_keep_composer_keys_out_of_the_app() {
    let keymap = Keymap::default_for(Platform::Mac);
    let enter = stroke("", "Enter", "Enter");
    assert_eq!(keymap.action(Scope::App, &enter), None);
    assert_eq!(keymap.action(Scope::Composer, &enter), Some(Action::Send));
    let escape = stroke("", "Escape", "Escape");
    assert_eq!(keymap.action(Scope::App, &escape), Some(Action::Stop));
    assert_eq!(keymap.action(Scope::Composer, &escape), None);
}

#[test]
fn send_can_move_to_mod_enter() {
    let (keymap, problems) = keymap("send = \"mod+enter\"", Platform::Mac);
    assert_eq!(problems, []);
    assert_eq!(
        keymap.action(Scope::Composer, &stroke("", "Enter", "Enter")),
        None
    );
    assert_eq!(
        keymap.action(Scope::Composer, &stroke("m", "Enter", "Enter")),
        Some(Action::Send)
    );
}

#[test]
fn user_chords_replace_the_defaults() {
    let (keymap, problems) = keymap("palette = [\"mod+p\", \"f1\"]", Platform::Mac);
    assert_eq!(problems, []);
    assert_eq!(keymap.action(Scope::App, &stroke("m", "k", "KeyK")), None);
    assert_eq!(
        keymap.action(Scope::App, &stroke("m", "p", "KeyP")),
        Some(Action::Palette)
    );
    assert_eq!(
        keymap.action(Scope::App, &stroke("", "F1", "F1")),
        Some(Action::Palette)
    );
    assert_eq!(keymap.hint(Action::Palette).as_deref(), Some("⌘P"));
}

#[test]
fn a_user_chord_is_taken_from_the_default_holding_it() {
    let (keymap, problems) = keymap("palette = \"mod+d\"", Platform::Mac);
    assert_eq!(problems, []);
    assert_eq!(
        keymap.action(Scope::App, &stroke("m", "d", "KeyD")),
        Some(Action::Palette)
    );
    assert_eq!(keymap.hint(Action::SplitRight), None);
}

#[test]
fn an_empty_list_unbinds() {
    let (keymap, problems) = keymap("close-pane = []", Platform::Mac);
    assert_eq!(problems, []);
    assert_eq!(keymap.action(Scope::App, &stroke("m", "w", "KeyW")), None);
    assert_eq!(keymap.hint(Action::ClosePane), None);
}

#[test]
fn invalid_chords_alone_keep_the_defaults() {
    let (keymap, problems) = keymap("palette = \"mod+kk\"", Platform::Mac);
    assert_eq!(problems.len(), 1);
    assert_eq!(
        keymap.action(Scope::App, &stroke("m", "k", "KeyK")),
        Some(Action::Palette)
    );
}

#[rstest]
#[case::unknown_action("open-door = \"mod+o\"", Problem::UnknownAction("open-door".into()))]
#[case::invalid_chord(
    "palette = \"hyper+k\"",
    Problem::InvalidChord {
        action: "palette".into(),
        chord: "hyper+k".into(),
        message: "unknown modifier `hyper`".into(),
    }
)]
#[case::bare_key_app_wide(
    "palette = \"tab\"",
    Problem::InvalidChord {
        action: "palette".into(),
        chord: "tab".into(),
        message: "needs ctrl, alt, or cmd: alone only function keys and escape work app-wide".into(),
    }
)]
#[case::conflict(
    "palette = \"mod+p\"\nnew-tab = \"cmd+P\"",
    Problem::Conflict { chord: "mod+p".into(), actions: vec!["palette".into(), "new-tab".into()] }
)]
fn keybindings_problem(#[case] src: &str, #[case] expected: Problem) {
    assert_eq!(keymap(src, Platform::Mac).1, vec![expected]);
}

#[test]
fn every_problem_is_reported() {
    let src = "open-door = \"mod+o\"\npalette = [\"hyper+k\", \"mod+p\"]\nnew-tab = \"mod+p\"";
    assert_eq!(keymap(src, Platform::Mac).1.len(), 3);
}
