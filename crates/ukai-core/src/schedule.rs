//! Schedules a gateway runs for its agents: when one is due, what is wrong
//! with one, and the recent runs it keeps. Pure, so the gateway that fires
//! them and the app that edits them agree, and so it is tested here.
//!
//! The types are [`ukai_acp::schedules`]'s, since they cross the wire.
//! Times are Unix seconds in storage; cron patterns are read at the offset
//! from UTC the schedule's author saved them with.

use std::collections::BTreeMap;

use chrono::{DateTime, FixedOffset, TimeZone, Utc};
use croner::Cron;
use croner::parser::{CronParser, Seconds, Year};
pub use ukai_acp::schedules::{Outcome, Run, Schedule, ScheduleInfo, ScheduleSpec, Task, Trigger};

/// Runs kept per schedule; older ones are dropped.
pub const RUNS_KEPT: usize = 20;

/// Bytes of output kept per run: the end, where errors and results are.
pub const OUTPUT_KEPT: usize = 16 * 1024;

/// What can be wrong with a schedule; [`problems`] lists them all.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Problem {
    #[error("give the schedule a name")]
    EmptyName,
    #[error("invalid cron pattern: {0}")]
    InvalidCron(String),
    #[error("the command is empty")]
    EmptyCommand,
    #[error("the prompt is empty")]
    EmptyPrompt,
    #[error("UTC offset {0} minutes is out of range")]
    InvalidOffset(i32),
}

/// Everything that keeps `spec` from being saved.
pub fn problems(spec: &ScheduleSpec) -> Vec<Problem> {
    let mut problems = Vec::new();
    if spec.name.trim().is_empty() {
        problems.push(Problem::EmptyName);
    }
    if let Err(error) = parse_cron(&spec.cron) {
        problems.push(Problem::InvalidCron(error));
    }
    match &spec.task {
        Task::Command { command } if command.trim().is_empty() => {
            problems.push(Problem::EmptyCommand);
        }
        Task::Prompt { prompt } if prompt.trim().is_empty() => {
            problems.push(Problem::EmptyPrompt);
        }
        Task::Command { .. } | Task::Prompt { .. } => {}
    }
    if offset(spec.utc_offset_minutes).is_none() {
        problems.push(Problem::InvalidOffset(spec.utc_offset_minutes));
    }
    problems
}

fn offset(minutes: i32) -> Option<FixedOffset> {
    FixedOffset::east_opt(minutes.checked_mul(60)?)
}

/// When `schedule` fires next after it was last due; `None` when it is
/// disabled or its pattern does not parse or never matches.
pub fn next_due(schedule: &Schedule) -> Option<DateTime<Utc>> {
    if !schedule.spec.enabled {
        return None;
    }
    let zone = offset(schedule.spec.utc_offset_minutes)?;
    let after = zone.timestamp_opt(schedule.last_due, 0).single()?;
    next_after(&schedule.spec.cron, &after)
        .ok()
        .map(|next| next.with_timezone(&Utc))
}

/// The first time after `after` that `pattern` matches, in `after`'s zone.
pub fn next_after<Tz: TimeZone>(
    pattern: &str,
    after: &DateTime<Tz>,
) -> Result<DateTime<Tz>, String> {
    parse_cron(pattern)?
        .find_next_occurrence(after, false)
        .map_err(|error| error.to_string())
}

/// `pattern` in words, e.g. "At 09:00 on Monday"; `None` when it does not parse.
pub fn describe(pattern: &str) -> Option<String> {
    parse_cron(pattern).ok().map(|cron| cron.describe())
}

fn parse_cron(pattern: &str) -> Result<Cron, String> {
    let pattern = pattern.trim();
    if pattern.is_empty() {
        return Err("empty".to_owned());
    }
    // Minute granularity: due schedules are looked for every few seconds,
    // not every second.
    CronParser::builder()
        .seconds(Seconds::Disallowed)
        .year(Year::Disallowed)
        .build()
        .parse(pattern)
        .map_err(|error| error.to_string())
}

/// The schedules of a gateway and their recent runs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Schedules {
    list: Vec<Schedule>,
    /// Oldest first.
    runs: BTreeMap<String, Vec<Run>>,
}

impl Schedules {
    pub fn iter(&self) -> impl Iterator<Item = &Schedule> {
        self.list.iter()
    }

    pub fn get(&self, id: &str) -> Option<&Schedule> {
        self.list.iter().find(|s| s.id == id)
    }

