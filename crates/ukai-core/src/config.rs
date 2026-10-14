//! Presenting the settings an agent lets the user change.

use ukai_acp::ConfigChoice;

/// A select's choices as runs under the same group, in the agent's order.
pub fn choice_groups(choices: &[ConfigChoice]) -> Vec<(Option<&str>, &[ConfigChoice])> {
    choices
        .chunk_by(|a, b| a.group == b.group)
        .map(|run| (run[0].group.as_deref(), run))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

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
