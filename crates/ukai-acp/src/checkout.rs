//! The checkout an agent works in, as the composer reaches it: its files, to
//! mention one with `@`, and running a command in it with `!`.
//!
//! ACP has neither: a prompt can link a file, but the client has to know the
//! files, and nothing asks the agent's machine to run a command for the
//! user. So a [`Checkout`] does both itself. A local agent's checkout is the
//! project root, listed and run here. A gateway agent's is in its pod: the
//! relay there answers [`FILES_PATH`] and [`RUN_PATH`] with the same
//! [`list_files`] and [`run`], and the gateway passes them on as
//! `ukai.gateway.v1.Files` and `Commands` (gRPC-Web; see `crate::grpc`).

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use ukai_config::{AgentSpec, Source};
use ukai_proto::gateway::v1 as gw;
use ukai_proto::gateway::v1::commands_client::CommandsClient;
use ukai_proto::gateway::v1::files_client::FilesClient;

use crate::grpc::{self, Channel};
use crate::{PaneWorkspace, ResolveError, header_map};

/// Where a relay lists its checkout's files (JSON, for the gateway).
pub const FILES_PATH: &str = "/files";

/// Where a relay runs a command in its checkout: a [`RunRequest`] posted as
/// JSON, answered with [`CommandOutput`].
pub const RUN_PATH: &str = "/run";

/// Files a listing carries at most.
pub const MAX_FILES: usize = 20_000;

/// Bytes of output kept, from the end.
pub const MAX_OUTPUT: usize = 64 * 1024;

/// How long a command may run before it is stopped.
pub const RUN_TIMEOUT: Duration = Duration::from_secs(120);

/// How long a command's output is waited for once it exits: a process it
/// left in the background can hold the output open for good.
const DRAIN: Duration = Duration::from_secs(1);

/// How long a listing may take through the gateway.
const LIST_TIMEOUT: Duration = Duration::from_secs(30);

/// The files of a checkout.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileListing {
    /// The checkout's absolute path, which `file://` URIs of its files
    /// start with.
    pub root: String,
    /// Paths relative to `root`, `/`-separated, in order; what the
    /// checkout's ignore files exclude is left out.
    pub files: Vec<String>,
    /// There were more than [`MAX_FILES`].
    #[serde(default)]
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunRequest {
    pub command: String,
}

/// What a command printed, and how it ended.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandOutput {
    /// `None` when it did not exit by itself: a signal, or the timeout.
    pub exit_code: Option<i32>,
    /// Standard output and error as they came; the last [`MAX_OUTPUT`]
    /// bytes when there was more.
    pub output: String,
    #[serde(default)]
    pub truncated: bool,
    #[serde(default)]
    pub timed_out: bool,
}

/// The files under `root`, as git would see them: what `.gitignore`,
/// `.ignore`, and git's own excludes leave out is left out, dotfiles are
/// kept, and `.git` itself is not listed. Blocking.
pub fn list_files(root: &Path) -> FileListing {
    let mut files = Vec::new();
    let mut truncated = false;
    let walk = ignore::WalkBuilder::new(root)
        .hidden(false)
        .require_git(false)
        .sort_by_file_name(Ord::cmp)
        .filter_entry(|entry| entry.file_name() != ".git")
        .build();
    for entry in walk.flatten() {
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        let Ok(relative) = entry.path().strip_prefix(root) else {
            continue;
        };
        if files.len() == MAX_FILES {
            truncated = true;
            break;
        }
        let parts: Vec<_> = relative
            .components()
            .map(|part| part.as_os_str().to_string_lossy())
            .collect();
        files.push(parts.join("/"));
    }
    FileListing {
        root: root.to_string_lossy().into_owned(),
        files,
        truncated,
    }
}

/// Run `command` with the user's shell (`$SHELL`, else `sh`) in `root`, with
/// no input, for up to [`RUN_TIMEOUT`].
pub async fn run(root: &Path, command: &str) -> std::io::Result<CommandOutput> {
    let (reader, writer) = std::io::pipe()?;
    let mut child = shell(command)
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(writer.try_clone()?)
        .stderr(writer)
        .kill_on_drop(true)
        .spawn()?;
    // The read end sees the end of the output once every writer is gone:
    // the command, and whatever it left running.
    let tail = Arc::new(Mutex::new(Tail::default()));
    let reading = tokio::task::spawn_blocking({
        let tail = tail.clone();
        move || read_into(reader, &tail)
    });
    let (exit_code, timed_out) = match tokio::time::timeout(RUN_TIMEOUT, child.wait()).await {
        Ok(status) => (status?.code(), false),
        Err(_) => {
            let _ = child.kill().await;
            (None, true)
        }
    };
    let _ = tokio::time::timeout(DRAIN, reading).await;
    let tail = std::mem::take(&mut *tail.lock().unwrap_or_else(|e| e.into_inner()));
    let (output, truncated) = tail.finish();
    Ok(CommandOutput {
        exit_code,
        output,
        truncated,
        timed_out,
    })
}

