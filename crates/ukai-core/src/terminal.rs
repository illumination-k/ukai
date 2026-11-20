//! What a terminal pane keeps of its shell's output while no view shows it.
//!
//! A pane's view is built again whenever its tab is shown again, but the
//! shell keeps running, so the app holds on to what the shell printed and
//! plays it to the new view ([`Backlog`]) — the last [`BACKLOG_BYTES`] of
//! it, cut where a line starts so the replay does not open in the middle of
//! an escape sequence.

/// Bytes of output a terminal pane keeps to replay.
pub const BACKLOG_BYTES: usize = 1024 * 1024;

/// The end of a shell's output, up to a size.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Backlog {
    bytes: Vec<u8>,
    limit: usize,
}

impl Default for Backlog {
    fn default() -> Self {
        Self::with_limit(BACKLOG_BYTES)
    }
}

impl Backlog {
    pub fn with_limit(limit: usize) -> Self {
        Self {
            bytes: Vec::new(),
            limit,
        }
    }

    /// Keep `output`, letting go of the oldest bytes past the limit. The cut
    /// falls after a newline when one is in the part let go of the second
    /// half, else exactly at the limit.
    pub fn push(&mut self, output: &[u8]) {
        self.bytes.extend_from_slice(output);
        let Some(excess) = self.bytes.len().checked_sub(self.limit) else {
            return;
        };
        if excess == 0 {
            return;
        }
        // Look for a line start in the next half of what stays, so a cut
        // never takes much more than it must.
        let window = self.limit / 2;
        let end = (excess + window).min(self.bytes.len());
        let cut = self.bytes[excess..end]
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(excess, |at| excess + at + 1);
        self.bytes.drain(..cut);
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn clear(&mut self) {
        self.bytes.clear();
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    #[test]
    fn keeps_everything_within_the_limit() {
        let mut backlog = Backlog::with_limit(10);
        backlog.push(b"abc");
        backlog.push(b"def\n");
        assert_eq!(backlog.bytes(), b"abcdef\n");
        backlog.push(b"ghi");
        assert_eq!(backlog.bytes(), b"abcdef\nghi");
    }

    #[rstest]
    // Past the limit by 9: the next line starts four bytes on.
    #[case::at_a_line(b"0123456789".as_slice(), b"ab\ncd\x1b[1m".as_slice(), b"cd\x1b[1m".as_slice())]
    // No newline near the cut: exactly the limit stays.
    #[case::mid_line(b"0123456789".as_slice(), b"abcdefgh".as_slice(), b"89abcdefgh".as_slice())]
    fn lets_go_of_the_oldest(#[case] first: &[u8], #[case] second: &[u8], #[case] kept: &[u8]) {
        let mut backlog = Backlog::with_limit(10);
        backlog.push(first);
        backlog.push(second);
        assert_eq!(backlog.bytes(), kept);
        assert!(backlog.bytes().len() <= 10);
    }

    #[test]
    fn a_push_larger_than_the_limit_keeps_its_end() {
        let mut backlog = Backlog::with_limit(4);
        backlog.push(b"abcdefgh");
        assert_eq!(backlog.bytes(), b"efgh");
        backlog.clear();
        assert!(backlog.bytes().is_empty());
    }
}
