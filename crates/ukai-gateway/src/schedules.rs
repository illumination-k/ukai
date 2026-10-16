//! Schedules: a shell command in an agent's workspace, or one prompt turn of
//! the agent, run on a cron pattern by the gateway, so they run while no
//! client is connected.
//!
//! Each schedule is a ConfigMap of its own (`ukai-schedule-<id>`, labelled
//! [`SCHEDULE_LABEL`]) holding the schedule and its recent runs, so they
//! survive the gateway restarting. Every run of a schedule asks for the same
//! workspace (`pod` scope, keyed by [`Schedule::workspace_key`]), so a client
//! can open that pod, and a prompt run's session, afterwards.
//!
//! When a schedule is due, and what is wrong with one, is
//! `ukai_core::schedule`'s business; this module stores, fires, and runs.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use futures::StreamExt;
use k8s_openapi::api::core::v1::ConfigMap;
use kube::api::ObjectMeta;
use tokio::io::AsyncReadExt;
use tokio::sync::{mpsc, oneshot};
use ukai_acp::{SessionEvent, SessionOptions};
use ukai_config::AgentSpec;
use ukai_core::schedule::{
    Outcome, Run, Schedule, ScheduleInfo, ScheduleSpec, Schedules, Task, Trigger,
};

use crate::Gateway;
use crate::cluster::RelayEndpoint;
use crate::job::{MANAGED_BY, Plan, REPO_DIR};

/// Label on every schedule's ConfigMap; its value is the schedule's id.
pub const SCHEDULE_LABEL: &str = "ukai.dev/schedule";
const SCHEDULE_KEY: &str = "schedule.json";
const RUNS_KEY: &str = "runs.json";

/// How often due schedules are looked for. Patterns have minute granularity.
const TICK: Duration = Duration::from_secs(15);
/// How often a running run's output is saved.
const FLUSH: Duration = Duration::from_secs(10);

/// A run's pid file in the agent container, for stopping a command.
fn pid_file(id: &str) -> String {
    format!("/tmp/ukai-schedule-{id}.pid")
}

/// Runs `$2` from the checkout, leaving its pid in `$1` for [`STOP`].
const WRAPPER: &str = r#"echo $$ > "$1"; cd "$3" && exec sh -c "$2""#;
const STOP: &str = r#"kill -TERM "$(cat "$1")" 2>/dev/null; rm -f "$1""#;

enum Store {
    Save(String),
    Delete(String),
}

/// The gateway's schedules, and the runs going on.
pub struct Scheduler {
    state: Mutex<Schedules>,
    /// A run going on, by schedule id; sending stops it.
    running: Mutex<HashMap<String, oneshot::Sender<()>>>,
    store: mpsc::UnboundedSender<Store>,
}

impl Scheduler {
    /// Read the saved schedules; runs that were going when the gateway
    /// stopped are marked interrupted. Also returns what writes them back.
    pub async fn load(
        gateway_cluster: &crate::cluster::Cluster,
    ) -> Result<(Self, Saver), crate::cluster::ClusterError> {
        let now = now();
        let mut schedules = Schedules::default();
        for map in gateway_cluster.config_maps(SCHEDULE_LABEL).await? {
            let name = map.metadata.name.clone().unwrap_or_default();
            match from_config_map(&map) {
                Some((schedule, runs)) => schedules.restore(schedule, runs, now),
                None => {
                    tracing::warn!(config_map = %name, "skipping a schedule that cannot be read")
                }
            }
        }
        let (store, saves) = mpsc::unbounded_channel();
        for schedule in schedules.iter() {
            let _ = store.send(Store::Save(schedule.id.clone()));
        }
        tracing::info!(schedules = schedules.iter().count(), "schedules loaded");
        Ok((
            Self {
                state: Mutex::new(schedules),
                running: Mutex::new(HashMap::new()),
                store,
            },
            Saver(saves),
        ))
    }

