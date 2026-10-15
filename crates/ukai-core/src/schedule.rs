//! Commands and prompts the app runs on a cron schedule while it is open.
//!
//! A schedule belongs to a project and runs one [`Task`]: a shell command in
//! the project root, or one prompt turn of one of its agents. Each firing is a
//! [`Run`], kept (with the tail of its output) so the user can see what
//! happened while they were away. Nothing here runs anything: the app asks
//! [`Schedules::tick`] what is due and reports back through
//! [`Schedules::start`], [`Schedules::append_output`] and [`Schedules::finish`].
//!
//! Times are Unix seconds in storage and `chrono` date-times at the edges;
//! cron patterns are read in whatever time zone the caller's date-time has
//! (the app passes local time).

use std::collections::BTreeMap;

use chrono::{DateTime, TimeZone, Utc};
use croner::Cron;
use croner::parser::{CronParser, Seconds, Year};
use serde::{Deserialize, Serialize};

use crate::ProjectId;

/// Runs kept per schedule; older ones are dropped.
pub const RUNS_KEPT: usize = 20;

/// Bytes of output kept per run: the end, where errors and results are.
pub const OUTPUT_KEPT: usize = 16 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ScheduleId(String);

impl ScheduleId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for ScheduleId {
    fn from(id: &str) -> Self {
        Self(id.to_owned())
    }
}

impl std::fmt::Display for ScheduleId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// What a schedule runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Task {
    /// A shell command (`sh -c`, `cmd /C` on Windows) in the project root.
    Command { command: String },
    /// One prompt turn of `[agents.<agent>]`, in a session of its own.
    Prompt { agent: String, prompt: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Schedule {
    pub id: ScheduleId,
    pub project: ProjectId,
    pub name: String,
    /// Five fields (minute hour day-of-month month day-of-week), or an alias
    /// such as `@daily`.
    pub cron: String,
    pub task: Task,
    pub enabled: bool,
    /// What a gateway agent's prompt runs ask for as their workspace key, so
    /// every run of the schedule lands in the same pod. Made by the caller
    /// (the app uses a random UUID) and unique across machines.
    pub workspace_key: String,
    /// When the schedule was last due, fired or not; the next firing is the
    /// first match of `cron` after it. Editing or enabling the schedule moves
    /// it to that moment, so a change never fires for a time already past.
    pub last_due: i64,
}

/// What can be wrong with a schedule; [`Schedule::problems`] lists them all.
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
    #[error("pick an agent")]
    NoAgent,
    #[error("agent `{0}` is not declared for this project")]
    UnknownAgent(String),
}

impl Schedule {
    /// Everything that keeps the schedule from running, given the agents its
    /// project declares.
    pub fn problems<'a>(&self, agents: impl IntoIterator<Item = &'a str>) -> Vec<Problem> {
        let mut problems = Vec::new();
        if self.name.trim().is_empty() {
            problems.push(Problem::EmptyName);
        }
        if let Err(error) = parse_cron(&self.cron) {
            problems.push(Problem::InvalidCron(error));
        }
        match &self.task {
            Task::Command { command } => {
                if command.trim().is_empty() {
                    problems.push(Problem::EmptyCommand);
                }
            }
            Task::Prompt { agent, prompt } => {
                if agent.is_empty() {
                    problems.push(Problem::NoAgent);
                } else if !agents.into_iter().any(|known| known == agent) {
                    problems.push(Problem::UnknownAgent(agent.clone()));
                }
                if prompt.trim().is_empty() {
                    problems.push(Problem::EmptyPrompt);
                }
            }
        }
        problems
    }

    /// When the schedule fires next, as seen from `now`'s time zone; `None`
    /// when it is disabled or its pattern is invalid or never matches.
    pub fn next_due<Tz: TimeZone>(&self, now: &DateTime<Tz>) -> Option<DateTime<Tz>> {
        if !self.enabled {
            return None;
        }
        let after = now.timezone().timestamp_opt(self.last_due, 0).single()?;
        next_after(&self.cron, &after).ok()
    }
}

