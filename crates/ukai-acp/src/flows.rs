//! Runs of [ayaito](https://github.com/illumination-k/ayaito) flows in a
//! checkout: reading them, starting one, and reaching its agents.
//!
//! ayaito runs a workflow of agents and code as a graph and saves each run
//! as a folder `<store>/<run id>/`: `graph.json` (the flow), `events.jsonl`
//! (what each node did, appended as it happens) and, once it has ended,
//! `result.json`. Its store is outside the working tree — `ayaito-runs` in
//! the repository's git common dir — so every run of a checkout is found
//! there, whoever started it. This module reads that folder
//! ([`Store`]), starts a run as a process of its own ([`start`]) and passes
//! messages to its agents through ayaito's own command line ([`order`]):
//! what the relay serves next to `/acp` for the agent's workspace, and what
//! the app does itself for a project on this machine. [`Flows`] is either,
//! as a pane reads it; `ukai_core::flow` says what the files mean.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use ukai_config::{AgentSpec, Source};
use ukai_proto::gateway::v1 as gw;
use ukai_proto::gateway::v1::flows_client::FlowsClient;

use crate::gateway::GatewayAgent;
use crate::grpc::{self, Channel};
use crate::{PaneWorkspace, ResolveError};

/// Where a relay lists the runs of its checkout (JSON, for the gateway);
/// `/<run id>` under it reads one, and a POST there sends an [`Order`].
pub const FLOWS_PATH: &str = "/flows";

/// How many runs a listing carries at most, the ones written last.
pub const RUNS_LISTED: usize = 100;

/// How much of a run's events one read carries at most; the rest follows
/// from [`RunChunk::next`].
pub const MAX_CHUNK: usize = 512 * 1024;

/// How much of a run's result a read carries.
const MAX_RESULT: usize = 64 * 1024;

/// How long a listing or a read may take.
const TIMEOUT: Duration = Duration::from_secs(20);

/// How long ayaito may take to pass an order on.
const ORDER_TIMEOUT: Duration = Duration::from_secs(20);

/// What a relay answers at [`FLOWS_PATH`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FlowListing {
    /// The runs kept for the checkout, the one written last first.
    pub runs: Vec<RunSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunSummary {
    /// The run's id: the name of its folder.
    pub id: String,
    pub state: RunState,
    /// When it last wrote something, in milliseconds since the epoch by the
    /// clock of the machine it runs on.
    pub at: u64,
}

/// How far a run is.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    /// No result yet.
    #[default]
    Running,
    Done,
    /// A node found nothing to do: an early end, as a success.
    Stopped,
    /// It failed, or its process ended without a result.
    Failed,
}

impl RunState {
    pub fn name(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Done => "done",
            Self::Stopped => "stopped",
            Self::Failed => "failed",
        }
    }
}

/// A run's graph, and its events from an offset on.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunChunk {
    /// The flow as ayaito's `toGraph` describes it, as JSON.
    pub graph: String,
    /// Whole lines of the run's `events.jsonl` from the offset asked for.
    pub events: String,
    /// Where to read on from.
    pub next: u64,
    pub state: RunState,
    /// What the run answered when it ended, as JSON, cut when long.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
}

/// The query of a read.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct ReadQuery {
    #[serde(default)]
    pub after: u64,
}

/// Something to tell a run: with a `message`, the agent of `node` reads it
/// and goes on with its task; without one, that agent is stopped and answers
/// from what it has done, or without a `node` either, the run is stopped.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Order {
    /// The agent to reach: its node's name, `name#item` in a forEach, or
    /// its session's id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

impl Order {
    /// What is wrong with it, if anything.
    fn problem(&self) -> Option<&'static str> {
        let one_line = |text: &str| !text.trim().is_empty() && !text.chars().any(char::is_control);
        match (&self.node, &self.message) {
            (Some(node), _) if !one_line(node) => Some("the node to reach is not a name"),
            (None, Some(_)) => Some("a message needs the node whose agent it is for"),
            (_, Some(message)) if message.trim().is_empty() => Some("the message is empty"),
            _ => None,
        }
    }
}

/// What ayaito said of an order.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ordered {
    pub said: String,
}

/// A flow for [`start`] to run.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StartRequest {
    /// What the run id starts with.
    pub name: Option<String>,
    /// The TypeScript module whose default export is the flow. Absent to
    /// run the flow saved with `run` again.
    pub script: Option<String>,
    /// The flow's input, as JSON. Absent, the script's export named `input`.
    pub input: Option<serde_json::Value>,
    /// The id of an earlier run to resume from its checkpoints.
    pub run: Option<String>,
}