    fn state(&self) -> MutexGuard<'_, Schedules> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn running(&self) -> MutexGuard<'_, HashMap<String, oneshot::Sender<()>>> {
        self.running
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn save(&self, id: &str) {
        let _ = self.store.send(Store::Save(id.to_owned()));
    }

    /// `agent`'s schedules, by name.
    pub fn list(&self, agent: &str) -> Vec<ScheduleInfo> {
        let state = self.state();
        let mut list: Vec<ScheduleInfo> = state
            .iter()
            .filter(|s| s.agent == agent)
            .filter_map(|s| state.info(&s.id))
            .collect();
        list.sort_by(|a, b| a.schedule.spec.name.cmp(&b.schedule.spec.name));
        list
    }

    /// `id` if it is one of `agent`'s.
    pub fn info(&self, agent: &str, id: &str) -> Option<ScheduleInfo> {
        self.state()
            .info(id)
            .filter(|info| info.schedule.agent == agent)
    }

    pub fn runs(&self, agent: &str, id: &str) -> Option<Vec<Run>> {
        let state = self.state();
        state.get(id).filter(|s| s.agent == agent)?;
        Some(state.runs(id).iter().rev().cloned().collect())
    }

    pub fn create(&self, agent: &str, spec: ScheduleSpec) -> Result<ScheduleInfo, Vec<String>> {
        refuse(&spec)?;
        let id = crate::cluster::token()
            .map_err(|error| vec![error.to_string()])?
            .chars()
            .take(12)
            .collect::<String>();
        let mut state = self.state();
        state.add(id.clone(), agent.to_owned(), spec, now());
        self.save(&id);
        Ok(state
            .info(&id)
            .unwrap_or_else(|| unreachable!("just added")))
    }

    /// `Ok(None)` when `id` is not one of `agent`'s.
    pub fn update(
        &self,
        agent: &str,
        id: &str,
        spec: ScheduleSpec,
    ) -> Result<Option<ScheduleInfo>, Vec<String>> {
        refuse(&spec)?;
        let mut state = self.state();
        if state.get(id).is_none_or(|s| s.agent != agent) {
            return Ok(None);
        }
        state.update(id, spec, now());
        self.save(id);
        Ok(state.info(id))
    }

    /// Remove `id`, stopping a run of it; `false` when it is not one of `agent`'s.
    pub fn delete(&self, agent: &str, id: &str) -> bool {
        let mut state = self.state();
        if state.get(id).is_none_or(|s| s.agent != agent) {
            return false;
        }
        state.remove(id);
        if let Some(stop) = self.running().remove(id) {
            let _ = stop.send(());
        }
        let _ = self.store.send(Store::Delete(id.to_owned()));
        true
    }

    /// Ask a run of `id` to stop; it records how it ended itself.
    pub fn stop(&self, agent: &str, id: &str) -> Option<ScheduleInfo> {
        let info = self.info(agent, id)?;
        if let Some(stop) = self.running().remove(id) {
            let _ = stop.send(());
        }
        Some(info)
    }

    fn append(&self, id: &str, text: &str) {
        self.state().append_output(id, text);
    }
}

/// `Err` with every problem when `spec` cannot be saved.
fn refuse(spec: &ScheduleSpec) -> Result<(), Vec<String>> {
    let problems = ukai_core::schedule::problems(spec);
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems.iter().map(ToString::to_string).collect())
    }
}

fn now() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}

/// Writes schedules to their ConfigMaps in the order they changed, each from
/// its latest state.
pub struct Saver(mpsc::UnboundedReceiver<Store>);

impl Saver {
    pub async fn run(mut self, gateway: Arc<Gateway>) {
        while let Some(store) = self.0.recv().await {
            let result = match store {
                Store::Save(id) => {
                    let map = {
                        let state = gateway.schedules.state();
                        state
                            .get(&id)
                            .map(|schedule| config_map(schedule, state.runs(&id)))
                    };
                    match map {
                        Some(map) => gateway.cluster.apply_config_map(&map).await,
                        None => Ok(()),
                    }
                }
                Store::Delete(id) => gateway.cluster.delete_config_map(&map_name(&id)).await,
            };
            if let Err(error) = result {
                tracing::warn!(%error, "cannot save a schedule");
            }
        }
    }
}

/// Fire due schedules until the gateway stops.
pub async fn tick(gateway: Arc<Gateway>) {
    let mut interval = tokio::time::interval(TICK);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        interval.tick().await;
        let now = now();
        let due = {
            let mut state = gateway.schedules.state();
            let due = state.tick(now);
            for schedule in state.iter().filter(|s| s.last_due == now) {
                gateway.schedules.save(&schedule.id);
            }
            due
        };
        for id in due {
            fire(&gateway, &id, Trigger::Schedule);
        }
    }
}

