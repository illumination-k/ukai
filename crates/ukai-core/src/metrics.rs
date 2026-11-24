//! Metrics per pull request (see "PR ごとのメトリクス" in `docs/design.md`):
//! what each prompt turn took, measured from the ACP messages a relay
//! passes between its clients and the agent ([`Meter`]); what a turn may be,
//! so the store refuses what is wrong with one ([`problems`]); how turns add
//! up to a branch ([`branches`]); and how the dashboard writes the numbers.
//!
//! Nothing here reads a clock: the caller passes the time, in Unix
//! milliseconds, with each message.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use serde_json::Value;
pub use ukai_acp::metrics::{Branch, Ci, Cost, PullRequest, Tokens, ToolStat, Turn};

use crate::branch;

/// Most names of tool calls one turn keeps; the rest are summed as
/// [`OTHERS`].
pub const TOOLS_MAX: usize = 64;
/// What tool calls past [`TOOLS_MAX`] names are summed as.
pub const OTHERS: &str = "…";
/// Longest name of a tool call, in characters.
pub const TOOL_NAME_MAX: usize = 80;
/// Longest session id, as for a publication.
pub const SESSION_MAX: usize = crate::publication::SESSION_MAX;
/// Longest branch name, in bytes.
pub const BRANCH_MAX: usize = 255;
/// Longest stop reason.
pub const STOP_REASON_MAX: usize = 64;

/// Programs whose second word names what they do (`cargo test`, `git
/// push`), so a command is named by both.
const SUBCOMMANDS: &[&str] = &[
    "bun", "cargo", "dx", "docker", "gh", "git", "go", "helm", "just", "kubectl", "make", "mise",
    "npm", "npx", "pip", "pnpm", "rustup", "uv", "yarn",
];

/// The name tool calls are summed under: `$ cargo test` for a command
/// (`kind` `execute`), the kind for the other kinds ACP names, and the
/// first word of the title (an MCP tool's name, say) for the rest.
pub fn tool_name(kind: Option<&str>, title: &str) -> String {
    let title = title.trim().trim_matches('`').trim();
    let name = match kind {
        Some("execute") => command_name(title),
        Some(kind) if !kind.is_empty() && kind != "other" => kind.to_owned(),
        _ => title
            .split_whitespace()
            .next()
            .unwrap_or("other")
            .trim_matches('`')
            .to_owned(),
    };
    let name = if name.is_empty() {
        "other".to_owned()
    } else {
        name
    };
    clip(&name, TOOL_NAME_MAX)
}

fn command_name(command: &str) -> String {
    let mut words = command
        .split_whitespace()
        .skip_while(|word| word.contains('=') && !word.starts_with('-'));
    let Some(program) = words.next() else {
        return "$".to_owned();
    };
    let program = program.rsplit('/').next().unwrap_or(program);
    let sub = words.next().filter(|word| {
        SUBCOMMANDS.contains(&program)
            && word
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == ':' || c == '_')
            && !word.starts_with('-')
    });
    match sub {
        Some(sub) => format!("$ {program} {sub}"),
        None => format!("$ {program}"),
    }
}

fn clip(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((end, _)) => format!("{}…", &text[..end]),
        None => text.to_owned(),
    }
}

/// A tool call of the running turn.
#[derive(Debug, Clone)]
struct Call {
    kind: Option<String>,
    title: String,
    started: i64,
    ended: Option<i64>,
    failed: bool,
}

#[derive(Debug, Clone)]
struct Running {
    started: i64,
    cost_before: Option<Cost>,
    calls: BTreeMap<String, Call>,
}

#[derive(Debug, Clone, Default)]
struct SessionMeter {
    running: Option<Running>,
    /// The agent's latest cumulative cost.
    cost: Option<Cost>,
    context_used: u64,
    context_size: u64,
}