/// `text` if a run can have it as its id: the name of a folder, and an
/// argument that is no option.
pub fn run_id(text: &str) -> Option<&str> {
    let named = !text.is_empty()
        && text.len() <= 128
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
        && !text.starts_with(['.', '-']);
    named.then_some(text)
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FlowError {
    #[error("cannot reach the gateway: {0}")]
    Unreachable(String),
    #[error("the agent's workspace is not running")]
    NotRunning,
    /// What is wrong with the request, as the one asking should read it.
    #[error("{0}")]
    Refused(String),
    #[error("{0}")]
    Failed(String),
}

impl From<tonic::Status> for FlowError {
    fn from(status: tonic::Status) -> Self {
        if status.code() == tonic::Code::Aborted {
            // What the relay said of the request, word for word.
            return Self::Refused(status.message().to_owned());
        }
        match grpc::Failure::from(status) {
            grpc::Failure::Unreachable(why) => Self::Unreachable(why),
            grpc::Failure::NotRunning => Self::NotRunning,
            grpc::Failure::NotFound => Self::Failed("the gateway has no such agent".to_owned()),
            grpc::Failure::Denied(why) | grpc::Failure::Failed(why) => Self::Failed(why),
            grpc::Failure::TooMany => Self::Failed("too many requests".to_owned()),
        }
    }
}

fn io(what: &str, error: &std::io::Error) -> FlowError {
    FlowError::Failed(format!("cannot {what}: {error}"))
}

/// The folders ayaito keeps a checkout's runs in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Store {
    /// Where runs are kept, and new ones go.
    pub dir: PathBuf,
    /// `.runs` in the checkout, where ayaito 0.2 and earlier kept them: a
    /// run that is there is still found.
    legacy: PathBuf,
}

impl Store {
    /// The store of the checkout at `root`, as `ayaito run` chooses it
    /// there: `ayaito-runs` in the git common dir, so that every worktree
    /// of the repository shares it, or outside git, the XDG state dir.
    pub async fn of(root: &Path) -> Self {
        let git = tokio::process::Command::new("git")
            .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
            .current_dir(root)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .await;
        let common = git
            .ok()
            .filter(|output| output.status.success())
            .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
            .filter(|common| !common.is_empty());
        let dir = match common {
            Some(common) => Path::new(&common).join("ayaito-runs"),
            None => state_home().join("ayaito").join("runs"),
        };
        Self::at(dir, root)
    }

    /// The store in `dir`, for the checkout at `root`.
    pub fn at(dir: PathBuf, root: &Path) -> Self {
        Self {
            dir,
            legacy: root.join(".runs"),
        }
    }

    /// The folder of `run`, if there is such a run.
    pub fn folder(&self, run: &str) -> Result<PathBuf, FlowError> {
        self.find(run)
    }

    fn find(&self, run: &str) -> Result<PathBuf, FlowError> {
        let Some(run) = run_id(run) else {
            return Err(FlowError::Refused(format!("`{run}` is not a run id")));
        };
        [&self.legacy, &self.dir]
            .into_iter()
            .map(|dir| dir.join(run))
            .find(|folder| folder.join("graph.json").is_file())
            .ok_or_else(|| FlowError::Refused(format!("there is no run `{run}`")))
    }

    /// The runs kept, the one written last first.
    pub fn list(&self) -> FlowListing {
        let mut runs: Vec<RunSummary> = Vec::new();
        for dir in [&self.legacy, &self.dir] {
            let Ok(entries) = std::fs::read_dir(dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let folder = entry.path();
                let Some(id) = entry
                    .file_name()
                    .to_str()
                    .and_then(run_id)
                    .map(str::to_owned)
                else {
                    continue;
                };
                if !folder.join("graph.json").is_file() || runs.iter().any(|run| run.id == id) {
                    continue;
                }
                let at = ["events.jsonl", "graph.json", "result.json"]
                    .iter()
                    .filter_map(|name| modified(&folder.join(name)))
                    .max()
                    .unwrap_or(0);
                runs.push(RunSummary {
                    id,
                    state: state(&folder),
                    at,
                });
            }
        }
        runs.sort_by(|a, b| b.at.cmp(&a.at).then_with(|| a.id.cmp(&b.id)));
        runs.truncate(RUNS_LISTED);
        FlowListing { runs }
    }

