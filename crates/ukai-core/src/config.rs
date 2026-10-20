//! Presenting the settings an agent lets the user change, and the ones a
//! project's new sessions of an agent start with.

use serde::{Deserialize, Serialize};
use ukai_acp::{
    ConfigChoice, ConfigInput, ConfigOption, ConfigValue, RestoreOutcome, SessionEvent,
};

/// What a select shows as its current value: the current choice's name, or
/// for an agent's `default` choice the model or setting it stands for, which
/// agents such as `claude-agent-acp` put in its description. A value no
/// choice has is shown as it is.
pub fn current_label(current: &str, choices: &[ConfigChoice]) -> String {
    match choices.iter().find(|choice| choice.value == current) {
        Some(choice) if choice.value == DEFAULT_VALUE => choice
            .description
            .as_deref()
            .map(str::trim)
            .filter(|description| !description.is_empty())
            .unwrap_or(&choice.name)
            .to_owned(),
        Some(choice) => choice.name.clone(),
        None => current.to_owned(),
    }
}

/// The value agents give a choice that follows their own default.
const DEFAULT_VALUE: &str = "default";

/// A select's choices as runs under the same group, in the agent's order.
pub fn choice_groups(choices: &[ConfigChoice]) -> Vec<(Option<&str>, &[ConfigChoice])> {
    choices
        .chunk_by(|a, b| a.group == b.group)
        .map(|run| (run[0].group.as_deref(), run))
        .collect()
}

/// A setting ukai keeps a default for, known by the ACP `category` agents
/// give its option rather than by the option's id, which is the agent's own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Setting {
    Model,
    Effort,
}

impl Setting {
    pub fn of(option: &ConfigOption) -> Option<Self> {
        match option.category.as_deref() {
            Some("model") => Some(Self::Model),
            Some("thought_level") => Some(Self::Effort),
            _ => None,
        }
    }
}

/// The values new sessions of one agent start with, kept per project in
/// `state.json`; what the user last picked unless edited there.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionDefaults {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
}

impl SessionDefaults {
    pub fn set(&mut self, setting: Setting, value: String) {
        match setting {
            Setting::Model => self.model = Some(value),
            Setting::Effort => self.effort = Some(value),
        }
    }

    /// The model first: changing it can change the efforts on offer.
    fn queue(&self) -> Vec<(Setting, String)> {
        [
            (Setting::Model, self.model.as_ref()),
            (Setting::Effort, self.effort.as_ref()),
        ]
        .into_iter()
        .filter_map(|(setting, value)| Some((setting, value?.clone())))
        .collect()
    }
}

/// Applies [`SessionDefaults`] to a new session one change at a time, each
/// checked against the options the agent last sent. A restored session keeps
/// its own settings.
#[derive(Debug, Clone, Default)]
pub struct ApplyDefaults {
    defaults: SessionDefaults,
    queue: Vec<(Setting, String)>,
}

impl ApplyDefaults {
    pub fn new(defaults: SessionDefaults) -> Self {
        Self {
            defaults,
            queue: Vec::new(),
        }
    }

    /// The change to request after `event`, if any.
    pub fn on_event(&mut self, event: &SessionEvent) -> Option<(String, ConfigInput)> {
        match event {
            SessionEvent::Ready {
                restore, config, ..
            } => {
                self.queue = match restore {
                    RestoreOutcome::Fresh
                    | RestoreOutcome::Unsupported
                    | RestoreOutcome::Missing => self.defaults.queue(),
                    RestoreOutcome::Replayed | RestoreOutcome::Resumed => Vec::new(),
                };
                self.next(config)
            }
            SessionEvent::ConfigChanged { options } => self.next(options),
            SessionEvent::ConfigFailed { .. } => {
                self.queue.clear();
                None
            }
            _ => None,
        }
    }

