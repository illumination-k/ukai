//! Comparing a presented token with the one expected, shared by the
//! gateway and `ukai-knowledge`, so a fix to it reaches both.

/// Whether `presented` is `secret`, taking as long for every `presented` of
/// the same length.
pub fn same_secret(secret: &str, presented: &str) -> bool {
    presented.len() == secret.len()
        && presented
            .bytes()
            .zip(secret.bytes())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case::same("s3cret", true)]
    #[case::one_byte_off("s3crex", false)]
    #[case::longer("s3cret!", false)]
    #[case::shorter("s3cre", false)]
    #[case::empty("", false)]
    fn compares_whole_tokens(#[case] presented: &str, #[case] same: bool) {
        assert_eq!(same_secret("s3cret", presented), same);
    }
}
