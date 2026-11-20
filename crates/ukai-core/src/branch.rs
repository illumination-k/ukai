//! The branch each session last worked on, and switching a checkout's branch
//! when the user changes sessions in a pane: a new session starts from the
//! default branch, and an earlier one goes back to the branch it was on.
//!
//! The git commands go through `Checkout::run`, so they reach a local
//! checkout and a gateway workspace alike. That runs them with the user's
//! shell, which may not be POSIX, so the script is handed to `sh -c` in
//! single quotes, holds neither single quotes nor backslashes, and takes the
//! branch as a positional argument; only branch names of [`is_safe`]
//! characters are recorded or passed.

use serde::{Deserialize, Serialize};
use ukai_acp::checkout::CommandOutput;

use crate::ProjectId;

/// Sessions whose branch is kept, the most recent ones.
pub const MAX_RECORDED: usize = 500;

/// Prints the branch the checkout is on; fails when its `HEAD` is detached
/// or it is not a git checkout.
pub const CURRENT: &str = "git symbolic-ref --quiet --short HEAD";

/// The branch `output` of [`CURRENT`] names, if it is one to keep.
pub fn parse_current(output: &CommandOutput) -> Option<String> {
    if output.exit_code != Some(0) {
        return None;
    }
    let branch = output.output.trim();
    is_safe(branch).then(|| branch.to_owned())
}

/// Whether `branch` can be recorded and passed through any shell quoted as
/// it is: letters, digits, and `-`, `_`, `.`, `/`, `+`, `@`, not starting
/// with `-`.
pub fn is_safe(branch: &str) -> bool {
    !branch.is_empty()
        && !branch.starts_with('-')
        && branch
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./+@".contains(c))
}

/// Where a switch goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// What `origin/HEAD` points at, else `main`, else `master`.
    Default,
    Branch(String),
}

/// Switches to `$1` (the default branch when empty), unless there is nothing
/// to switch or tracked files have changes, which would follow the switch.
/// Its last line tells [`parse_switch`] what happened.
const SWITCH: &str = r#"git rev-parse --git-dir >/dev/null 2>&1 || { echo "ukai:norepo"; exit 0; }
target="$1"
if [ -z "$target" ]; then
  target=$(git symbolic-ref --quiet --short refs/remotes/origin/HEAD 2>/dev/null) && target=${target#origin/}
fi
if [ -z "$target" ]; then
  for candidate in main master; do
    if git rev-parse --verify --quiet "refs/heads/$candidate" >/dev/null || git rev-parse --verify --quiet "refs/remotes/origin/$candidate" >/dev/null; then
      target=$candidate
      break
    fi
  done
fi
[ -n "$target" ] || { echo "ukai:nodefault"; exit 0; }
current=$(git symbolic-ref --quiet --short HEAD 2>/dev/null)
[ "$current" = "$target" ] && { echo "ukai:already $target"; exit 0; }
[ -z "$(git status --porcelain --untracked-files=no 2>/dev/null)" ] || { echo "ukai:dirty $current"; exit 0; }
git switch --quiet "$target" 2>&1 || exit 1
echo "ukai:switched $target""#;

/// The command that switches the checkout to `target`, for `Checkout::run`.
/// `None` for a branch that is not [`is_safe`].
pub fn switch_command(target: &Target) -> Option<String> {
    let branch = match target {
        Target::Default => "",
        Target::Branch(branch) if is_safe(branch) => branch,
        Target::Branch(_) => return None,
    };
    debug_assert!(!SWITCH.contains(['\'', '\\']));
    Some(format!("sh -c '{SWITCH}' ukai '{branch}'"))
}

/// What a switch did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Switched {
    /// The checkout is now on the branch.
    To(String),
    /// It was on the branch already.
    Already(String),
    /// Tracked files have changes, so it stayed on the branch (`None` when
    /// detached).
    Dirty(Option<String>),
    /// Not a git checkout, or no default branch to go to: nothing to do.
    Nothing,
    /// Git refused; what it said.
    Failed(String),
}

