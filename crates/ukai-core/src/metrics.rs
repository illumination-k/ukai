//! How the metrics panes put what the gateway's store sums up of the turns
//! ([`ukai_acp::metrics::TurnsSummary`]; see "ターンのメトリクス" in
//! `docs/design.md`) into words.

/// A duration for display: `850ms`, `4.2s`, `3m 05s`, `1h 02m`.
pub fn duration_label(ms: u64) -> String {
    match ms {
        0..1_000 => format!("{ms}ms"),
        1_000..60_000 => {
            let text = format!("{:.1}", ms as f64 / 1_000.0);
            format!("{}s", text.trim_end_matches(".0"))
        }
        60_000..3_600_000 => format!("{}m {:02}s", ms / 60_000, ms / 1_000 % 60),
        _ => format!("{}h {:02}m", ms / 3_600_000, ms / 60_000 % 60),
    }
}

/// A token count for display: `950`, `1.2k`, `53k`, `1.5M`.
pub fn count_label(count: u64) -> String {
    crate::transcript::tokens(count)
}

/// How long before `now_ms` something at `at_ms` was: `just now`, `5m ago`,
/// `3h ago`, `2d ago` (both in milliseconds since the Unix epoch).
pub fn age_label(now_ms: u64, at_ms: u64) -> String {
    let minutes = now_ms.saturating_sub(at_ms) / 60_000;
    match minutes {
        0 => "just now".to_owned(),
        1..60 => format!("{minutes}m ago"),
        60..1_440 => format!("{}h ago", minutes / 60),
        _ => format!("{}d ago", minutes / 1_440),
    }
}

/// `part` of `whole` as a whole percentage, 0 for an empty whole.
pub fn percent(part: u64, whole: u64) -> u64 {
    if whole == 0 {
        0
    } else {
        (part as f64 / whole as f64 * 100.0).round() as u64
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case(0, "0ms")]
    #[case(850, "850ms")]
    #[case(1_000, "1s")]
    #[case(4_250, "4.2s")]
    #[case(185_000, "3m 05s")]
    #[case(3_720_000, "1h 02m")]
    fn labels_durations(#[case] ms: u64, #[case] label: &str) {
        assert_eq!(duration_label(ms), label);
    }

    #[rstest]
    #[case(1_000, 1_000, "just now")]
    #[case(10 * 60_000, 0, "10m ago")]
    #[case(3 * 3_600_000 + 5, 0, "3h ago")]
    #[case(50 * 3_600_000, 0, "2d ago")]
    #[case(0, 5_000, "just now")]
    fn labels_ages(#[case] now: u64, #[case] at: u64, #[case] label: &str) {
        assert_eq!(age_label(now, at), label);
    }

    #[rstest]
    #[case(1, 4, 25)]
    #[case(1, 3, 33)]
    #[case(1, 0, 0)]
    fn percents(#[case] part: u64, #[case] whole: u64, #[case] expected: u64) {
        assert_eq!(percent(part, whole), expected);
    }
}
