//! Runs [ayaito] flows in the checkout, with the agent this relay keeps as
//! the agent of their nodes.
//!
//! A flow is a script: one the checkout's `ukai.toml` declares
//! (`[workflows.<name>]`, started by the user), or one the agent wrote for
//! the task at hand and starts through the MCP server (`crate::mcp`), by
//! path or as source. The relay runs it with Node (`runner.mjs`, kept in
//! this binary) as a process of its own and reads what the run leaves in
//! its directory: ayaito's events, the flow's outline, and how it ended.
//!
//! An agent node does not start an agent of its own. The run asks the relay
//! for a turn ([`AGENT_PATH`]), and the relay opens a session on its own
//! `/acp`, as a client would. So a node's work is an ordinary session of
//! the workspace's agent: recorded by the hub, listed with the run, and
//! opened in a pane like any other. A flow runs with nobody watching, so a
//! node's permission requests are answered from its `access`
//! ([`ukai_core::workflow::permit`]).
//!
//! Runs are kept under the relay's temporary directory, out of the checkout,
//! and last as long as the pod.
//!
//! [ayaito]: https://github.com/illumination-k/ayaito

use std::collections::{BTreeMap, HashMap};
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::extract::{Path as UrlPath, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;
use ukai_acp::workflows::{AGENT_PATH, RUNS_PATH, WORKFLOWS_PATH};
use ukai_acp::{
    ConfigInput, ConfigOption, ConfigValue, SessionEvent, SessionEvents, SessionHandle,
    SessionOptions,
};
use ukai_config::AgentSpec;
use ukai_core::workflow::{
    self, AgentAnswer, AgentCall, DeclaredFlow, Launcher, NodeSession, Outcome, RUNS_KEPT, Run,
    RunSummary, StartRequest, WorkflowListing,
};

/// The runner, written next to the runs when the first one starts.
const RUNNER: &str = include_str!("workflows/runner.mjs");

/// Names a `node_modules` directory holding ayaito, for scripts the agent
/// passes as source: they are kept out of the checkout, where they could
/// import nothing. Without it, the checkout's own `node_modules` is used.
pub const MODULES_ENV: &str = "UKAI_WORKFLOW_MODULES";

/// How long a turn that is being abandoned may take to end.
const CANCEL_GRACE: Duration = Duration::from_secs(10);

/// What a run's `run.json` keeps: what was started, by whom, and when.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Meta {
    flow: String,
    #[serde(default)]
    by: Launcher,
    started: u64,
}

/// The workflows of one relay.
#[derive(Clone)]
pub struct Workflows(Arc<Inner>);

struct Inner {
    /// The checkout.
    root: PathBuf,
    /// Where runs and the agent's scripts are kept.
    home: PathBuf,
    /// The `node` to run flows with.
    node: OsString,
    /// This relay, as a run and its agent calls reach it.
    port: u16,
    token: Option<String>,
    /// The runs whose process lives, each with what stops it.
    running: Mutex<HashMap<String, oneshot::Sender<()>>>,
}

impl Workflows {
    /// Workflows of the checkout at `root`, kept in `home`, on the relay
    /// listening on `port` with `token`.
    pub fn new(root: PathBuf, home: PathBuf, port: u16, token: Option<String>) -> Self {
        Self(Arc::new(Inner {
            root,
            home,
            node: OsString::from("node"),
            port,
            token,
            running: Mutex::new(HashMap::new()),
        }))
    }

    fn runs(&self) -> PathBuf {
        self.0.home.join("runs")
    }

    fn dir(&self, id: &str) -> PathBuf {
        self.runs().join(id)
    }

    fn alive(&self, id: &str) -> bool {
        self.0
            .running
            .lock()
            .is_ok_and(|running| running.contains_key(id))
    }

    /// Whether a run is going, which keeps the relay from leaving as idle.
    pub fn busy(&self) -> bool {
        self.0
            .running
            .lock()
            .is_ok_and(|running| !running.is_empty())
    }

