//! The settings an agent lets the user change, in the composer's footer.

use dioxus::prelude::*;
use ukai_acp::{ConfigChoice, ConfigInput, ConfigOption, ConfigValue};
use ukai_core::config::Setting;
use ukai_core::{PaneContent, PaneId};

use crate::icons::{Icon, Svg};
use crate::projects::AppSignals;

/// The agent's settings, e.g. model and reasoning effort, changed through
/// `session/set_config_option`. A change shows once the agent confirms it.
/// The model and effort picked become what the project's new sessions of
/// the agent start with.
#[component]
pub(super) fn ConfigControls(pane: PaneId, options: Vec<ConfigOption>, disabled: bool) -> Element {
    let AppSignals {
        sessions,
        mut projects,
        layout,
        ..
    } = use_context::<AppSignals>();
    let set = move |id: String, value: ConfigInput| {
        if let Some(handle) = sessions.read().get(&pane).and_then(|s| s.handle.clone()) {
            handle.set_config(id, value);
        }
    };
    let mut remember = move |setting: Setting, value: String| {
        if let Some(PaneContent::Agent { project, agent, .. }) = layout.peek().content(pane) {
            projects.write().set_default(project, agent, setting, value);
        }
    };
    rsx! {
        div { class: "composer-config",
            for option in options {
                {
                    let id = option.id.clone();
                    let setting = Setting::of(&option);
                    let title = match &option.description {
                        Some(description) => format!("{}: {description}", option.name),
                        None => option.name.clone(),
                    };
                    match option.value {
                        ConfigValue::Select { current, choices } => rsx! {
                            ConfigSelect {
                                key: "{option.id}",
                                name: option.name.clone(),
                                title,
                                current,
                                choices,
                                disabled,
                                onpick: move |value: String| {
                                    if let Some(setting) = setting {
                                        remember(setting, value.clone());
                                    }
                                    set(id.clone(), ConfigInput::Select(value));
                                },
                            }
                        },
                        ConfigValue::Toggle(on) => rsx! {
                            label { key: "{option.id}", class: "config-toggle", title: "{title}",
                                input {
                                    r#type: "checkbox",
                                    checked: on,
                                    disabled,
                                    onchange: move |event| set(id.clone(), ConfigInput::Toggle(event.checked())),
                                }
                                "{option.name}"
                            }
                        },
                    }
                }
            }
        }
    }
}

/// One select setting as a button and a menu drawn like the app's other
/// lists, instead of the native `select` whose popup the web view leaves to
/// the platform.
#[component]
fn ConfigSelect(
    name: String,
    title: String,
    current: String,
    choices: Vec<ConfigChoice>,
    disabled: bool,
    onpick: EventHandler<String>,
) -> Element {
    let mut open = use_signal(|| false);
    let mut highlighted = use_signal(|| 0usize);
    let shown = open() && !disabled;
    let len = choices.len();
    let selected = choices.iter().position(|choice| choice.value == current);
    let label = ukai_core::config::current_label(&current, &choices);
    let mut pick = move |value: String| {
        open.set(false);
        onpick.call(value);
    };
    let on_key = {
        let choices = choices.clone();
        move |event: KeyboardEvent| {
            let step = |delta: isize| {
                (highlighted() as isize + delta).rem_euclid(len.max(1) as isize) as usize
            };
            match event.key() {
                Key::ArrowDown => highlighted.set(step(1)),
                Key::ArrowUp => highlighted.set(step(-1)),
                Key::Enter if !event.is_composing() => {
                    if let Some(choice) = choices.get(highlighted()) {
                        pick(choice.value.clone());
                    }
                }
                // Not the app's: escape here only closes the menu.
                Key::Escape | Key::Tab => open.set(false),
                _ => return,
            }
            event.prevent_default();
            event.stop_propagation();
        }
    };
    // Each choice with its index in `choices` and, first in a group, the
    // group's name to head it.
    let rows: Vec<(usize, Option<String>, ConfigChoice)> =
        ukai_core::config::choice_groups(&choices)
            .into_iter()
            .flat_map(|(group, run)| {
                run.iter().enumerate().map(move |(i, choice)| {
                    (group.filter(|_| i == 0).map(str::to_owned), choice.clone())
                })
            })
            .enumerate()
            .map(|(index, (group, choice))| (index, group, choice))
            .collect();

    rsx! {
        div { class: "config-select",
            button {
                class: if shown { "config-trigger open" } else { "config-trigger" },
                title: "{title}",
                aria_label: "{name}",
                aria_haspopup: "listbox",
                aria_expanded: shown,
                disabled,
                onclick: move |_| {
                    highlighted.set(selected.unwrap_or(0));
                    open.toggle();
                },
                span { class: "name", "{label}" }
                span { class: "caret",
                    Svg { icon: Icon::Chevron, size: 10 }
                }
            }
            if shown {
                div {
                    class: "config-backdrop",
                    onclick: move |_| open.set(false),
                }
                div {
                    class: "config-menu",
                    role: "listbox",
                    aria_label: "{name}",
                    tabindex: "-1",
                    onmounted: move |event| async move {
                        let _ = event.set_focus(true).await;
                    },
                    onkeydown: on_key,
                    for (at, group, choice) in rows {
                        if let Some(group) = group {
                            div { class: "config-group", "{group}" }
                        }
                        div {
                            class: if at == highlighted() { "config-item highlighted" } else { "config-item" },
                            role: "option",
                            aria_selected: Some(at) == selected,
                            title: choice.description.clone().unwrap_or_default(),
                            onmouseenter: move |_| highlighted.set(at),
                            onclick: {
                                let value = choice.value.clone();
                                move |_| pick(value.clone())
                            },
                            span { class: "check",
                                if Some(at) == selected {
                                    Svg { icon: Icon::Check, size: 12 }
                                }
                            }
                            span { class: "label", "{choice.name}" }
                            if let Some(description) = &choice.description {
                                span { class: "description", "{description}" }
                            }
                        }
                    }
                }
            }
        }
    }
}
