//! The checkout an agent works in, as the user reaches it: its files, to
//! mention one with `@`, running a command in it with `!`, and its changes,
//! for the diff pane.
//!
//! ACP has none of these: a prompt can link a file, but the client has to
//! know the files, nothing asks the agent's machine to run a command for the
//! user, and the agent reports only the edits of its own tool calls. So a
//! [`Checkout`] does them itself. A local agent's checkout is the project
//! root, read and run here. A gateway agent's is in its pod: the relay there
//! answers [`FILES_PATH`], [`RUN_PATH`], and [`DIFF_PATH`] with the same
//! [`list_files`], [`run`], and [`diff`], and the gateway passes them on as
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
use crate::review::{AnnotateRequest, Review};
use crate::{PaneWorkspace, ResolveError, header_map};

/// Where a relay lists its checkout's files (JSON, for the gateway).
pub const FILES_PATH: &str = "/files";

/// Where a relay runs a command in its checkout: a [`RunRequest`] posted as
/// JSON, answered with [`CommandOutput`].
pub const RUN_PATH: &str = "/run";

/// Where a relay diffs its checkout: `GET`, with `?base=` a [`DiffBase`],
/// answered with a [`CheckoutDiff`].
pub const DIFF_PATH: &str = "/diff";

/// Files a listing carries at most.
pub const MAX_FILES: usize = 20_000;

/// Bytes of patch a diff carries at most: well within a gRPC message.
pub const MAX_PATCH: usize = 2 * 1024 * 1024;

/// Untracked files a diff shows at most.
const MAX_UNTRACKED: usize = 200;

/// Untracked files larger than this are listed without their lines.
const MAX_UNTRACKED_SIZE: u64 = 256 * 1024;

/// How long the git commands of a diff may take, each.
const GIT_TIMEOUT: Duration = Duration::from_secs(30);

/// What `git hash-object -t tree /dev/null` names: a diff from it shows
/// every file, for a branch with no commit yet.
const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

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

/// What a diff is taken from.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffBase {
    /// Where the branch left the default branch: its commits and what is
    /// not committed yet, since an agent commits as it goes.
    #[default]
    Branch,
    /// `HEAD`: what is not committed yet.
    Uncommitted,
}

/// `?base=` of [`DIFF_PATH`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffQuery {
    #[serde(default)]
    pub base: DiffBase,
}

/// A checkout's changes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckoutDiff {
    /// What the patch is taken from, as the user reads it: `origin/main @
    /// 1a2b3c4d`, or `HEAD @ 1a2b3c4d`, and why when it is not what was asked.
    pub base: String,
    /// `git diff` from the base to the working tree, with untracked files
    /// as added ones; see `ukai_core::diff` for reading it.
    pub patch: String,
    /// The patch was cut at [`MAX_PATCH`], or there were more untracked files
    /// than a diff shows.
    #[serde(default)]
    pub truncated: bool,
    /// What the agent says of these changes, when it has said anything.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review: Option<Review>,
}