/// Start a run of `id`; `false` when one is already going or it does not
/// exist.
pub fn fire(gateway: &Arc<Gateway>, id: &str, trigger: Trigger) -> bool {
    let schedule = {
        let mut state = gateway.schedules.state();
        if !state.start(id, trigger, now()) {
            return false;
        }
        state.get(id).cloned()
    };
    let Some(schedule) = schedule else {
        return false;
    };
    gateway.schedules.save(id);
    tracing::info!(schedule = %id, agent = %schedule.agent, ?trigger, "schedule run starting");
    let (stop_tx, stop_rx) = oneshot::channel();
    // Held while spawning, so the run cannot remove its entry before it is in.
    let mut running = gateway.schedules.running();
    running.insert(id.to_owned(), stop_tx);
    let gateway = gateway.clone();
    let id = id.to_owned();
    tokio::spawn(async move {
        let outcome = execute(&gateway, &schedule, stop_rx).await;
        tracing::info!(schedule = %id, ?outcome, "schedule run ended");
        gateway.schedules.running().remove(&id);
        gateway.schedules.state().finish(&id, outcome, now());
        gateway.schedules.save(&id);
    });
    drop(running);
    true
}

async fn execute(
    gateway: &Arc<Gateway>,
    schedule: &Schedule,
    mut stop: oneshot::Receiver<()>,
) -> Outcome {
    let Some(agent) = gateway.agents.get(&schedule.agent).cloned() else {
        return failed(format!("the gateway has no agent `{}`", schedule.agent));
    };
    let plan = Plan::new(
        agent,
        &gateway.user,
        gateway.cluster.namespace(),
        Some(&schedule.workspace_key),
    );
    let id = schedule.id.as_str();
    let pod = tokio::select! {
        pod = gateway.cluster.ensure_pod(&plan, &gateway.relay_image, |message| {
            tracing::debug!(schedule = %id, %message, "waiting for the workspace");
        }) => pod,
        _ = &mut stop => return Outcome::Stopped,
    };
    let pod = match pod {
        Ok(pod) => pod,
        Err(error) => return failed(error.to_string()),
    };
    let Some(endpoint) = RelayEndpoint::of(&pod) else {
        return failed("the workspace's pod has no address".to_owned());
    };
    let flush = {
        let gateway = gateway.clone();
        let id = schedule.id.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval_at(tokio::time::Instant::now() + FLUSH, FLUSH);
            loop {
                interval.tick().await;
                gateway.schedules.save(&id);
            }
        })
    };
    let outcome = match &schedule.spec.task {
        Task::Command { command } => {
            let pod_name = pod.metadata.name.clone().unwrap_or_default();
            run_command(gateway, id, &pod_name, &endpoint, command, stop).await
        }
        Task::Prompt { prompt } => run_prompt(gateway, id, &endpoint, prompt, stop).await,
    };
    flush.abort();
    outcome
}

fn failed(message: String) -> Outcome {
    Outcome::Failed { message }
}

/// `sh -c <command>` in the agent container, from the checkout. A connection
/// to the relay is held meanwhile, so the pod does not idle out under it.
async fn run_command(
    gateway: &Arc<Gateway>,
    id: &str,
    pod: &str,
    endpoint: &RelayEndpoint,
    command: &str,
    mut stop: oneshot::Receiver<()>,
) -> Outcome {
    let hold = match crate::connect_relay_at(endpoint).await {
        Ok(upstream) => tokio::spawn(async move {
            // Read, so pings are answered, until the run drops it.
            let (_tx, mut rx) = upstream.split();
            while rx.next().await.is_some() {}
        }),
        Err(error) => return failed(format!("cannot reach the workspace: {error}")),
    };
    let pid = pid_file(id);
    let argv = ["sh", "-c", WRAPPER, "sh", &pid, command, REPO_DIR]
        .map(str::to_owned)
        .to_vec();
    let mut process = match gateway.cluster.exec_in_agent(pod, argv).await {
        Ok(process) => process,
        Err(error) => {
            hold.abort();
            return failed(format!("cannot run the command: {error}"));
        }
    };
    let status = process.take_status();
    let (Some(mut stdout), Some(mut stderr)) = (process.stdout(), process.stderr()) else {
        hold.abort();
        return failed("the command's output cannot be read".to_owned());
    };
    let mut out = Utf8::default();
    let mut err = Utf8::default();
    let mut out_buf = [0u8; 8192];
    let mut err_buf = [0u8; 8192];
    let (mut out_open, mut err_open) = (true, true);
    let stopped = loop {
        if !out_open && !err_open {
            break false;
        }
        tokio::select! {
            read = stdout.read(&mut out_buf), if out_open => match read {
                Ok(0) | Err(_) => out_open = false,
                Ok(n) => gateway.schedules.append(id, &out.push(&out_buf[..n])),
            },
            read = stderr.read(&mut err_buf), if err_open => match read {
                Ok(0) | Err(_) => err_open = false,
                Ok(n) => gateway.schedules.append(id, &err.push(&err_buf[..n])),
            },
            _ = &mut stop => break true,
        }
    };
    let outcome = if stopped {
        let kill = ["sh", "-c", STOP, "sh", &pid].map(str::to_owned).to_vec();
        if let Ok(killer) = gateway.cluster.exec_in_agent(pod, kill).await {
            let _ = killer.join().await;
        }
        Outcome::Stopped
    } else {
        match status {
            Some(status) => match status.await {
                Some(status) if status.status.as_deref() == Some("Success") => Outcome::Succeeded,
                Some(status) => failed(
                    status
                        .message
                        .unwrap_or_else(|| "the command failed".to_owned()),
                ),
                None => failed("the command's exit status was lost".to_owned()),
            },
            None => failed("the command's exit status was lost".to_owned()),
        }
    };
    hold.abort();
    outcome
}

