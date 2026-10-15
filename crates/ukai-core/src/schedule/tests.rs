use rstest::rstest;
use time::macros::datetime;

use super::*;

/// `YYYY-MM-DDTHH:MM:SSZ` in Unix seconds.
fn at(text: &str) -> i64 {
    let field = |range: std::ops::Range<usize>| -> i64 {
        text.get(range)
            .and_then(|digits| digits.parse().ok())
            .unwrap_or_else(|| panic!("{text}"))
    };
    let month = time::Month::try_from(u8::try_from(field(5..7)).unwrap_or(0))
        .unwrap_or_else(|e| panic!("{text}: {e}"));
    let date = time::Date::from_calendar_date(
        i32::try_from(field(0..4)).unwrap_or(0),
        month,
        u8::try_from(field(8..10)).unwrap_or(0),
    )
    .unwrap_or_else(|e| panic!("{text}: {e}"));
    let seconds = field(11..13) * 3600 + field(14..16) * 60 + field(17..19);
    date.midnight().assume_utc().unix_timestamp() + seconds
}

fn spec(cron: &str, offset: i32) -> ScheduleSpec {
    ScheduleSpec {
        name: "Daily".into(),
        cron: cron.into(),
        utc_offset_minutes: offset,
        task: Task::Command {
            command: "true".into(),
        },
        enabled: true,
    }
}

/// A daily-at-09:00-UTC schedule saved at `saved`.
fn daily(saved: &str) -> Schedules {
    let mut schedules = Schedules::default();
    schedules.add(
        "a1".into(),
        "claude".into(),
        spec("0 9 * * *", 0),
        at(saved),
    );
    schedules
}

#[rstest]
#[case::named("0 9 * * MON", true)]
#[case::alias("@daily", true)]
#[case::steps("*/15 * * * *", true)]
#[case::seconds("0 0 9 * * *", false)]
#[case::garbage("every day", false)]
#[case::empty("  ", false)]
fn cron_patterns_have_minute_granularity(#[case] pattern: &str, #[case] valid: bool) {
    assert_eq!(Cron::parse(pattern).is_ok(), valid, "{pattern}");
}

#[test]
fn the_pattern_is_read_at_the_authors_offset() {
    // 09:00 in JST is 00:00 UTC.
    let mut schedules = Schedules::default();
    let id = schedules
        .add(
            "a1".into(),
            "claude".into(),
            spec("0 9 * * *", 540),
            at("2026-10-15T01:00:00Z"),
        )
        .id
        .clone();
    let next = schedules.get(&id).and_then(next_due);
    assert_eq!(next, Some(datetime!(2026-10-16 00:00 UTC)));
}

#[rstest]
#[case::nothing_wrong(spec("@daily", 0), vec![])]
#[case::everything_wrong(
    ScheduleSpec { name: " ".into(), cron: "bad".into(), utc_offset_minutes: 100_000, task: Task::Prompt { prompt: String::new() }, enabled: true },
    vec![Problem::EmptyName, Problem::InvalidCron(String::new()), Problem::EmptyPrompt, Problem::InvalidOffset(100_000)],
)]
#[case::empty_command(
    ScheduleSpec { task: Task::Command { command: "\n".into() }, ..spec("@daily", 0) },
    vec![Problem::EmptyCommand],
)]
fn problems_are_all_collected(#[case] spec: ScheduleSpec, #[case] expected: Vec<Problem>) {
    let blank = |problems: Vec<Problem>| -> Vec<Problem> {
        problems
            .into_iter()
            .map(|p| match p {
                Problem::InvalidCron(_) => Problem::InvalidCron(String::new()),
                other => other,
            })
            .collect()
    };
    assert_eq!(blank(problems(&spec)), expected);
}

#[test]
fn disabled_schedules_are_never_due() {
    let mut schedules = daily("2026-10-15T08:00:00Z");
    let disabled = ScheduleSpec {
        enabled: false,
        ..spec("0 9 * * *", 0)
    };
    schedules.update("a1", disabled, at("2026-10-15T08:00:00Z"));
    assert!(schedules.tick(at("2026-10-20T09:00:00Z")).is_empty());
    assert_eq!(schedules.info("a1").and_then(|i| i.next_due), None);
}

#[test]
fn fires_once_when_due_and_not_again_until_the_next_time() {
    let mut schedules = daily("2026-10-15T08:00:00Z");
    assert!(schedules.tick(at("2026-10-15T08:59:59Z")).is_empty());
    assert_eq!(schedules.tick(at("2026-10-15T09:00:05Z")), vec!["a1"]);
    assert!(schedules.tick(at("2026-10-15T09:00:30Z")).is_empty());
    assert!(schedules.tick(at("2026-10-16T08:59:00Z")).is_empty());
    assert_eq!(schedules.tick(at("2026-10-16T09:00:00Z")), vec!["a1"]);
}