    /// The graph of `run` and its events from `after` bytes on, as whole
    /// lines. An offset past the end — the events were written anew — reads
    /// from the start.
    pub fn read(&self, run: &str, after: u64) -> Result<RunChunk, FlowError> {
        use std::io::{Read, Seek, SeekFrom};

        let folder = self.find(run)?;
        let graph = std::fs::read_to_string(folder.join("graph.json"))
            .map_err(|error| io("read the run's graph", &error))?;
        let (events, next) = match std::fs::File::open(folder.join("events.jsonl")) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (String::new(), 0),
            Err(error) => return Err(io("read the run's events", &error)),
            Ok(mut file) => {
                let len = file
                    .metadata()
                    .map_err(|error| io("read the run's events", &error))?
                    .len();
                let from = if after > len { 0 } else { after };
                file.seek(SeekFrom::Start(from))
                    .map_err(|error| io("read the run's events", &error))?;
                let mut bytes = Vec::new();
                // One line more than fits may follow, to end on a line.
                file.take(2 * MAX_CHUNK as u64)
                    .read_to_end(&mut bytes)
                    .map_err(|error| io("read the run's events", &error))?;
                let whole = whole_lines(&bytes);
                (
                    String::from_utf8_lossy(&bytes[..whole]).into_owned(),
                    from + whole as u64,
                )
            }
        };
        let result = result_text(&folder).map(|mut text| {
            if text.len() > MAX_RESULT {
                let mut end = MAX_RESULT;
                while !text.is_char_boundary(end) {
                    end -= 1;
                }
                text.truncate(end);
                text.push_str("\n… (cut)");
            }
            text
        });
        Ok(RunChunk {
            graph,
            events,
            next,
            state: state(&folder),
            result,
        })
    }
}

/// How much of `bytes` to pass on: its whole lines, as many as fit
/// [`MAX_CHUNK`], or one line when the first is longer than that.
fn whole_lines(bytes: &[u8]) -> usize {
    let fits = &bytes[..bytes.len().min(MAX_CHUNK)];
    match fits.iter().rposition(|byte| *byte == b'\n') {
        Some(end) => end + 1,
        None => bytes
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(0, |end| end + 1),
    }
}

fn state_home() -> PathBuf {
    // The spec says to ignore a relative XDG_STATE_HOME.
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|state| state.is_absolute())
        .or_else(|| {
            let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?;
            Some(PathBuf::from(home).join(".local").join("state"))
        })
        .unwrap_or_else(std::env::temp_dir)
}

fn modified(file: &Path) -> Option<u64> {
    let modified = std::fs::metadata(file).ok()?.modified().ok()?;
    let since = modified.duration_since(UNIX_EPOCH).ok()?;
    u64::try_from(since.as_millis()).ok()
}

/// The result a run left, unless it has none yet.
fn result_text(folder: &Path) -> Option<String> {
    std::fs::read_to_string(folder.join("result.json"))
        .ok()
        .filter(|text| !text.trim().is_empty())
}

/// How far the run in `folder` is: what its result says, and without one,
/// whether the process [`start`] recorded for it is still there.
fn state(folder: &Path) -> RunState {
    #[derive(Deserialize)]
    struct Outcome {
        #[serde(default)]
        status: String,
    }
    if let Some(result) = result_text(folder) {
        return match serde_json::from_str::<Outcome>(&result) {
            Ok(outcome) if outcome.status == "done" => RunState::Done,
            Ok(outcome) if outcome.status == "stopped" => RunState::Stopped,
            _ => RunState::Failed,
        };
    }
    let pid = std::fs::read_to_string(folder.join(PID_FILE))
        .ok()
        .and_then(|text| text.trim().parse::<u32>().ok());
    match pid {
        Some(pid) if !alive(pid) => RunState::Failed,
        _ => RunState::Running,
    }
}

/// Where [`start`] leaves the id of the run's process, and its output.
const PID_FILE: &str = "run.pid";
const LOG_FILE: &str = "run.log";

/// Whether a process `pid` is there. Only Linux says without a signal;
/// elsewhere a run without a result counts as going on.
fn alive(pid: u32) -> bool {
    if cfg!(target_os = "linux") {
        // A process that ended and was not waited for is still listed.
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
        let state = stat.rsplit_once(')').map(|(_, rest)| rest.trim_start());
        state.is_some_and(|state| !state.starts_with('Z'))
    } else {
        true
    }
}