/// One prompt turn in a session of its own on the workspace's relay. A
/// permission request is left for a client to answer: the relay sends it to
/// whoever opens the session.
async fn run_prompt(
    gateway: &Arc<Gateway>,
    id: &str,
    endpoint: &RelayEndpoint,
    prompt: &str,
    mut stop: oneshot::Receiver<()>,
) -> Outcome {
    let spec = AgentSpec::Websocket {
        url: endpoint.acp_url(),
        headers: BTreeMap::from([(
            "Authorization".to_owned(),
            format!("Bearer {}", endpoint.token),
        )]),
    };
    let transport = match ukai_acp::resolve(&spec, |_, _| None, None) {
        Ok(transport) => transport,
        Err(error) => return failed(error.to_string()),
    };
    let (session, mut events) = ukai_acp::spawn_session(
        transport,
        SessionOptions {
            cwd: REPO_DIR.into(),
            fs_root: None,
            restore: None,
            update_check: None,
        },
    );
    loop {
        let event = tokio::select! {
            event = events.recv() => event,
            _ = &mut stop => {
                session.cancel();
                // Wait briefly for the turn to end, so the agent is not left
                // working on a run nobody follows.
                match tokio::time::timeout(Duration::from_secs(10), async {
                    while let Some(event) = events.recv().await {
                        if matches!(event, SessionEvent::TurnEnded { .. } | SessionEvent::PromptFailed { .. } | SessionEvent::Closed { .. }) {
                            break;
                        }
                    }
                })
                .await
                {
                    Ok(()) | Err(_) => return Outcome::Stopped,
                }
            }
        };
        let Some(event) = event else {
            return failed("the session ended".to_owned());
        };
        match event {
            SessionEvent::Ready { session_id, .. } => {
                gateway.schedules.state().record_session(id, session_id);
                gateway.schedules.save(id);
                session.prompt(prompt);
            }
            SessionEvent::AgentMessage { text } => gateway.schedules.append(id, &text),
            SessionEvent::ToolCall { title, .. } => {
                gateway.schedules.append(id, &format!("\n▸ {title}\n"));
            }
            SessionEvent::PermissionRequested { title, .. } => {
                gateway.schedules.append(
                    id,
                    &format!("\n⏸ Waiting for permission: {title}. Open the session to answer.\n"),
                );
            }
            SessionEvent::TurnEnded { stop_reason } => {
                return if stop_reason == "end_turn" {
                    Outcome::Succeeded
                } else {
                    failed(format!("the turn ended: {stop_reason}"))
                };
            }
            SessionEvent::PromptFailed { message } => return failed(message),
            SessionEvent::Closed { error } => {
                return failed(error.unwrap_or_else(|| "the connection closed".to_owned()));
            }
            _ => {}
        }
    }
}

/// Bytes read in chunks, decoded without splitting a character.
#[derive(Default)]
struct Utf8(Vec<u8>);