    /// The flows the checkout declares. Read each time, so an edit to
    /// `ukai.toml` in the pod shows at once.
    pub fn declared(&self) -> Vec<DeclaredFlow> {
        declared(&self.0.root)
    }

    /// The declared flows and the runs kept, newest first.
    pub fn listing(&self) -> WorkflowListing {
        let mut runs: Vec<RunSummary> = std::fs::read_dir(self.runs())
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|entry| self.summary(&entry.file_name().to_string_lossy()))
            .collect();
        runs.sort_by(|a, b| b.started.cmp(&a.started).then_with(|| b.id.cmp(&a.id)));
        WorkflowListing {
            flows: self.declared(),
            runs,
        }
    }

    /// The run `id`, or `None` when none is kept under it.
    pub fn summary(&self, id: &str) -> Option<RunSummary> {
        if !workflow::is_run_id(id) {
            return None;
        }
        let dir = self.dir(id);
        let meta: Meta = read_json(&dir.join("run.json"))?;
        let result = dir.join("result.json");
        let outcome: Option<Outcome> = read_json(&result);
        let (state, detail) = workflow::run_state(outcome.as_ref(), self.alive(id));
        let finished = outcome.is_some().then(|| modified(&result)).flatten();
        Some(RunSummary {
            id: id.to_owned(),
            flow: meta.flow,
            by: meta.by,
            started: meta.started,
            finished,
            state,
            detail,
        })
    }

    /// The run `id` with its nodes, output, and what it printed.
    pub fn detail(&self, id: &str) -> Option<Run> {
        let summary = self.summary(id)?;
        let dir = self.dir(id);
        let text = |name: &str| std::fs::read_to_string(dir.join(name)).unwrap_or_default();
        let graph = read_json(&dir.join("graph.json")).unwrap_or_else(|| {
            // A run that has drawn nothing yet (or never will) still has
            // the flow itself to hang its sessions and state on.
            vec![workflow::Node {
                name: summary.flow.clone(),
                kind: workflow::NodeKind::Flow,
                ..workflow::Node::default()
            }]
        });
        let sessions: Vec<NodeSession> = text("sessions.jsonl")
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect();
        let nodes = workflow::fold(
            graph,
            &workflow::events(&text("events.jsonl")),
            &sessions,
            summary.state,
        );
        let output = match read_json(&dir.join("result.json")) {
            Some(Outcome::Done { output }) => serde_json::to_string_pretty(&output)
                .ok()
                .map(|output| workflow::head(&output).to_owned()),
            _ => None,
        };
        Some(Run {
            summary,
            input: text("input.json"),
            nodes,
            output,
            log: workflow::tail(&text("log.txt")).to_owned(),
        })
    }

    /// Start what `request` names. Everything wrong with it is told at
    /// once, a line each.
    pub fn start(&self, request: &StartRequest) -> Result<RunSummary, Vec<String>> {
        let problems = workflow::problems(request);
        if !problems.is_empty() {
            return Err(problems.iter().map(ToString::to_string).collect());
        }
        let id = request.run.clone().unwrap_or_else(|| self.new_id());
        if self.alive(&id) {
            return Err(vec![format!("run {id} is still running")]);
        }
        let dir = self.dir(&id);
        std::fs::create_dir_all(&dir).map_err(|error| vec![cannot("keep the run", &error)])?;
        let (flow, file) = self.source(request, &id)?;
        let input = match request.input.trim() {
            "" => "null",
            input => input,
        };
        let meta = Meta {
            flow,
            by: request.by,
            started: now(),
        };
        // A run started again resumes: ayaito's events and checkpoints stay,
        // and how the last attempt ended goes.
        let _ = std::fs::remove_file(dir.join("result.json"));
        let runner = self.0.home.join("runner.mjs");
        std::fs::write(&runner, RUNNER)
            .and_then(|()| std::fs::write(dir.join("input.json"), input))
            .and_then(|()| {
                std::fs::write(
                    dir.join("run.json"),
                    serde_json::to_vec(&meta).unwrap_or_default(),
                )
            })
            .map_err(|error| vec![cannot("keep the run", &error)])?;
        self.spawn(&id, &runner, &file)
            .map_err(|error| vec![cannot("start node", &error)])?;
        self.prune();
        self.summary(&id)
            .ok_or_else(|| vec!["the run was not kept".to_owned()])
    }

    /// The label and the script of what `request` names; the script of a
    /// `script` request is written first.
    fn source(&self, request: &StartRequest, id: &str) -> Result<(String, PathBuf), Vec<String>> {
        if let Some(name) = &request.flow {
            let flows = self.declared();
            let Some(flow) = flows.iter().find(|flow| &flow.name == name) else {
                let known: Vec<_> = flows.iter().map(|flow| flow.name.as_str()).collect();
                return Err(vec![format!(
                    "the checkout's ukai.toml declares no workflow `{name}` (it declares: {})",
                    if known.is_empty() {
                        "none".to_owned()
                    } else {
                        known.join(", ")
                    }
                )]);
            };
            let file = self.in_checkout(&flow.path)?;
            return Ok((name.clone(), file));
        }
        if let Some(path) = &request.path {
            return Ok((path.clone(), self.in_checkout(path)?));
        }
        let script = request.script.as_deref().unwrap_or_default();
        let scripts = self.0.home.join("scripts");
        let file = scripts.join(format!("{id}.ts"));
        std::fs::create_dir_all(&scripts)
            .and_then(|()| std::fs::write(&file, script))
            .map_err(|error| vec![cannot("keep the script", &error)])?;
        self.link_modules()?;
        Ok((format!("script {id}"), file))
    }

    /// `path` as a file of the checkout.
    fn in_checkout(&self, path: &str) -> Result<PathBuf, Vec<String>> {
        let relative = Path::new(path);
        let inside = !path.is_empty()
            && relative
                .components()
                .all(|c| matches!(c, Component::Normal(_) | Component::CurDir));
        let file = self.0.root.join(relative);
        if inside && file.is_file() {
            Ok(file)
        } else {
            Err(vec![format!("`{path}` is not a file in the checkout")])
        }
    }

    /// Lets the scripts kept here import ayaito: `node_modules` next to
    /// them points at [`MODULES_ENV`]'s directory, or else the checkout's.
    fn link_modules(&self) -> Result<(), Vec<String>> {
        let link = self.0.home.join("node_modules");
        let target = std::env::var_os(MODULES_ENV)
            .map(PathBuf::from)
            .unwrap_or_else(|| self.0.root.join("node_modules"));
        if !target.join("ayaito").is_dir() {
            return Err(vec![format!(
                "ayaito is not installed in {}: run `npm install ayaito zod` in the checkout, or start a script of the checkout by `path`",
                target.display()
            )]);
        }
        let _ = std::fs::remove_file(&link);
        symlink(&target, &link).map_err(|error| vec![cannot("reach ayaito", &error)])
    }

    fn new_id(&self) -> String {
        let base = format!("run-{:x}", now());
        (0..)
            .map(|n| match n {
                0 => base.clone(),
                n => format!("{base}-{n}"),
            })
            .find(|id| !self.dir(id).exists())
            .unwrap_or(base)
    }

    fn spawn(&self, id: &str, runner: &Path, file: &Path) -> std::io::Result<()> {
        let dir = self.dir(id);
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("log.txt"))?;
        let mut command = tokio::process::Command::new(&self.0.node);
        command
            // A script outside a `"type": "module"` package and TypeScript's
            // types being stripped are both as meant; the log is for the run.
            .arg("--disable-warning=MODULE_TYPELESS_PACKAGE_JSON")
            .arg("--disable-warning=ExperimentalWarning")
            .arg(runner)
            .arg(file)
            .arg(self.runs())
            .arg(id)
            .current_dir(&self.0.root)
            .env(
                "UKAI_WORKFLOW_URL",
                format!("http://127.0.0.1:{}", self.0.port),
            )
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            .kill_on_drop(true);
        match &self.0.token {
            Some(token) => command.env(crate::mcp::TOKEN_ENV, token),
            None => command.env_remove(crate::mcp::TOKEN_ENV),
        };
        // A group of its own, so that stopping the run also stops what its
        // code nodes started.
        #[cfg(unix)]
        command.process_group(0);
        let mut child = command.spawn()?;
        let (stop, stopped) = oneshot::channel();
        if let Ok(mut running) = self.0.running.lock() {
            running.insert(id.to_owned(), stop);
        }
        tracing::info!(run = id, "workflow run started");
        let (this, id) = (self.clone(), id.to_owned());
        tokio::spawn(async move {
            let result = dir.join("result.json");
            let exited = tokio::select! {
                status = child.wait() => Some(status),
                _ = stopped => None,
            };
            let outcome = match exited {
                Some(status) => Outcome::Failed {
                    error: match status {
                        Ok(status) => format!("the run ended without a result ({status})"),
                        Err(error) => format!("the run was lost: {error}"),
                    },
                },
                None => {
                    kill(&mut child);
                    let _ = child.wait().await;
                    Outcome::Cancelled
                }
            };
            // The run's own word on how it ended stands, unless it was
            // stopped before it had one.
            if !result.exists() {
                let _ = std::fs::write(&result, serde_json::to_vec(&outcome).unwrap_or_default());
            }
            if let Ok(mut running) = this.0.running.lock() {
                running.remove(&id);
            }
            tracing::info!(run = %id, "workflow run ended");
        });
        Ok(())
    }

    /// Stop the run `id` if it runs; the run as it is once that is asked.
    pub async fn stop(&self, id: &str) -> Option<RunSummary> {
        let stop = self
            .0
            .running
            .lock()
            .ok()
            .and_then(|mut running| running.remove(id));
        if let Some(stop) = stop {
            // Cancelled from here on, whatever the process still writes.
            let result = self.dir(id).join("result.json");
            if !result.exists() {
                let cancelled = serde_json::to_vec(&Outcome::Cancelled).unwrap_or_default();
                let _ = std::fs::write(&result, cancelled);
            }
            let _ = stop.send(());
        }
        self.summary(id)
    }

    /// Drops the oldest finished runs beyond [`RUNS_KEPT`].
    fn prune(&self) {
        let runs = self.listing().runs;
        for run in runs.iter().skip(RUNS_KEPT) {
            if !run.state.is_running() {
                let _ = std::fs::remove_dir_all(self.dir(&run.id));
                let _ = std::fs::remove_file(
                    self.0.home.join("scripts").join(format!("{}.ts", run.id)),
                );
            }
        }
    }

    /// One turn of the agent for a node of a run: a session on this relay's
    /// own `/acp`, new or (`call.session`) one of an earlier call.
    pub async fn turn(&self, call: AgentCall) -> Result<AgentAnswer, String> {
        if !self.alive(&call.run) {
            return Err(format!("no run {} is running", call.run));
        }
        let headers = self
            .0
            .token
            .iter()
            .map(|token| ("Authorization".to_owned(), format!("Bearer {token}")))
            .collect::<BTreeMap<_, _>>();
        let spec = AgentSpec::Websocket {
            url: format!("ws://127.0.0.1:{}/acp", self.0.port),
            headers,
        };
        let transport =
            ukai_acp::resolve(&spec, |_, _| None, None).map_err(|error| error.to_string())?;
        // Reconnected, the turn would see `Ready` again and prompt twice;
        // this is the relay talking to itself, so a dropped connection is a
        // failure.
        let (session, events) = ukai_acp::spawn_session_with(
            transport,
            SessionOptions {
                cwd: self.0.root.clone(),
                fs_root: None,
                restore: call.session.clone(),
                update_check: None,
            },
            ukai_acp::ReconnectPolicy::NEVER,
        );
        let mut turn = Turn {
            session: Some(session),
            events: Some(events),
        };
        let answer = self.follow(&mut turn, &call).await;
        if answer.is_ok() {
            // Over: nothing is left to cancel.
            turn.session = None;
        }
        answer
    }

    async fn follow(&self, turn: &mut Turn, call: &AgentCall) -> Result<AgentAnswer, String> {
        let (Some(session), Some(events)) = (&turn.session, &mut turn.events) else {
            return Err("the session ended".to_owned());
        };
        let mut id = None;
        let mut answer = Answer::default();
        while let Some(event) = events.recv().await {
            match event {
                // What a resumed session replays comes before it is ready.
                SessionEvent::Ready {
                    session_id, config, ..
                } => {
                    self.record(call, &session_id);
                    id = Some(session_id);
                    answer = Answer::default();
                    if let Some(model) = &call.model
                        && let Some((option, value)) = model_choice(&config, model)?
                    {
                        session.set_config(option, ConfigInput::Select(value));
                    }
                    session.prompt(call.prompt.as_str());
                }
                SessionEvent::AgentMessage { text } if id.is_some() => answer.say(&text),
                SessionEvent::ToolCall { .. } if id.is_some() => answer.tool(),
                SessionEvent::PermissionRequested { id, options, .. } => {
                    session.resolve_permission(id, workflow::permit(call.access, &options));
                }
                SessionEvent::ConfigFailed { message } => {
                    return Err(format!("cannot set the model: {message}"));
                }
                SessionEvent::TurnEnded { stop_reason } => {
                    return match (stop_reason.as_str(), id) {
                        ("end_turn", Some(session)) => Ok(AgentAnswer {
                            text: answer.text(),
                            session,
                        }),
                        _ => Err(format!("the turn ended: {stop_reason}")),
                    };
                }
                SessionEvent::PromptFailed { message } => return Err(message),
                SessionEvent::Closed { error } => {
                    return Err(error.unwrap_or_else(|| "the connection closed".to_owned()));
                }
                _ => {}
            }
        }
        Err("the session ended".to_owned())
    }

    /// Notes which session `call`'s node works in, for the run's listing.
    fn record(&self, call: &AgentCall, session: &str) {
        use std::io::Write as _;
        let line = serde_json::to_string(&NodeSession {
            node: call.node.clone(),
            session: session.to_owned(),
        })
        .unwrap_or_default();
        let written = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.dir(&call.run).join("sessions.jsonl"))
            .and_then(|mut file| writeln!(file, "{line}"));
        if let Err(error) = written {
            tracing::warn!(%error, run = %call.run, "cannot note a node's session");
        }
    }
}

