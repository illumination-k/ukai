//! Fuzzy matching for the command palette.

/// Characters after which a match counts as the start of a word.
const SEPARATORS: &[char] = &[' ', '/', '\\', '-', '_', '.', '·', ':'];

/// How well `text` matches `query`, higher being better; `None` when some
/// whitespace-separated word of `query` does not appear in `text` as a
/// subsequence. Case is ignored, and matches at word starts and runs of
/// consecutive characters score higher.
pub fn score(query: &str, text: &str) -> Option<i64> {
    let text: Vec<char> = text.chars().flat_map(char::to_lowercase).collect();
    query
        .split_whitespace()
        .map(|word| word_score(word, &text))
        .sum()
}

fn word_score(word: &str, text: &[char]) -> Option<i64> {
    let mut score = 0;
    let mut from = 0;
    let mut previous: Option<usize> = None;
    for wanted in word.chars().flat_map(char::to_lowercase) {
        let at = from + text.get(from..)?.iter().position(|&c| c == wanted)?;
        let word_start = at == 0 || text.get(at - 1).is_some_and(|c| SEPARATORS.contains(c));
        score += 1;
        if word_start {
            score += 8;
        }
        match previous {
            Some(p) if p + 1 == at => score += 5,
            Some(p) => score -= i64::try_from(at - p - 1).unwrap_or(i64::MAX).min(5),
            None => score -= i64::try_from(at).unwrap_or(i64::MAX).min(5),
        }
        previous = Some(at);
        from = at + 1;
    }
    Some(score)
}

/// The items whose label matches `query`, best first; ties keep their order.
pub fn filter<T>(query: &str, items: impl IntoIterator<Item = (String, T)>) -> Vec<T> {
    let mut scored: Vec<(i64, T)> = items
        .into_iter()
        .filter_map(|(label, item)| Some((score(query, &label)?, item)))
        .collect();
    scored.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
    scored.into_iter().map(|(_, item)| item).collect()
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case::empty("", "anything")]
    #[case::subsequence("cld", "Open claude")]
    #[case::case_insensitive("OPEN", "open claude")]
    #[case::words_in_any_order("ukai claude", "claude · ukai")]
    fn matches(#[case] query: &str, #[case] text: &str) {
        assert!(score(query, text).is_some());
    }

    #[rstest]
    #[case::missing_char("x", "claude")]
    #[case::out_of_order("dc", "cd")]
    #[case::one_word_missing("claude zzz", "claude")]
    fn rejects(#[case] query: &str, #[case] text: &str) {
        assert_eq!(score(query, text), None);
    }

    #[test]
    fn prefers_word_starts_and_runs() {
        let ranked = filter(
            "rep",
            [
                ("Close pane (scattered r-e-p)".to_owned(), "scattered"),
                ("Open report".to_owned(), "word start"),
                ("prepare".to_owned(), "inside a word"),
            ],
        );
        assert_eq!(ranked, ["word start", "inside a word", "scattered"]);
    }

    #[test]
    fn empty_query_keeps_order() {
        let ranked = filter("", [("b".to_owned(), 1), ("a".to_owned(), 2)]);
        assert_eq!(ranked, [1, 2]);
    }
}
