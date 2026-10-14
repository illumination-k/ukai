//! Completion in the composer: the agent's `/commands` and the project's
//! files behind `@`.
//!
//! Completion looks at the end of the draft, so the caret is taken to be
//! there — the same bargain a shell prompt makes, and it keeps the UI from
//! having to track a caret through every edit.

use std::path::{Path, PathBuf};

/// What the end of a draft is asking for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Trigger {
    /// A command, written `/name`; only ever the whole draft, because that is
    /// where agents accept one. Carries the text after the slash.
    Command(String),
    /// A file reference, written `@path` anywhere in the draft. Carries the
    /// text after the at sign.
    File(String),
}

/// Directory names never walked for [`files`]: build output and dependency
/// trees, which are large and not what an `@` reference means.
const SKIPPED: &[&str] = &["target", "node_modules", "dist", "__pycache__"];

/// What `draft` is completing, or `None` when its last word is finished (it
/// ends in whitespace) or is plain text.
pub fn trigger(draft: &str) -> Option<Trigger> {
    let word = last_word(draft)?;
    if let Some(name) = word.strip_prefix('/') {
        // A command is the prompt's first word or not a command at all.
        return (draft.trim_start() == word).then(|| Trigger::Command(name.to_owned()));
    }
    word.strip_prefix('@')
        .map(|path| Trigger::File(path.to_owned()))
}

/// `draft` with the word [`trigger`] looked at replaced by `value`, plus a
/// space so the user can keep typing.
pub fn complete(draft: &str, value: &str) -> String {
    let start = last_word(draft).map_or(draft.len(), |word| draft.len() - word.len());
    format!("{}{value} ", &draft[..start])
}

/// The word at the end of `draft`: `None` when it is empty or ends in
/// whitespace, so nothing is being typed.
fn last_word(draft: &str) -> Option<&str> {
    let word = draft.split_whitespace().next_back()?;
    // `split_whitespace` would hand back the last word of "foo " as well.
    (!draft.ends_with(char::is_whitespace)).then_some(word)
}

/// Files under `root` to offer for a `@` reference, relative to it, in path
/// order, at most `limit` of them. Hidden entries and [`SKIPPED`] directories
/// are left out, as are unreadable directories.
pub fn files(root: &Path, limit: usize) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut pending = vec![PathBuf::new()];
    while let Some(relative) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(root.join(&relative)) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with('.') || SKIPPED.contains(&name.as_ref()) {
                continue;
            }
            let path = relative.join(name.as_ref());
            match entry.file_type() {
                Ok(file_type) if file_type.is_dir() => pending.push(path),
                // A symlink is followed only as a file, so a loop cannot
                // make the walk run forever.
                Ok(_) => found.push(path),
                Err(_) => {}
            }
            if found.len() >= limit {
                pending.clear();
                break;
            }
        }
    }
    found.sort();
    found
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case::command("/pl", Some(Trigger::Command("pl".into())))]
    #[case::bare_slash("/", Some(Trigger::Command(String::new())))]
    #[case::command_after_spaces("  /plan", Some(Trigger::Command("plan".into())))]
    #[case::command_not_first("do /plan", None)]
    #[case::finished_command("/plan ", None)]
    #[case::file("look at @src/m", Some(Trigger::File("src/m".into())))]
    #[case::bare_at("@", Some(Trigger::File(String::new())))]
    #[case::file_first("@a.rs", Some(Trigger::File("a.rs".into())))]
    #[case::finished_file("@a.rs ", None)]
    #[case::plain_text("hello", None)]
    #[case::empty("", None)]
    fn triggers(#[case] draft: &str, #[case] expected: Option<Trigger>) {
        assert_eq!(trigger(draft), expected);
    }

    #[rstest]
    #[case::command("/pl", "/plan", "/plan ")]
    #[case::file("look at @src/m", "@src/main.rs", "look at @src/main.rs ")]
    #[case::keeps_earlier_words("a @b @c", "@ccc", "a @b @ccc ")]
    #[case::appends_when_finished("done ", "@a", "done @a ")]
    fn completes(#[case] draft: &str, #[case] value: &str, #[case] expected: &str) {
        assert_eq!(complete(draft, value), expected);
    }

    #[test]
    fn lists_project_files_without_hidden_or_build_output() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let root = dir.path();
        for path in ["src/main.rs", "target/debug/x", ".git/config", "README.md"] {
            let file = root.join(path);
            if let Some(parent) = file.parent() {
                std::fs::create_dir_all(parent).unwrap_or_else(|e| panic!("{e}"));
            }
            std::fs::write(&file, "").unwrap_or_else(|e| panic!("{e}"));
        }
        assert_eq!(
            files(root, 100),
            [PathBuf::from("README.md"), PathBuf::from("src/main.rs")]
        );
    }

    #[test]
    fn stops_at_the_limit() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        for name in ["a", "b", "c"] {
            std::fs::write(dir.path().join(name), "").unwrap_or_else(|e| panic!("{e}"));
        }
        assert_eq!(files(dir.path(), 2).len(), 2);
    }
}