/// What the [`switch_command`] that printed `output` did.
pub fn parse_switch(output: &CommandOutput) -> Switched {
    let text = output.output.trim_end();
    let last = text.lines().last().unwrap_or("");
    if output.exit_code == Some(0)
        && let Some(marker) = last.strip_prefix("ukai:")
    {
        let (kind, branch) = marker.split_once(' ').unwrap_or((marker, ""));
        let branch = branch.trim().to_owned();
        match kind {
            "switched" => return Switched::To(branch),
            "already" => return Switched::Already(branch),
            "dirty" => return Switched::Dirty((!branch.is_empty()).then_some(branch)),
            "norepo" | "nodefault" => return Switched::Nothing,
            _ => {}
        }
    }
    let why = if output.timed_out {
        "git took too long".to_owned()
    } else if text.is_empty() {
        match output.exit_code {
            Some(code) => format!("exited with {code}"),
            None => "was stopped".to_owned(),
        }
    } else {
        text.to_owned()
    };
    Switched::Failed(why)
}

/// The notice a pane shows for a switch, if any: nothing when the branch
/// stayed as it was meant to be.
pub fn notice(switched: &Switched) -> Option<(String, bool)> {
    match switched {
        Switched::To(branch) => Some((format!("Switched the checkout to `{branch}`."), false)),
        Switched::Already(_) | Switched::Nothing => None,
        Switched::Dirty(current) => Some((
            match current {
                Some(branch) => format!(
                    "Stayed on `{branch}`: the checkout has uncommitted changes, so its branch was not switched."
                ),
                None => "The checkout has uncommitted changes, so its branch was not switched."
                    .to_owned(),
            },
            false,
        )),
        Switched::Failed(why) => Some((format!("Could not switch the branch: {why}"), true)),
    }
}

/// Which session a branch is recorded for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionKey {
    pub project: ProjectId,
    pub agent: String,
    pub session: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Recorded {
    #[serde(flatten)]
    key: SessionKey,
    branch: String,
}

/// The branch each session last worked on, the most recently recorded last,
/// at most [`MAX_RECORDED`] of them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionBranches(Vec<Recorded>);

impl SessionBranches {
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn get(&self, key: &SessionKey) -> Option<&str> {
        self.0
            .iter()
            .find(|recorded| &recorded.key == key)
            .map(|recorded| recorded.branch.as_str())
    }

    /// Record that `key`'s session is on `branch`. Answers whether anything
    /// changed, so an unchanged record need not be saved.
    pub fn record(&mut self, key: SessionKey, branch: String) -> bool {
        if !is_safe(&branch) {
            return false;
        }
        if let Some(last) = self.0.last_mut()
            && last.key == key
        {
            if last.branch == branch {
                return false;
            }
            last.branch = branch;
            return true;
        }
        self.0.retain(|recorded| recorded.key != key);
        self.0.push(Recorded { key, branch });
        let excess = self.0.len().saturating_sub(MAX_RECORDED);
        self.0.drain(..excess);
        true
    }

