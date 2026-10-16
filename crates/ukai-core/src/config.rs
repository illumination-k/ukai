//! Presenting the settings an agent lets the user change.

use ukai_acp::ConfigChoice;

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

#[cfg(test)]
mod tests {
    use rstest::rstest;

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
}
