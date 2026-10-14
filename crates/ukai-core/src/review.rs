//! Reviewing the file changes an agent's tool calls report: line diffs to
//! show, and comments on their lines folded into the next prompt.

use similar::{ChangeTag, TextDiff};

/// Unchanged lines shown around each change.
const CONTEXT: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    Context,
    Added,
    Removed,
}

/// One line of a diff, with its 1-based number on each side it exists on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffLine {
    pub kind: LineKind,
    pub old: Option<usize>,
    pub new: Option<usize>,
    pub text: String,
}

/// A run of changed lines with their context.
pub type Hunk = Vec<DiffLine>;

/// The hunks turning `old` into `new`; `None` for `old` is a new file.
pub fn hunks(old: Option<&str>, new: &str) -> Vec<Hunk> {
    let diff = TextDiff::from_lines(old.unwrap_or(""), new);
    diff.grouped_ops(CONTEXT)
        .iter()
        .map(|group| {
            group
                .iter()
                .flat_map(|op| diff.iter_changes(op))
                .map(|change| DiffLine {
                    kind: match change.tag() {
                        ChangeTag::Equal => LineKind::Context,
                        ChangeTag::Insert => LineKind::Added,
                        ChangeTag::Delete => LineKind::Removed,
                    },
                    old: change.old_index().map(|i| i + 1),
                    new: change.new_index().map(|i| i + 1),
                    text: change.value().trim_end_matches(['\n', '\r']).to_owned(),
                })
                .collect()
        })
        .collect()
}

/// Lines added and removed across `hunks`.
pub fn stats(hunks: &[Hunk]) -> (usize, usize) {
    hunks
        .iter()
        .flatten()
        .fold((0, 0), |(added, removed), line| match line.kind {
            LineKind::Added => (added + 1, removed),
            LineKind::Removed => (added, removed + 1),
            LineKind::Context => (added, removed),
        })
}

/// A comment on one line of a diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewComment {
    /// The file as the user sees it, e.g. relative to the project root.
    pub path: String,
    pub line: DiffLine,
    pub body: String,
}

impl ReviewComment {
    /// `path:line`, naming the old line for a removed one.
    pub fn location(&self) -> String {
        match (self.line.kind, self.line.new, self.line.old) {
            (LineKind::Removed, _, Some(old)) => format!("{} (removed line {old})", self.path),
            (_, Some(new), _) => format!("{}:{new}", self.path),
            _ => self.path.clone(),
        }
    }
}

/// The prompt that sends `comments` with the user's `message`, if any.
pub fn review_prompt(comments: &[ReviewComment], message: &str) -> String {
    let message = message.trim();
    if comments.is_empty() {
        return message.to_owned();
    }
    let mut prompt = String::from("Review comments on your changes:\n");
    for comment in comments {
        let marker = match comment.line.kind {
            LineKind::Added => "+",
            LineKind::Removed => "-",
            LineKind::Context => " ",
        };
        prompt.push_str(&format!(
            "\n- `{}`: {}\n  > {marker}{}\n",
            comment.location(),
            comment.body.trim(),
            comment.line.text
        ));
    }
    if !message.is_empty() {
        prompt.push('\n');
        prompt.push_str(message);
    }
    prompt
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    fn line(kind: LineKind, old: Option<usize>, new: Option<usize>, text: &str) -> DiffLine {
        DiffLine {
            kind,
            old,
            new,
            text: text.into(),
        }
    }

    #[test]
    fn numbers_lines_on_both_sides() {
        let hunks = hunks(Some("a\nb\nc\n"), "a\nB\nc\nd\n");
        assert_eq!(
            hunks,
            vec![vec![
                line(LineKind::Context, Some(1), Some(1), "a"),
                line(LineKind::Removed, Some(2), None, "b"),
                line(LineKind::Added, None, Some(2), "B"),
                line(LineKind::Context, Some(3), Some(3), "c"),
                line(LineKind::Added, None, Some(4), "d"),
            ]]
        );
        assert_eq!(stats(&hunks), (2, 1));
    }

    #[test]
    fn new_file_is_all_added() {
        let hunks = hunks(None, "x\ny");
        assert_eq!(stats(&hunks), (2, 0));
        assert!(hunks.iter().flatten().all(|l| l.kind == LineKind::Added));
    }

    #[test]
    fn distant_changes_form_separate_hunks() {
        let old: String = (1..=20).map(|i| format!("{i}\n")).collect();
        let new = old.replace("2\n", "two\n").replace("19\n", "nineteen\n");
        assert_eq!(hunks(Some(&old), &new).len(), 2);
    }

    #[test]
    fn unchanged_text_has_no_hunks() {
        assert!(hunks(Some("same\n"), "same\n").is_empty());
    }

    fn comment(line: DiffLine, body: &str) -> ReviewComment {
        ReviewComment {
            path: "src/a.rs".into(),
            line,
            body: body.into(),
        }
    }

    #[rstest]
    #[case::added(line(LineKind::Added, None, Some(4), "x"), "src/a.rs:4")]
    #[case::context(line(LineKind::Context, Some(2), Some(3), "x"), "src/a.rs:3")]
    #[case::removed(
        line(LineKind::Removed, Some(7), None, "x"),
        "src/a.rs (removed line 7)"
    )]
    fn locations(#[case] line: DiffLine, #[case] expected: &str) {
        assert_eq!(comment(line, "").location(), expected);
    }

    #[test]
    fn prompt_lists_comments_then_the_message() {
        let comments = [
            comment(
                line(LineKind::Added, None, Some(4), "let x = 1;"),
                " rename x ",
            ),
            comment(
                line(LineKind::Removed, Some(9), None, "keep()"),
                "why removed?",
            ),
        ];
        assert_eq!(
            review_prompt(&comments, " and add a test \n"),
            "Review comments on your changes:\n\
             \n- `src/a.rs:4`: rename x\n  > +let x = 1;\n\
             \n- `src/a.rs (removed line 9)`: why removed?\n  > -keep()\n\
             \nand add a test"
        );
    }

    #[test]
    fn prompt_without_comments_is_the_message() {
        assert_eq!(review_prompt(&[], " hi "), "hi");
    }
}