/// The changes of the checkout at `root` since `base`, or why there are
/// none to show (it is not a git checkout).
pub async fn diff(root: &Path, base: DiffBase) -> Result<CheckoutDiff, String> {
    git(root, &["rev-parse", "--is-inside-work-tree"])
        .await?
        .ok_or_else(|| "this checkout is not a git repository".to_owned())?;
    let (from, label) = base_commit(root, base).await?;
    let mut patch = git(
        root,
        &[
            "diff",
            "--no-color",
            "--no-ext-diff",
            // Whatever the user's `diff.noprefix` or `diff.mnemonicPrefix`.
            "--src-prefix=a/",
            "--dst-prefix=b/",
            "--find-renames",
            &from,
            "--",
        ],
    )
    .await?
    .ok_or_else(|| "git cannot diff this checkout".to_owned())?;
    let untracked = git(root, &["ls-files", "--others", "--exclude-standard", "-z"])
        .await?
        .unwrap_or_default();
    let untracked: Vec<&str> = untracked.split('\0').filter(|p| !p.is_empty()).collect();
    let mut truncated = untracked.len() > MAX_UNTRACKED;
    for path in untracked.into_iter().take(MAX_UNTRACKED) {
        if patch.len() > MAX_PATCH {
            break;
        }
        let size = std::fs::metadata(root.join(path)).map_or(0, |m| m.len());
        if size > MAX_UNTRACKED_SIZE {
            // Shown as a file whose lines are not, as git shows a binary one.
            patch.push_str(&format!(
                "diff --git a/{path} b/{path}\nnew file mode 100644\nBinary files /dev/null and b/{path} differ\n"
            ));
            continue;
        }
        // Exits with 1 when there is a difference, which there always is.
        if let Some(added) = git_any(
            root,
            &[
                "diff",
                "--no-color",
                "--no-ext-diff",
                "--src-prefix=a/",
                "--dst-prefix=b/",
                "--no-index",
                "--",
                "/dev/null",
                path,
            ],
        )
        .await?
        {
            patch.push_str(&added);
        }
    }
    if patch.len() > MAX_PATCH {
        truncated = true;
        patch = cut(&patch, MAX_PATCH);
    }
    Ok(CheckoutDiff {
        base: label,
        patch,
        truncated,
        review: None,
    })
}

/// `patch` cut to at most `max` bytes, at the start of a file when one
/// starts before then, else at a line.
fn cut(patch: &str, max: usize) -> String {
    if patch.len() <= max {
        return patch.to_owned();
    }
    let mut end = max;
    while !patch.is_char_boundary(end) {
        end -= 1;
    }
    let head = &patch[..end];
    let at = head
        .rfind("\ndiff --git ")
        .filter(|&at| at > 0)
        .map(|at| at + 1)
        .or_else(|| head.rfind('\n').map(|at| at + 1))
        .unwrap_or(end);
    patch[..at].to_owned()
}

/// The commit a diff of `base` starts at, and how to name it.
async fn base_commit(root: &Path, base: DiffBase) -> Result<(String, String), String> {
    let Some(head) = rev(root, "HEAD").await? else {
        return Ok((EMPTY_TREE.to_owned(), "nothing committed yet".to_owned()));
    };
    let short = |commit: &str| commit.get(..8).unwrap_or(commit).to_owned();
    if base == DiffBase::Uncommitted {
        return Ok((head.clone(), format!("HEAD @ {}", short(&head))));
    }
    let Some(branch) = default_branch(root).await? else {
        return Ok((
            head.clone(),
            format!("HEAD @ {} (no default branch to start from)", short(&head)),
        ));
    };
    match git(root, &["merge-base", "HEAD", &branch]).await? {
        Some(found) => {
            let found = found.trim().to_owned();
            let label = format!("{branch} @ {}", short(&found));
            Ok((found, label))
        }
        None => Ok((
            head.clone(),
            format!("HEAD @ {} (nothing in common with {branch})", short(&head)),
        )),
    }
}

/// The branch others start from: what `origin/HEAD` points at, else the
/// first of `origin/main`, `origin/master`, `main`, and `master` there is.
async fn default_branch(root: &Path) -> Result<Option<String>, String> {
    if let Some(branch) = git(
        root,
        &[
            "symbolic-ref",
            "--quiet",
            "--short",
            "refs/remotes/origin/HEAD",
        ],
    )
    .await?
    {
        return Ok(Some(branch.trim().to_owned()));
    }
    for candidate in ["origin/main", "origin/master", "main", "master"] {
        if rev(root, candidate).await?.is_some() {
            return Ok(Some(candidate.to_owned()));
        }
    }
    Ok(None)
}