/// How to start ayaito's command line for the checkout at `root`: the
/// project's own, or else one on the `PATH`. With the latter, the second
/// value is the `node_modules` it is installed in, which a flow kept
/// outside it needs to import `ayaito` from.
fn cli(root: &Path) -> Option<(PathBuf, Option<PathBuf>)> {
    let own = root.join("node_modules").join(".bin").join("ayaito");
    if own.is_file() {
        return Some((own, None));
    }
    let path = std::env::var_os("PATH")?;
    let found = std::env::split_paths(&path)
        .map(|dir| dir.join("ayaito"))
        .find(|file| file.is_file())?;
    let modules = found.canonicalize().ok().and_then(|real| {
        real.ancestors()
            .find(|dir| dir.file_name().is_some_and(|name| name == "node_modules"))
            .map(Path::to_owned)
    });
    Some((found, modules))
}

const NOT_INSTALLED: &str = "ayaito is not installed in this checkout: run `npm install ayaito zod` \
     and install the SDK of the runtime the flow's agents use (for claudeRuntime, \
     `@anthropic-ai/claude-agent-sdk`)";

/// What [`start`] started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Started {
    pub run: String,
    /// The run's folder: its flow, log, events and result.
    pub folder: PathBuf,
}

/// Starts a run of `request`'s flow in the checkout at `root`, as a process
/// of its own that outlives the caller, and answers once it is started.
/// The flow, its input and its output are kept in the run's folder
/// (`flow.ts`, `input.json`, `run.log`), beside what ayaito saves there.
pub fn start(root: &Path, store: &Store, request: &StartRequest) -> Result<Started, FlowError> {
    let refused = |why: String| Err(FlowError::Refused(why));
    let Some((command, modules)) = cli(root) else {
        return refused(NOT_INSTALLED.to_owned());
    };
    let run = match &request.run {
        Some(run) => match run_id(run) {
            Some(run) => run.to_owned(),
            None => return refused(format!("`{run}` is not a run id")),
        },
        None => {
            let name: String = request
                .name
                .as_deref()
                .unwrap_or("flow")
                .chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() || c == '_' {
                        c
                    } else {
                        '-'
                    }
                })
                .take(64)
                .collect();
            let name = name.trim_matches('-');
            let stamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |since| since.as_millis());
            format!("{}-{stamp:x}", if name.is_empty() { "flow" } else { name })
        }
    };
    // A run kept where ayaito used to keep them goes on there.
    let folder = match store.find(&run) {
        Ok(folder) => folder,
        Err(_) => store.dir.join(&run),
    };
    if state_of_started(&folder) == Some(true) {
        return refused(format!(
            "run {run} is still going: stop it first, or wait for its end"
        ));
    }
    let flow = folder.join("flow.ts");
    let script = request
        .script
        .as_deref()
        .filter(|script| !script.trim().is_empty());
    if script.is_none() && !flow.is_file() {
        return refused(match &request.run {
            Some(_) => format!("there is no saved flow for run {run}: pass script"),
            None => "pass script: the flow to run".to_owned(),
        });
    }
    std::fs::create_dir_all(&folder).map_err(|error| io("make the run's folder", &error))?;
    if let Some(script) = script {
        std::fs::write(&flow, script).map_err(|error| io("save the flow", &error))?;
    }
    if let Some(modules) = modules {
        link_modules(&store.dir, &modules);
    }
    let mut ayaito = std::process::Command::new(command);
    ayaito
        .arg("run")
        .arg(&flow)
        .arg("--run-id")
        .arg(&run)
        .arg("--store")
        .arg(folder.parent().unwrap_or(&store.dir));
    if let Some(input) = &request.input {
        let file = folder.join("input.json");
        std::fs::write(&file, input.to_string()).map_err(|error| io("save the input", &error))?;
        ayaito.arg("--input-file").arg(file);
    }
    // An earlier attempt's end is not this one's.
    let _ = std::fs::remove_file(folder.join("result.json"));
    let log = std::fs::File::create(folder.join(LOG_FILE))
        .map_err(|error| io("open the run's log", &error))?;
    let errors = log
        .try_clone()
        .map_err(|error| io("open the run's log", &error))?;
    ayaito
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(log)
        .stderr(errors);
    #[cfg(unix)]
    {
        // A group of its own: a signal to the caller's is not the run's.
        use std::os::unix::process::CommandExt;
        ayaito.process_group(0);
    }
    let mut child = ayaito.spawn().map_err(|error| io("start ayaito", &error))?;
    std::fs::write(folder.join(PID_FILE), child.id().to_string())
        .map_err(|error| io("record the run's process", &error))?;
    // Nobody waits for the run, but its end has to be taken, or the process
    // stays listed for as long as this one lives.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(Started { run, folder })
}

