//! Schedules: commands and prompts a gateway agent runs on a cron pattern.
//! The gateway keeps them and fires them, so they run while ukai is closed;
//! the app lists them under the selected project's gateway agents, edits
//! them in a schedule pane, and reads their runs back.
//!
//! A prompt run is a session of its own in the schedule's workspace. Once it
//! is over, or while it waits on a permission request, the user can open
//! that session in a pane to read the whole turn, answer, or carry on.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::OnceLock;
use std::time::Duration;

use dioxus::prelude::*;
use time::{OffsetDateTime, UtcOffset};
use tokio::time::Instant;
use ukai_acp::schedules::{SchedulesEndpoint, SchedulesError};
use ukai_acp::{PaneWorkspace, WorkspaceScope};
use ukai_config::{AgentSpec, Config};
use ukai_core::schedule::{self, Outcome, Run, ScheduleInfo, ScheduleSpec, Task, Trigger};
use ukai_core::{PaneContent, PaneId, ProjectId};

use crate::agent_pane::connect;
use crate::alert;
use crate::icons::{Icon, Svg};
use crate::layout::launch_target;
use crate::projects::AppSignals;

/// How often the sidebar asks the gateways for their schedules.
const LIST_EVERY: Duration = Duration::from_secs(15);
/// How often an open schedule pane asks for its schedule and runs.
const PANE_EVERY: Duration = Duration::from_secs(4);
/// How soon a poller notices that an edit asked for a fresh read.
const NOTICE: Duration = Duration::from_millis(300);

/// The gateway agents of `config`, by name.
pub(crate) fn gateway_agents(config: &Config) -> Vec<String> {
    config
        .agents
        .iter()
        .filter(|(_, spec)| matches!(spec, AgentSpec::Gateway { .. }))
        .map(|(name, _)| name.clone())
        .collect()
}

/// Where `agent`'s schedules are kept.
fn endpoint(config: &Config, agent: &str) -> Result<SchedulesEndpoint, String> {
    let spec = config
        .agents
        .get(agent)
        .ok_or_else(|| format!("agent `{agent}` is not declared anymore"))?;
    SchedulesEndpoint::resolve(spec, ukai_acp::system_lookup)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| format!("agent `{agent}` is not a gateway agent, so it keeps no schedules"))
}

pub(crate) fn config_of(app: AppSignals, project: &ProjectId) -> Result<Config, String> {
    app.configs
        .read()
        .get(project)
        .cloned()
        .unwrap_or_else(|| Err("this project's configuration is not loaded".to_owned()))
}

/// `error` as the user reads it, one problem per line.
fn explain(error: &SchedulesError) -> Vec<String> {
    match error {
        SchedulesError::Refused(problems) => problems.clone(),
        SchedulesError::NotFound => vec![
            "The gateway does not know this schedule or agent; it may need upgrading.".to_owned(),
        ],
        other => vec![other.to_string()],
    }
}

/// Ask the sidebar and open schedule panes to read their schedules again.
fn refresh(app: AppSignals) {
    let mut revision = app.schedule_revision;
    *revision.write() += 1;
}

/// Calls `read` now, every `every`, and soon after `revision` is bumped
/// (for schedules, by [`refresh`]).
pub(crate) async fn poll<F: Future<Output = ()>>(
    revision: Signal<u64>,
    every: Duration,
    mut read: impl FnMut() -> F,
) {
    let mut last = None;
    let mut next = Instant::now();
    loop {
        let current = *revision.peek();
        if last != Some(current) || Instant::now() >= next {
            last = Some(current);
            next = Instant::now() + every;
            read().await;
        }
        tokio::time::sleep(NOTICE).await;
    }
}

/// Show a schedule (or a new one, with `schedule` `None`) where
/// [`launch_target`] puts it, or focus the pane that already shows it.
pub fn open_schedule(app: AppSignals, project: &ProjectId, agent: &str, schedule: Option<String>) {
    let mut layout = app.layout;
    let shown = schedule.as_ref().and_then(|id| {
        layout.peek().contents().find_map(|(pane, content)| {
            matches!(content, PaneContent::Schedule { schedule: Some(s), .. } if s == id)
                .then_some(pane)
        })
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
                agent: agent.to_owned(),
                schedule,
            },
        );
        layout.focus(pane);
    }
}