impl Utf8 {
    fn push(&mut self, bytes: &[u8]) -> String {
        self.0.extend_from_slice(bytes);
        let valid = match std::str::from_utf8(&self.0) {
            Ok(_) => self.0.len(),
            // An incomplete character at the end waits for the next chunk;
            // anything else invalid is replaced.
            Err(error) if error.error_len().is_none() => error.valid_up_to(),
            Err(_) => self.0.len(),
        };
        let rest = self.0.split_off(valid);
        let text = String::from_utf8_lossy(&self.0).into_owned();
        self.0 = rest;
        text
    }
}

fn map_name(id: &str) -> String {
    format!("ukai-schedule-{id}")
}

/// The ConfigMap `schedule` and its `runs` are saved in.
pub fn config_map(schedule: &Schedule, runs: &[Run]) -> ConfigMap {
    ConfigMap {
        metadata: ObjectMeta {
            name: Some(map_name(&schedule.id)),
            labels: Some(BTreeMap::from([
                (MANAGED_BY.to_owned(), "ukai".to_owned()),
                (SCHEDULE_LABEL.to_owned(), schedule.id.clone()),
            ])),
            ..ObjectMeta::default()
        },
        data: Some(BTreeMap::from([
            (SCHEDULE_KEY.to_owned(), json(schedule)),
            (RUNS_KEY.to_owned(), json(runs)),
        ])),
        ..ConfigMap::default()
    }
}

fn json<T: serde::Serialize + ?Sized>(value: &T) -> String {
    serde_json::to_string(value).unwrap_or_default()
}

/// A schedule and its runs back from [`config_map`]; `None` when the
/// schedule cannot be read. Runs that cannot be read are dropped.
pub fn from_config_map(map: &ConfigMap) -> Option<(Schedule, Vec<Run>)> {
    let data = map.data.as_ref()?;
    let schedule: Schedule = serde_json::from_str(data.get(SCHEDULE_KEY)?).ok()?;
    let runs = data
        .get(RUNS_KEY)
        .and_then(|runs| serde_json::from_str(runs).ok())
        .unwrap_or_default();
    Some((schedule, runs))
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    fn schedule() -> Schedule {
        Schedule {
            id: "a1b2".into(),
            agent: "claude".into(),
            spec: ScheduleSpec {
                name: "Nightly".into(),
                cron: "0 3 * * *".into(),
                utc_offset_minutes: 540,
                task: Task::Command {
                    command: "make test".into(),
                },
                enabled: true,
            },
            workspace_key: "schedule-a1b2".into(),
            last_due: 100,
        }
    }

    #[test]
    fn a_schedule_round_trips_through_its_config_map() {
        let runs = vec![Run {
            trigger: Trigger::Manual,
            started: 100,
            finished: Some(160),
            outcome: Outcome::Failed {
                message: "exit 2".into(),
            },
            output: "boom".into(),
            truncated: false,
            session: None,
        }];
        let map = config_map(&schedule(), &runs);
        assert_eq!(map.metadata.name.as_deref(), Some("ukai-schedule-a1b2"));
        assert_eq!(
            map.metadata
                .labels
                .as_ref()
                .and_then(|labels| labels.get(SCHEDULE_LABEL))
                .map(String::as_str),
            Some("a1b2")
        );
        assert_eq!(from_config_map(&map), Some((schedule(), runs)));
    }

    #[test]
    fn unreadable_runs_do_not_lose_the_schedule() {
        let mut map = config_map(&schedule(), &[]);
        if let Some(data) = map.data.as_mut() {
            data.insert(RUNS_KEY.into(), "not json".into());
        }
        assert_eq!(from_config_map(&map), Some((schedule(), Vec::new())));
        map.data = None;
        assert_eq!(from_config_map(&map), None);
    }

    #[rstest]
    #[case::whole(&[&b"h\xc3\xa9"[..]], &["hé"])]
    #[case::split(&[&b"h\xc3"[..], &b"\xa9!"[..]], &["h", "é!"])]
    #[case::invalid(&[&b"a\xffb"[..]], &["a\u{fffd}b"])]
    fn decodes_output_across_chunks(#[case] chunks: &[&[u8]], #[case] expected: &[&str]) {
        let mut utf8 = Utf8::default();
        let decoded: Vec<String> = chunks.iter().map(|chunk| utf8.push(chunk)).collect();
        assert_eq!(decoded, expected);
    }
}
