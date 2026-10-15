//! Schedules: commands and prompts the app runs on a cron pattern while it is
//! open. [`Scheduler`] asks [`Schedules::tick`] what is due; each run is a
//! task kept in [`Running`] so it can be stopped, and reports its output and
//! outcome back into the schedules signal, which is saved with the layout.
//!
//! A prompt run is a session of its own, not a pane: it declines every
//! permission request (there is no one to ask), and once it is over the user
//! can open its session in a pane to read the whole turn or carry on.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use chrono::{DateTime, Local, TimeZone};
use dioxus::core::spawn_forever;
use dioxus::prelude::*;
use tokio::io::{AsyncBufReadExt, BufReader};
use ukai_acp::{PaneWorkspace, SessionEvent};
use ukai_config::{AgentSpec, Config};
use ukai_core::schedule::{self, Outcome, Run, Schedule, ScheduleId, Schedules, Task, Trigger};
use ukai_core::{PaneContent, ProjectId};

use crate::agent_pane::{connect, start_session};
use crate::alert;
use crate::icons::{Icon, Svg};
use crate::layout::launch_target;
use crate::projects::{AppSignals, ProjectContext, project_context};

/// How often due schedules are looked for; cron patterns have minute
/// granularity, so a run starts at most this late.
const TICK: Duration = Duration::from_secs(15);

/// The task of each schedule that is running, to stop it.
pub type Running = Signal<HashMap<ScheduleId, dioxus::core::Task>>;

/// Starts whatever is due, now and every [`TICK`]. Renders nothing.
#[component]
pub fn Scheduler() -> Element {
    let app = use_context::<AppSignals>();
    let mut schedules = app.schedules;
    use_future(move || async move {
        loop {
            let now = Local::now();
            let configs = app.configs.peek().clone();
            let due = schedules.write().tick(&now, |schedule| {
                configs
                    .get(&schedule.project)
                    .and_then(|config| config.as_ref().ok())
                    .is_some_and(|config| problems(schedule, config).is_empty())
            });
            for id in due {
                fire(app, &id, Trigger::Schedule);
            }
            tokio::time::sleep(TICK).await;
        }
    });
    rsx! {}
}

/// Everything that keeps `schedule` from running under `config`.
pub fn problems(schedule: &Schedule, config: &Config) -> Vec<schedule::Problem> {
    schedule.problems(config.agents.keys().map(String::as_str))
}

/// Start a run of `id` unless one is going.
pub fn fire(app: AppSignals, id: &ScheduleId, trigger: Trigger) {
    let AppSignals {
        mut schedules,
        mut running,
        projects,
        configs,
        ..
    } = app;
    let Some(schedule) = schedules.peek().get(id).cloned() else {
        return;
    };
    let context = project_context(&projects.peek(), &configs.peek(), &schedule.project);
    if !schedules
        .write()
        .start(id, trigger, Local::now().timestamp())
    {
        return;
    }
    let source = projects.peek().get(&schedule.project).map_or_else(
        || schedule.name.clone(),
        |p| format!("{} · {}", schedule.name, p.name()),
    );
    let id = id.clone();
    let task = spawn_forever({
        let id = id.clone();
        async move {
            let outcome = match context {
                Err(error) => Outcome::Failed { message: error },
                Ok(context) => match schedule.task.clone() {
                    Task::Command { command } => {
                        run_command(app, &id, context.root, &command).await
                    }
                    Task::Prompt { agent, prompt } => {
                        run_prompt(app, &schedule, &context, &agent, &prompt).await
                    }
                },
            };
            if let Outcome::Failed { message } = &outcome {
                alert::notify_if_away(&source, &format!("Failed: {message}"));
            }
            schedules
                .write()
                .finish(&id, outcome, Local::now().timestamp());
            running.write().remove(&id);
        }
    });
    running.write().insert(id, task);
}

/// Stop the running run of `id`: its command is killed, or its session ended.
pub fn stop(app: AppSignals, id: &ScheduleId) {
    let AppSignals {
        mut schedules,
        mut running,
        ..
    } = app;
    if let Some(task) = running.write().remove(id) {
        task.cancel();
    }
    schedules
        .write()
        .finish(id, Outcome::Stopped, Local::now().timestamp());
}

