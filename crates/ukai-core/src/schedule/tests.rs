use chrono::{DateTime, TimeZone, Utc};
use rstest::rstest;

use super::*;

fn utc(text: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(text)
        .unwrap_or_else(|e| panic!("{text}: {e}"))
        .with_timezone(&Utc)
}

fn project() -> ProjectId {
    ProjectId("p1".to_owned())
}

/// An enabled daily-at-09:00 command created at `created`.
fn daily(created: &str) -> (Schedules, ScheduleId) {
    let mut schedules = Schedules::default();
    let id = schedules.add(&project(), "k".into(), &utc(created));
    schedules.update(&id, &utc(created), |s| {
        s.enabled = true;
        s.task = Task::Command {
            command: "true".to_owned(),
        };
    });
    (schedules, id)
}

#[rstest]
#[case::named("0 9 * * MON", true)]
#[case::alias("@daily", true)]
#[case::steps("*/15 * * * *", true)]
#[case::seconds("0 0 9 * * *", false)]
#[case::garbage("every day", false)]
#[case::empty("  ", false)]
fn cron_patterns_have_minute_granularity(#[case] pattern: &str, #[case] valid: bool) {
    assert_eq!(is_valid(pattern), valid, "{pattern}");
}

#[test]
fn next_after_reads_the_pattern_in_the_given_zone() {
    let tokyo = chrono::FixedOffset::east_opt(9 * 3600).unwrap_or_else(|| panic!("offset"));
    let after = tokyo
        .with_ymd_and_hms(2026, 10, 15, 10, 0, 0)
        .single()
        .unwrap_or_else(|| panic!("time"));
    let next = next_after("0 9 * * *", &after).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(next.to_rfc3339(), "2026-10-16T09:00:00+09:00");
}

#[rstest]
#[case::nothing_wrong(Task::Command { command: "ls".into() }, "Daily", "@daily", vec![])]
#[case::everything_wrong(
    Task::Prompt { agent: "nope".into(), prompt: " ".into() },
    "",
    "bad",
    vec!["EmptyName", "InvalidCron", "UnknownAgent", "EmptyPrompt"],
)]
#[case::no_agent(Task::Prompt { agent: String::new(), prompt: "hi".into() }, "x", "@daily", vec!["NoAgent"])]
#[case::empty_command(Task::Command { command: "\n".into() }, "x", "@daily", vec!["EmptyCommand"])]
fn problems_are_all_collected(
    #[case] task: Task,
    #[case] name: &str,
    #[case] cron: &str,
    #[case] expected: Vec<&str>,
) {
    let schedule = Schedule {
        id: ScheduleId("s1".into()),
        project: project(),
        name: name.into(),
        cron: cron.into(),
        task,
        enabled: true,
        workspace_key: "k".into(),
        last_due: 0,
    };
    let kinds: Vec<String> = schedule
        .problems(["claude"])
        .iter()
        .map(|p| {
            format!("{p:?}")
                .split(['(', ' '])
                .next()
                .unwrap_or_default()
                .to_owned()
        })
        .collect();
    assert_eq!(kinds, expected);
}

#[test]
fn new_schedules_are_disabled_and_never_due() {
    let mut schedules = Schedules::default();
    let id = schedules.add(&project(), "k".into(), &utc("2026-10-15T08:00:00Z"));
    assert!(
        schedules
            .tick(&utc("2026-10-20T09:00:00Z"), |_| true)
            .is_empty()
    );
    assert_eq!(schedules.get(&id).map(|s| s.enabled), Some(false));
}

#[test]
fn fires_once_when_due_and_not_again_until_the_next_time() {
    let (mut schedules, id) = daily("2026-10-15T08:00:00Z");
    assert!(
        schedules
            .tick(&utc("2026-10-15T08:59:59Z"), |_| true)
            .is_empty()
    );
    assert_eq!(
        schedules.tick(&utc("2026-10-15T09:00:05Z"), |_| true),
        vec![id.clone()]
    );
    assert!(
        schedules
            .tick(&utc("2026-10-15T09:00:30Z"), |_| true)
            .is_empty()
    );
    assert!(
        schedules
            .tick(&utc("2026-10-16T08:59:00Z"), |_| true)
            .is_empty()
    );
    assert_eq!(
        schedules.tick(&utc("2026-10-16T09:00:00Z"), |_| true),
        vec![id]
    );
}

#[test]
fn missed_times_fire_once() {
    // The app was closed for three days.
    let (mut schedules, id) = daily("2026-10-15T08:00:00Z");
    assert_eq!(
        schedules.tick(&utc("2026-10-18T12:00:00Z"), |_| true),
        vec![id]
    );
    assert!(
        schedules
            .tick(&utc("2026-10-18T12:01:00Z"), |_| true)
            .is_empty()
    );
}