/// Measures the turns of every session a relay passes messages for. Feed
/// it what goes to the agent ([`Meter::to_agent`]) and what comes from it
/// ([`Meter::from_agent`]), which hands back each turn as it ends.
#[derive(Debug, Default)]
pub struct Meter {
    sessions: HashMap<String, SessionMeter>,
    /// `session/prompt` requests the agent has not answered: id (as JSON
    /// text) -> session.
    prompts: HashMap<String, String>,
    /// `session/load` requests likewise: the agent replays the session's
    /// history before it answers, which is not happening now.
    loads: HashMap<String, String>,
}

impl Meter {
    /// A message sent to the agent at `now`.
    pub fn to_agent(&mut self, message: &Value, now: i64) {
        let (Some(method), Some(id)) = (
            message.get("method").and_then(Value::as_str),
            message.get("id"),
        ) else {
            return;
        };
        let Some(session) = message
            .pointer("/params/sessionId")
            .and_then(Value::as_str)
            .map(str::to_owned)
        else {
            return;
        };
        match method {
            "session/prompt" => {
                let state = self.sessions.entry(session.clone()).or_default();
                state.running = Some(Running {
                    started: now,
                    cost_before: state.cost.clone(),
                    calls: BTreeMap::new(),
                });
                self.prompts.insert(id.to_string(), session);
            }
            "session/load" => {
                self.loads.insert(id.to_string(), session);
            }
            _ => {}
        }
    }