/// The commit `name` names, if any.
async fn rev(root: &Path, name: &str) -> Result<Option<String>, String> {
    let spec = format!("{name}^{{commit}}");
    Ok(git(root, &["rev-parse", "--verify", "--quiet", &spec])
        .await?
        .map(|commit| commit.trim().to_owned()))
}

/// What git printed when it succeeded; `None` when it did not. `Err` when
/// it could not be run at all.
async fn git(root: &Path, args: &[&str]) -> Result<Option<String>, String> {
    let output = git_output(root, args).await?;
    Ok(output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned()))
}

/// Like [`git`], taking exit code 1 as success too, as `diff --no-index`
/// exits when the files differ.
async fn git_any(root: &Path, args: &[&str]) -> Result<Option<String>, String> {
    let output = git_output(root, args).await?;
    Ok(matches!(output.status.code(), Some(0 | 1))
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned()))
}

async fn git_output(root: &Path, args: &[&str]) -> Result<std::process::Output, String> {
    let mut command = tokio::process::Command::new("git");
    command
        // Paths as they are, not escaped, unless git must.
        .args(["-c", "core.quotePath=false"])
        .args(args)
        .current_dir(root)
        // Reading must not take the index lock the agent's git wants.
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    tokio::time::timeout(GIT_TIMEOUT, command.output())
        .await
        .map_err(|_| "git took too long".to_owned())?
        .map_err(|error| format!("cannot run git: {error}"))
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
    /// For commands and diffs: a timeout past the relay's own.
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

    pub async fn diff(&self, base: DiffBase) -> Result<CheckoutDiff, CheckoutError> {
        match self {
            Self::Local(root) => diff(root, base).await.map_err(CheckoutError::Failed),
            Self::Gateway(gateway) => {
                let request = gw::DiffRequest {
                    agent: gateway.agent.clone(),
                    workspace: Some(gateway.workspace.clone()),
                    base: gw::DiffBase::from(base).into(),
                };
                // A few git commands, which a large checkout makes slow.
                let diff = FilesClient::new(gateway.run.clone())
                    .diff(request)
                    .await
                    .map_err(|status| match status.code() {
                        // What the relay said of the checkout, word for word.
                        tonic::Code::Aborted => CheckoutError::Failed(status.message().to_owned()),
                        _ => status.into(),
                    })?
                    .into_inner();
                Ok(diff.into())
            }
        }
    }

    /// Reply to an annotation of the agent's review, or resolve or reopen
    /// it. Only a gateway workspace has a review to annotate.
    pub async fn annotate(&self, request: AnnotateRequest) -> Result<Review, CheckoutError> {
        let Self::Gateway(gateway) = self else {
            return Err(CheckoutError::Failed(
                "only a gateway agent's workspace keeps a review".to_owned(),
            ));
        };
        let request = gw::AnnotateRequest {
            agent: gateway.agent.clone(),
            workspace: Some(gateway.workspace.clone()),
            id: request.id,
            reply: request.reply,
            resolved: request.resolved,
        };
        let review = FilesClient::new(gateway.list.clone())
            .annotate(request)
            .await
            .map_err(|status| match status.code() {
                // Why the relay refused it, word for word.
                tonic::Code::Aborted => CheckoutError::Failed(status.message().to_owned()),
                _ => status.into(),
            })?
            .into_inner();
        Ok(review.into())
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

impl From<DiffBase> for gw::DiffBase {
    fn from(base: DiffBase) -> Self {
        match base {
            DiffBase::Branch => Self::Branch,
            DiffBase::Uncommitted => Self::Uncommitted,
        }
    }
}

impl From<gw::DiffBase> for DiffBase {
    fn from(base: gw::DiffBase) -> Self {
        match base {
            gw::DiffBase::Branch => Self::Branch,
            gw::DiffBase::Uncommitted => Self::Uncommitted,
        }
    }
}

impl From<CheckoutDiff> for gw::CheckoutDiff {
    fn from(diff: CheckoutDiff) -> Self {
        Self {
            base: diff.base,
            patch: diff.patch,
            truncated: diff.truncated,
            review: diff.review.map(Into::into),
        }
    }
}

impl From<gw::CheckoutDiff> for CheckoutDiff {
    fn from(diff: gw::CheckoutDiff) -> Self {
        Self {
            base: diff.base,
            patch: diff.patch,
            truncated: diff.truncated,
            review: diff.review.map(Into::into),
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

    /// Run git in `root` as a test sets a checkout up, whatever the user's
    /// configuration.
    fn git_in(root: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .args([
                "-c",
                "init.defaultBranch=main",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .current_dir(root)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap_or_else(|e| panic!("git: {e}"));
        assert!(status.success(), "git {args:?}");
    }

    fn write(root: &Path, path: &str, text: &str) {
        std::fs::write(root.join(path), text).unwrap_or_else(|e| panic!("write: {e}"));
    }

    fn paths(patch: &str) -> Vec<String> {
        patch
            .lines()
            .filter_map(|line| line.strip_prefix("+++ b/").or(line.strip_prefix("--- a/")))
            .map(str::to_owned)
            .collect()
    }

    /// `main` with one file, and `work` off it with a commit, an edit not
    /// committed, and a file not tracked.
    fn branched() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
        let root = dir.path();
        git_in(root, &["init", "-q"]);
        write(root, "base.txt", "one\n");
        git_in(root, &["add", "."]);
        git_in(root, &["commit", "-q", "-m", "base"]);
        git_in(root, &["checkout", "-q", "-b", "work"]);
        write(root, "committed.txt", "on the branch\n");
        git_in(root, &["add", "."]);
        git_in(root, &["commit", "-q", "-m", "work"]);
        write(root, "base.txt", "one\ntwo\n");
        write(root, "untracked.txt", "new\n");
        dir
    }

    #[tokio::test]
    async fn diffs_the_branch_from_where_it_left_main() {
        let dir = branched();
        let diff = diff(dir.path(), DiffBase::Branch)
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        assert!(diff.base.starts_with("main @ "), "{}", diff.base);
        assert!(!diff.truncated);
        let mut found = paths(&diff.patch);
        found.sort();
        found.dedup();
        assert_eq!(found, ["base.txt", "committed.txt", "untracked.txt"]);
    }

    #[tokio::test]
    async fn diffs_what_is_not_committed_yet() {
        let dir = branched();
        let diff = diff(dir.path(), DiffBase::Uncommitted)
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        assert!(diff.base.starts_with("HEAD @ "), "{}", diff.base);
        let mut found = paths(&diff.patch);
        found.sort();
        found.dedup();
        assert_eq!(found, ["base.txt", "untracked.txt"]);
        assert!(diff.patch.contains("+two"), "{}", diff.patch);
    }

    #[tokio::test]
    async fn diffs_a_repository_with_nothing_committed() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
        git_in(dir.path(), &["init", "-q"]);
        write(dir.path(), "first.txt", "hello\n");
        let diff = diff(dir.path(), DiffBase::Branch)
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(diff.base, "nothing committed yet");
        assert_eq!(paths(&diff.patch), ["first.txt"]);
    }

    #[tokio::test]
    async fn says_a_directory_is_not_a_repository() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
        assert_eq!(
            diff(dir.path(), DiffBase::Branch).await,
            Err("this checkout is not a git repository".to_owned())
        );
    }

    #[test]
    fn cuts_a_patch_at_a_file() {
        let patch = "diff --git a/a b/a\n+1\ndiff --git a/b b/b\n+2\n";
        assert_eq!(cut(patch, 30), "diff --git a/a b/a\n+1\n");
        // With no second file before the cut, at a line.
        assert_eq!(cut(patch, 21), "diff --git a/a b/a\n");
        assert_eq!(cut(patch, 1000), patch);
    }
}