#[test]
fn missed_times_fire_once() {
    // The gateway was down for three days.
    let mut schedules = daily("2026-10-15T08:00:00Z");
    assert_eq!(schedules.tick(at("2026-10-18T12:00:00Z")), vec!["a1"]);
    assert!(schedules.tick(at("2026-10-18T12:01:00Z")).is_empty());
}

#[test]
fn a_time_that_comes_while_running_is_skipped() {
    let mut schedules = daily("2026-10-15T08:00:00Z");
    assert!(schedules.start("a1", Trigger::Manual, 0));
    assert!(schedules.tick(at("2026-10-15T09:00:00Z")).is_empty());
    schedules.finish("a1", Outcome::Succeeded, 1);
    assert!(schedules.tick(at("2026-10-15T09:01:00Z")).is_empty());
}

#[test]
fn editing_counts_from_the_edit() {
    let mut schedules = daily("2026-10-15T08:00:00Z");
    // Saved at 10:00 to run at 09:00: today's 09:00 is past and does not fire.
    schedules.update("a1", spec("0 9 * * *", 0), at("2026-10-15T10:00:00Z"));
    assert!(schedules.tick(at("2026-10-15T10:00:30Z")).is_empty());
    assert_eq!(
        schedules.info("a1").and_then(|i| i.next_due),
        Some(at("2026-10-16T09:00:00Z"))
    );
}

#[test]
fn runs_record_output_session_and_outcome() {
    let mut schedules = daily("2026-10-15T08:00:00Z");
    assert!(schedules.start("a1", Trigger::Schedule, 10));
    assert!(
        !schedules.start("a1", Trigger::Manual, 11),
        "already running"
    );
    assert!(!schedules.start("nope", Trigger::Manual, 11));
    schedules.append_output("a1", "hello ");
    schedules.record_session("a1", "sess".into());
    schedules.append_output("a1", "world");
    assert!(schedules.finish(
        "a1",
        Outcome::Failed {
            message: "exit status 1".into()
        },
        20
    ));
    assert!(
        !schedules.finish("a1", Outcome::Stopped, 21),
        "nothing running"
    );
    let run = schedules
        .last_run("a1")
        .cloned()
        .unwrap_or_else(|| panic!("a run"));
    assert_eq!(run.output, "hello world");
    assert_eq!(run.session.as_deref(), Some("sess"));
    assert_eq!(run.finished, Some(20));
    schedules.append_output("a1", "late");
    assert_eq!(
        schedules.last_run("a1").map(|r| r.output.as_str()),
        Some("hello world")
    );
}

#[test]
fn output_keeps_its_tail_on_a_char_boundary() {
    let mut schedules = daily("2026-10-15T08:00:00Z");
    schedules.start("a1", Trigger::Manual, 0);
    schedules.append_output("a1", "x");
    schedules.append_output("a1", &"あ".repeat(OUTPUT_KEPT));
    let run = schedules
        .last_run("a1")
        .cloned()
        .unwrap_or_else(|| panic!("a run"));
    assert!(run.truncated);
    assert!(run.output.len() <= OUTPUT_KEPT);
    assert!(run.output.chars().all(|c| c == 'あ'));
}

#[test]
fn only_the_latest_runs_are_kept() {
    let mut schedules = daily("2026-10-15T08:00:00Z");
    for i in 0..(RUNS_KEPT as i64 + 5) {
        schedules.start("a1", Trigger::Manual, i);
        schedules.finish("a1", Outcome::Succeeded, i);
    }
    let runs = schedules.runs("a1");
    assert_eq!(runs.len(), RUNS_KEPT);
    assert_eq!(runs.first().map(|r| r.started), Some(5));
}

#[test]
fn restoring_interrupts_runs_that_were_going() {
    let mut schedules = daily("2026-10-15T08:00:00Z");
    schedules.start("a1", Trigger::Manual, 5);
    let schedule = schedules.get("a1").cloned().unwrap_or_else(|| panic!("a1"));
    let runs = schedules.runs("a1").to_vec();
    let mut restored = Schedules::default();
    restored.restore(schedule, runs, 9);
    let run = restored
        .last_run("a1")
        .cloned()
        .unwrap_or_else(|| panic!("a run"));
    assert_eq!(run.outcome, Outcome::Interrupted);
    assert_eq!(run.finished, Some(9));
    assert!(!restored.is_running("a1"));
}

#[test]
fn removing_a_schedule_removes_its_runs() {
    let mut schedules = daily("2026-10-15T08:00:00Z");
    schedules.start("a1", Trigger::Manual, 0);
    assert!(schedules.remove("a1").is_some());
    assert!(schedules.get("a1").is_none());
    assert!(schedules.runs("a1").is_empty());
}

#[test]
fn each_schedule_has_a_workspace_of_its_own() {
    let schedules = daily("2026-10-15T08:00:00Z");
    assert_eq!(
        schedules.get("a1").map(|s| s.workspace_key.as_str()),
        Some("schedule-a1")
    );
}