/// Open the session of a prompt run in a pane, in the schedule's workspace.
fn open_run_session(app: AppSignals, project: &ProjectId, info: &ScheduleInfo, session: &str) {
    let Some(pane) = launch_target(app) else {
        return;
    };
    let mut layout = app.layout;
    {
        let mut layout = layout.write();
        layout.set_content(
            pane,
            PaneContent::Agent {
                project: project.clone(),
                agent: info.schedule.agent.clone(),
                session: Some(session.to_owned()),
                workspace: Some(PaneWorkspace {
                    key: info.schedule.workspace_key.clone(),
                    scope: WorkspaceScope::Pod,
                }),
            },
        );
        layout.focus(pane);
    }
    connect(app, pane, true);
}

/// The machine's offset from UTC, read once at startup: `time` can only read
/// it while the process has a single thread.
static LOCAL_OFFSET: OnceLock<UtcOffset> = OnceLock::new();

/// Read the local offset; call before any thread starts. UTC if it cannot be read.
pub fn read_local_offset() {
    let _ = LOCAL_OFFSET.set(UtcOffset::current_local_offset().unwrap_or(UtcOffset::UTC));
}

fn local_offset() -> UtcOffset {
    LOCAL_OFFSET.get().copied().unwrap_or(UtcOffset::UTC)
}

pub(crate) fn local_now() -> OffsetDateTime {
    OffsetDateTime::now_utc().to_offset(local_offset())
}

/// `at` as the user reads it: time today, else date and time.
fn when(at: OffsetDateTime, now: OffsetDateTime) -> String {
    let at = at.to_offset(now.offset());
    let clock = format!("{:02}:{:02}", at.hour(), at.minute());
    if at.date() == now.date() {
        clock
    } else if at.year() == now.year() {
        let month = at.month().to_string();
        format!("{} {} {clock}", month.get(..3).unwrap_or(&month), at.day())
    } else {
        format!(
            "{}-{:02}-{:02} {clock}",
            at.year(),
            u8::from(at.month()),
            at.day()
        )
    }
}

pub(crate) fn when_unix(seconds: i64, now: OffsetDateTime) -> String {
    OffsetDateTime::from_unix_timestamp(seconds).map_or_else(|_| String::new(), |at| when(at, now))
}

/// The dot class and short label of a schedule, as the sidebar shows it.
fn state_of(info: &ScheduleInfo, now: OffsetDateTime) -> (&'static str, String) {
    let last = info.last_run.as_ref().map(|run| &run.outcome);
    if last == Some(&Outcome::Running) {
        return ("busy", "running".to_owned());
    }
    let next = info
        .next_due
        .map(|next| format!("next {}", when_unix(next, now)));
    match last {
        Some(Outcome::Failed { .. }) => ("closed", next.unwrap_or_else(|| "failed".to_owned())),
        _ if !info.schedule.spec.enabled => ("idle", "off".to_owned()),
        _ => ("ready", next.unwrap_or_default()),
    }
}

/// What one gateway agent listed.
type Listing = (String, Result<Vec<ScheduleInfo>, String>);

