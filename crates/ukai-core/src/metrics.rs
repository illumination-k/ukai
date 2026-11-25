//! What the metrics pane shows of a workspace's turns ([`TurnRecord`]s the
//! gateway's store keeps; see "ターンのメトリクス" in `docs/design.md`):
//! how long turns take and where that time goes, the tokens they take, and
//! which kinds of tool call cost the most.

use std::collections::BTreeMap;

use ukai_acp::metrics::TurnRecord;

/// Count, mean, median, 90th percentile, and maximum of some durations, in
/// milliseconds; all 0 when there are none.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Spread {
    pub count: usize,
    pub total: u64,
    pub mean: u64,
    pub median: u64,
    pub p90: u64,
    pub max: u64,
}

impl Spread {
    pub fn of(values: impl IntoIterator<Item = u64>) -> Self {
        let mut values: Vec<u64> = values.into_iter().collect();
        if values.is_empty() {
            return Self::default();
        }
        values.sort_unstable();
        let count = values.len();
        let total: u64 = values.iter().sum();
        // Nearest rank: the smallest value at least `p` of them do not exceed.
        let rank = |p: usize| values[(count * p).div_ceil(100).saturating_sub(1)];
        Self {
            count,
            total,
            mean: total / count as u64,
            median: rank(50),
            p90: rank(90),
            max: values[count - 1],
        }
    }
}

/// Where the turns' time went, summed over them, in milliseconds.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Split {
    /// Before the agent's first output.
    pub waiting: u64,
    pub thinking: u64,
    pub message: u64,
    /// The agent's own tool calls (a subagent's run inside the call that
    /// started it).
    pub tools: u64,
    pub permission: u64,
    /// The rest: between events, or tool calls running alongside each other
    /// counted once too often.
    pub other: u64,
}

impl Split {
    /// Each part with its share of the whole, in the order they are shown.
    pub fn parts(&self) -> [(&'static str, u64); 6] {
        [
            ("waiting", self.waiting),
            ("thinking", self.thinking),
            ("reply", self.message),
            ("tools", self.tools),
            ("permission", self.permission),
            ("other", self.other),
        ]
    }

    pub fn total(&self) -> u64 {
        self.parts().iter().map(|(_, ms)| ms).sum()
    }
}

/// The tokens of the turns that say, summed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenTotals {
    /// How many turns said.
    pub turns: usize,
    pub input: u64,
    pub output: u64,
    pub thought: u64,
    pub cached_read: u64,
    pub cached_write: u64,
}

impl TokenTotals {
    /// The share of input read from the cache, if any input was counted.
    pub fn cache_share(&self) -> Option<f64> {
        (self.input > 0).then(|| self.cached_read as f64 / self.input as f64)
    }
}

/// One kind of tool call across the turns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolKind {
    pub kind: String,
    pub calls: usize,
    pub failed: usize,
    /// Of the calls that finished.
    pub spread: Spread,
}

/// A tool call that took long, for the list of the slowest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlowCall {
    pub kind: String,
    pub title: String,
    pub duration_ms: u64,
    pub session: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Summary {
    pub turns: usize,
    pub sessions: usize,
    /// How many turns ended each way, most first.
    pub outcomes: Vec<(String, usize)>,
    pub duration: Spread,
    pub first_output: Spread,
    pub split: Split,
    pub tokens: TokenTotals,
    /// The latest cumulative cost of each session, summed, in the currency
    /// they report; `None` when none does, or they report more than one.
    pub cost: Option<(f64, String)>,
    /// Most time first.
    pub tools: Vec<ToolKind>,
    /// The slowest calls, slowest first.
    pub slowest: Vec<SlowCall>,
}

/// How many of the slowest calls [`Summary::slowest`] keeps.
pub const SLOWEST: usize = 10;

