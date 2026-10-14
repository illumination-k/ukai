//! Pure conversions from what the microphone gives to what whisper takes,
//! and how a transcript joins the draft.

/// The sample rate whisper expects.
pub const WHISPER_RATE: u32 = 16_000;

/// Recordings are cut off after this long, which keeps one transcription
/// from tying up the machine for minutes.
pub const MAX_SECONDS: u32 = 300;

/// Average interleaved frames of `channels` samples into one channel.
pub fn downmix(interleaved: &[f32], channels: usize) -> Vec<f32> {
    if channels <= 1 {
        return interleaved.to_vec();
    }
    #[allow(clippy::cast_precision_loss)]
    let scale = 1.0 / channels as f32;
    interleaved
        .chunks_exact(channels)
        .map(|frame| frame.iter().sum::<f32>() * scale)
        .collect()
}

/// Linearly resample mono `samples` from `from` Hz to `to` Hz. Speech
/// survives linear interpolation well enough for recognition.
pub fn resample(samples: &[f32], from: u32, to: u32) -> Vec<f32> {
    if from == to || samples.is_empty() || from == 0 || to == 0 {
        return samples.to_vec();
    }
    let len = (samples.len() as u64 * u64::from(to) / u64::from(from)) as usize;
    let step = f64::from(from) / f64::from(to);
    (0..len)
        .map(|i| {
            #[allow(clippy::cast_precision_loss)]
            let position = i as f64 * step;
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let index = position as usize;
            #[allow(clippy::cast_possible_truncation)]
            let fraction = (position - index as f64) as f32;
            let current = samples[index.min(samples.len() - 1)];
            let next = samples[(index + 1).min(samples.len() - 1)];
            current + (next - current) * fraction
        })
        .collect()
}

/// Whether `samples` hold anything louder than background noise. Whisper
/// tends to invent words for silence, so silent recordings are not sent.
pub fn has_speech(samples: &[f32]) -> bool {
    const THRESHOLD: f32 = 0.01;
    samples.iter().any(|sample| sample.abs() > THRESHOLD)
}

/// Whisper's segments joined into one line: each trimmed, empty ones and
/// the non-speech markers it emits (`[BLANK_AUDIO]`, `(music)`) dropped.
pub fn join_segments<'a>(segments: impl IntoIterator<Item = &'a str>) -> String {
    let mut out = String::new();
    for segment in segments {
        let segment = segment.trim();
        let marker = (segment.starts_with('[') && segment.ends_with(']'))
            || (segment.starts_with('(') && segment.ends_with(')'));
        if segment.is_empty() || marker {
            continue;
        }
        if !out.is_empty() && needs_space(&out, segment) {
            out.push(' ');
        }
        out.push_str(segment);
    }
    out
}

/// `draft` with `text` appended where dictation puts it: after a space
/// unless the draft is empty or already ends in whitespace.
pub fn insert(draft: &str, text: &str) -> String {
    let text = text.trim();
    if text.is_empty() {
        return draft.to_owned();
    }
    let mut out = draft.to_owned();
    if !out.is_empty() && !out.ends_with(char::is_whitespace) && needs_space(&out, text) {
        out.push(' ');
    }
    out.push_str(text);
    out
}

/// Words in scripts written without spaces (Japanese, Chinese) join
/// directly; everything else gets a space between.
fn needs_space(before: &str, after: &str) -> bool {
    let unspaced = |c: Option<char>| c.is_some_and(is_unspaced);
    !(unspaced(before.chars().next_back()) || unspaced(after.chars().next()))
}

fn is_unspaced(c: char) -> bool {
    matches!(c,
        '\u{3000}'..='\u{30FF}'   // CJK punctuation, hiragana, katakana
        | '\u{3400}'..='\u{4DBF}' // CJK extension A
        | '\u{4E00}'..='\u{9FFF}' // CJK unified ideographs
        | '\u{F900}'..='\u{FAFF}' // CJK compatibility ideographs
        | '\u{FF00}'..='\u{FFEF}' // full-width forms
    )
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case::mono(&[0.1, 0.2], 1, &[0.1, 0.2])]
    #[case::stereo(&[0.2, 0.4, -0.2, 0.0], 2, &[0.3, -0.1])]
    #[case::partial_frame_dropped(&[0.2, 0.4, 0.6], 2, &[0.3])]
    fn downmixes(#[case] input: &[f32], #[case] channels: usize, #[case] expected: &[f32]) {
        let out = downmix(input, channels);
        assert_eq!(out.len(), expected.len());
        for (a, b) in out.iter().zip(expected) {
            assert!((a - b).abs() < 1e-6, "{out:?} != {expected:?}");
        }
    }

    #[test]
    fn resamples_to_the_target_length() {
        let second = vec![0.0; 48_000];
        assert_eq!(resample(&second, 48_000, WHISPER_RATE).len(), 16_000);
        assert_eq!(
            resample(&second[..44_100], 44_100, WHISPER_RATE).len(),
            16_000
        );
        assert_eq!(
            resample(&second[..8_000], 8_000, WHISPER_RATE).len(),
            16_000
        );
    }

    #[test]
    fn resampling_interpolates_between_samples() {
        assert_eq!(resample(&[0.0, 1.0], 1, 2), vec![0.0, 0.5, 1.0, 1.0]);
        assert_eq!(resample(&[0.0, 1.0, 0.0, -1.0], 2, 1), vec![0.0, 0.0]);
        assert!(resample(&[], 48_000, WHISPER_RATE).is_empty());
    }

    #[rstest]
    #[case::silence(&[0.0, 0.001, -0.005], false)]
    #[case::speech(&[0.0, 0.2, -0.3], true)]
    #[case::empty(&[], false)]
    fn detects_speech(#[case] samples: &[f32], #[case] expected: bool) {
        assert_eq!(has_speech(samples), expected);
    }

    #[rstest]
    #[case::words(&[" Hello", " world."], "Hello world.")]
    #[case::markers(&[" [BLANK_AUDIO]", " Fix it", " (music)"], "Fix it")]
    #[case::japanese(&["テストを", "直して"], "テストを直して")]
    #[case::empty(&["  ", ""], "")]
    fn joins_segments(#[case] segments: &[&str], #[case] expected: &str) {
        assert_eq!(join_segments(segments.iter().copied()), expected);
    }

    #[rstest]
    #[case::into_empty("", " run the tests ", "run the tests")]
    #[case::after_word("Please", "run the tests", "Please run the tests")]
    #[case::after_space("Please ", "run", "Please run")]
    #[case::after_newline("Please\n", "run", "Please\nrun")]
    #[case::japanese("テストを", "実行して", "テストを実行して")]
    #[case::latin_after_japanese("テストを", "cargo test", "テストをcargo test")]
    #[case::nothing_heard("Please", "  ", "Please")]
    fn inserts_into_the_draft(#[case] draft: &str, #[case] text: &str, #[case] expected: &str) {
        assert_eq!(insert(draft, text), expected);
    }
}