/// A turn being followed. Dropped before it ends (the run was stopped, so
/// its request went away), it cancels the turn rather than leave the agent
/// working for nobody.
struct Turn {
    session: Option<SessionHandle>,
    events: Option<SessionEvents>,
}

impl Drop for Turn {
    fn drop(&mut self) {
        let (Some(session), Some(mut events)) = (self.session.take(), self.events.take()) else {
            return;
        };
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        runtime.spawn(async move {
            session.cancel();
            let ended = async {
                while let Some(event) = events.recv().await {
                    if matches!(
                        event,
                        SessionEvent::TurnEnded { .. }
                            | SessionEvent::PromptFailed { .. }
                            | SessionEvent::Closed { .. }
                    ) {
                        break;
                    }
                }
            };
            let _ = tokio::time::timeout(CANCEL_GRACE, ended).await;
        });
    }
}

/// What the agent said in a turn. Its answer is what it says last: text
/// before a tool call is the agent thinking aloud on the way.
#[derive(Debug, Default)]
struct Answer {
    all: String,
    last: String,
}

impl Answer {
    fn say(&mut self, text: &str) {
        self.all.push_str(text);
        self.last.push_str(text);
    }

    fn tool(&mut self) {
        self.last.clear();
    }

    /// What was said after the last tool call, or everything when that is
    /// nothing.
    fn text(self) -> String {
        if self.last.trim().is_empty() {
            self.all
        } else {
            self.last
        }
    }
}