/// Whether the process [`start`] recorded for the run in `folder` is still
/// at it; `None` when it recorded none.
fn state_of_started(folder: &Path) -> Option<bool> {
    let pid = std::fs::read_to_string(folder.join(PID_FILE)).ok()?;
    let pid = pid.trim().parse::<u32>().ok()?;
    Some(result_text(folder).is_none() && cfg!(target_os = "linux") && alive(pid))
}

/// Lets a flow saved under `store` import what is installed in `modules`:
/// Node looks for packages in the `node_modules` of the folders above a
/// module, and the store is outside the tree ayaito is installed in.
fn link_modules(store: &Path, modules: &Path) {
    let link = store.join("node_modules");
    if link.exists() {
        return;
    }
    #[cfg(unix)]
    if let Err(error) = std::os::unix::fs::symlink(modules, &link) {
        tracing::warn!(link = %link.display(), %error, "cannot link ayaito's packages for flows");
    }
    #[cfg(not(unix))]
    let _ = (modules, link);
}

/// Passes `order` to `run` through ayaito's command line, which knows
/// whether the run and the node's agent are there to take it.
pub async fn order(
    root: &Path,
    store: &Store,
    run: &str,
    order: &Order,
) -> Result<Ordered, FlowError> {
    if let Some(problem) = order.problem() {
        return Err(FlowError::Refused(problem.to_owned()));
    }
    let folder = store.find(run)?;
    let Some((command, _)) = cli(root) else {
        return Err(FlowError::Refused(NOT_INSTALLED.to_owned()));
    };
    let mut ayaito = tokio::process::Command::new(command);
    ayaito
        .arg(if order.message.is_some() {
            "steer"
        } else {
            "stop"
        })
        .arg("--store")
        .arg(folder.parent().unwrap_or(&store.dir))
        .arg("--")
        .arg(run)
        .args(&order.node)
        .args(&order.message)
        .current_dir(root)
        .stdin(Stdio::null())
        .kill_on_drop(true);
    let output = tokio::time::timeout(ORDER_TIMEOUT, ayaito.output())
        .await
        .map_err(|_| FlowError::Failed("ayaito did not answer in time".to_owned()))?
        .map_err(|error| io("start ayaito", &error))?;
    let said = |bytes: &[u8]| String::from_utf8_lossy(bytes).trim().to_owned();
    if output.status.success() {
        return Ok(Ordered {
            said: said(&output.stdout),
        });
    }
    let why = [said(&output.stderr), said(&output.stdout)]
        .into_iter()
        .find(|text| !text.is_empty())
        .unwrap_or_else(|| "ayaito refused".to_owned());
    Err(FlowError::Refused(why))
}

impl From<RunState> for gw::FlowState {
    fn from(state: RunState) -> Self {
        match state {
            RunState::Running => Self::Running,
            RunState::Done => Self::Done,
            RunState::Stopped => Self::Stopped,
            RunState::Failed => Self::Failed,
        }
    }
}

impl From<gw::FlowState> for RunState {
    fn from(state: gw::FlowState) -> Self {
        match state {
            gw::FlowState::Running => Self::Running,
            gw::FlowState::Done => Self::Done,
            gw::FlowState::Stopped => Self::Stopped,
            gw::FlowState::Failed => Self::Failed,
        }
    }
}

impl From<FlowListing> for gw::FlowListing {
    fn from(listing: FlowListing) -> Self {
        Self {
            runs: listing
                .runs
                .into_iter()
                .map(|run| gw::FlowSummary {
                    id: run.id,
                    state: gw::FlowState::from(run.state).into(),
                    at: run.at,
                })
                .collect(),
        }
    }
}

impl From<gw::FlowListing> for FlowListing {
    fn from(listing: gw::FlowListing) -> Self {
        Self {
            runs: listing
                .runs
                .into_iter()
                .map(|run| RunSummary {
                    state: run.state().into(),
                    id: run.id,
                    at: run.at,
                })
                .collect(),
        }
    }
}