    /// Put back a schedule and its runs as they were saved. Runs that were
    /// going when they were saved are marked interrupted at `now`: whatever
    /// ran them is gone.
    pub fn restore(&mut self, schedule: Schedule, mut runs: Vec<Run>, now: i64) {
        for run in &mut runs {
            if run.outcome == Outcome::Running {
                run.outcome = Outcome::Interrupted;
                run.finished.get_or_insert(now);
            }
        }
        let id = schedule.id.clone();
        self.list.retain(|s| s.id != id);
        self.list.push(schedule);
        self.runs.insert(id, runs);
    }

    /// Add a schedule under `id` (made by the caller, unique), counting from `now`.
    pub fn add(&mut self, id: String, agent: String, spec: ScheduleSpec, now: i64) -> &Schedule {
        let index = self.list.len();
        self.list.push(Schedule {
            workspace_key: format!("schedule-{id}"),
            id,
            agent,
            spec,
            last_due: now,
        });
        &self.list[index]
    }

    /// Replace a schedule's spec. Counting starts again from `now`, so an
    /// edit never fires for a time already past.
    pub fn update(&mut self, id: &str, spec: ScheduleSpec, now: i64) -> Option<&Schedule> {
        let schedule = self.list.iter_mut().find(|s| s.id == id)?;
        schedule.spec = spec;
        schedule.last_due = now;
        Some(schedule)
    }

    pub fn remove(&mut self, id: &str) -> Option<Schedule> {
        let index = self.list.iter().position(|s| s.id == id)?;
        self.runs.remove(id);
        Some(self.list.remove(index))
    }

    /// Oldest first.
    pub fn runs(&self, id: &str) -> &[Run] {
        self.runs.get(id).map_or(&[], Vec::as_slice)
    }

    pub fn last_run(&self, id: &str) -> Option<&Run> {
        self.runs(id).last()
    }

    pub fn is_running(&self, id: &str) -> bool {
        self.last_run(id)
            .is_some_and(|run| run.outcome == Outcome::Running)
    }

    /// `id` as listed.
    pub fn info(&self, id: &str) -> Option<ScheduleInfo> {
        let schedule = self.get(id)?;
        Some(ScheduleInfo {
            schedule: schedule.clone(),
            next_due: next_due(schedule).map(|next| next.timestamp()),
            last_run: self.last_run(id).cloned(),
        })
    }

    /// The enabled schedules due at `now` and not already running. Each
    /// schedule that came due counts once, even if several of its times
    /// passed (the gateway was down) or it was still running (that time is
    /// skipped).
    pub fn tick(&mut self, now: i64) -> Vec<String> {
        let mut fire = Vec::new();
        let running: Vec<bool> = self.list.iter().map(|s| self.is_running(&s.id)).collect();
        for (schedule, running) in self.list.iter_mut().zip(running) {
            let due = next_due(schedule).is_some_and(|next| next.timestamp() <= now);
            if !due {
                continue;
            }
            schedule.last_due = now;
            if !running {
                fire.push(schedule.id.clone());
            }
        }
        fire
    }

    /// Record that a run of `id` started; `false` (and nothing recorded) when
    /// one is already running or the schedule is gone.
    pub fn start(&mut self, id: &str, trigger: Trigger, now: i64) -> bool {
        if self.get(id).is_none() || self.is_running(id) {
            return false;
        }
        let runs = self.runs.entry(id.to_owned()).or_default();
        runs.push(Run {
            trigger,
            started: now,
            finished: None,
            outcome: Outcome::Running,
            output: String::new(),
            truncated: false,
            session: None,
        });
        if runs.len() > RUNS_KEPT {
            runs.drain(..runs.len() - RUNS_KEPT);
        }
        true
    }

    /// Add to the output of the running run of `id`, keeping its tail.
    pub fn append_output(&mut self, id: &str, text: &str) {
        if let Some(run) = self.running_mut(id) {
            run.output.push_str(text);
            if run.output.len() > OUTPUT_KEPT {
                let mut cut = run.output.len() - OUTPUT_KEPT;
                while !run.output.is_char_boundary(cut) {
                    cut += 1;
                }
                run.output.drain(..cut);
                run.truncated = true;
            }
        }
    }

    pub fn record_session(&mut self, id: &str, session: String) {
        if let Some(run) = self.running_mut(id) {
            run.session = Some(session);
        }
    }

    /// End the running run of `id`; `false` when none was running.
    pub fn finish(&mut self, id: &str, outcome: Outcome, now: i64) -> bool {
        let Some(run) = self.running_mut(id) else {
            return false;
        };
        run.outcome = outcome;
        run.finished = Some(now);
        true
    }

    fn running_mut(&mut self, id: &str) -> Option<&mut Run> {
        self.runs
            .get_mut(id)
            .and_then(|runs| runs.last_mut())
            .filter(|run| run.outcome == Outcome::Running)
    }
}

#[cfg(test)]
mod tests;