/// The selected project's schedules, under each of its gateway agents.
#[component]
pub fn ScheduleList(project: ProjectId, name: String) -> Element {
    let app = use_context::<AppSignals>();
    let endpoints = use_memo({
        let project = project.clone();
        move || -> Vec<(String, Result<SchedulesEndpoint, String>)> {
            let Ok(config) = config_of(app, &project) else {
                return Vec::new();
            };
            gateway_agents(&config)
                .into_iter()
                .map(|agent| {
                    let endpoint = endpoint(&config, &agent);
                    (agent, endpoint)
                })
                .collect()
        }
    });
    let mut listings = use_signal(Vec::<Listing>::new);
    // The run each schedule last showed, so a failure is announced once.
    let seen = use_hook(|| Rc::new(RefCell::new(HashMap::<String, i64>::new())));
    use_future({
        let source = name.clone();
        move || {
            let seen = seen.clone();
            let source = source.clone();
            poll(app.schedule_revision, LIST_EVERY, move || {
                let seen = seen.clone();
                let source = source.clone();
                async move {
                    let mut fetched = Vec::new();
                    for (agent, endpoint) in endpoints.peek().clone() {
                        let listed = match endpoint {
                            Ok(endpoint) => {
                                endpoint.list().await.map_err(|error| error.to_string())
                            }
                            Err(error) => Err(error),
                        };
                        fetched.push((agent, listed));
                    }
                    remember(app, &fetched, &mut seen.borrow_mut(), &source);
                    if *listings.peek() != fetched {
                        listings.set(fetched);
                    }
                }
            })
        }
    });
    let now = local_now();
    let open = match app.layout.read().content(app.layout.read().focused()) {
        Some(PaneContent::Schedule { schedule, .. }) => schedule.clone(),
        _ => None,
    };
    let agents: Vec<String> = endpoints
        .read()
        .iter()
        .map(|(agent, _)| agent.clone())
        .collect();
    let listings = listings.read().clone();

    rsx! {
        div { class: "sidebar-section",
            div { class: "sidebar-header",
                span { class: "section-label", "Schedules · {name}" }
            }
            if agents.is_empty() {
                p { class: "muted hint",
                    "Schedules run on a gateway agent, so they keep going while ukai is closed. This project has none."
                }
            }
            for agent in agents {
                div { key: "{agent}", class: "schedule-agent",
                    div { class: "schedule-agent-head",
                        span { class: "muted", "{agent}" }
                        button {
                            class: "icon",
                            title: "New schedule on {agent}",
                            onclick: {
                                let project = project.clone();
                                let agent = agent.clone();
                                move |_| open_schedule(app, &project, &agent, None)
                            },
                            Svg { icon: Icon::Plus }
                        }
                    }
                    match listings.iter().find(|(a, _)| *a == agent).map(|(_, listed)| listed.clone()) {
                        None => rsx! {},
                        Some(Err(error)) => rsx! {
                            p { class: "muted hint", title: "{error}", "Cannot read schedules: {error}" }
                        },
                        Some(Ok(list)) if list.is_empty() => rsx! {
                            p { class: "muted hint", "Run a command or a prompt on a schedule." }
                        },
                        Some(Ok(list)) => rsx! {
                            for info in list {
                                ScheduleRow {
                                    key: "{info.schedule.id}",
                                    project: project.clone(),
                                    focused: open.as_deref() == Some(info.schedule.id.as_str()),
                                    state: state_of(&info, now),
                                    info,
                                }
                            }
                        },
                    }
                }
            }
        }
    }
}