impl From<RunChunk> for gw::FlowRun {
    fn from(chunk: RunChunk) -> Self {
        Self {
            graph: chunk.graph,
            events: chunk.events,
            next: chunk.next,
            state: gw::FlowState::from(chunk.state).into(),
            result: chunk.result,
        }
    }
}

impl From<gw::FlowRun> for RunChunk {
    fn from(run: gw::FlowRun) -> Self {
        Self {
            state: run.state().into(),
            graph: run.graph,
            events: run.events,
            next: run.next,
            result: run.result,
        }
    }
}

/// Where a pane's flows run: where its agent works.
#[derive(Debug, Clone)]
pub enum Flows {
    /// The project root on this machine.
    Local(PathBuf),
    /// A gateway agent's workspace, in its pod.
    Gateway(GatewayFlows),
}

#[derive(Debug, Clone)]
pub struct GatewayFlows {
    channel: Channel,
    root: String,
    agent: String,
    workspace: gw::Workspace,
}

impl PartialEq for Flows {
    /// Two are the same when they reach the same checkout.
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Local(one), Self::Local(other)) => one == other,
            (Self::Gateway(one), Self::Gateway(other)) => {
                (&one.root, &one.agent, &one.workspace)
                    == (&other.root, &other.agent, &other.workspace)
            }
            _ => false,
        }
    }
}

impl Flows {
    /// The flows of `workspace` for the gateway agent `spec`; `None` for
    /// any other agent, whose flows are the project's own
    /// ([`Flows::Local`]).
    pub fn resolve(
        spec: &AgentSpec,
        lookup: impl Fn(Source, &str) -> Option<String> + Copy,
        workspace: Option<&PaneWorkspace>,
    ) -> Result<Option<Self>, ResolveError> {
        let Some(agent) = GatewayAgent::resolve(spec, lookup)? else {
            return Ok(None);
        };
        Ok(Some(Self::Gateway(GatewayFlows {
            channel: agent.channel(TIMEOUT + ORDER_TIMEOUT)?,
            workspace: agent.gateway.workspace(workspace),
            root: agent.root,
            agent: agent.id,
        })))
    }

    pub async fn list(&self) -> Result<FlowListing, FlowError> {
        match self {
            Self::Local(root) => {
                let store = Store::of(root).await;
                blocking(move || Ok(store.list())).await
            }
            Self::Gateway(gateway) => {
                let request = gw::ListFlowsRequest {
                    agent: gateway.agent.clone(),
                    workspace: Some(gateway.workspace.clone()),
                };
                let listing = FlowsClient::new(gateway.channel.clone())
                    .list(request)
                    .await?
                    .into_inner();
                Ok(listing.into())
            }
        }
    }

    /// The graph of `run` and its events from `after` on.
    pub async fn read(&self, run: &str, after: u64) -> Result<RunChunk, FlowError> {
        match self {
            Self::Local(root) => {
                let store = Store::of(root).await;
                let run = run.to_owned();
                blocking(move || store.read(&run, after)).await
            }
            Self::Gateway(gateway) => {
                let request = gw::ReadFlowRequest {
                    agent: gateway.agent.clone(),
                    workspace: Some(gateway.workspace.clone()),
                    run: run.to_owned(),
                    after,
                };
                let chunk = FlowsClient::new(gateway.channel.clone())
                    .read(request)
                    .await?
                    .into_inner();
                Ok(chunk.into())
            }
        }
    }

    /// Tell `run` something; see [`Order`].
    pub async fn order(&self, run: &str, sent: Order) -> Result<Ordered, FlowError> {
        match self {
            Self::Local(root) => order(root, &Store::of(root).await, run, &sent).await,
            Self::Gateway(gateway) => {
                let request = gw::OrderFlowRequest {
                    agent: gateway.agent.clone(),
                    workspace: Some(gateway.workspace.clone()),
                    run: run.to_owned(),
                    node: sent.node,
                    message: sent.message,
                };
                let ordered = FlowsClient::new(gateway.channel.clone())
                    .order(request)
                    .await?
                    .into_inner();
                Ok(Ordered { said: ordered.said })
            }
        }
    }
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, FlowError> + Send + 'static,
) -> Result<T, FlowError> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|error| FlowError::Failed(error.to_string()))?
}

#[cfg(test)]
mod tests;