/// Forget the schedules of a project that is being closed, stopping their runs.
pub fn remove_project(app: AppSignals, project: &ProjectId) {
    let ids: Vec<_> = app
        .schedules
        .peek()
        .of_project(project)
        .map(|s| s.id.clone())
        .collect();
    for id in &ids {
        stop(app, id);
    }
    let mut schedules = app.schedules;
    schedules.write().remove_project(project);
}

async fn run_command(app: AppSignals, id: &ScheduleId, root: PathBuf, command: &str) -> Outcome {
    let mut schedules = app.schedules;
    let mut process = if cfg!(windows) {
        let mut process = tokio::process::Command::new("cmd");
        process.arg("/C").arg(command);
        process
    } else {
        let mut process = tokio::process::Command::new("sh");
        process.arg("-c").arg(command);
        process
    };
    process
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Stopping the run drops this future, and with it the child.
        .kill_on_drop(true);
    let mut child = match process.spawn() {
        Ok(child) => child,
        Err(error) => {
            return Outcome::Failed {
                message: format!("cannot start the command: {error}"),
            };
        }
    };
    let mut out = child.stdout.take().map(BufReader::new);
    let mut err = child.stderr.take().map(BufReader::new);
    let (mut out_line, mut err_line) = (Vec::new(), Vec::new());
    // Interleave both streams line by line, as a terminal would show them.
    let mut keep = |line: &mut Vec<u8>| {
        schedules
            .write()
            .append_output(id, &String::from_utf8_lossy(line));
        line.clear();
    };
    while out.is_some() || err.is_some() {
        tokio::select! {
            read = async { out.as_mut()?.read_until(b'\n', &mut out_line).await.ok() }, if out.is_some() => {
                match read {
                    Some(0) | None => out = None,
                    Some(_) => keep(&mut out_line),
                }
            }
            read = async { err.as_mut()?.read_until(b'\n', &mut err_line).await.ok() }, if err.is_some() => {
                match read {
                    Some(0) | None => err = None,
                    Some(_) => keep(&mut err_line),
                }
            }
        }
    }
    match child.wait().await {
        Ok(status) if status.success() => Outcome::Succeeded,
        Ok(status) => Outcome::Failed {
            message: status.code().map_or_else(
                || "killed by a signal".to_owned(),
                |code| format!("exit status {code}"),
            ),
        },
        Err(error) => Outcome::Failed {
            message: error.to_string(),
        },
    }
}

async fn run_prompt(
    app: AppSignals,
    schedule: &Schedule,
    context: &ProjectContext,
    agent: &str,
    prompt: &str,
) -> Outcome {
    let mut schedules = app.schedules;
    let id = &schedule.id;
    let workspace = workspace(&context.config, schedule, agent);
    let (session, mut events) = match start_session(context, agent, workspace.as_ref(), None) {
        Ok(started) => started,
        Err(message) => return Outcome::Failed { message },
    };
    let mut prompted = false;
    let mut out = |text: &str| schedules.write().append_output(id, text);
    while let Some(event) = events.recv().await {
        match event {
            SessionEvent::Ready { session_id, .. } => {
                let mut schedules = app.schedules;
                schedules.write().record_session(id, session_id);
                if !prompted {
                    prompted = session.prompt(prompt);
                    if !prompted {
                        return Outcome::Failed {
                            message: "the session ended before the prompt was sent".to_owned(),
                        };
                    }
                }
            }
            SessionEvent::AgentMessage { text } => out(&text),
            SessionEvent::ToolCall { title, .. } => out(&format!("\n› {title}\n")),
            SessionEvent::PermissionRequested {
                id: request, title, ..
            } => {
                session.resolve_permission(request, None);
                out(&format!("\n› declined: {title}\n"));
            }
            SessionEvent::TurnEnded { stop_reason } if stop_reason == "end_turn" => {
                return Outcome::Succeeded;
            }
            SessionEvent::TurnEnded { stop_reason } => {
                return Outcome::Failed {
                    message: format!("the turn ended with `{stop_reason}`"),
                };
            }
            SessionEvent::PromptFailed { message } => return Outcome::Failed { message },
            SessionEvent::Closed { error } => {
                return Outcome::Failed {
                    message: error.unwrap_or_else(|| "the agent closed the session".to_owned()),
                };
            }
            _ => {}
        }
    }
    Outcome::Failed {
        message: "the session ended".to_owned(),
    }
}