/// The option and value that set the agent's model to `model` (by value or
/// by name), or `None` when it is the current one. An agent that offers no
/// such model is an error naming the ones it has; one that lets no model be
/// chosen runs with its own.
fn model_choice(config: &[ConfigOption], model: &str) -> Result<Option<(String, String)>, String> {
    let Some((option, current, choices)) = config.iter().find_map(|option| match &option.value {
        ConfigValue::Select { current, choices } if option.category.as_deref() == Some("model") => {
            Some((option, current, choices))
        }
        _ => None,
    }) else {
        return Ok(None);
    };
    let Some(choice) = choices
        .iter()
        .find(|choice| choice.value == model || choice.name == model)
    else {
        let offered: Vec<_> = choices.iter().map(|choice| choice.value.as_str()).collect();
        return Err(format!(
            "the agent has no model `{model}` (it has: {})",
            offered.join(", ")
        ));
    };
    Ok((&choice.value != current).then(|| (option.id.clone(), choice.value.clone())))
}

/// The flows the `ukai.toml` of the checkout at `root` declares, by name.
pub fn declared(root: &Path) -> Vec<DeclaredFlow> {
    let config = match ukai_config::load_layered(None, root) {
        Ok(config) => config,
        Err(error) => {
            tracing::warn!(%error, root = %root.display(), "cannot read the declared workflows");
            return Vec::new();
        }
    };
    config
        .workflows
        .into_iter()
        .map(|(name, flow)| DeclaredFlow {
            name,
            path: flow.path.to_string_lossy().into_owned(),
            description: flow.description,
        })
        .collect()
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Option<T> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

fn modified(path: &Path) -> Option<u64> {
    let time = std::fs::metadata(path).ok()?.modified().ok()?;
    millis(time)
}

fn millis(time: SystemTime) -> Option<u64> {
    let since = time.duration_since(UNIX_EPOCH).ok()?;
    u64::try_from(since.as_millis()).ok()
}

fn now() -> u64 {
    millis(SystemTime::now()).unwrap_or_default()
}

fn cannot(what: &str, error: &std::io::Error) -> String {
    format!("cannot {what}: {error}")
}

#[cfg(unix)]
fn symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(not(unix))]
fn symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::windows::fs::symlink_dir(target, link)
}