    /// Skips defaults already in effect and values no option offers.
    fn next(&mut self, options: &[ConfigOption]) -> Option<(String, ConfigInput)> {
        while !self.queue.is_empty() {
            let (setting, value) = self.queue.remove(0);
            let Some(option) = options.iter().find(|o| Setting::of(o) == Some(setting)) else {
                continue;
            };
            let ConfigValue::Select { current, choices } = &option.value else {
                continue;
            };
            if *current != value && choices.iter().any(|choice| choice.value == value) {
                return Some((option.id.clone(), ConfigInput::Select(value)));
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;
    use ukai_acp::PromptCaps;

    use super::*;

    fn described(value: &str, name: &str, description: Option<&str>) -> ConfigChoice {
        ConfigChoice {
            value: value.into(),
            name: name.into(),
            description: description.map(str::to_owned),
            group: None,
        }
    }

    #[rstest]
    #[case::default_shows_its_model("default", Some("Opus 4.7"), "Opus 4.7")]
    #[case::default_without_description("default", None, "Default (recommended)")]
    #[case::default_with_blank_description("default", Some("  "), "Default (recommended)")]
    #[case::concrete_choice("sonnet", None, "Sonnet 4.6")]
    #[case::unknown_value("opus[1m]", None, "opus[1m]")]
    fn labels_the_current_choice(
        #[case] current: &str,
        #[case] default_description: Option<&str>,
        #[case] expected: &str,
    ) {
        let choices = [
            described("default", "Default (recommended)", default_description),
            described("sonnet", "Sonnet 4.6", Some("Fast and capable")),
        ];
        assert_eq!(current_label(current, &choices), expected);
    }

    fn choice(value: &str, group: Option<&str>) -> ConfigChoice {
        ConfigChoice {
            value: value.into(),
            name: value.into(),
            description: None,
            group: group.map(str::to_owned),
        }
    }

    #[test]
    fn groups_runs_in_order() {
        let choices = [
            choice("a", None),
            choice("b", Some("x")),
            choice("c", Some("x")),
            choice("d", Some("y")),
        ];
        let groups: Vec<(Option<&str>, Vec<&str>)> = choice_groups(&choices)
            .into_iter()
            .map(|(group, run)| (group, run.iter().map(|c| c.value.as_str()).collect()))
            .collect();
        assert_eq!(
            groups,
            [
                (None, vec!["a"]),
                (Some("x"), vec!["b", "c"]),
                (Some("y"), vec!["d"]),
            ]
        );
    }

    #[test]
    fn no_choices_no_groups() {
        assert!(choice_groups(&[]).is_empty());
    }

    fn select(id: &str, category: &str, current: &str, values: &[&str]) -> ConfigOption {
        ConfigOption {
            id: id.into(),
            name: id.into(),
            description: None,
            category: Some(category.into()),
            value: ConfigValue::Select {
                current: current.into(),
                choices: values.iter().map(|value| choice(value, None)).collect(),
            },
        }
    }

    fn ready(restore: RestoreOutcome, config: Vec<ConfigOption>) -> SessionEvent {
        SessionEvent::Ready {
            agent: None,
            session_id: "s".into(),
            restore,
            prompt: PromptCaps::default(),
            config,
            list_sessions: false,
        }
    }

    fn defaults(model: Option<&str>, effort: Option<&str>) -> SessionDefaults {
        SessionDefaults {
            model: model.map(str::to_owned),
            effort: effort.map(str::to_owned),
        }
    }

    fn set(id: &str, value: &str) -> Option<(String, ConfigInput)> {
        Some((id.into(), ConfigInput::Select(value.into())))
    }

    #[rstest]
    #[case::fresh(RestoreOutcome::Fresh, set("model", "opus"))]
    #[case::not_restorable(RestoreOutcome::Unsupported, set("model", "opus"))]
    #[case::missing(RestoreOutcome::Missing, set("model", "opus"))]
    #[case::replayed(RestoreOutcome::Replayed, None)]
    #[case::resumed(RestoreOutcome::Resumed, None)]
    fn applies_only_to_new_sessions(
        #[case] restore: RestoreOutcome,
        #[case] expected: Option<(String, ConfigInput)>,
    ) {
        let mut apply = ApplyDefaults::new(defaults(Some("opus"), None));
        let config = vec![select("model", "model", "default", &["default", "opus"])];
        assert_eq!(apply.on_event(&ready(restore, config)), expected);
    }

    #[test]
    fn sets_the_model_then_an_effort_the_new_model_offers() {
        let mut apply = ApplyDefaults::new(defaults(Some("opus"), Some("max")));
        let before = vec![
            select("model", "model", "sonnet", &["sonnet", "opus"]),
            select("effort", "thought_level", "high", &["low", "high"]),
        ];
        assert_eq!(
            apply.on_event(&ready(RestoreOutcome::Fresh, before)),
            set("model", "opus")
        );
        let after = vec![
            select("model", "model", "opus", &["sonnet", "opus"]),
            select("effort", "thought_level", "high", &["low", "high", "max"]),
        ];
        assert_eq!(
            apply.on_event(&SessionEvent::ConfigChanged {
                options: after.clone()
            }),
            set("effort", "max")
        );
        assert_eq!(
            apply.on_event(&SessionEvent::ConfigChanged { options: after }),
            None
        );
    }

    #[rstest]
    #[case::already_in_effect("opus")]
    #[case::not_offered("gpt-5")]
    fn skips_to_the_next_default(#[case] model: &str) {
        let mut apply = ApplyDefaults::new(defaults(Some(model), Some("low")));
        let config = vec![
            select("model", "model", "opus", &["sonnet", "opus"]),
            select("effort", "thought_level", "high", &["low", "high"]),
        ];
        assert_eq!(
            apply.on_event(&ready(RestoreOutcome::Fresh, config)),
            set("effort", "low")
        );
    }

    #[test]
    fn options_without_a_known_category_are_left_alone() {
        let mut apply = ApplyDefaults::new(defaults(Some("opus"), Some("low")));
        let config = vec![select("model", "mode", "default", &["opus", "low"])];
        assert_eq!(apply.on_event(&ready(RestoreOutcome::Fresh, config)), None);
    }

    #[test]
    fn a_rejected_change_ends_the_defaults() {
        let mut apply = ApplyDefaults::new(defaults(Some("opus"), Some("low")));
        let config = vec![
            select("model", "model", "sonnet", &["sonnet", "opus"]),
            select("effort", "thought_level", "high", &["low", "high"]),
        ];
        assert!(
            apply
                .on_event(&ready(RestoreOutcome::Fresh, config.clone()))
                .is_some()
        );
        apply.on_event(&SessionEvent::ConfigFailed {
            message: "no".into(),
        });
        assert_eq!(
            apply.on_event(&SessionEvent::ConfigChanged { options: config }),
            None
        );
    }

    #[test]
    fn changes_before_the_session_is_ready_are_not_answered() {
        // A session being loaded may report its settings while replaying.
        let mut apply = ApplyDefaults::new(defaults(Some("opus"), None));
        let options = vec![select("model", "model", "sonnet", &["sonnet", "opus"])];
        assert_eq!(
            apply.on_event(&SessionEvent::ConfigChanged { options }),
            None
        );
    }
}
