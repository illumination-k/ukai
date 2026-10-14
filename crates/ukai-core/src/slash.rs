//! Completing the slash commands an agent offers in the composer.
//!
//! ACP has no request for running a command: a prompt whose text starts with
//! `/name` is one, so completion only has to write that prefix.

use ukai_acp::SlashCommand;

use crate::palette;

/// The commands `draft` may be starting, best first: `draft` is a `/` and
/// the start of a name, with nothing after it yet.
pub fn suggestions<'a>(draft: &str, commands: &'a [SlashCommand]) -> Vec<&'a SlashCommand> {
    let Some(query) = draft.strip_prefix('/') else {
        return Vec::new();
    };
    if query.contains(char::is_whitespace) {
        return Vec::new();
    }
    palette::filter(
        query,
        commands
            .iter()
            .map(|command| (command.name.clone(), command)),
    )
}

/// The draft after choosing `command`, ready for its input.
pub fn completion(command: &SlashCommand) -> String {
    format!("/{} ", command.name)
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    fn command(name: &str) -> SlashCommand {
        SlashCommand {
            name: name.into(),
            description: String::new(),
            hint: None,
        }
    }

    fn names(draft: &str) -> Vec<String> {
        let commands = [command("compact"), command("review"), command("clear")];
        suggestions(draft, &commands)
            .into_iter()
            .map(|c| c.name.clone())
            .collect()
    }

    #[test]
    fn a_bare_slash_lists_every_command_in_order() {
        assert_eq!(names("/"), ["compact", "review", "clear"]);
    }

    #[test]
    fn a_partial_name_filters() {
        assert_eq!(names("/cl"), ["clear"]);
    }

    #[rstest]
    #[case::no_slash("compact")]
    #[case::slash_later(" /compact")]
    #[case::input_started("/review 12")]
    #[case::no_match("/zzz")]
    fn nothing_to_suggest(#[case] draft: &str) {
        assert!(names(draft).is_empty());
    }

    #[test]
    fn completion_leaves_room_for_input() {
        assert_eq!(completion(&command("review")), "/review ");
    }
}
