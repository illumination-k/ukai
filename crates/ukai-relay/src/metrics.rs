//! Measuring each turn of the sessions the relay runs, and sending what each
//! took to the gateway that started its pod, which has `ukai-store` keep it
//! (see "ターンのメトリクス" in `docs/design.md`). The relay hears the agent
//! itself, so its times hold no client's network, and turns with no client
//! attached are measured too.

use std::cell::RefCell;
use std::collections::HashMap;
use std::time::{Instant, SystemTime};

use ukai_acp::SessionEvent;
use ukai_acp::metrics::{MetricsEndpoint, Recorder, TurnRecord};

/// A [`Recorder`] per session, fed the hub's [`crate::hub::Output::Metered`].
pub struct Meter {
    recorders: RefCell<HashMap<String, Recorder>>,
    /// Where finished turns go; with no gateway they are only logged.
    endpoint: Option<MetricsEndpoint>,
}

impl Meter {
    pub fn new(endpoint: Option<MetricsEndpoint>) -> Self {
        Self {
            recorders: RefCell::default(),
            endpoint,
        }
    }

    /// Take `event` of `session`, as it happens; a turn it ends is sent.
    pub fn observe(&self, session: &str, event: &SessionEvent) {
        let ended = self
            .recorders
            .borrow_mut()
            .entry(session.to_owned())
            .or_insert_with(|| Recorder::of_session(session))
            .observe(event, Instant::now(), SystemTime::now());
        if let Some(turn) = ended {
            self.send(turn);
        }
    }

    fn send(&self, turn: TurnRecord) {
        tracing::debug!(
            outcome = %turn.outcome,
            duration_ms = turn.duration_ms,
            tools = turn.tools.len(),
            "turn measured"
        );
        let Some(endpoint) = self.endpoint.clone() else {
            return;
        };
        tokio::spawn(async move {
            if let Err(error) = endpoint.record(turn).await {
                tracing::warn!(%error, "cannot record the turn's metrics");
            }
        });
    }
}
