//! Which entries of a transcript a pane mounts. The web view pays for every
//! mounted entry on each frame it draws, each key typed in the composer
//! included, so a long transcript shows only its end: more as the user
//! reaches back for it, and the end alone again once they return to it.

/// How many entries the end of a transcript shows, and how many more each
/// step back adds.
pub const PAGE: usize = 100;

/// Where the mounted entries start.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Tail {
    /// The first entry kept mounted while the view is away from the end:
    /// what the user reads there stays in place as entries arrive.
    held: Option<usize>,
}

impl Tail {
    /// The index of the first entry to mount, of `len`.
    pub fn start(self, len: usize) -> usize {
        let end = len.saturating_sub(PAGE);
        // A transcript that started over is shorter than what was held.
        self.held.map_or(end, |held| held.min(end))
    }

    /// The view came to the end of the `len` entries (`following`), or left
    /// it.
    pub fn follow(&mut self, following: bool, len: usize) {
        self.held = if following {
            None
        } else {
            Some(self.start(len))
        };
    }

    /// Show a page more of the `len` entries.
    pub fn earlier(&mut self, len: usize) {
        self.held = Some(self.start(len).saturating_sub(PAGE));
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case::empty(0, 0)]
    #[case::short(PAGE, 0)]
    #[case::long(PAGE + 1, 1)]
    #[case::very_long(10 * PAGE, 9 * PAGE)]
    fn the_end_is_shown_while_following(#[case] len: usize, #[case] start: usize) {
        assert_eq!(Tail::default().start(len), start);
    }

    #[test]
    fn entries_stay_mounted_while_the_view_is_away_from_the_end() {
        let mut tail = Tail::default();
        tail.follow(false, 3 * PAGE);
        assert_eq!(tail.start(3 * PAGE), 2 * PAGE);
        // The agent goes on: nothing the user may be reading is taken away.
        assert_eq!(tail.start(5 * PAGE), 2 * PAGE);
    }

    #[test]
    fn returning_to_the_end_shows_the_end_alone_again() {
        let mut tail = Tail::default();
        tail.follow(false, 3 * PAGE);
        tail.follow(true, 5 * PAGE);
        assert_eq!(tail.start(5 * PAGE), 4 * PAGE);
    }

    #[test]
    fn each_step_back_adds_a_page_down_to_the_first_entry() {
        let len = 2 * PAGE + 30;
        let mut tail = Tail::default();
        tail.follow(false, len);
        assert_eq!(tail.start(len), PAGE + 30);
        tail.earlier(len);
        assert_eq!(tail.start(len), 30);
        tail.earlier(len);
        assert_eq!(tail.start(len), 0);
        tail.earlier(len);
        assert_eq!(tail.start(len), 0);
    }

    #[test]
    fn a_transcript_that_started_over_shows_its_end() {
        let mut tail = Tail::default();
        tail.follow(false, 5 * PAGE);
        assert_eq!(tail.start(3), 0);
        assert_eq!(tail.start(2 * PAGE), PAGE);
    }
}