    /// A message from the agent at `now`: the turn it ends, if it answers
    /// a `session/prompt`. The turn's branch is left for the caller.
    pub fn from_agent(&mut self, message: &Value, now: i64) -> Option<Turn> {
        if message.get("method").and_then(Value::as_str) == Some("session/update") {
            self.update(message, now);
            return None;
        }
        let id = message.get("id")?.to_string();
        if message.get("method").is_some() {
            return None;
        }
        if self.loads.remove(&id).is_some() {
            return None;
        }
        let session = self.prompts.remove(&id)?;
        let state = self.sessions.get_mut(&session)?;
        let running = state.running.take()?;
        let tokens = message.pointer("/result/usage").and_then(tokens);
        let stop_reason = message
            .pointer("/result/stopReason")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| message.get("error").map(|_| "error".to_owned()));
        Some(Turn {
            session,
            branch: None,
            started: running.started,
            ended: now.max(running.started),
            stop_reason,
            tokens,
            cost: spent(running.cost_before.as_ref(), state.cost.as_ref()),
            context_used: state.context_used,
            context_size: state.context_size,
            tools: tool_stats(&running.calls, now),
        })
    }

    fn update(&mut self, message: &Value, now: i64) {
        let Some(session) = message.pointer("/params/sessionId").and_then(Value::as_str) else {
            return;
        };
        let Some(update) = message.pointer("/params/update") else {
            return;
        };
        let replaying = self.loads.values().any(|loading| loading == session);
        let state = self.sessions.entry(session.to_owned()).or_default();
        let str_of = |key: &str| update.get(key).and_then(Value::as_str);
        match str_of("sessionUpdate") {
            Some("usage_update") => {
                if let Some(used) = update.get("used").and_then(Value::as_u64) {
                    state.context_used = used;
                }
                if let Some(size) = update.get("size").and_then(Value::as_u64) {
                    state.context_size = size;
                }
                if let Some(cost) = update.get("cost").and_then(cost) {
                    state.cost = Some(cost);
                }
            }
            Some(kind @ ("tool_call" | "tool_call_update")) if !replaying => {
                let (Some(running), Some(id)) = (state.running.as_mut(), str_of("toolCallId"))
                else {
                    return;
                };
                if kind == "tool_call" || !running.calls.contains_key(id) {
                    running.calls.entry(id.to_owned()).or_insert(Call {
                        kind: None,
                        title: String::new(),
                        started: now,
                        ended: None,
                        failed: false,
                    });
                }
                let Some(call) = running.calls.get_mut(id) else {
                    return;
                };
                if let Some(title) = str_of("title") {
                    title.clone_into(&mut call.title);
                }
                if let Some(kind) = str_of("kind") {
                    call.kind = Some(kind.to_owned());
                }
                match str_of("status") {
                    Some("completed") => call.ended = Some(now),
                    Some("failed") => {
                        call.ended = Some(now);
                        call.failed = true;
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
}

fn tokens(usage: &Value) -> Option<Tokens> {
    let field = |key: &str| usage.get(key).and_then(Value::as_u64);
    Some(Tokens {
        total: field("totalTokens")?,
        input: field("inputTokens").unwrap_or(0),
        output: field("outputTokens").unwrap_or(0),
        thought: field("thoughtTokens"),
        cached_read: field("cachedReadTokens"),
        cached_write: field("cachedWriteTokens"),
    })
}

fn cost(value: &Value) -> Option<Cost> {
    Some(Cost {
        amount: value.get("amount")?.as_f64()?,
        currency: value.get("currency")?.as_str()?.to_owned(),
    })
}

/// What was spent between the agent's cumulative costs `before` and
/// `after`; none when it reports none, or changes currency.
fn spent(before: Option<&Cost>, after: Option<&Cost>) -> Option<Cost> {
    let after = after?;
    let base = match before {
        Some(before) if before.currency == after.currency => before.amount,
        Some(_) => return None,
        None => 0.0,
    };
    Some(Cost {
        amount: (after.amount - base).max(0.0),
        currency: after.currency.clone(),
    })
}

/// The calls summed by name, the longest first; a call the turn ended
/// before it did runs until `end`.
fn tool_stats(calls: &BTreeMap<String, Call>, end: i64) -> Vec<ToolStat> {
    let mut by_name: BTreeMap<String, ToolStat> = BTreeMap::new();
    for call in calls.values() {
        let name = tool_name(call.kind.as_deref(), &call.title);
        let stat = by_name.entry(name.clone()).or_insert_with(|| ToolStat {
            name,
            ..ToolStat::default()
        });
        stat.count += 1;
        stat.failed += u32::from(call.failed);
        stat.millis += millis(call.started, call.ended.unwrap_or(end));
    }
    cap(by_name.into_values().collect())
}

fn millis(from: i64, to: i64) -> u64 {
    u64::try_from(to.saturating_sub(from)).unwrap_or(0)
}

/// `stats` longest first, at most [`TOOLS_MAX`], the rest summed as
/// [`OTHERS`].
fn cap(mut stats: Vec<ToolStat>) -> Vec<ToolStat> {
    stats.sort_by(|a, b| b.millis.cmp(&a.millis).then_with(|| a.name.cmp(&b.name)));
    if stats.len() > TOOLS_MAX {
        let rest = stats.split_off(TOOLS_MAX - 1);
        let mut others = ToolStat {
            name: OTHERS.to_owned(),
            ..ToolStat::default()
        };
        for stat in rest {
            add_stat(&mut others, &stat);
        }
        stats.push(others);
    }
    stats
}

fn add_stat(to: &mut ToolStat, stat: &ToolStat) {
    to.count = to.count.saturating_add(stat.count);
    to.failed = to.failed.saturating_add(stat.failed);
    to.millis = to.millis.saturating_add(stat.millis);
}

/// What can be wrong with a turn; [`problems`] lists them all.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Problem {
    #[error("the turn names no session")]
    EmptySession,
    #[error("a session id is at most {SESSION_MAX} characters, with no control characters")]
    InvalidSession,
    #[error("a branch is a git branch name of at most {BRANCH_MAX} bytes")]
    InvalidBranch,
    #[error("a turn ends after it starts, both after 1970")]
    InvalidTimes,
    #[error("a stop reason is at most {STOP_REASON_MAX} characters")]
    InvalidStopReason,
    #[error("a cost is a finite amount, not negative, in a three-letter currency")]
    InvalidCost,
    #[error("a turn names at most {TOOLS_MAX} tools, each at most {TOOL_NAME_MAX} characters")]
    InvalidTools,
}

/// Everything wrong with `turn`.
pub fn problems(turn: &Turn) -> Vec<Problem> {
    let mut problems = Vec::new();
    if turn.session.is_empty() {
        problems.push(Problem::EmptySession);
    } else if turn.session.chars().count() > SESSION_MAX
        || turn.session.chars().any(char::is_control)
    {
        problems.push(Problem::InvalidSession);
    }
    if let Some(name) = &turn.branch
        && (name.len() > BRANCH_MAX || !branch::is_safe(name))
    {
        problems.push(Problem::InvalidBranch);
    }
    if turn.started < 0 || turn.ended < turn.started {
        problems.push(Problem::InvalidTimes);
    }
    if turn.stop_reason.as_ref().is_some_and(|stop| {
        stop.chars().count() > STOP_REASON_MAX || stop.chars().any(char::is_control)
    }) {
        problems.push(Problem::InvalidStopReason);
    }
    if turn.cost.as_ref().is_some_and(|cost| {
        !cost.amount.is_finite()
            || cost.amount < 0.0
            || cost.currency.len() != 3
            || !cost.currency.chars().all(|c| c.is_ascii_uppercase())
    }) {
        problems.push(Problem::InvalidCost);
    }
    // One more than the cap, for "…".
    if turn.tools.len() > TOOLS_MAX
        || turn.tools.iter().any(|tool| {
            tool.name.is_empty()
                || tool.name.chars().count() > TOOL_NAME_MAX + 1
                || tool.name.chars().any(char::is_control)
        })
    {
        problems.push(Problem::InvalidTools);
    }
    problems
}

/// A branch being summed: its numbers, its sessions (by workspace), and
/// its tools by name.
type Summing<'a> = (
    Branch,
    BTreeSet<(&'a str, &'a str)>,
    BTreeMap<String, ToolStat>,
);

/// The turns, each with its workspace, summed per branch (turns off any
/// branch under `""`), the most recently active first. GitHub's side is
/// left for the caller.
pub fn branches(turns: &[(String, Turn)]) -> Vec<Branch> {
    let mut by_branch: BTreeMap<&str, Summing<'_>> = BTreeMap::new();
    for (workspace, turn) in turns {
        let name = turn.branch.as_deref().unwrap_or("");
        let (sum, sessions, tools) = by_branch.entry(name).or_insert_with(|| {
            (
                Branch {
                    branch: name.to_owned(),
                    first: turn.started,
                    last: turn.ended,
                    ..Branch::default()
                },
                BTreeSet::new(),
                BTreeMap::new(),
            )
        });
        sessions.insert((workspace.as_str(), turn.session.as_str()));
        sum.turns += 1;
        sum.agent_millis = sum
            .agent_millis
            .saturating_add(millis(turn.started, turn.ended));
        sum.first = sum.first.min(turn.started);
        sum.last = sum.last.max(turn.ended);
        sum.context_peak = sum.context_peak.max(turn.context_used);
        if let Some(used) = turn.tokens {
            add_tokens(sum.tokens.get_or_insert_with(Tokens::default), used);
        }
        if let Some(cost) = &turn.cost {
            match sum.cost.iter_mut().find(|c| c.currency == cost.currency) {
                Some(total) => total.amount += cost.amount,
                None => sum.cost.push(cost.clone()),
            }
        }
        for tool in &turn.tools {
            add_stat(
                tools.entry(tool.name.clone()).or_insert_with(|| ToolStat {
                    name: tool.name.clone(),
                    ..ToolStat::default()
                }),
                tool,
            );
        }
    }
    let mut branches: Vec<Branch> = by_branch
        .into_values()
        .map(|(mut sum, sessions, tools)| {
            sum.sessions = u32::try_from(sessions.len()).unwrap_or(u32::MAX);
            let mut tools: Vec<ToolStat> = tools.into_values().collect();
            tools.sort_by(|a, b| b.millis.cmp(&a.millis).then_with(|| a.name.cmp(&b.name)));
            sum.tools = tools;
            sum.cost.sort_by(|a, b| a.currency.cmp(&b.currency));
            sum
        })
        .collect();
    branches.sort_by(|a, b| b.last.cmp(&a.last).then_with(|| a.branch.cmp(&b.branch)));
    branches
}

fn add_tokens(to: &mut Tokens, add: Tokens) {
    let opt = |a: Option<u64>, b: Option<u64>| match (a, b) {
        (None, None) => None,
        (a, b) => Some(a.unwrap_or(0).saturating_add(b.unwrap_or(0))),
    };
    to.total = to.total.saturating_add(add.total);
    to.input = to.input.saturating_add(add.input);
    to.output = to.output.saturating_add(add.output);
    to.thought = opt(to.thought, add.thought);
    to.cached_read = opt(to.cached_read, add.cached_read);
    to.cached_write = opt(to.cached_write, add.cached_write);
}

/// Every branch's numbers summed, for the dashboard's totals.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Totals {
    pub branches: usize,
    pub pull_requests: usize,
    pub merged: usize,
    pub turns: u64,
    pub agent_millis: u64,
    pub ci_millis: u64,
    pub tokens: u64,
    pub cost: Vec<Cost>,
}

pub fn totals(branches: &[Branch]) -> Totals {
    let mut totals = Totals {
        branches: branches.len(),
        ..Totals::default()
    };
    for branch in branches {
        if let Some(pr) = &branch.pull_request {
            totals.pull_requests += 1;
            totals.merged += usize::from(pr.state == "merged");
        }
        totals.turns += u64::from(branch.turns);
        totals.agent_millis = totals.agent_millis.saturating_add(branch.agent_millis);
        totals.ci_millis = totals
            .ci_millis
            .saturating_add(branch.ci.as_ref().map_or(0, |ci| ci.millis));
        totals.tokens = totals
            .tokens
            .saturating_add(branch.tokens.map_or(0, |t| t.total));
        for cost in &branch.cost {
            match totals.cost.iter_mut().find(|c| c.currency == cost.currency) {
                Some(total) => total.amount += cost.amount,
                None => totals.cost.push(cost.clone()),
            }
        }
    }
    totals
}

/// How long a pull request took from being opened to being merged, or
/// closed, or until `now` while it is open.
pub fn lead_millis(pr: &PullRequest, now: i64) -> u64 {
    let end = pr.merged.or(pr.closed).unwrap_or(now);
    millis(pr.created, end)
}

/// `1h 05m`, `3m 07s`, `12s`.
pub fn duration_label(millis: u64) -> String {
    let seconds = millis / 1000;
    let (days, hours, minutes, seconds) = (
        seconds / 86_400,
        seconds / 3600 % 24,
        seconds / 60 % 60,
        seconds % 60,
    );
    if days > 0 {
        format!("{days}d {hours:02}h")
    } else if hours > 0 {
        format!("{hours}h {minutes:02}m")
    } else if minutes > 0 {
        format!("{minutes}m {seconds:02}s")
    } else {
        format!("{seconds}s")
    }
}

/// `950`, `12.3k`, `4.56M`.
pub fn count_label(count: u64) -> String {
    #[allow(clippy::cast_precision_loss)]
    let value = count as f64;
    if count >= 1_000_000 {
        format!("{:.2}M", value / 1_000_000.0)
    } else if count >= 1000 {
        format!("{:.1}k", value / 1000.0)
    } else {
        count.to_string()
    }
}

/// `$1.23`, or `1.23 EUR`; several currencies joined with ` + `.
pub fn cost_label(costs: &[Cost]) -> String {
    if costs.is_empty() {
        return "—".to_owned();
    }
    costs
        .iter()
        .map(|cost| match cost.currency.as_str() {
            "USD" => format!("${:.2}", cost.amount),
            currency => format!("{:.2} {currency}", cost.amount),
        })
        .collect::<Vec<_>>()
        .join(" + ")
}

#[cfg(test)]
mod tests {
    use rstest::rstest;
    use serde_json::json;

    use super::*;

    #[rstest]
    #[case::cargo(Some("execute"), "`cargo test -p ukai-core`", "$ cargo test")]
    #[case::git(Some("execute"), "git push -u origin x", "$ git push")]
    #[case::flag_is_no_subcommand(Some("execute"), "git -C dir status", "$ git")]
    #[case::other_program(Some("execute"), "ls -la src", "$ ls")]
    #[case::path(Some("execute"), "/usr/bin/grep foo bar", "$ grep")]
    #[case::env(Some("execute"), "RUST_LOG=debug cargo run", "$ cargo run")]
    #[case::empty_command(Some("execute"), "", "$")]
    #[case::kind(Some("read"), "Read /a/b.rs", "read")]
    #[case::mcp(
        Some("other"),
        "mcp__ukai__publish_report",
        "mcp__ukai__publish_report"
    )]
    #[case::no_kind(None, "Task do things", "Task")]
    #[case::nothing(None, "", "other")]
    fn names_tool_calls(#[case] kind: Option<&str>, #[case] title: &str, #[case] expected: &str) {
        assert_eq!(tool_name(kind, title), expected);
    }

    fn prompt(id: u64, session: &str) -> Value {
        json!({"jsonrpc":"2.0","id":id,"method":"session/prompt","params":{"sessionId":session,"prompt":[]}})
    }

    fn update(session: &str, update: Value) -> Value {
        json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":session,"update":update}})
    }

    fn usage(amount: f64, used: u64) -> Value {
        json!({"sessionUpdate":"usage_update","used":used,"size":200_000,"cost":{"amount":amount,"currency":"USD"}})
    }

    #[test]
    fn measures_a_turn() {
        let mut meter = Meter::default();
        meter.from_agent(&update("s", usage(1.0, 10)), 0);
        meter.to_agent(&prompt(7, "s"), 1000);
        meter.from_agent(
            &update(
                "s",
                json!({"sessionUpdate":"tool_call","toolCallId":"a","title":"cargo test","kind":"execute","status":"pending"}),
            ),
            2000,
        );
        meter.from_agent(
            &update(
                "s",
                json!({"sessionUpdate":"tool_call_update","toolCallId":"a","status":"failed"}),
            ),
            5000,
        );
        meter.from_agent(
            &update(
                "s",
                json!({"sessionUpdate":"tool_call","toolCallId":"b","title":"Read x","kind":"read"}),
            ),
            6000,
        );
        meter.from_agent(&update("s", usage(1.5, 50_000)), 7000);
        let turn = meter.from_agent(
            &json!({"jsonrpc":"2.0","id":7,"result":{"stopReason":"end_turn","usage":{"totalTokens":30,"inputTokens":20,"outputTokens":10,"cachedReadTokens":5}}}),
            9000,
        );
        let turn = turn.unwrap_or_else(|| panic!("no turn"));
        assert_eq!(turn.session, "s");
        assert_eq!((turn.started, turn.ended), (1000, 9000));
        assert_eq!(turn.stop_reason.as_deref(), Some("end_turn"));
        assert_eq!(
            turn.tokens,
            Some(Tokens {
                total: 30,
                input: 20,
                output: 10,
                thought: None,
                cached_read: Some(5),
                cached_write: None,
            })
        );
        assert_eq!(
            turn.cost,
            Some(Cost {
                amount: 0.5,
                currency: "USD".into()
            })
        );
        assert_eq!((turn.context_used, turn.context_size), (50_000, 200_000));
        assert_eq!(
            turn.tools,
            [
                ToolStat {
                    name: "$ cargo test".into(),
                    count: 1,
                    failed: 1,
                    millis: 3000
                },
                // Still running when the turn ended.
                ToolStat {
                    name: "read".into(),
                    count: 1,
                    failed: 0,
                    millis: 3000
                },
            ]
        );
        assert!(
            meter
                .from_agent(&json!({"jsonrpc":"2.0","id":7,"result":{}}), 9500)
                .is_none()
        );
    }

    #[test]
    fn a_replayed_history_sets_the_cost_but_counts_no_calls() {
        let mut meter = Meter::default();
        meter.to_agent(
            &json!({"jsonrpc":"2.0","id":1,"method":"session/load","params":{"sessionId":"s"}}),
            0,
        );
        meter.from_agent(&update("s", usage(2.0, 10)), 1);
        meter.from_agent(
            &update(
                "s",
                json!({"sessionUpdate":"tool_call","toolCallId":"old","title":"x","kind":"read"}),
            ),
            2,
        );
        assert!(
            meter
                .from_agent(&json!({"jsonrpc":"2.0","id":1,"result":{}}), 3)
                .is_none()
        );
        meter.to_agent(&prompt(2, "s"), 10);
        meter.from_agent(&update("s", usage(2.25, 10)), 11);
        let turn = meter
            .from_agent(
                &json!({"jsonrpc":"2.0","id":2,"result":{"stopReason":"cancelled"}}),
                20,
            )
            .unwrap_or_else(|| panic!("no turn"));
        assert_eq!(turn.cost.map(|c| c.amount), Some(0.25));
        assert!(turn.tools.is_empty());
        assert!(turn.tokens.is_none());
    }

    #[test]
    fn a_failed_prompt_is_a_turn_that_ended_in_error() {
        let mut meter = Meter::default();
        meter.to_agent(&prompt(1, "s"), 0);
        let turn = meter.from_agent(
            &json!({"jsonrpc":"2.0","id":1,"error":{"code":-1,"message":"x"}}),
            5,
        );
        assert_eq!(turn.and_then(|t| t.stop_reason).as_deref(), Some("error"));
    }

    #[test]
    fn a_turn_keeps_at_most_tools_max_names() {
        let calls: BTreeMap<String, Call> = (0..TOOLS_MAX + 5)
            .map(|i| {
                (
                    i.to_string(),
                    Call {
                        kind: None,
                        title: format!("tool{i}"),
                        started: 0,
                        ended: Some(1),
                        failed: false,
                    },
                )
            })
            .collect();
        let stats = tool_stats(&calls, 1);
        assert_eq!(stats.len(), TOOLS_MAX);
        assert_eq!(
            stats.last().map(|s| (s.name.as_str(), s.count)),
            Some((OTHERS, 6))
        );
        let turn = Turn {
            session: "s".into(),
            tools: stats,
            ..Turn::default()
        };
        assert!(problems(&turn).is_empty());
    }

    #[rstest]
    #[case::empty_session(Turn { session: String::new(), ..Turn::default() }, Problem::EmptySession)]
    #[case::session(Turn { session: "a\nb".into(), ..Turn::default() }, Problem::InvalidSession)]
    #[case::branch(Turn { session: "s".into(), branch: Some("a b".into()), ..Turn::default() }, Problem::InvalidBranch)]
    #[case::times(Turn { session: "s".into(), started: 5, ended: 4, ..Turn::default() }, Problem::InvalidTimes)]
    #[case::stop(Turn { session: "s".into(), stop_reason: Some("x".repeat(65)), ..Turn::default() }, Problem::InvalidStopReason)]
    #[case::cost(Turn { session: "s".into(), cost: Some(Cost { amount: -1.0, currency: "USD".into() }), ..Turn::default() }, Problem::InvalidCost)]
    #[case::currency(Turn { session: "s".into(), cost: Some(Cost { amount: 1.0, currency: "usd".into() }), ..Turn::default() }, Problem::InvalidCost)]
    #[case::tools(Turn { session: "s".into(), tools: vec![ToolStat::default()], ..Turn::default() }, Problem::InvalidTools)]
    fn refuses(#[case] turn: Turn, #[case] problem: Problem) {
        assert_eq!(problems(&turn), [problem]);
    }

    #[test]
    fn lists_every_problem() {
        let turn = Turn {
            session: String::new(),
            branch: Some("-x".into()),
            started: -1,
            ended: -2,
            stop_reason: Some("a\u{7}".into()),
            cost: Some(Cost {
                amount: f64::NAN,
                currency: "USD".into(),
            }),
            tools: vec![ToolStat::default()],
            ..Turn::default()
        };
        assert_eq!(problems(&turn).len(), 6);
    }

    fn turn(session: &str, branch: Option<&str>, started: i64, ended: i64, cost: f64) -> Turn {
        Turn {
            session: session.into(),
            branch: branch.map(Into::into),
            started,
            ended,
            tokens: Some(Tokens {
                total: 10,
                input: 6,
                output: 4,
                thought: None,
                cached_read: Some(1),
                cached_write: None,
            }),
            cost: Some(Cost {
                amount: cost,
                currency: "USD".into(),
            }),
            context_used: u64::try_from(ended).unwrap_or(0),
            tools: vec![ToolStat {
                name: "read".into(),
                count: 1,
                failed: 0,
                millis: 5,
            }],
            ..Turn::default()
        }
    }

    #[test]
    fn sums_turns_per_branch() {
        let turns = vec![
            ("w1".to_owned(), turn("s1", Some("a"), 0, 100, 1.0)),
            ("w1".to_owned(), turn("s1", Some("a"), 200, 250, 0.5)),
            ("w2".to_owned(), turn("s1", Some("a"), 300, 310, 0.25)),
            ("w1".to_owned(), turn("s2", Some("b"), 1000, 1100, 2.0)),
            ("w1".to_owned(), turn("s3", None, 50, 60, 0.0)),
        ];
        let branches = branches(&turns);
        assert_eq!(
            branches
                .iter()
                .map(|b| b.branch.as_str())
                .collect::<Vec<_>>(),
            ["b", "a", ""]
        );
        let a = &branches[1];
        assert_eq!((a.sessions, a.turns), (2, 3));
        assert_eq!(a.agent_millis, 160);
        assert_eq!((a.first, a.last), (0, 310));
        assert_eq!(a.context_peak, 310);
        assert_eq!(
            a.cost,
            [Cost {
                amount: 1.75,
                currency: "USD".into()
            }]
        );
        assert_eq!(
            a.tokens,
            Some(Tokens {
                total: 30,
                input: 18,
                output: 12,
                thought: None,
                cached_read: Some(3),
                cached_write: None,
            })
        );
        assert_eq!(
            a.tools,
            [ToolStat {
                name: "read".into(),
                count: 3,
                failed: 0,
                millis: 15
            }]
        );
        let totals = totals(&branches);
        assert_eq!(totals.branches, 3);
        assert_eq!(totals.turns, 5);
        assert_eq!(totals.tokens, 50);
        assert_eq!(cost_label(&totals.cost), "$3.75");
    }

    #[rstest]
    #[case(0, "0s")]
    #[case(12_345, "12s")]
    #[case(187_000, "3m 07s")]
    #[case(3_900_000, "1h 05m")]
    #[case(90_000_000, "1d 01h")]
    fn writes_durations(#[case] millis: u64, #[case] expected: &str) {
        assert_eq!(duration_label(millis), expected);
    }

    #[rstest]
    #[case(950, "950")]
    #[case(12_345, "12.3k")]
    #[case(4_560_000, "4.56M")]
    fn writes_counts(#[case] count: u64, #[case] expected: &str) {
        assert_eq!(count_label(count), expected);
    }

    #[test]
    fn writes_costs() {
        assert_eq!(cost_label(&[]), "—");
        assert_eq!(
            cost_label(&[
                Cost {
                    amount: 1.234,
                    currency: "USD".into()
                },
                Cost {
                    amount: 2.0,
                    currency: "EUR".into()
                },
            ]),
            "$1.23 + 2.00 EUR"
        );
    }

    #[test]
    fn lead_time_runs_until_merged_or_now() {
        let mut pr = PullRequest {
            created: 1000,
            ..PullRequest::default()
        };
        assert_eq!(lead_millis(&pr, 5000), 4000);
        pr.merged = Some(3000);
        assert_eq!(lead_millis(&pr, 5000), 2000);
    }
}