#[cfg(unix)]
fn shell(command: &str) -> tokio::process::Command {
    let program = std::env::var_os("SHELL")
        .filter(|shell| !shell.is_empty())
        .unwrap_or_else(|| "sh".into());
    let mut shell = tokio::process::Command::new(program);
    shell.arg("-c").arg(command);
    shell
}

#[cfg(windows)]
fn shell(command: &str) -> tokio::process::Command {
    let mut shell = tokio::process::Command::new("cmd");
    shell.arg("/C").arg(command);
    shell
}

/// The end of a stream, at most [`MAX_OUTPUT`] bytes of it.
#[derive(Debug, Default)]
struct Tail {
    bytes: Vec<u8>,
    dropped: bool,
}

impl Tail {
    fn push(&mut self, chunk: &[u8]) {
        self.bytes.extend_from_slice(chunk);
        // Trimmed in steps, not on every chunk.
        if self.bytes.len() > 2 * MAX_OUTPUT {
            let excess = self.bytes.len() - MAX_OUTPUT;
            self.bytes.drain(..excess);
            self.dropped = true;
        }
    }

    fn finish(mut self) -> (String, bool) {
        if self.bytes.len() > MAX_OUTPUT {
            let excess = self.bytes.len() - MAX_OUTPUT;
            self.bytes.drain(..excess);
            self.dropped = true;
        }
        let mut text = String::from_utf8_lossy(&self.bytes).into_owned();
        if self.dropped {
            // The cut may have split a character.
            text = text.trim_start_matches('\u{FFFD}').to_owned();
        }
        (text, self.dropped)
    }
}

