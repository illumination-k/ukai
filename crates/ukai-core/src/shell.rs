//! `!` in the composer: a command the user runs in the agent's checkout
//! (`ukai_acp::checkout`), shown in the transcript and sent to the agent
//! with the next prompt, so it knows what the user saw.

use ukai_acp::checkout::CommandOutput;

/// The command a draft runs: what follows a leading `!`.
pub fn command(draft: &str) -> Option<&str> {
    let command = draft.trim_start().strip_prefix('!')?.trim();
    (!command.is_empty()).then_some(command)
}

/// How a run ended, for a person: `exit 1`, `timed out`.
pub fn outcome(output: &CommandOutput) -> String {
    match (output.timed_out, output.exit_code) {
        (true, _) => "timed out".to_owned(),
        (false, Some(code)) => format!("exit {code}"),
        (false, None) => "killed".to_owned(),
    }
}

/// `text` with the commands the user ran since the last prompt before it.
pub fn prompt(runs: &[(&str, &CommandOutput)], text: &str) -> String {
    if runs.is_empty() {
        return text.to_owned();
    }
    let mut prompt = String::from("I ran these commands in the workspace:\n");
    for (command, output) in runs {
        let shown = format!("$ {command}\n{}", output.output);
        let fence = "`".repeat(longest_run(&shown, '`').max(2) + 1);
        let shown = shown.trim_end();
        let cut = if output.truncated {
            ", output cut to its end"
        } else {
            ""
        };
        prompt.push_str(&format!(
            "\n{fence}console\n{shown}\n{fence}\n({}{cut})\n",
            outcome(output)
        ));
    }
    if !text.is_empty() {
        prompt.push('\n');
        prompt.push_str(text);
    }
    prompt
}

fn longest_run(text: &str, c: char) -> usize {
    let mut longest = 0;
    let mut run = 0;
    for next in text.chars() {
        run = if next == c { run + 1 } else { 0 };
        longest = longest.max(run);
    }
    longest
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case::command("!cargo test", Some("cargo test"))]
    #[case::spaced("  ! ls -la  ", Some("ls -la"))]
    #[case::bang_only("!", None)]
    #[case::prose("hi!", None)]
    fn finds_the_command(#[case] draft: &str, #[case] expected: Option<&str>) {
        assert_eq!(command(draft), expected);
    }

    fn output(text: &str, exit_code: Option<i32>) -> CommandOutput {
        CommandOutput {
            exit_code,
            output: text.into(),
            ..CommandOutput::default()
        }
    }

    #[test]
    fn no_runs_leave_the_text_alone() {
        assert_eq!(prompt(&[], "hi"), "hi");
    }

    #[test]
    fn runs_go_before_the_text() {
        let failed = output("error: boom\n", Some(1));
        assert_eq!(
            prompt(&[("cargo test", &failed)], "fix it"),
            "I ran these commands in the workspace:\n\n```console\n$ cargo test\nerror: boom\n```\n(exit 1)\n\nfix it"
        );
    }

    #[test]
    fn a_fence_in_the_output_gets_a_longer_one() {
        let fenced = output("```rust\nfn x() {}\n```\n", Some(0));
        let sent = prompt(&[("cat README.md", &fenced)], "");
        assert!(sent.contains("````console\n"), "{sent}");
        assert!(sent.ends_with("````\n(exit 0)\n"), "{sent}");
    }

    #[test]
    fn says_how_a_run_ended() {
        let timed_out = CommandOutput {
            timed_out: true,
            ..CommandOutput::default()
        };
        assert_eq!(outcome(&timed_out), "timed out");
        assert_eq!(outcome(&output("", None)), "killed");
    }
}