/// Kills the run and, where the relay can, everything it started.
fn kill(child: &mut tokio::process::Child) {
    #[cfg(target_os = "linux")]
    if let Some(pid) = child.id().and_then(|pid| i32::try_from(pid).ok()) {
        use nix::sys::signal::{Signal, killpg};
        let _ = killpg(nix::unistd::Pid::from_raw(pid), Signal::SIGKILL);
    }
    let _ = child.start_kill();
}

/// The workflows' routes: the listing, a run, starting and stopping one,
/// and the turns a run asks for.
pub fn router(workflows: Workflows) -> Router {
    Router::new()
        .route(WORKFLOWS_PATH, get(list))
        .route(RUNS_PATH, post(start))
        .route(&format!("{RUNS_PATH}/{{id}}"), get(run))
        .route(&format!("{RUNS_PATH}/{{id}}/stop"), post(stop))
        .route(AGENT_PATH, post(agent))
        .with_state(workflows)
}

async fn list(State(workflows): State<Workflows>) -> Json<WorkflowListing> {
    Json(workflows.listing())
}

async fn start(State(workflows): State<Workflows>, Json(request): Json<StartRequest>) -> Response {
    match workflows.start(&request) {
        Ok(run) => Json(run).into_response(),
        Err(problems) => (StatusCode::UNPROCESSABLE_ENTITY, problems.join("\n")).into_response(),
    }
}

async fn run(State(workflows): State<Workflows>, UrlPath(id): UrlPath<String>) -> Response {
    match workflows.detail(&id) {
        Some(run) => Json(run).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn stop(State(workflows): State<Workflows>, UrlPath(id): UrlPath<String>) -> Response {
    match workflows.stop(&id).await {
        Some(run) => Json(run).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn agent(State(workflows): State<Workflows>, Json(call): Json<AgentCall>) -> Response {
    match workflows.turn(call).await {
        Ok(answer) => Json(answer).into_response(),
        Err(problem) => (StatusCode::UNPROCESSABLE_ENTITY, problem).into_response(),
    }
}

#[cfg(test)]
mod tests;