#[test]
fn a_time_that_comes_while_running_is_skipped() {
    let (mut schedules, id) = daily("2026-10-15T08:00:00Z");
    assert!(schedules.start(&id, Trigger::Manual, 0));
    assert!(
        schedules
            .tick(&utc("2026-10-15T09:00:00Z"), |_| true)
            .is_empty()
    );
    schedules.finish(&id, Outcome::Succeeded, 1);
    assert!(
        schedules
            .tick(&utc("2026-10-15T09:01:00Z"), |_| true)
            .is_empty()
    );
}

#[test]
fn invalid_schedules_are_not_due() {
    let (mut schedules, _) = daily("2026-10-15T08:00:00Z");
    assert!(
        schedules
            .tick(&utc("2026-10-15T09:00:00Z"), |_| false)
            .is_empty()
    );
}

#[test]
fn editing_counts_from_the_edit() {
    let (mut schedules, id) = daily("2026-10-15T08:00:00Z");
    // Edited at 10:00 to run at 09:00: today's 09:00 is past and does not fire.
    schedules.update(&id, &utc("2026-10-15T10:00:00Z"), |s| {
        s.name = "renamed".into()
    });
    assert!(
        schedules
            .tick(&utc("2026-10-15T10:00:30Z"), |_| true)
            .is_empty()
    );
    let next = schedules
        .get(&id)
        .and_then(|s| s.next_due(&utc("2026-10-15T10:00:30Z")));
    assert_eq!(next, Some(utc("2026-10-16T09:00:00Z")));
}

#[test]
fn runs_record_output_tail_and_outcome() {
    let (mut schedules, id) = daily("2026-10-15T08:00:00Z");
    assert!(schedules.start(&id, Trigger::Schedule, 10));
    assert!(
        !schedules.start(&id, Trigger::Manual, 11),
        "already running"
    );
    assert!(schedules.is_running(&id));
    schedules.append_output(&id, "hello ");
    schedules.record_session(&id, "sess".into());
    schedules.append_output(&id, "world");
    schedules.finish(
        &id,
        Outcome::Failed {
            message: "exit status 1".into(),
        },
        20,
    );
    let run = schedules
        .last_run(&id)
        .cloned()
        .unwrap_or_else(|| panic!("a run"));
    assert_eq!(run.output, "hello world");
    assert_eq!(run.session.as_deref(), Some("sess"));
    assert_eq!(run.finished, Some(20));
    assert!(!schedules.is_running(&id));
    // Output after the run ended goes nowhere.
    schedules.append_output(&id, "late");
    assert_eq!(
        schedules.last_run(&id).map(|r| r.output.as_str()),
        Some("hello world")
    );
}

#[test]
fn output_keeps_its_tail_on_a_char_boundary() {
    let (mut schedules, id) = daily("2026-10-15T08:00:00Z");
    schedules.start(&id, Trigger::Manual, 0);
    schedules.append_output(&id, "x");
    schedules.append_output(&id, &"あ".repeat(OUTPUT_KEPT));
    let run = schedules
        .last_run(&id)
        .cloned()
        .unwrap_or_else(|| panic!("a run"));
    assert!(run.truncated);
    assert!(run.output.len() <= OUTPUT_KEPT);
    assert!(run.output.chars().all(|c| c == 'あ'));
}

#[test]
fn only_the_latest_runs_are_kept() {
    let (mut schedules, id) = daily("2026-10-15T08:00:00Z");
    for i in 0..(RUNS_KEPT as i64 + 5) {
        schedules.start(&id, Trigger::Manual, i);
        schedules.finish(&id, Outcome::Succeeded, i);
    }
    let runs = schedules.runs(&id);
    assert_eq!(runs.len(), RUNS_KEPT);
    assert_eq!(runs.first().map(|r| r.started), Some(5));
}

#[test]
fn loading_interrupts_running_runs_and_drops_orphans() {
    let (mut schedules, id) = daily("2026-10-15T08:00:00Z");
    let other = schedules.add(
        &ProjectId("p2".into()),
        "k2".into(),
        &utc("2026-10-15T08:00:00Z"),
    );
    schedules.start(&id, Trigger::Manual, 5);
    let json = serde_json::to_string(&schedules).unwrap_or_default();
    let mut loaded: Schedules = serde_json::from_str(&json).unwrap_or_else(|e| panic!("{e}"));
    loaded.after_load(|p| *p == project(), 9);
    assert!(loaded.get(&other).is_none());
    let run = loaded
        .last_run(&id)
        .cloned()
        .unwrap_or_else(|| panic!("a run"));
    assert_eq!(run.outcome, Outcome::Interrupted);
    assert_eq!(run.finished, Some(9));
}

#[test]
fn removing_a_project_removes_its_schedules_and_runs() {
    let (mut schedules, id) = daily("2026-10-15T08:00:00Z");
    schedules.start(&id, Trigger::Manual, 0);
    schedules.remove_project(&project());
    assert!(schedules.get(&id).is_none());
    assert!(schedules.runs(&id).is_empty());
    // Ids are not reused.
    assert_ne!(
        schedules.add(&project(), "k".into(), &utc("2026-10-15T08:00:00Z")),
        id
    );
}