    /// Forget the sessions of projects no longer registered.
    pub fn retain_projects(&mut self, registered: impl Fn(&ProjectId) -> bool) {
        self.0.retain(|recorded| registered(&recorded.key.project));
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    fn output(exit_code: Option<i32>, text: &str) -> CommandOutput {
        CommandOutput {
            exit_code,
            output: text.to_owned(),
            truncated: false,
            timed_out: false,
        }
    }

    fn key(session: &str) -> SessionKey {
        SessionKey {
            project: ProjectId("p".into()),
            agent: "claude".into(),
            session: session.into(),
        }
    }

    #[rstest]
    #[case("main", true)]
    #[case("feature/x-1.2", true)]
    #[case("user@fix+y_z", true)]
    #[case("", false)]
    #[case("-x", false)]
    #[case("it's", false)]
    #[case("a b", false)]
    #[case("a\\b", false)]
    #[case("$(rm)", false)]
    fn safe_branch_names(#[case] branch: &str, #[case] safe: bool) {
        assert_eq!(is_safe(branch), safe);
    }

    #[rstest]
    #[case(Some(0), "feature/x\n", Some("feature/x"))]
    #[case(Some(1), "", None)]
    #[case(Some(0), "it's\n", None)]
    fn reads_the_current_branch(
        #[case] code: Option<i32>,
        #[case] text: &str,
        #[case] branch: Option<&str>,
    ) {
        assert_eq!(parse_current(&output(code, text)).as_deref(), branch);
    }

    #[test]
    fn refuses_an_unsafe_branch() {
        assert_eq!(switch_command(&Target::Branch("a'b".into())), None);
        let command = switch_command(&Target::Branch("main".into())).unwrap_or_default();
        assert!(command.ends_with("' ukai 'main'"));
    }

    #[rstest]
    #[case(Some(0), "ukai:switched main\n", Switched::To("main".into()))]
    #[case(Some(0), "ukai:already dev\n", Switched::Already("dev".into()))]
    #[case(Some(0), "ukai:dirty dev\n", Switched::Dirty(Some("dev".into())))]
    #[case(Some(0), "ukai:dirty \n", Switched::Dirty(None))]
    #[case(Some(0), "ukai:norepo\n", Switched::Nothing)]
    #[case(Some(0), "ukai:nodefault\n", Switched::Nothing)]
    #[case(Some(1), "fatal: invalid reference: gone\n", Switched::Failed("fatal: invalid reference: gone".into()))]
    #[case(Some(127), "", Switched::Failed("exited with 127".into()))]
    fn reads_what_a_switch_did(
        #[case] code: Option<i32>,
        #[case] text: &str,
        #[case] switched: Switched,
    ) {
        assert_eq!(parse_switch(&output(code, text)), switched);
    }

    #[test]
    fn a_timed_out_switch_failed() {
        let mut timed_out = output(None, "");
        timed_out.timed_out = true;
        assert_eq!(
            parse_switch(&timed_out),
            Switched::Failed("git took too long".into())
        );
    }

    #[rstest]
    #[case(Switched::To("main".into()), Some(false))]
    #[case(Switched::Already("main".into()), None)]
    #[case(Switched::Nothing, None)]
    #[case(Switched::Dirty(Some("dev".into())), Some(false))]
    #[case(Switched::Dirty(None), Some(false))]
    #[case(Switched::Failed("no".into()), Some(true))]
    fn notices(#[case] switched: Switched, #[case] error: Option<bool>) {
        assert_eq!(notice(&switched).map(|(_, error)| error), error);
    }

    #[test]
    fn records_the_latest_branch_of_a_session() {
        let mut branches = SessionBranches::default();
        assert!(branches.record(key("a"), "dev".into()));
        assert!(!branches.record(key("a"), "dev".into()));
        assert!(branches.record(key("b"), "main".into()));
        assert!(branches.record(key("a"), "fix".into()));
        assert_eq!(branches.get(&key("a")), Some("fix"));
        assert_eq!(branches.get(&key("b")), Some("main"));
        assert_eq!(branches.get(&key("c")), None);
        assert!(!branches.record(key("c"), "it's".into()));
    }

    #[test]
    fn keeps_the_most_recent_sessions() {
        let mut branches = SessionBranches::default();
        for n in 0..=MAX_RECORDED {
            branches.record(key(&n.to_string()), "main".into());
        }
        assert_eq!(branches.get(&key("0")), None);
        assert_eq!(branches.get(&key("1")), Some("main"));
        assert_eq!(branches.get(&key(&MAX_RECORDED.to_string())), Some("main"));
    }

    #[test]
    fn forgets_removed_projects() {
        let mut branches = SessionBranches::default();
        branches.record(key("a"), "dev".into());
        branches.retain_projects(|_| false);
        assert!(branches.is_empty());
    }

    #[test]
    fn round_trips_through_json() {
        let mut branches = SessionBranches::default();
        branches.record(key("a"), "dev".into());
        let json = serde_json::to_string(&branches).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(
            json,
            r#"[{"project":"p","agent":"claude","session":"a","branch":"dev"}]"#
        );
        assert_eq!(
            serde_json::from_str::<SessionBranches>(&json).unwrap_or_else(|e| panic!("{e}")),
            branches
        );
    }

    /// The script against real git, in a throwaway repository.
    #[cfg(unix)]
    mod git {
        use std::path::Path;
        use std::process::Command;

        use super::super::*;

        fn git(root: &Path, args: &[&str]) {
            let status = Command::new("git")
                .args(args)
                .current_dir(root)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .status()
                .unwrap_or_else(|e| panic!("{e}"));
            assert!(status.success(), "git {args:?}");
        }

        fn repo() -> tempfile::TempDir {
            let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
            let root = dir.path();
            git(root, &["init", "--quiet", "--initial-branch=main"]);
            std::fs::write(root.join("a.txt"), "a\n").unwrap_or_else(|e| panic!("{e}"));
            git(root, &["add", "."]);
            git(
                root,
                &[
                    "-c",
                    "user.name=t",
                    "-c",
                    "user.email=t@example.com",
                    "commit",
                    "--quiet",
                    "-m",
                    "first",
                ],
            );
            git(root, &["switch", "--quiet", "-c", "feature/x"]);
            dir
        }

        async fn switch(root: &Path, target: Target) -> Switched {
            let command = switch_command(&target).unwrap_or_else(|| panic!("unsafe: {target:?}"));
            let output = ukai_acp::checkout::run(root, &command)
                .await
                .unwrap_or_else(|e| panic!("{e}"));
            parse_switch(&output)
        }

        async fn current(root: &Path) -> Option<String> {
            parse_current(
                &ukai_acp::checkout::run(root, CURRENT)
                    .await
                    .unwrap_or_else(|e| panic!("{e}")),
            )
        }

        #[tokio::test]
        async fn goes_to_the_default_branch_and_back() {
            let dir = repo();
            let root = dir.path();
            assert_eq!(current(root).await.as_deref(), Some("feature/x"));
            assert_eq!(
                switch(root, Target::Default).await,
                Switched::To("main".into())
            );
            assert_eq!(current(root).await.as_deref(), Some("main"));
            assert_eq!(
                switch(root, Target::Default).await,
                Switched::Already("main".into())
            );
            assert_eq!(
                switch(root, Target::Branch("feature/x".into())).await,
                Switched::To("feature/x".into())
            );
        }

        #[tokio::test]
        async fn stays_with_uncommitted_changes() {
            let dir = repo();
            let root = dir.path();
            std::fs::write(root.join("a.txt"), "changed\n").unwrap_or_else(|e| panic!("{e}"));
            assert_eq!(
                switch(root, Target::Default).await,
                Switched::Dirty(Some("feature/x".into()))
            );
            assert_eq!(current(root).await.as_deref(), Some("feature/x"));
        }

        #[tokio::test]
        async fn untracked_files_do_not_hold_it_back() {
            let dir = repo();
            let root = dir.path();
            std::fs::write(root.join("new.txt"), "new\n").unwrap_or_else(|e| panic!("{e}"));
            assert_eq!(
                switch(root, Target::Default).await,
                Switched::To("main".into())
            );
        }

        #[tokio::test]
        async fn a_branch_that_is_gone_fails() {
            let dir = repo();
            assert!(matches!(
                switch(dir.path(), Target::Branch("gone".into())).await,
                Switched::Failed(_)
            ));
        }

        #[tokio::test]
        async fn nothing_to_do_outside_git() {
            let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
            assert_eq!(switch(dir.path(), Target::Default).await, Switched::Nothing);
            assert_eq!(current(dir.path()).await, None);
        }
    }
}
