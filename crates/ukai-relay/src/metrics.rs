//! What each prompt turn took, recorded with the gateway as it ends (see
//! "PR ごとのメトリクス" in `docs/design.md`): the relay passes every
//! message between its clients and the agent, so it measures the turns of
//! every session ([`ukai_core::metrics::Meter`]), adds the branch the
//! checkout is on when a turn ends, and sends the turn to the gateway's
//! `WorkspaceMetrics`, which keeps it in `ukai-store`. Without a gateway
//! (a relay run by hand) nothing is measured.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;
use ukai_acp::metrics::TurnsEndpoint;
use ukai_core::metrics::Meter;

/// Measures turns and records each with the gateway.
pub struct Recorder {
    meter: Meter,
    endpoint: TurnsEndpoint,
    cwd: PathBuf,
}

impl Recorder {
    /// A recorder sending turns to the gateway at `gateway` with the
    /// relay's `token`, reading the branch of the checkout at `cwd`; `None`
    /// without either.
    pub fn new(gateway: Option<&str>, token: Option<&str>, cwd: Option<PathBuf>) -> Option<Self> {
        let endpoint = match TurnsEndpoint::for_workspace(gateway?, token?) {
            Ok(endpoint) => endpoint,
            Err(error) => {
                tracing::warn!(%error, "cannot record turns with the gateway");
                return None;
            }
        };
        Some(Self {
            meter: Meter::default(),
            endpoint,
            cwd: cwd?,
        })
    }

    /// A message the relay sends the agent.
    pub fn to_agent(&mut self, message: &Value) {
        self.meter.to_agent(message, now_millis());
    }

    /// A message from the agent: when it ends a turn, record the turn.
    pub fn from_agent(&mut self, message: &Value) {
        let Some(mut turn) = self.meter.from_agent(message, now_millis()) else {
            return;
        };
        let endpoint = self.endpoint.clone();
        let cwd = self.cwd.clone();
        tokio::spawn(async move {
            turn.branch = tokio::task::spawn_blocking(move || current_branch(&cwd))
                .await
                .ok()
                .flatten();
            if let Err(error) = endpoint.record(turn).await {
                tracing::warn!(%error, "cannot record a turn's metrics");
            }
        });
    }
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| i64::try_from(elapsed.as_millis()).ok())
        .unwrap_or(0)
}

/// The branch the checkout at `cwd` is on, if one worth recording.
fn current_branch(cwd: &Path) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(["symbolic-ref", "--quiet", "--short", "HEAD"])
        .current_dir(cwd)
        .output()
        .ok()?;
    let branch = String::from_utf8(output.stdout).ok()?;
    let branch = branch.trim();
    (output.status.success()
        && branch.len() <= ukai_core::metrics::BRANCH_MAX
        && ukai_core::branch::is_safe(branch))
    .then(|| branch.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_branch_of_a_checkout() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(dir.path())
                .output()
                .map(|output| output.status.success())
                .unwrap_or(false)
        };
        assert_eq!(current_branch(dir.path()), None);
        if !git(&["init", "-q", "-b", "feat/metrics"]) {
            return;
        }
        assert_eq!(current_branch(dir.path()).as_deref(), Some("feat/metrics"));
    }

    #[test]
    fn needs_a_gateway_and_a_token() {
        let cwd = Some(PathBuf::from("."));
        assert!(Recorder::new(None, Some("t"), cwd.clone()).is_none());
        assert!(Recorder::new(Some("http://ukai-gateway"), None, cwd).is_none());
    }
}