/// The workspace prompt runs of `schedule` ask a gateway `agent` for: the
/// same one every run, in the agent's default scope.
fn workspace(config: &Config, schedule: &Schedule, agent: &str) -> Option<PaneWorkspace> {
    match config.agents.get(agent) {
        Some(AgentSpec::Gateway { workspace, .. }) => Some(PaneWorkspace {
            key: schedule.workspace_key.clone(),
            scope: *workspace,
        }),
        _ => None,
    }
}

/// Show `schedule` where [`launch_target`] puts it, or focus the pane that
/// already shows it.
pub fn open_schedule(app: AppSignals, project: &ProjectId, schedule: &ScheduleId) {
    let mut layout = app.layout;
    let shown = layout.peek().contents().find_map(|(pane, content)| {
        matches!(content, PaneContent::Schedule { schedule: s, .. } if s == schedule)
            .then_some(pane)
    });
    if let Some(pane) = shown {
        layout.write().focus(pane);
        return;
    }
    if let Some(pane) = launch_target(app) {
        let mut layout = layout.write();
        layout.set_content(
            pane,
            PaneContent::Schedule {
                project: project.clone(),
                schedule: schedule.clone(),
            },
        );
        layout.focus(pane);
    }
}

/// Add a schedule to `project` and open it for editing.
pub fn add_schedule(app: AppSignals, project: &ProjectId) {
    let mut schedules = app.schedules;
    let key = uuid::Uuid::new_v4().simple().to_string();
    let id = schedules.write().add(project, key, &Local::now());
    open_schedule(app, project, &id);
}

/// Open the session of a finished prompt run in a pane, to read it in full
/// or carry on.
fn open_run_session(app: AppSignals, schedule: &Schedule, session: &str) {
    let Task::Prompt { agent, .. } = &schedule.task else {
        return;
    };
    let workspace = match app.configs.peek().get(&schedule.project) {
        Some(Ok(config)) => workspace(config, schedule, agent),
        _ => None,
    };
    let Some(pane) = launch_target(app) else {
        return;
    };
    let mut layout = app.layout;
    {
        let mut layout = layout.write();
        layout.set_content(
            pane,
            PaneContent::Agent {
                project: schedule.project.clone(),
                agent: agent.clone(),
                session: Some(session.to_owned()),
                workspace,
            },
        );
        layout.focus(pane);
    }
    connect(app, pane, true);
}

/// `at` as the user reads it: time today, else date and time.
fn when<Tz: TimeZone>(at: &DateTime<Tz>, now: &DateTime<Local>) -> String {
    let at = at.with_timezone(&Local);
    if at.date_naive() == now.date_naive() {
        at.format("%H:%M").to_string()
    } else if at.format("%Y").to_string() == now.format("%Y").to_string() {
        at.format("%b %-d %H:%M").to_string()
    } else {
        at.format("%Y-%m-%d %H:%M").to_string()
    }
}

fn when_unix(seconds: i64, now: &DateTime<Local>) -> String {
    schedule::at(seconds).map_or_else(String::new, |at| when(&at, now))
}

/// The dot class and short label of a schedule, as the sidebar shows it.
fn state_of(
    schedules: &Schedules,
    schedule: &Schedule,
    now: &DateTime<Local>,
) -> (&'static str, String) {
    if schedules.is_running(&schedule.id) {
        return ("busy", "running".to_owned());
    }
    let next = schedule
        .next_due(now)
        .map(|next| format!("next {}", when(&next, now)));
    match schedules.last_run(&schedule.id).map(|run| &run.outcome) {
        Some(Outcome::Failed { .. }) => ("closed", next.unwrap_or_else(|| "failed".to_owned())),
        _ if !schedule.enabled => ("idle", "off".to_owned()),
        _ => ("ready", next.unwrap_or_default()),
    }
}