/// Why a run started.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Trigger {
    Schedule,
    /// "Run now".
    Manual,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Outcome {
    Running,
    Succeeded,
    Failed {
        message: String,
    },
    /// Stopped by the user.
    Stopped,
    /// The app quit while it ran.
    Interrupted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Run {
    pub trigger: Trigger,
    pub started: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished: Option<i64>,
    pub outcome: Outcome,
    /// The last [`OUTPUT_KEPT`] bytes: the command's stdout and stderr, or
    /// the agent's messages.
    #[serde(default)]
    pub output: String,
    /// Whether `output` lost its beginning to [`OUTPUT_KEPT`].
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
    /// The ACP session of a prompt run, to open it in a pane afterwards.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Schedules {
    list: Vec<Schedule>,
    /// Oldest first.
    #[serde(default)]
    runs: BTreeMap<ScheduleId, Vec<Run>>,
    /// Ids are never reused, so a stale reference cannot hit a new schedule.
    next: u64,
}

impl Schedules {
    pub fn iter(&self) -> impl Iterator<Item = &Schedule> {
        self.list.iter()
    }

    pub fn of_project<'a>(&'a self, project: &'a ProjectId) -> impl Iterator<Item = &'a Schedule> {
        self.list.iter().filter(move |s| &s.project == project)
    }

    pub fn get(&self, id: &ScheduleId) -> Option<&Schedule> {
        self.list.iter().find(|s| &s.id == id)
    }

    /// A new, disabled schedule for `project`: a daily command with nothing
    /// to run yet, for the user to fill in.
    pub fn add<Tz: TimeZone>(
        &mut self,
        project: &ProjectId,
        workspace_key: String,
        now: &DateTime<Tz>,
    ) -> ScheduleId {
        self.next += 1;
        let id = ScheduleId(format!("s{}", self.next));
        self.list.push(Schedule {
            id: id.clone(),
            project: project.clone(),
            name: "New schedule".to_owned(),
            cron: "0 9 * * *".to_owned(),
            task: Task::Command {
                command: String::new(),
            },
            enabled: false,
            workspace_key,
            last_due: now.timestamp(),
        });
        id
    }

    /// Change a schedule. Counting starts again from `now`, so an edit never
    /// fires for a time already past.
    pub fn update<Tz: TimeZone>(
        &mut self,
        id: &ScheduleId,
        now: &DateTime<Tz>,
        edit: impl FnOnce(&mut Schedule),
    ) {
        if let Some(schedule) = self.list.iter_mut().find(|s| &s.id == id) {
            let (key, project) = (schedule.workspace_key.clone(), schedule.project.clone());
            edit(schedule);
            schedule.id = id.clone();
            schedule.workspace_key = key;
            schedule.project = project;
            schedule.last_due = now.timestamp();
        }
    }

    pub fn remove(&mut self, id: &ScheduleId) -> Option<Schedule> {
        let index = self.list.iter().position(|s| &s.id == id)?;
        self.runs.remove(id);
        Some(self.list.remove(index))
    }

    /// Forget the schedules of a project that was closed.
    pub fn remove_project(&mut self, project: &ProjectId) {
        let ids: Vec<_> = self.of_project(project).map(|s| s.id.clone()).collect();
        for id in ids {
            self.remove(&id);
        }
    }

    /// Drop schedules whose project is gone, and mark runs that were going
    /// when the app last quit as interrupted.
    pub fn after_load(&mut self, has_project: impl Fn(&ProjectId) -> bool, now: i64) {
        let orphans: Vec<_> = self
            .list
            .iter()
            .filter(|s| !has_project(&s.project))
            .map(|s| s.id.clone())
            .collect();
        for id in orphans {
            self.remove(&id);
        }
        for run in self.runs.values_mut().flatten() {
            if run.outcome == Outcome::Running {
                run.outcome = Outcome::Interrupted;
                run.finished.get_or_insert(now);
            }
        }
    }

    /// Oldest first.
    pub fn runs(&self, id: &ScheduleId) -> &[Run] {
        self.runs.get(id).map_or(&[], Vec::as_slice)
    }

    pub fn last_run(&self, id: &ScheduleId) -> Option<&Run> {
        self.runs(id).last()
    }

    pub fn is_running(&self, id: &ScheduleId) -> bool {
        self.last_run(id)
            .is_some_and(|run| run.outcome == Outcome::Running)
    }

    /// The enabled schedules that are due at `now` and not already running.
    /// Every schedule that came due counts as due once, even if several of its
    /// times passed (the app was closed) or it was still running (that time is
    /// skipped). `valid` says whether a schedule can run at all; one that
    /// cannot is not due.
    pub fn tick<Tz: TimeZone>(
        &mut self,
        now: &DateTime<Tz>,
        valid: impl Fn(&Schedule) -> bool,
    ) -> Vec<ScheduleId> {
        let mut fire = Vec::new();
        let running: Vec<bool> = self.list.iter().map(|s| self.is_running(&s.id)).collect();
        for (schedule, running) in self.list.iter_mut().zip(running) {
            let due = schedule.next_due(now).is_some_and(|next| next <= *now);
            if !due || !valid(schedule) {
                continue;
            }
            schedule.last_due = now.timestamp();
            if !running {
                fire.push(schedule.id.clone());
            }
        }
        fire
    }

    /// Record that a run of `id` started; `false` (and nothing recorded) when
    /// one is already running or the schedule is gone.
    pub fn start(&mut self, id: &ScheduleId, trigger: Trigger, now: i64) -> bool {
        if self.get(id).is_none() || self.is_running(id) {
            return false;
        }
        let runs = self.runs.entry(id.clone()).or_default();
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
    pub fn append_output(&mut self, id: &ScheduleId, text: &str) {
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

    pub fn record_session(&mut self, id: &ScheduleId, session: String) {
        if let Some(run) = self.running_mut(id) {
            run.session = Some(session);
        }
    }

    /// End the running run of `id`.
    pub fn finish(&mut self, id: &ScheduleId, outcome: Outcome, now: i64) {
        if let Some(run) = self.running_mut(id) {
            run.outcome = outcome;
            run.finished = Some(now);
        }
    }

    fn running_mut(&mut self, id: &ScheduleId) -> Option<&mut Run> {
        self.runs
            .get_mut(id)
            .and_then(|runs| runs.last_mut())
            .filter(|run| run.outcome == Outcome::Running)
    }
}

fn parse_cron(pattern: &str) -> Result<Cron, String> {
    let pattern = pattern.trim();
    if pattern.is_empty() {
        return Err("empty".to_owned());
    }
    // Minute granularity: the app checks for due schedules every few seconds,
    // not every second.
    CronParser::builder()
        .seconds(Seconds::Disallowed)
        .year(Year::Disallowed)
        .build()
        .parse(pattern)
        .map_err(|error| error.to_string())
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

/// Whether `pattern` parses; for checking input as it is typed.
pub fn is_valid(pattern: &str) -> bool {
    parse_cron(pattern).is_ok()
}

/// A Unix time as UTC, for callers that convert it to their own zone.
pub fn at(seconds: i64) -> Option<DateTime<Utc>> {
    Utc.timestamp_opt(seconds, 0).single()
}

#[cfg(test)]
mod tests;
