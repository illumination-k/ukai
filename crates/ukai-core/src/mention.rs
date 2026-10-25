//! `@` in the composer: mentioning a file of the checkout the agent works
//! in. Picking one writes `@path` into the draft and keeps the file, which
//! goes with the prompt as a link (ACP's `resource_link`) while `@path` is
//! still in its text; the agent reads the file itself.
//!
//! Only an `@` the draft ends with is completed: the composer does not know
//! where the caret is.

use crate::palette;

/// A file picked from the list: how the draft names it, and its `file:` URI
/// on the agent's side.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mention {
    pub path: String,
    pub uri: String,
}

/// What the `@` the draft ends with asks for: the text after an `@` that
/// starts a word, with no whitespace after it yet.
pub fn query(draft: &str) -> Option<&str> {
    let at = draft.rfind('@')?;
    let before = &draft[..at];
    let query = &draft[at + 1..];
    let starts_word = before.chars().next_back().is_none_or(char::is_whitespace);
    (starts_word && !query.contains(char::is_whitespace)).then_some(query)
}

/// The files `query` may be after, best first. An empty query lists the
/// files in order.
pub fn suggestions<'a>(query: &str, files: &'a [String], limit: usize) -> Vec<&'a String> {
    if query.is_empty() {
        return files.iter().take(limit).collect();
    }
    let mut found = palette::filter(query, files.iter().map(|file| (file.clone(), file)));
    found.truncate(limit);
    found
}

/// The draft after picking `path` for the `@` it ends with.
pub fn complete(draft: &str, path: &str) -> String {
    let at = draft.rfind('@').unwrap_or(draft.len());
    format!("{}@{path} ", &draft[..at])
}

/// The mentions `text` still names, each once.
pub fn named<'a>(text: &str, mentions: &'a [Mention]) -> Vec<&'a Mention> {
    let mut named: Vec<&Mention> = Vec::new();
    for mention in mentions {
        let written = format!("@{}", mention.path);
        let in_text = text.match_indices(&written).any(|(at, _)| {
            let after = text[at + written.len()..].chars().next();
            after.is_none_or(|c| c.is_whitespace() || ",.;:)!?".contains(c))
        });
        if in_text && !named.iter().any(|m| m.path == mention.path) {
            named.push(mention);
        }
    }
    named
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case::bare("@", Some(""))]
    #[case::partial("look at @src/ma", Some("src/ma"))]
    #[case::finished("look at @src/main.rs ", None)]
    #[case::email("mail me@example", None)]
    #[case::none("no mention", None)]
    #[case::after_newline("fix\n@lib", Some("lib"))]
    fn finds_the_query_the_draft_ends_with(#[case] draft: &str, #[case] expected: Option<&str>) {
        assert_eq!(query(draft), expected);
    }

    fn files() -> Vec<String> {
        ["Cargo.toml", "src/lib.rs", "src/main.rs", "tests/main.rs"]
            .map(String::from)
            .to_vec()
    }

    #[test]
    fn an_empty_query_lists_files_in_order() {
        let files = files();
        assert_eq!(suggestions("", &files, 2), ["Cargo.toml", "src/lib.rs"]);
    }

    #[test]
    fn a_query_matches_fuzzily_best_first() {
        let files = files();
        assert_eq!(
            suggestions("main", &files, 8),
            ["src/main.rs", "tests/main.rs"]
        );
        assert!(suggestions("zzz", &files, 8).is_empty());
    }

    #[test]
    fn completing_replaces_the_query() {
        assert_eq!(
            complete("look at @src/ma", "src/main.rs"),
            "look at @src/main.rs "
        );
        assert_eq!(complete("@", "Cargo.toml"), "@Cargo.toml ");
    }

    fn mention(path: &str) -> Mention {
        Mention {
            path: path.into(),
            uri: format!("file:///w/{path}"),
        }
    }

    #[test]
    fn only_mentions_still_in_the_text_go() {
        let mentions = [mention("a.rs"), mention("b.rs"), mention("a.rs")];
        let named: Vec<&str> = named("compare @a.rs, and @b.rsx", &mentions)
            .into_iter()
            .map(|m| m.path.as_str())
            .collect();
        assert_eq!(named, ["a.rs"]);
    }
}
