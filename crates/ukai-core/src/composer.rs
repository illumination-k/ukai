//! What the composer offers to complete the draft with: the slash commands
//! it may be starting, or the checkout's files for the `@` it ends with, and
//! the listing of files those come from. Also how a message handed back
//! from the queue joins what is being written.

use std::time::{Duration, Instant};

use ukai_acp::SlashCommand;
use ukai_acp::checkout::FileListing;

use crate::{mention, slash};

/// Suggestions listed at once.
pub const MAX_SUGGESTIONS: usize = 8;

/// How long a listing of the checkout's files is used before an `@` reads
/// it again; a failed one is retried sooner.
const FILES_FRESH: Duration = Duration::from_secs(30);
const FILES_RETRY: Duration = Duration::from_secs(5);

/// The last listing of the checkout's files, read when an `@` is typed.
#[derive(Debug, Clone, PartialEq)]
pub enum Files {
    Unread,
    Loading,
    Listed(FileListing, Instant),
    Failed(String, Instant),
}

impl Files {
    /// Whether an `@` typed at `now` should read the files again.
    pub fn stale(&self, now: Instant) -> bool {
        match self {
            Self::Unread => true,
            Self::Loading => false,
            Self::Listed(_, at) => now.duration_since(*at) > FILES_FRESH,
            Self::Failed(_, at) => now.duration_since(*at) > FILES_RETRY,
        }
    }
}

/// What the composer lists under the draft.
#[derive(Debug, Clone, Default, PartialEq)]
pub enum Suggestions {
    #[default]
    None,
    Commands(Vec<SlashCommand>),
    /// The files for the `@` the draft ends with, relative to the checkout's
    /// `root`, and a note when there is nothing to pick.
    Files {
        found: Vec<String>,
        root: String,
        note: Option<String>,
    },
}

impl Suggestions {
    /// What `draft` may be completed with: the agent's `commands` it may be
    /// starting, else the `files` its `@` may name.
    pub fn of(draft: &str, commands: &[SlashCommand], files: &Files) -> Self {
        let commands = slash::suggestions(draft, commands);
        if !commands.is_empty() {
            return Self::Commands(
                commands
                    .into_iter()
                    .take(MAX_SUGGESTIONS)
                    .cloned()
                    .collect(),
            );
        }
        let Some(query) = mention::query(draft) else {
            return Self::None;
        };
        let (found, root, note) = match files {
            Files::Listed(listing, _) => {
                let found: Vec<String> =
                    mention::suggestions(query, &listing.files, MAX_SUGGESTIONS)
                        .into_iter()
                        .cloned()
                        .collect();
                let note = found.is_empty().then(|| "No matching files".to_owned());
                (found, listing.root.clone(), note)
            }
            Files::Failed(message, _) => (Vec::new(), String::new(), Some(message.clone())),
            Files::Unread | Files::Loading => {
                (Vec::new(), String::new(), Some("Listing files…".to_owned()))
            }
        };
        Self::Files { found, root, note }
    }

    /// How many there are to pick from.
    pub fn len(&self) -> usize {
        match self {
            Self::None => 0,
            Self::Commands(commands) => commands.len(),
            Self::Files { found, .. } => found.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// The highlight `by` places on from `current` among `len` suggestions,
/// wrapping around either end.
pub fn step(current: usize, len: usize, by: isize) -> usize {
    if len == 0 {
        return 0;
    }
    let len = isize::try_from(len).unwrap_or(isize::MAX);
    let current = isize::try_from(current).unwrap_or(0);
    usize::try_from((current + by).rem_euclid(len)).unwrap_or(0)
}

/// The draft with `message`, handed back from the queue, in front of what
/// was being written.
pub fn restored(message: &str, draft: &str) -> String {
    match (message.is_empty(), draft.is_empty()) {
        (_, true) => message.to_owned(),
        (true, false) => draft.to_owned(),
        (false, false) => format!("{message}\n\n{draft}"),
    }
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

    fn listed(files: &[&str]) -> Files {
        Files::Listed(
            FileListing {
                root: "/work".into(),
                files: files.iter().map(|file| (*file).to_owned()).collect(),
                truncated: false,
            },
            Instant::now(),
        )
    }

    #[rstest]
    #[case::unread(Files::Unread, 0, true)]
    #[case::loading(Files::Loading, 60, false)]
    #[case::fresh_listing(listed(&[]), 29, false)]
    #[case::old_listing(listed(&[]), 31, true)]
    #[case::recent_failure(Files::Failed("no".into(), Instant::now()), 4, false)]
    #[case::failure_to_retry(Files::Failed("no".into(), Instant::now()), 6, true)]
    fn reads_the_files_again_when_stale(
        #[case] files: Files,
        #[case] seconds_later: u64,
        #[case] stale: bool,
    ) {
        let now = Instant::now() + Duration::from_secs(seconds_later);
        assert_eq!(files.stale(now), stale);
    }

    #[test]
    fn suggests_commands_before_files() {
        let commands = [command("review"), command("init")];
        assert_eq!(
            Suggestions::of("/re", &commands, &listed(&["review.md"])),
            Suggestions::Commands(vec![command("review")])
        );
    }

    #[test]
    fn suggests_at_most_a_screenful() {
        let commands: Vec<SlashCommand> = (0..20).map(|i| command(&format!("c{i}"))).collect();
        assert_eq!(
            Suggestions::of("/", &commands, &Files::Unread).len(),
            MAX_SUGGESTIONS
        );
    }

    #[rstest]
    #[case::matching(listed(&["src/main.rs", "README.md"]), "see @main", &["src/main.rs"], None)]
    #[case::none_matching(listed(&["README.md"]), "see @zzz", &[], Some("No matching files"))]
    #[case::listing(Files::Loading, "see @main", &[], Some("Listing files…"))]
    #[case::failed(Files::Failed("cannot list".into(), Instant::now()), "@x", &[], Some("cannot list"))]
    fn suggests_files_for_an_at(
        #[case] files: Files,
        #[case] draft: &str,
        #[case] found: &[&str],
        #[case] note: Option<&str>,
    ) {
        let Suggestions::Files {
            found: got,
            note: got_note,
            ..
        } = Suggestions::of(draft, &[], &files)
        else {
            panic!("no file suggestions for {draft:?}");
        };
        assert_eq!(got, found);
        assert_eq!(got_note.as_deref(), note);
    }

    #[test]
    fn suggests_nothing_for_plain_text() {
        let suggestions = Suggestions::of("hello", &[command("review")], &listed(&["a"]));
        assert_eq!(suggestions, Suggestions::None);
        assert!(suggestions.is_empty());
    }

    #[rstest]
    #[case::down(0, 3, 1, 1)]
    #[case::down_wraps(2, 3, 1, 0)]
    #[case::up_wraps(0, 3, -1, 2)]
    #[case::nothing_listed(0, 0, 1, 0)]
    fn steps_through_the_suggestions(
        #[case] current: usize,
        #[case] len: usize,
        #[case] by: isize,
        #[case] expected: usize,
    ) {
        assert_eq!(step(current, len, by), expected);
    }

    #[rstest]
    #[case::into_empty("queued", "", "queued")]
    #[case::empty_message("", "draft", "draft")]
    #[case::ahead_of_the_draft("queued", "draft", "queued\n\ndraft")]
    fn hands_a_message_back_ahead_of_the_draft(
        #[case] message: &str,
        #[case] draft: &str,
        #[case] expected: &str,
    ) {
        assert_eq!(restored(message, draft), expected);
    }
}
