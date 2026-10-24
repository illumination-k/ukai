//! The schedule types as `ukai.gateway.v1` carries them, both ways. Reading
//! a message is fallible, since proto3 leaves every message field optional;
//! writing one is not.

use ukai_proto::gateway::v1 as gw;
use ukai_proto::gateway::v1::outcome::Kind;

use super::{Outcome, Run, Schedule, ScheduleInfo, ScheduleSpec, Task, Trigger};

/// What a message from the wire lacks.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("a schedule message has no {0}")]
pub struct Malformed(pub &'static str);

impl From<Task> for gw::Task {
    fn from(task: Task) -> Self {
        let kind = match task {
            Task::Command { command } => gw::task::Kind::Command(command),
            Task::Prompt { prompt } => gw::task::Kind::Prompt(prompt),
        };
        Self { kind: Some(kind) }
    }
}

impl TryFrom<gw::Task> for Task {
    type Error = Malformed;

    fn try_from(task: gw::Task) -> Result<Self, Malformed> {
        match task.kind.ok_or(Malformed("task"))? {
            gw::task::Kind::Command(command) => Ok(Self::Command { command }),
            gw::task::Kind::Prompt(prompt) => Ok(Self::Prompt { prompt }),
        }
    }
}

impl From<ScheduleSpec> for gw::ScheduleSpec {
    fn from(spec: ScheduleSpec) -> Self {
        Self {
            name: spec.name,
            cron: spec.cron,
            utc_offset_minutes: spec.utc_offset_minutes,
            task: Some(spec.task.into()),
            enabled: spec.enabled,
        }
    }
}

impl TryFrom<gw::ScheduleSpec> for ScheduleSpec {
    type Error = Malformed;

    fn try_from(spec: gw::ScheduleSpec) -> Result<Self, Malformed> {
        Ok(Self {
            name: spec.name,
            cron: spec.cron,
            utc_offset_minutes: spec.utc_offset_minutes,
            task: spec.task.ok_or(Malformed("task"))?.try_into()?,
            enabled: spec.enabled,
        })
    }
}

impl From<Schedule> for gw::Schedule {
    fn from(schedule: Schedule) -> Self {
        Self {
            id: schedule.id,
            agent: schedule.agent,
            spec: Some(schedule.spec.into()),
            workspace_key: schedule.workspace_key,
            last_due: schedule.last_due,
        }
    }
}

impl TryFrom<gw::Schedule> for Schedule {
    type Error = Malformed;

    fn try_from(schedule: gw::Schedule) -> Result<Self, Malformed> {
        Ok(Self {
            id: schedule.id,
            agent: schedule.agent,
            spec: schedule.spec.ok_or(Malformed("spec"))?.try_into()?,
            workspace_key: schedule.workspace_key,
            last_due: schedule.last_due,
        })
    }
}

impl From<Trigger> for gw::Trigger {
    fn from(trigger: Trigger) -> Self {
        match trigger {
            Trigger::Schedule => Self::Schedule,
            Trigger::Manual => Self::Manual,
        }
    }
}

fn trigger(value: i32) -> Result<Trigger, Malformed> {
    match gw::Trigger::try_from(value) {
        Ok(gw::Trigger::Schedule) => Ok(Trigger::Schedule),
        Ok(gw::Trigger::Manual) => Ok(Trigger::Manual),
        Ok(gw::Trigger::Unspecified) | Err(_) => Err(Malformed("trigger it knows")),
    }
}

impl From<Outcome> for gw::Outcome {
    fn from(outcome: Outcome) -> Self {
        let kind = match outcome {
            Outcome::Running => Kind::Running(gw::outcome::Running {}),
            Outcome::Succeeded => Kind::Succeeded(gw::outcome::Succeeded {}),
            Outcome::Failed { message } => Kind::Failed(gw::outcome::Failed { message }),
            Outcome::Stopped => Kind::Stopped(gw::outcome::Stopped {}),
            Outcome::Interrupted => Kind::Interrupted(gw::outcome::Interrupted {}),
        };
        Self { kind: Some(kind) }
    }
}

impl TryFrom<gw::Outcome> for Outcome {
    type Error = Malformed;

    fn try_from(outcome: gw::Outcome) -> Result<Self, Malformed> {
        Ok(match outcome.kind.ok_or(Malformed("outcome"))? {
            Kind::Running(_) => Self::Running,
            Kind::Succeeded(_) => Self::Succeeded,
            Kind::Failed(failed) => Self::Failed {
                message: failed.message,
            },
            Kind::Stopped(_) => Self::Stopped,
            Kind::Interrupted(_) => Self::Interrupted,
        })
    }
}

impl From<Run> for gw::Run {
    fn from(run: Run) -> Self {
        Self {
            trigger: gw::Trigger::from(run.trigger).into(),
            started: run.started,
            finished: run.finished,
            outcome: Some(run.outcome.into()),
            output: run.output,
            truncated: run.truncated,
            session: run.session,
        }
    }
}

impl TryFrom<gw::Run> for Run {
    type Error = Malformed;

    fn try_from(run: gw::Run) -> Result<Self, Malformed> {
        Ok(Self {
            trigger: trigger(run.trigger)?,
            started: run.started,
            finished: run.finished,
            outcome: run.outcome.ok_or(Malformed("outcome"))?.try_into()?,
            output: run.output,
            truncated: run.truncated,
            session: run.session,
        })
    }
}

impl From<ScheduleInfo> for gw::ScheduleInfo {
    fn from(info: ScheduleInfo) -> Self {
        Self {
            schedule: Some(info.schedule.into()),
            next_due: info.next_due,
            last_run: info.last_run.map(Into::into),
        }
    }
}

impl TryFrom<gw::ScheduleInfo> for ScheduleInfo {
    type Error = Malformed;

    fn try_from(info: gw::ScheduleInfo) -> Result<Self, Malformed> {
        Ok(Self {
            schedule: info.schedule.ok_or(Malformed("schedule"))?.try_into()?,
            next_due: info.next_due,
            last_run: info.last_run.map(Run::try_from).transpose()?,
        })
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    fn info(task: Task, outcome: Outcome) -> ScheduleInfo {
        ScheduleInfo {
            schedule: Schedule {
                id: "a1".into(),
                agent: "claude".into(),
                spec: ScheduleSpec {
                    name: "Nightly".into(),
                    cron: "0 3 * * *".into(),
                    utc_offset_minutes: 540,
                    task,
                    enabled: true,
                },
                workspace_key: "s-a1".into(),
                last_due: 1,
            },
            next_due: Some(2),
            last_run: Some(Run {
                trigger: Trigger::Manual,
                started: 3,
                finished: Some(4),
                outcome,
                output: "ok".into(),
                truncated: true,
                session: Some("sess".into()),
            }),
        }
    }

    #[rstest]
    #[case::prompt_running(Task::Prompt { prompt: "triage".into() }, Outcome::Running)]
    #[case::command_failed(Task::Command { command: "make".into() }, Outcome::Failed { message: "exit 2".into() })]
    #[case::succeeded(Task::Command { command: "true".into() }, Outcome::Succeeded)]
    #[case::stopped(Task::Command { command: "sleep 9".into() }, Outcome::Stopped)]
    #[case::interrupted(Task::Prompt { prompt: "p".into() }, Outcome::Interrupted)]
    fn a_schedule_crosses_the_wire_unchanged(#[case] task: Task, #[case] outcome: Outcome) {
        let info = info(task, outcome);
        assert_eq!(
            ScheduleInfo::try_from(gw::ScheduleInfo::from(info.clone())),
            Ok(info)
        );
    }

    #[test]
    fn a_message_lacking_what_a_schedule_needs_is_malformed() {
        let mut wire =
            gw::ScheduleInfo::from(info(Task::Prompt { prompt: "p".into() }, Outcome::Running));
        if let Some(spec) = wire.schedule.as_mut().and_then(|s| s.spec.as_mut()) {
            spec.task = Some(gw::Task { kind: None });
        }
        assert_eq!(ScheduleInfo::try_from(wire), Err(Malformed("task")));
        let mut wire =
            gw::ScheduleInfo::from(info(Task::Prompt { prompt: "p".into() }, Outcome::Running));
        if let Some(run) = wire.last_run.as_mut() {
            run.trigger = gw::Trigger::Unspecified.into();
        }
        assert!(ScheduleInfo::try_from(wire).is_err());
    }
}