impl Summary {
    /// `turns`, newest first as the gateway lists them.
    pub fn of(turns: &[TurnRecord]) -> Self {
        let mut outcomes: BTreeMap<&str, usize> = BTreeMap::new();
        let mut split = Split::default();
        let mut tokens = TokenTotals::default();
        let mut kinds: BTreeMap<&str, (usize, usize, Vec<u64>)> = BTreeMap::new();
        let mut slowest = Vec::new();
        // Newest first, so the first cost seen of a session is its latest.
        let mut costs: BTreeMap<Option<&str>, (f64, &str)> = BTreeMap::new();
        for turn in turns {
            *outcomes.entry(&turn.outcome).or_default() += 1;
            let waiting = turn.first_output_ms.unwrap_or(0);
            let own_tools: u64 = turn
                .tools
                .iter()
                .filter(|tool| tool.parent.is_none())
                .filter_map(|tool| tool.duration_ms)
                .sum();
            let counted =
                waiting + turn.thinking_ms + turn.message_ms + own_tools + turn.permission_ms;
            split.waiting += waiting;
            split.thinking += turn.thinking_ms;
            split.message += turn.message_ms;
            split.tools += own_tools;
            split.permission += turn.permission_ms;
            split.other += turn.duration_ms.saturating_sub(counted);
            if let Some(used) = turn.tokens {
                tokens.turns += 1;
                tokens.input += used.input;
                tokens.output += used.output;
                tokens.thought += used.thought.unwrap_or(0);
                tokens.cached_read += used.cached_read.unwrap_or(0);
                tokens.cached_write += used.cached_write.unwrap_or(0);
            }
            if let Some(context) = &turn.context
                && let (Some(cost), Some(currency)) = (context.cost, &context.currency)
            {
                costs
                    .entry(turn.session_id.as_deref())
                    .or_insert((cost, currency));
            }
            for tool in &turn.tools {
                let entry = kinds.entry(&tool.kind).or_default();
                entry.0 += 1;
                if tool.status == "failed" {
                    entry.1 += 1;
                }
                if let Some(ms) = tool.duration_ms {
                    entry.2.push(ms);
                    slowest.push(SlowCall {
                        kind: tool.kind.clone(),
                        title: tool.title.clone(),
                        duration_ms: ms,
                        session: turn.session_id.clone(),
                    });
                }
            }
        }
        let mut outcomes: Vec<(String, usize)> = outcomes
            .into_iter()
            .map(|(outcome, count)| (outcome.to_owned(), count))
            .collect();
        outcomes.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        let mut tools: Vec<ToolKind> = kinds
            .into_iter()
            .map(|(kind, (calls, failed, durations))| ToolKind {
                kind: kind.to_owned(),
                calls,
                failed,
                spread: Spread::of(durations),
            })
            .collect();
        tools.sort_by(|a, b| {
            b.spread
                .total
                .cmp(&a.spread.total)
                .then_with(|| a.kind.cmp(&b.kind))
        });
        slowest.sort_by(|a, b| b.duration_ms.cmp(&a.duration_ms));
        slowest.truncate(SLOWEST);
        let currencies: Vec<&str> = costs.values().map(|(_, currency)| *currency).collect();
        let cost = match currencies.first() {
            Some(first) if currencies.iter().all(|c| c == first) => Some((
                costs.values().map(|(cost, _)| cost).sum(),
                (*first).to_owned(),
            )),
            _ => None,
        };
        let mut sessions: Vec<Option<&str>> =
            turns.iter().map(|t| t.session_id.as_deref()).collect();
        sessions.sort_unstable();
        sessions.dedup();
        Self {
            turns: turns.len(),
            sessions: sessions.len(),
            outcomes,
            duration: Spread::of(turns.iter().map(|t| t.duration_ms)),
            first_output: Spread::of(turns.iter().filter_map(|t| t.first_output_ms)),
            split,
            tokens,
            cost,
            tools,
            slowest,
        }
    }
}

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
    use ukai_acp::metrics::{Context, Tokens, ToolRecord};

    use super::*;

    fn turn(session: &str, duration_ms: u64) -> TurnRecord {
        TurnRecord {
            session_id: Some(session.into()),
            agent: None,
            model: None,
            effort: None,
            started_at_ms: 0,
            outcome: "end_turn".into(),
            error: None,
            duration_ms,
            first_output_ms: None,
            thinking_ms: 0,
            message_ms: 0,
            permission_ms: 0,
            tools: Vec::new(),
            tokens: None,
            context: None,
            joined: false,
            reconnected: false,
        }
    }

    fn tool(
        kind: &str,
        status: &str,
        duration_ms: Option<u64>,
        parent: Option<&str>,
    ) -> ToolRecord {
        ToolRecord {
            kind: kind.into(),
            title: format!("{kind} {duration_ms:?}"),
            status: status.into(),
            duration_ms,
            parent: parent.map(Into::into),
        }
    }

    #[rstest]
    #[case::none(&[], Spread::default())]
    #[case::one(&[5], Spread { count: 1, total: 5, mean: 5, median: 5, p90: 5, max: 5 })]
    #[case::ten(
        &[10, 1, 9, 2, 8, 3, 7, 4, 6, 5],
        Spread { count: 10, total: 55, mean: 5, median: 5, p90: 9, max: 10 },
    )]
    fn spreads_durations(#[case] values: &[u64], #[case] expected: Spread) {
        assert_eq!(Spread::of(values.iter().copied()), expected);
    }

    #[test]
    fn splits_where_the_time_went() {
        let mut busy = turn("s1", 10_000);
        busy.first_output_ms = Some(1_000);
        busy.thinking_ms = 2_000;
        busy.message_ms = 1_000;
        busy.permission_ms = 500;
        busy.tools = vec![
            tool("execute", "completed", Some(4_000), None),
            // Inside the call that started its subagent: not counted again.
            tool("read", "completed", Some(3_000), Some("task")),
            tool("edit", "pending", None, None),
        ];
        let summary = Summary::of(&[busy]);
        assert_eq!(
            summary.split,
            Split {
                waiting: 1_000,
                thinking: 2_000,
                message: 1_000,
                tools: 4_000,
                permission: 500,
                other: 1_500,
            }
        );
        assert_eq!(summary.split.total(), 10_000);
    }

    #[test]
    fn parallel_tool_calls_leave_nothing_negative() {
        let mut parallel = turn("s1", 1_000);
        parallel.tools = vec![
            tool("read", "completed", Some(900), None),
            tool("read", "completed", Some(900), None),
        ];
        assert_eq!(Summary::of(&[parallel]).split.other, 0);
    }

    #[test]
    fn ranks_tool_kinds_by_time_and_counts_failures() {
        let mut first = turn("s1", 0);
        first.tools = vec![
            tool("execute", "completed", Some(5_000), None),
            tool("execute", "failed", Some(1_000), None),
            tool("read", "completed", Some(100), None),
        ];
        let mut second = turn("s2", 0);
        second.tools = vec![tool("read", "completed", Some(200), None)];
        let summary = Summary::of(&[first, second]);
        let kinds: Vec<(&str, usize, usize, u64)> = summary
            .tools
            .iter()
            .map(|k| (k.kind.as_str(), k.calls, k.failed, k.spread.total))
            .collect();
        assert_eq!(kinds, [("execute", 2, 1, 6_000), ("read", 2, 0, 300)]);
        assert_eq!(summary.slowest[0].duration_ms, 5_000);
        assert_eq!(summary.slowest.len(), 4);
        assert_eq!(summary.sessions, 2);
    }

    #[test]
    fn sums_tokens_and_each_sessions_latest_cost() {
        let with = |session: &str, cost: f64| {
            let mut t = turn(session, 0);
            t.tokens = Some(Tokens {
                total: 150,
                input: 100,
                output: 50,
                thought: Some(10),
                cached_read: Some(60),
                cached_write: None,
            });
            t.context = Some(Context {
                used: 0,
                size: 0,
                cost: Some(cost),
                currency: Some("USD".into()),
            });
            t
        };
        // Newest first: s1's latest cost is 0.30.
        let summary = Summary::of(&[
            with("s1", 0.30),
            with("s2", 0.05),
            with("s1", 0.10),
            turn("s3", 0),
        ]);
        assert_eq!(summary.tokens.turns, 3);
        assert_eq!(
            (
                summary.tokens.input,
                summary.tokens.output,
                summary.tokens.thought
            ),
            (300, 150, 30)
        );
        assert_eq!(summary.tokens.cache_share(), Some(0.6));
        let (cost, currency) = summary.cost.unwrap_or_default();
        assert!((cost - 0.35).abs() < 1e-9, "{cost}");
        assert_eq!(currency, "USD");
    }

    #[test]
    fn mixed_currencies_give_no_cost() {
        let priced = |session: &str, currency: &str| {
            let mut t = turn(session, 0);
            t.context = Some(Context {
                used: 0,
                size: 0,
                cost: Some(1.0),
                currency: Some(currency.into()),
            });
            t
        };
        assert_eq!(
            Summary::of(&[priced("a", "USD"), priced("b", "EUR")]).cost,
            None
        );
    }

    #[test]
    fn counts_outcomes_most_first() {
        let mut failed = turn("s", 0);
        failed.outcome = "failed".into();
        let summary = Summary::of(&[turn("s", 0), failed, turn("s", 0)]);
        assert_eq!(
            summary.outcomes,
            [("end_turn".to_owned(), 2), ("failed".to_owned(), 1)]
        );
    }

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