#[component]
fn ScheduleRow(
    project: ProjectId,
    info: ScheduleInfo,
    focused: bool,
    state: (&'static str, String),
) -> Element {
    let app = use_context::<AppSignals>();
    let (dot, label) = state;
    let title = info.schedule.spec.name.clone();
    rsx! {
        div {
            class: if focused { "row schedule-row focused" } else { "row schedule-row" },
            title: "{title} · {label}",
            onclick: move |_| {
                open_schedule(
                    app,
                    &project,
                    &info.schedule.agent,
                    Some(info.schedule.id.clone()),
                )
            },
            span { class: "dot {dot}" }
            span { class: "name", "{title}" }
            span { class: "meta", "{label}" }
        }
    }
}

/// Keep the names for pane titles, and announce runs that failed since the
/// last listing while the window is in the background.
fn remember(app: AppSignals, fetched: &[Listing], seen: &mut HashMap<String, i64>, source: &str) {
    let mut names = app.schedule_names;
    for (_, listed) in fetched {
        let Ok(list) = listed else {
            continue;
        };
        for info in list {
            let id = &info.schedule.id;
            if names.peek().get(id) != Some(&info.schedule.spec.name) {
                names
                    .write()
                    .insert(id.clone(), info.schedule.spec.name.clone());
            }
            let Some(run) = &info.last_run else {
                continue;
            };
            // A running run is looked at again once it ends.
            let key = if run.outcome == Outcome::Running {
                i64::MIN
            } else {
                run.started
            };
            let before = seen.insert(id.clone(), key);
            if let (Outcome::Failed { message }, Some(before)) = (&run.outcome, before)
                && before != key
            {
                alert::notify_if_away(
                    &format!("{} · {source}", info.schedule.spec.name),
                    &format!("Failed: {message}"),
                );
            }
        }
    }
}

/// A schedule's title for its pane header.
pub fn schedule_label(names: &HashMap<String, String>, schedule: Option<&String>) -> String {
    match schedule {
        None => "New schedule".to_owned(),
        Some(id) => names
            .get(id)
            .filter(|name| !name.trim().is_empty())
            .cloned()
            .unwrap_or_else(|| "Schedule".to_owned()),
    }
}

const PRESETS: [(&str, &str); 4] = [
    ("Hourly", "0 * * * *"),
    ("Daily 9:00", "0 9 * * *"),
    ("Weekdays 9:00", "0 9 * * 1-5"),
    ("Mondays 9:00", "0 9 * * 1"),
];

/// The local offset from UTC now, which a saved pattern is read at.
fn local_offset_minutes() -> i32 {
    i32::from(local_offset().whole_minutes())
}

fn new_spec() -> ScheduleSpec {
    ScheduleSpec {
        name: String::new(),
        cron: "0 9 * * 1-5".to_owned(),
        utc_offset_minutes: local_offset_minutes(),
        task: Task::Command {
            command: String::new(),
        },
        enabled: true,
    }
}

/// Whether the editor changed nothing: `enabled` has its own switch, and
/// the offset is taken afresh at each save.
fn same_edit(a: &ScheduleSpec, b: &ScheduleSpec) -> bool {
    a.name == b.name && a.cron.trim() == b.cron.trim() && a.task == b.task
}

/// What the gateway last said about the pane's schedule.
#[derive(Clone, PartialEq)]
enum Loaded {
    /// Not saved yet; nothing to read.
    New,
    Waiting,
    Found(Box<ScheduleInfo>, Vec<Run>),
    Failed(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    Run,
    Stop,
    Delete,
}

/// One schedule's settings, edited as a draft and saved together, and its
/// recent runs. `schedule` is `None` for one not saved yet.
#[component]
pub fn ScheduleView(
    pane: PaneId,
    project: ProjectId,
    agent: String,
    schedule: Option<String>,
) -> Element {
    let app = use_context::<AppSignals>();
    let endpoint = use_memo({
        let agent = agent.clone();
        let project = project.clone();
        move || config_of(app, &project).and_then(|config| endpoint(&config, &agent))
    });
    let mut loaded = use_signal(|| {
        if schedule.is_some() {
            Loaded::Waiting
        } else {
            Loaded::New
        }
    });
    let mut draft = use_signal(|| schedule.is_none().then(new_spec));
    let errors = use_signal(Vec::<String>::new);
    let busy = use_signal(|| false);

    use_future({
        let schedule = schedule.clone();
        move || {
            let schedule = schedule.clone();
            async move {
                let Some(id) = schedule else {
                    return;
                };
                poll(app.schedule_revision, PANE_EVERY, move || {
                    let id = id.clone();
                    async move {
                        let result = match endpoint.peek().clone() {
                            Ok(endpoint) => read(&endpoint, &id).await,
                            Err(error) => Loaded::Failed(error),
                        };
                        if let Loaded::Found(info, _) = &result
                            && draft.peek().is_none()
                        {
                            draft.set(Some(info.schedule.spec.clone()));
                        }
                        if *loaded.peek() != result {
                            loaded.set(result);
                        }
                    }
                })
                .await;
            }
        }
    });

    let (info, runs) = match loaded() {
        Loaded::Waiting => {
            return rsx! {
                div { class: "pane-body placeholder",
                    p { class: "muted", "Reading the schedule from the gateway…" }
                }
            };
        }
        Loaded::Failed(error) => {
            return rsx! {
                div { class: "pane-body placeholder",
                    p { class: "muted", "{error}" }
                }
            };
        }
        Loaded::New => (None, Vec::new()),
        Loaded::Found(info, runs) => (Some(*info), runs),
    };
    let Some(current) = draft() else {
        return rsx! {};
    };
    let saved = info.as_ref().map(|info| info.schedule.spec.clone());
    let changed = saved
        .as_ref()
        .is_none_or(|saved| !same_edit(saved, &current));
    let problems: Vec<String> = schedule::problems(&current)
        .iter()
        .map(ToString::to_string)
        .collect();
    let now = local_now();
    let description = schedule::describe(&current.cron);
    let next = schedule::next_after(&current.cron, now)
        .ok()
        .map(|next| when(next, now));
    let running = info
        .as_ref()
        .and_then(|info| info.last_run.as_ref())
        .is_some_and(|run| run.outcome == Outcome::Running);
    let is_prompt = matches!(current.task, Task::Prompt { .. });
    let endpoint_error = endpoint.read().as_ref().err().cloned();

    // Send `spec` to the gateway: create the schedule, or change it.
    let save = {
        let schedule = schedule.clone();
        let project = project.clone();
        let agent = agent.clone();
        move |spec: ScheduleSpec| {
            // Copies, so this stays callable from every handler.
            let (mut busy, mut errors, mut draft, mut loaded) = (busy, errors, draft, loaded);
            let Ok(endpoint) = endpoint.peek().clone() else {
                return;
            };
            let schedule = schedule.clone();
            let project = project.clone();
            let agent = agent.clone();
            busy.set(true);
            spawn(async move {
                let result = match &schedule {
                    Some(id) => endpoint.update(id, &spec).await,
                    None => endpoint.create(&spec).await,
                };
                busy.set(false);
                match result {
                    Ok(info) if schedule.is_none() => {
                        // The pane now shows the saved schedule.
                        let mut layout = app.layout;
                        layout.write().set_content(
                            pane,
                            PaneContent::Schedule {
                                project,
                                agent,
                                schedule: Some(info.schedule.id),
                            },
                        );
                        refresh(app);
                    }
                    Ok(info) => {
                        errors.set(Vec::new());
                        draft.set(Some(info.schedule.spec.clone()));
                        let runs = match &*loaded.peek() {
                            Loaded::Found(_, runs) => runs.clone(),
                            _ => Vec::new(),
                        };
                        loaded.set(Loaded::Found(Box::new(info), runs));
                        refresh(app);
                    }
                    Err(error) => errors.set(explain(&error)),
                }
            });
        }
    };
    // Act on the saved schedule, then read it again.
    let act = {
        let schedule = schedule.clone();
        move |action: Action| {
            let (mut busy, mut errors) = (busy, errors);
            let (Some(id), Ok(endpoint)) = (schedule.clone(), endpoint.peek().clone()) else {
                return;
            };
            busy.set(true);
            spawn(async move {
                let result = match action {
                    Action::Run => endpoint.run(&id).await.map(drop),
                    Action::Stop => endpoint.stop(&id).await.map(drop),
                    Action::Delete => endpoint.delete(&id).await,
                };
                busy.set(false);
                match result {
                    Ok(()) => {
                        errors.set(Vec::new());
                        if action == Action::Delete {
                            let mut layout = app.layout;
                            layout.write().set_content(pane, PaneContent::Empty);
                        }
                        refresh(app);
                    }
                    Err(error) => errors.set(explain(&error)),
                }
            });
        }
    };
    let shown_problems: Vec<String> = problems
        .iter()
        .chain(errors.read().iter())
        .chain(endpoint_error.iter())
        .cloned()
        .collect();

    rsx! {
        div { class: "pane-body schedule",
            div { class: "schedule-panel",
                div { class: "schedule-head",
                    input {
                        class: "schedule-name",
                        value: "{current.name}",
                        placeholder: "Name",
                        oninput: move |event| {
                            if let Some(draft) = draft.write().as_mut() {
                                draft.name = event.value();
                            }
                        },
                    }
                    if let Some(saved) = saved.clone() {
                        label { class: "toggle", title: "Run on schedule",
                            input {
                                r#type: "checkbox",
                                checked: saved.enabled,
                                disabled: busy(),
                                onchange: {
                                    let save = save.clone();
                                    move |event: FormEvent| {
                                        let mut spec = saved.clone();
                                        spec.enabled = event.checked();
                                        save(spec);
                                    }
                                },
                            }
                            "Enabled"
                        }
                    }
                }
                p { class: "muted field-hint",
                    "Runs on the gateway as {agent}, so it keeps going while ukai is closed."
                }
                div { class: "field",
                    span { class: "field-label", "Run" }
                    div { class: "segmented",
                        button {
                            class: if !is_prompt { "active" } else { "" },
                            onclick: move |_| {
                                if let Some(draft) = draft.write().as_mut()
                                    && matches!(draft.task, Task::Prompt { .. })
                                {
                                    draft.task = Task::Command {
                                        command: String::new(),
                                    };
                                }
                            },
                            "Command"
                        }
                        button {
                            class: if is_prompt { "active" } else { "" },
                            onclick: move |_| {
                                if let Some(draft) = draft.write().as_mut()
                                    && matches!(draft.task, Task::Command { .. })
                                {
                                    draft.task = Task::Prompt {
                                        prompt: String::new(),
                                    };
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
                                    if let Some(draft) = draft.write().as_mut() {
                                        draft.task = Task::Command {
                                            command: event.value(),
                                        };
                                    }
                                },
                            }
                            span { class: "muted field-hint",
                                "Runs with sh -c in the agent's checkout, in a pod of this schedule's own. Its output is kept with the run."
                            }
                        }
                    },
                    Task::Prompt { prompt } => rsx! {
                        div { class: "field",
                            span { class: "field-label", "Prompt" }
                            textarea {
                                rows: 5,
                                placeholder: "Look at the new issues and label them",
                                value: "{prompt}",
                                oninput: move |event| {
                                    if let Some(draft) = draft.write().as_mut() {
                                        draft.task = Task::Prompt {
                                            prompt: event.value(),
                                        };
                                    }
                                },
                            }
                            span { class: "muted field-hint",
                                "One turn in a session of its own. A permission request waits until you open the session and answer it."
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
                            oninput: move |event| {
                                if let Some(draft) = draft.write().as_mut() {
                                    draft.cron = event.value();
                                }
                            },
                        }
                        div { class: "segmented",
                            for (label, pattern) in PRESETS {
                                button {
                                    key: "{pattern}",
                                    class: if current.cron.trim() == pattern { "active" } else { "" },
                                    onclick: move |_| {
                                        if let Some(draft) = draft.write().as_mut() {
                                            draft.cron = pattern.to_owned();
                                        }
                                    },
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
                if !shown_problems.is_empty() {
                    ul { class: "problems",
                        for problem in shown_problems {
                            li { key: "{problem}", "{problem}" }
                        }
                    }
                }
                div { class: "schedule-actions",
                    button {
                        disabled: !changed || !problems.is_empty() || busy(),
                        onclick: {
                            let save = save.clone();
                            move |_| {
                                if let Some(mut spec) = draft.peek().clone() {
                                    spec.utc_offset_minutes = local_offset_minutes();
                                    save(spec);
                                }
                            }
                        },
                        if schedule.is_some() {
                            "Save"
                        } else {
                            "Create"
                        }
                    }
                    if let (true, Some(saved)) = (changed, saved.clone()) {
                        button {
                            class: "outline",
                            onclick: move |_| draft.set(Some(saved.clone())),
                            "Revert"
                        }
                    }
                    if schedule.is_some() {
                        if running {
                            button {
                                class: "outline",
                                disabled: busy(),
                                onclick: {
                                    let act = act.clone();
                                    move |_| act(Action::Stop)
                                },
                                Svg { icon: Icon::Stop }
                                "Stop"
                            }
                        } else {
                            button {
                                class: "outline",
                                title: if changed { "Save first" } else { "Run once now" },
                                disabled: changed || busy(),
                                onclick: {
                                    let act = act.clone();
                                    move |_| act(Action::Run)
                                },
                                "Run now"
                            }
                        }
                        span { class: "spacer" }
                        button {
                            class: "outline danger",
                            disabled: busy(),
                            onclick: {
                                let act = act.clone();
                                move |_| act(Action::Delete)
                            },
                            "Delete"
                        }
                    }
                }
            }
            if let Some(info) = info {
                div { class: "schedule-panel",
                    span { class: "section-label", "Runs" }
                    if runs.is_empty() {
                        p { class: "muted hint", "No runs yet." }
                    }
                    for (index, run) in runs.into_iter().enumerate() {
                        RunRow {
                            key: "{run.started}-{index}",
                            project: project.clone(),
                            info: info.clone(),
                            run,
                            open: index == 0,
                        }
                    }
                }
            }
        }
    }
}

/// The schedule `id` and its runs, newest first.
async fn read(endpoint: &SchedulesEndpoint, id: &str) -> Loaded {
    let info = match endpoint.get(id).await {
        Ok(info) => info,
        Err(SchedulesError::NotFound) => {
            return Loaded::Failed("This schedule was deleted.".to_owned());
        }
        Err(error) => return Loaded::Failed(error.to_string()),
    };
    match endpoint.runs(id).await {
        Ok(runs) => Loaded::Found(Box::new(info), runs),
        Err(error) => Loaded::Failed(error.to_string()),
    }
}

#[component]
fn RunRow(project: ProjectId, info: ScheduleInfo, run: Run, open: bool) -> Element {
    let app = use_context::<AppSignals>();
    let now = local_now();
    let (state, outcome) = match &run.outcome {
        Outcome::Running => ("busy", "Running".to_owned()),
        Outcome::Succeeded => ("ready", "Succeeded".to_owned()),
        Outcome::Failed { message } => ("closed", format!("Failed: {message}")),
        Outcome::Stopped => ("idle", "Stopped".to_owned()),
        Outcome::Interrupted => ("idle", "Interrupted (the gateway restarted)".to_owned()),
    };
    let started = when_unix(run.started, now);
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
            if let Some(session) = run.session.clone() {
                button {
                    class: "outline small",
                    onclick: move |_| open_run_session(app, &project, &info, &session),
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
