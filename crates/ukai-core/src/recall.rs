//! Bringing back the prompts sent earlier in a session, as a shell does: ↑
//! in an empty composer recalls the newest, again the one before it, and ↓
//! walks forward to an empty composer again.
//!
//! Recalling only takes the arrows while the draft is empty or still the
//! prompt it recalled, so they move the caret in anything the user wrote.

use crate::{Entry, Transcript};

/// Where ↑ and ↓ have got to among a session's prompts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Recall {
    /// Prompts back from the newest: 0 is the newest.
    back: Option<usize>,
}

impl Recall {
    /// The draft ↑ leaves: the prompt before the one recalled, or the newest
    /// when the draft is empty. `None` when ↑ is the caret's.
    pub fn older(&mut self, prompts: &[String], draft: &str) -> Option<String> {
        let back = match self.browsing(prompts, draft) {
            Some(back) => back + 1,
            None if draft.is_empty() => 0,
            None => return None,
        };
        let index = prompts.len().checked_sub(back + 1)?;
        self.back = Some(back);
        Some(prompts[index].clone())
    }

    /// The draft ↓ leaves: the prompt after the one recalled, or an empty
    /// draft past the newest. `None` when ↓ is the caret's.
    pub fn newer(&mut self, prompts: &[String], draft: &str) -> Option<String> {
        let back = self.browsing(prompts, draft)?;
        if back == 0 {
            self.back = None;
            return Some(String::new());
        }
        self.back = Some(back - 1);
        Some(prompts[prompts.len() - back].clone())
    }

    /// How far back the draft is, if it is still the prompt recalled.
    fn browsing(&mut self, prompts: &[String], draft: &str) -> Option<usize> {
        let back = self.back?;
        let current = prompts.len().checked_sub(back + 1).map(|i| &prompts[i]);
        if current.is_some_and(|prompt| prompt == draft) {
            Some(back)
        } else {
            self.back = None;
            None
        }
    }
}

/// The prompts sent in `transcript` and the commands run with `!`, as they
/// were written, oldest first, without one repeated right after itself.
pub fn prompts(transcript: &Transcript) -> Vec<String> {
    let mut prompts: Vec<String> = Vec::new();
    for entry in &transcript.entries {
        let written = match entry {
            Entry::User { text, .. } => text.clone(),
            Entry::Shell { command, .. } => format!("!{command}"),
            _ => continue,
        };
        if !written.trim().is_empty() && prompts.last() != Some(&written) {
            prompts.push(written);
        }
    }
    prompts
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sent() -> Vec<String> {
        vec!["one".into(), "two".into(), "three".into()]
    }

    #[test]
    fn up_in_an_empty_draft_walks_back_and_stops_at_the_oldest() {
        let prompts = sent();
        let mut recall = Recall::default();
        let mut draft = String::new();
        let mut seen = Vec::new();
        while let Some(older) = recall.older(&prompts, &draft) {
            seen.push(older.clone());
            draft = older;
            if seen.len() > 5 {
                break;
            }
        }
        assert_eq!(seen, ["three", "two", "one"]);
        assert_eq!(draft, "one");
    }

    /// The drafts a run of arrow presses leaves, from an empty draft;
    /// `None` where a press is left to the caret.
    fn press(keys: &str) -> Vec<Option<String>> {
        let prompts = sent();
        let mut recall = Recall::default();
        let mut draft = String::new();
        keys.chars()
            .map(|key| {
                let next = match key {
                    '↑' => recall.older(&prompts, &draft),
                    _ => recall.newer(&prompts, &draft),
                };
                if let Some(next) = &next {
                    draft.clone_from(next);
                }
                next
            })
            .collect()
    }

    #[test]
    fn down_walks_forward_to_an_empty_draft() {
        assert_eq!(
            press("↑↑↓↓↓"),
            [
                Some("three".into()),
                Some("two".into()),
                Some("three".into()),
                Some(String::new()),
                None,
            ]
        );
    }

    #[test]
    fn the_arrows_are_the_carets_in_a_draft_the_user_wrote() {
        let prompts = sent();
        let mut recall = Recall::default();
        assert_eq!(recall.older(&prompts, "my own words"), None);
        assert_eq!(recall.newer(&prompts, "my own words"), None);
    }

    #[test]
    fn editing_a_recalled_prompt_ends_the_walk() {
        let prompts = sent();
        let mut recall = Recall::default();
        assert_eq!(recall.older(&prompts, "").as_deref(), Some("three"));
        assert_eq!(recall.older(&prompts, "three, edited"), None);
        assert_eq!(recall.newer(&prompts, "three, edited"), None);
        // Cleared again, ↑ starts over from the newest.
        assert_eq!(recall.older(&prompts, "").as_deref(), Some("three"));
    }

    #[test]
    fn nothing_sent_leaves_the_arrows_alone() {
        let mut recall = Recall::default();
        assert_eq!(recall.older(&[], ""), None);
        assert_eq!(recall.newer(&[], ""), None);
    }

    #[test]
    fn prompts_and_commands_skip_agent_messages_and_repeats() {
        let mut transcript = Transcript::default();
        transcript.push_prompt("fix it", Vec::new());
        transcript.push_prompt("fix it", Vec::new());
        transcript.entries.push(Entry::Agent {
            text: "done".into(),
        });
        transcript.start_shell("cargo test");
        transcript.push_prompt("thanks", Vec::new());
        assert_eq!(prompts(&transcript), ["fix it", "!cargo test", "thanks"]);
    }
}