fn read_into(mut reader: std::io::PipeReader, tail: &Mutex<Tail>) {
    let mut buffer = [0u8; 8192];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(read) => tail
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(&buffer[..read]),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CheckoutError {
    #[error("cannot reach the gateway: {0}")]
    Unreachable(String),
    #[error("the agent's workspace is not running")]
    NotRunning,
    #[error("{0}")]
    Failed(String),
}

impl From<tonic::Status> for CheckoutError {
    fn from(status: tonic::Status) -> Self {
        match grpc::Failure::from(status) {
            grpc::Failure::Unreachable(why) => Self::Unreachable(why),
            grpc::Failure::NotRunning => Self::NotRunning,
            grpc::Failure::NotFound => Self::Failed("the gateway has no such agent".to_owned()),
            grpc::Failure::Denied(why) | grpc::Failure::Failed(why) => Self::Failed(why),
            grpc::Failure::TooMany => Self::Failed("too many requests".to_owned()),
        }
    }
}

/// Where a pane's agent works.
#[derive(Debug, Clone)]
pub enum Checkout {
    /// The project root on this machine.
    Local(PathBuf),
    /// A gateway agent's workspace, in its pod.
    Gateway(GatewayCheckout),
}

#[derive(Debug, Clone)]
pub struct GatewayCheckout {
    /// For listing: the agent's headers, and a timeout.
    list: Channel,
    /// For commands: a timeout past the relay's own.
    run: Channel,
    agent: String,
    workspace: gw::Workspace,
}

impl Checkout {
    /// The checkout of `spec` working in `root` (local) or in `workspace`
    /// (gateway). `None` for a gateway URL that names no agent.
    pub fn resolve(
        spec: &AgentSpec,
        lookup: impl Fn(Source, &str) -> Option<String> + Copy,
        workspace: Option<&PaneWorkspace>,
        root: &Path,
    ) -> Result<Option<Self>, ResolveError> {
        let AgentSpec::Gateway {
            url,
            headers,
            workspace: default_scope,
        } = spec
        else {
            return Ok(Some(Self::Local(root.to_owned())));
        };
        let url = ukai_config::interpolate(url, lookup)?;
        let Some((root, agent)) = grpc::agent_root(&url) else {
            return Ok(None);
        };
        let client = |timeout| {
            reqwest::Client::builder()
                .default_headers(header_map(headers, &[], lookup)?)
                .timeout(timeout)
                .build()
                .map_err(|error| ResolveError::Client(error.to_string()))
        };
        Ok(Some(Self::Gateway(GatewayCheckout {
            list: grpc::channel(client(LIST_TIMEOUT)?, root.clone()),
            run: grpc::channel(client(RUN_TIMEOUT + LIST_TIMEOUT)?, root),
            agent,
            workspace: grpc::workspace(workspace, *default_scope),
        })))
    }

    pub async fn files(&self) -> Result<FileListing, CheckoutError> {
        match self {
            Self::Local(root) => {
                let root = root.clone();
                tokio::task::spawn_blocking(move || list_files(&root))
                    .await
                    .map_err(|error| CheckoutError::Failed(error.to_string()))
            }
            Self::Gateway(gateway) => {
                let request = gw::ListFilesRequest {
                    agent: gateway.agent.clone(),
                    workspace: Some(gateway.workspace.clone()),
                };
                let listing = FilesClient::new(gateway.list.clone())
                    .list(request)
                    .await?
                    .into_inner();
                Ok(listing.into())
            }
        }
    }

    pub async fn run(&self, command: &str) -> Result<CommandOutput, CheckoutError> {
        match self {
            Self::Local(root) => run(root, command)
                .await
                .map_err(|error| CheckoutError::Failed(error.to_string())),
            Self::Gateway(gateway) => {
                let request = gw::RunCommandRequest {
                    agent: gateway.agent.clone(),
                    workspace: Some(gateway.workspace.clone()),
                    command: command.to_owned(),
                };
                let output = CommandsClient::new(gateway.run.clone())
                    .run(request)
                    .await?
                    .into_inner();
                Ok(output.into())
            }
        }
    }
}

impl From<FileListing> for gw::FileListing {
    fn from(listing: FileListing) -> Self {
        Self {
            root: listing.root,
            files: listing.files,
            truncated: listing.truncated,
        }
    }
}

impl From<gw::FileListing> for FileListing {
    fn from(listing: gw::FileListing) -> Self {
        Self {
            root: listing.root,
            files: listing.files,
            truncated: listing.truncated,
        }
    }
}

impl From<CommandOutput> for gw::CommandOutput {
    fn from(output: CommandOutput) -> Self {
        Self {
            exit_code: output.exit_code,
            output: output.output,
            truncated: output.truncated,
            timed_out: output.timed_out,
        }
    }
}

impl From<gw::CommandOutput> for CommandOutput {
    fn from(output: gw::CommandOutput) -> Self {
        Self {
            exit_code: output.exit_code,
            output: output.output,
            truncated: output.truncated,
            timed_out: output.timed_out,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn checkout() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
        let root = dir.path();
        for (path, text) in [
            (".gitignore", "target/\n*.log\n"),
            ("src/main.rs", "fn main() {}"),
            ("src/lib.rs", ""),
            ("target/debug/app", ""),
            ("run.log", ""),
            (".github/workflows/ci.yml", ""),
            (".git/HEAD", "ref: refs/heads/main"),
            ("README.md", ""),
        ] {
            let path = root.join(path);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).unwrap_or_else(|e| panic!("mkdir: {e}"));
            }
            std::fs::write(path, text).unwrap_or_else(|e| panic!("write: {e}"));
        }
        dir
    }

    #[test]
    fn lists_what_git_would_track_with_dotfiles() {
        let dir = checkout();
        let listing = list_files(dir.path());
        assert_eq!(
            listing.files,
            [
                ".github/workflows/ci.yml",
                ".gitignore",
                "README.md",
                "src/lib.rs",
                "src/main.rs",
            ]
        );
        assert!(!listing.truncated);
        assert_eq!(listing.root, dir.path().to_string_lossy());
    }

    #[test]
    fn keeps_the_end_of_long_output() {
        let mut tail = Tail::default();
        for _ in 0..3 {
            tail.push(&[b'a'; MAX_OUTPUT]);
        }
        tail.push(b"the end");
        let (text, truncated) = tail.finish();
        assert!(truncated);
        assert_eq!(text.len(), MAX_OUTPUT);
        assert!(text.ends_with("the end"));
    }

    #[test]
    fn a_cut_through_a_character_leaves_no_replacement() {
        let mut tail = Tail::default();
        tail.push("é".repeat(MAX_OUTPUT).as_bytes());
        let (text, truncated) = tail.finish();
        assert!(truncated);
        assert!(text.chars().all(|c| c == 'é'));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn runs_in_the_checkout_with_both_streams() {
        let dir = checkout();
        let output = run(dir.path(), "cat src/main.rs; echo; echo oops >&2; exit 3")
            .await
            .unwrap_or_else(|e| panic!("run: {e}"));
        assert_eq!(output.exit_code, Some(3));
        assert_eq!(output.output, "fn main() {}\noops\n");
        assert!(!output.timed_out && !output.truncated);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_background_process_does_not_hold_the_answer() {
        let dir = checkout();
        let started = std::time::Instant::now();
        let output = run(dir.path(), "sleep 3 & echo started")
            .await
            .unwrap_or_else(|e| panic!("run: {e}"));
        assert_eq!(output.output, "started\n");
        assert_eq!(output.exit_code, Some(0));
        assert!(started.elapsed() < Duration::from_secs(3));
    }
}
