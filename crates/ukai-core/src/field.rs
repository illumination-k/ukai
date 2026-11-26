//! What a text field shows. The field keeps what the user types, and the
//! app's value follows it; only a value the app changed itself (a prompt
//! recalled, a draft cleared on send, a URL the page moved to) is written
//! into the field.
//!
//! Writing the app's value back on every change, as a `value` binding does,
//! lands each keystroke a round trip late: the keys typed meanwhile are
//! lost, the cursor jumps to the end, and an input method's composition is
//! cut off.

/// What the field holds, as far as the app knows.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Field {
    shown: Option<String>,
}

impl Field {
    /// The user changed the field to `value`.
    pub fn typed(&mut self, value: &str) {
        self.shown = Some(value.to_owned());
    }

    /// The app's value is now `value`: what to write into the field, if it
    /// does not hold that already.
    pub fn wanted(&mut self, value: &str) -> Option<String> {
        if self.shown.as_deref() == Some(value) {
            return None;
        }
        self.shown = Some(value.to_owned());
        Some(value.to_owned())
    }

    /// A new element shows nothing the app wrote yet.
    pub fn mounted(&mut self) {
        self.shown = None;
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    #[test]
    fn typing_is_never_written_back() {
        let mut field = Field::default();
        assert_eq!(field.wanted(""), Some(String::new()));
        // The field is ahead of the app: `b` was typed before `a` came back.
        field.typed("a");
        assert_eq!(field.wanted("a"), None);
        field.typed("ab");
        assert_eq!(field.wanted("ab"), None);
    }

    #[test]
    fn a_composition_in_progress_is_left_alone() {
        let mut field = Field::default();
        field.typed("にほ");
        assert_eq!(field.wanted("にほ"), None);
        field.typed("日本");
        assert_eq!(field.wanted("日本"), None);
    }

    #[rstest]
    #[case::recalled("draft", "an earlier prompt")]
    #[case::cleared_on_send("sent this", "")]
    #[case::normalized("1a", "1")]
    fn the_apps_own_change_is_written(#[case] typed: &str, #[case] value: &str) {
        let mut field = Field::default();
        field.typed(typed);
        assert_eq!(field.wanted(value), Some(value.to_owned()));
        assert_eq!(field.wanted(value), None);
    }

    #[test]
    fn a_new_element_gets_the_value_again() {
        let mut field = Field::default();
        field.typed("half a draft");
        assert_eq!(field.wanted("half a draft"), None);
        field.mounted();
        assert_eq!(
            field.wanted("half a draft"),
            Some("half a draft".to_owned())
        );
    }
}