/// The selected project's schedules, under the agents it can launch.
#[component]
pub fn ScheduleList(project: ProjectId, name: String) -> Element {
    let app = use_context::<AppSignals>();
    let now = Local::now();
    // Labels say "next 09:00"; redraw now and then so they stay true.
    let mut clock = use_signal(|| 0_u64);
    use_future(move || async move {
        loop {
            tokio::time::sleep(Duration::from_secs(30)).await;
            *clock.write() += 1;
        }
    });
    let _ = clock();
    let open = match app.layout.read().content(app.layout.read().focused()) {
        Some(PaneContent::Schedule { schedule, .. }) => Some(schedule.clone()),
        _ => None,
    };
    let schedules = app.schedules.read();
    let rows: Vec<(ScheduleId, String, &'static str, String)> = schedules
        .of_project(&project)
        .map(|schedule| {
            let (state, label) = state_of(&schedules, schedule, &now);
            (schedule.id.clone(), schedule.name.clone(), state, label)
        })
        .collect();
    drop(schedules);

    rsx! {
        div { class: "sidebar-section",
            div { class: "sidebar-header",
                span { class: "section-label", "Schedules · {name}" }
                button {
                    class: "icon",
                    title: "New schedule",
                    onclick: {
                        let project = project.clone();
                        move |_| add_schedule(app, &project)
                    },
                    Svg { icon: Icon::Plus }
                }
            }
            if rows.is_empty() {
                p { class: "muted hint",
                    "Run a command or a prompt on a schedule while ukai is open."
                }
            }
            for (id, title, state, label) in rows {
                div {
                    key: "{id}",
                    class: if open.as_ref() == Some(&id) { "row schedule-row focused" } else { "row schedule-row" },
                    title: "{title} · {label}",
                    onclick: {
                        let project = project.clone();
                        let id = id.clone();
                        move |_| open_schedule(app, &project, &id)
                    },
                    span { class: "dot {state}" }
                    span { class: "name", "{title}" }
                    span { class: "meta", "{label}" }
                }
            }
        }
    }
}

/// A schedule's title for its pane header.
pub fn schedule_label(schedules: &Schedules, id: &ScheduleId) -> String {
    schedules
        .get(id)
        .map_or_else(|| "Schedule".to_owned(), |s| s.name.clone())
}

const PRESETS: [(&str, &str); 4] = [
    ("Hourly", "0 * * * *"),
    ("Daily 9:00", "0 9 * * *"),
    ("Weekdays 9:00", "0 9 * * 1-5"),
    ("Mondays 9:00", "0 9 * * 1"),
];

/// A schedule's settings, edited as a draft and saved together, and its
/// recent runs.
#[component]
pub fn ScheduleView(project: ProjectId, schedule: ScheduleId) -> Element {
    let app = use_context::<AppSignals>();
    let saved = app.schedules.read().get(&schedule).cloned();
    let Some(saved) = saved else {
        return rsx! {
            div { class: "pane-body placeholder",
                p { class: "muted", "This schedule was deleted." }
            }
        };
    };
    // The pane is keyed by schedule, so the draft starts from this one's settings.
    let mut draft = use_signal(|| saved.clone());
    let agents: Vec<String> = match app.configs.read().get(&project) {
        Some(Ok(config)) => config.agents.keys().cloned().collect(),
        _ => Vec::new(),
    };
    let current = draft();
    let problems: Vec<String> = current
        .problems(agents.iter().map(String::as_str))
        .iter()
        .map(ToString::to_string)
        .collect();
    let changed = {
        let mut a = current.clone();
        a.enabled = saved.enabled;
        a.last_due = saved.last_due;
        a != saved
    };
    let now = Local::now();
    let description = schedule::describe(&current.cron);
    let next = if schedule::is_valid(&current.cron) {
        schedule::next_after(&current.cron, &now)
            .ok()
            .map(|next| when(&next, &now))
    } else {
        None
    };
    let running = app.schedules.read().is_running(&schedule);
    let runs: Vec<Run> = app
        .schedules
        .read()
        .runs(&schedule)
        .iter()
        .rev()
        .cloned()
        .collect();
    let is_prompt = matches!(current.task, Task::Prompt { .. });
    let save = {
        let schedule = schedule.clone();
        move |_| {
            let edited = draft.peek().clone();
            let mut schedules = app.schedules;
            schedules.write().update(&schedule, &Local::now(), |s| {
                let enabled = s.enabled;
                *s = edited;
                s.enabled = enabled;
            });
        }
    };

    rsx! {
        div { class: "pane-body schedule",
            div { class: "schedule-panel",
                div { class: "schedule-head",
                    input {
                        class: "schedule-name",
                        value: "{current.name}",
                        placeholder: "Name",
                        oninput: move |event| draft.write().name = event.value(),
                    }
                    label {
                        class: "toggle",
                        title: if problems.is_empty() { "Run on schedule" } else { "Fix the problems below first" },
                        input {
                            r#type: "checkbox",
                            checked: saved.enabled,
                            disabled: !saved.enabled && (!problems.is_empty() || changed),
                            onchange: {
                                let schedule = schedule.clone();
                                move |event: FormEvent| {
                                    let on = event.checked();
                                    let mut schedules = app.schedules;
                                    schedules.write().update(&schedule, &Local::now(), |s| s.enabled = on);
                                }
                            },
                        }
                        "Enabled"
                    }
                }
                div { class: "field",
                    span { class: "field-label", "Run" }
                    div { class: "segmented",
                        button {
                            class: if !is_prompt { "active" } else { "" },
                            onclick: move |_| {
                                if matches!(draft.peek().task, Task::Prompt { .. }) {
                                    draft.write().task = Task::Command {
                                        command: String::new(),
                                    };
                                }
                            },
                            "Command"
                        }
                        button {
                            class: if is_prompt { "active" } else { "" },
                            onclick: {
                                let first = agents.first().cloned().unwrap_or_default();
                                move |_| {
                                    if matches!(draft.peek().task, Task::Command { .. }) {
                                        draft.write().task = Task::Prompt {
                                            agent: first.clone(),
                                            prompt: String::new(),
                                        };
                                    }
                                }
                            },
                            "Prompt"
                        }
                    }
                }
                match current.task.clone() {
                    Task::Command { command } => rsx! {
                        div { class: "field",
                            span { class: "field-label", "Command" }
                            textarea {
                                class: "mono",
                                rows: 4,
                                spellcheck: false,
                                placeholder: "mise run ci",
                                value: "{command}",
                                oninput: move |event| {
                                    draft.write().task = Task::Command {
                                        command: event.value(),
                                    };
                                },
                            }
                            span { class: "muted field-hint",
                                "Runs with sh -c in the project root. Its output is kept with the run."
                            }
                        }
                    },
                    Task::Prompt { agent, prompt } => rsx! {
                        div { class: "field",
                            span { class: "field-label", "Agent" }
                            div { class: "segmented",
                                for name in agents.clone() {
                                    button {
                                        key: "{name}",
                                        class: if name == agent { "active" } else { "" },
                                        onclick: {
                                            let name = name.clone();
                                            let prompt = prompt.clone();
                                            move |_| {
                                                draft.write().task = Task::Prompt {
                                                    agent: name.clone(),
                                                    prompt: prompt.clone(),
                                                };
                                            }
                                        },
                                        "{name}"
                                    }
                                }
                            }
                        }
                        div { class: "field",
                            span { class: "field-label", "Prompt" }
                            textarea {
                                rows: 5,
                                placeholder: "Look at the new issues and label them",
                                value: "{prompt}",
                                oninput: {
                                    let agent = agent.clone();
                                    move |event: FormEvent| {
                                        draft.write().task = Task::Prompt {
                                            agent: agent.clone(),
                                            prompt: event.value(),
                                        };
                                    }
                                },
                            }
                            span { class: "muted field-hint",
                                "One turn in a session of its own. Permission requests are declined; open the session afterwards to carry on."
                            }
                        }
                    },
                }
                div { class: "field",
                    span { class: "field-label", "When" }
                    div { class: "cron-row",
                        input {
                            class: "mono cron",
                            value: "{current.cron}",
                            spellcheck: false,
                            placeholder: "0 9 * * 1-5",
                            oninput: move |event| draft.write().cron = event.value(),
                        }
                        div { class: "segmented",
                            for (label, pattern) in PRESETS {
                                button {
                                    key: "{pattern}",
                                    class: if current.cron.trim() == pattern { "active" } else { "" },
                                    onclick: move |_| draft.write().cron = pattern.to_owned(),
                                    "{label}"
                                }
                            }
                        }
                    }
                    span { class: "muted field-hint",
                        match (&description, &next) {
                            (Some(description), Some(next)) => {
                                format!("{} Next: {next}.", sentence(description))
                            }
                            (Some(description), None) => sentence(description),
                            _ => {
                                "minute hour day-of-month month day-of-week, or @hourly, @daily, @weekly"
                                    .to_owned()
                            }
                        }
                    }
                }
                if !problems.is_empty() {
                    ul { class: "problems",
                        for problem in problems.iter() {
                            li { key: "{problem}", "{problem}" }
                        }
                    }
                }
                div { class: "schedule-actions",
                    button { disabled: !changed, onclick: save, "Save" }
                    if changed {
                        button {
                            class: "outline",
                            onclick: {
                                let saved = saved.clone();
                                move |_| draft.set(saved.clone())
                            },
                            "Revert"
                        }
                    }
                    if running {
                        button {
                            class: "outline",
                            onclick: {
                                let schedule = schedule.clone();
                                move |_| stop(app, &schedule)
                            },
                            Svg { icon: Icon::Stop }
                            "Stop"
                        }
                    } else {
                        button {
                            class: "outline",
                            title: if changed { "Save first" } else { "Run once now" },
                            disabled: changed || !saved.problems(agents.iter().map(String::as_str)).is_empty(),
                            onclick: {
                                let schedule = schedule.clone();
                                move |_| fire(app, &schedule, Trigger::Manual)
                            },
                            "Run now"
                        }
                    }
                    span { class: "spacer" }
                    button {
                        class: "outline danger",
                        onclick: {
                            let schedule = schedule.clone();
                            move |_| {
                                stop(app, &schedule);
                                let mut schedules = app.schedules;
                                schedules.write().remove(&schedule);
                            }
                        },
                        "Delete"
                    }
                }
            }
            div { class: "schedule-panel",
                span { class: "section-label", "Runs" }
                if runs.is_empty() {
                    p { class: "muted hint", "No runs yet." }
                }
                for (index, run) in runs.into_iter().enumerate() {
                    RunRow {
                        key: "{run.started}-{index}",
                        schedule: saved.clone(),
                        run,
                        open: index == 0,
                    }
                }
            }
        }
    }
}

#[component]
fn RunRow(schedule: Schedule, run: Run, open: bool) -> Element {
    let app = use_context::<AppSignals>();
    let now = Local::now();
    let (state, outcome) = match &run.outcome {
        Outcome::Running => ("busy", "Running".to_owned()),
        Outcome::Succeeded => ("ready", "Succeeded".to_owned()),
        Outcome::Failed { message } => ("closed", format!("Failed: {message}")),
        Outcome::Stopped => ("idle", "Stopped".to_owned()),
        Outcome::Interrupted => ("idle", "Interrupted (ukai quit)".to_owned()),
    };
    let started = when_unix(run.started, &now);
    let took = run
        .finished
        .map(|finished| duration(finished - run.started))
        .unwrap_or_default();
    let trigger = match run.trigger {
        Trigger::Schedule => "",
        Trigger::Manual => " · manual",
    };
    let output = if run.output.trim().is_empty() {
        None
    } else if run.truncated {
        Some(format!("…\n{}", run.output))
    } else {
        Some(run.output.clone())
    };

    rsx! {
        details { class: "run", open,
            summary {
                span { class: "dot {state}" }
                span { class: "run-time", "{started}{trigger}" }
                span { class: "run-outcome", "{outcome}" }
                span { class: "spacer" }
                span { class: "muted", "{took}" }
            }
            if let Some(output) = output {
                pre { class: "run-output", "{output}" }
            } else if run.outcome != Outcome::Running {
                p { class: "muted hint", "No output." }
            }
            if let (Some(session), false) = (run.session.clone(), run.outcome == Outcome::Running) {
                button {
                    class: "outline small",
                    onclick: move |_| open_run_session(app, &schedule, &session),
                    "Open session in a pane"
                }
            }
        }
    }
}

fn duration(seconds: i64) -> String {
    match seconds {
        ..60 => format!("{}s", seconds.max(0)),
        60..3600 => format!("{}m {}s", seconds / 60, seconds % 60),
        _ => format!("{}h {}m", seconds / 3600, seconds % 3600 / 60),
    }
}

/// `text` ending in exactly one full stop.
fn sentence(text: &str) -> String {
    format!("{}.", text.trim_end().trim_end_matches('.'))
}
