//! When a session whose connection dropped tries to reach the agent again.
//!
//! A server that keeps sessions (a gateway, `ukai-relay`, any agent that
//! advertises `loadSession` over HTTP or WebSocket) lets a client come back
//! with `initialize` + `session/load` and pick up where it left off, so a
//! dropped connection need not end the session:
//!
//! - During a turn, the session reconnects at once and keeps trying with
//!   exponential backoff (capped, with jitter), so the turn's output goes on.
//! - While idle, nothing is lost by waiting: the session stays detached and
//!   reconnects when something needs the agent, e.g. the next prompt.
//! - Only after [`ReconnectPolicy::max_attempts`] failed attempts in a row, or
//!   once [`ReconnectPolicy::budget`] has passed since the drop, does it give
//!   up and end.
//!
//! [`Reconnect`] is the pure decision; the session loop does the waiting and
//! the connecting, so the schedule is tested without a network or a clock.

use std::time::{Duration, Instant};

/// How hard to try before telling the user the connection is gone.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ReconnectPolicy {
    /// Wait before the second attempt; the first is immediate.
    pub initial: Duration,
    /// The longest wait between two attempts.
    pub max_delay: Duration,
    /// Each wait is spread by up to this fraction either way, so many panes
    /// cut off together do not come back in lockstep.
    pub jitter: f64,
    /// Failed attempts in a row before giving up.
    pub max_attempts: u32,
    /// How long after the drop to keep trying at all.
    pub budget: Duration,
}

impl Default for ReconnectPolicy {
    /// 0, 0.5, 1, 2, 4, 8, 16, 30 s (±20%): eight attempts over about a
    /// minute and a half, within three minutes in all.
    fn default() -> Self {
        Self {
            initial: Duration::from_millis(500),
            max_delay: Duration::from_secs(30),
            jitter: 0.2,
            max_attempts: 8,
            budget: Duration::from_secs(180),
        }
    }
}

impl ReconnectPolicy {
    /// Never reconnect: a dropped connection ends the session.
    pub const NEVER: Self = Self {
        initial: Duration::ZERO,
        max_delay: Duration::ZERO,
        jitter: 0.0,
        max_attempts: 0,
        budget: Duration::ZERO,
    };

    /// The wait before attempt `attempt` (1-based), before jitter.
    pub fn delay(&self, attempt: u32) -> Duration {
        match attempt {
            0 | 1 => Duration::ZERO,
            n => {
                let doublings = (n - 2).min(31);
                self.initial
                    .saturating_mul(1 << doublings)
                    .min(self.max_delay)
            }
        }
    }

    /// `delay` spread by `spread` in `[-1, 1]` times [`Self::jitter`].
    fn jittered(&self, delay: Duration, spread: f64) -> Duration {
        let factor = 1.0 + self.jitter * spread.clamp(-1.0, 1.0);
        delay.mul_f64(factor.max(0.0))
    }
}

/// What the session should do about its connection now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Next {
    /// Make attempt `attempt` (1-based) after `after`.
    Attempt { attempt: u32, after: Duration },
    /// Stay detached until something needs the agent, then call
    /// [`Reconnect::needed`].
    WhenNeeded,
    /// Stop trying: the session ends with the last attempt's error.
    GiveUp { attempts: u32 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Connected,
    Detached,
    /// Trying since `since`; `attempt` is the attempt being made.
    Retrying {
        attempt: u32,
        since: Instant,
    },
}

/// Whether and when to reconnect, given what happened to the connection.
#[derive(Debug, Clone)]
pub struct Reconnect {
    policy: ReconnectPolicy,
    state: State,
}

impl Reconnect {
    pub fn new(policy: ReconnectPolicy) -> Self {
        Self {
            policy,
            state: State::Connected,
        }
    }

    /// The connection dropped; `running` says a turn was going on.
    pub fn dropped(&mut self, running: bool, now: Instant) -> Next {
        if self.policy.max_attempts == 0 {
            return Next::GiveUp { attempts: 0 };
        }
        if running {
            self.start(now)
        } else {
            self.state = State::Detached;
            Next::WhenNeeded
        }
    }

    /// Something needs the agent while detached, e.g. the user sent a prompt.
    pub fn needed(&mut self, now: Instant) -> Next {
        match self.state {
            State::Detached => self.start(now),
            State::Connected => Next::WhenNeeded,
            State::Retrying { attempt, .. } => Next::Attempt {
                attempt,
                after: Duration::ZERO,
            },
        }
    }

    /// The attempt in progress failed. `spread` in `[-1, 1]` jitters the wait.
    pub fn failed(&mut self, now: Instant, spread: f64) -> Next {
        let State::Retrying { attempt, since } = self.state else {
            return self.start(now);
        };
        if attempt >= self.policy.max_attempts
            || now.saturating_duration_since(since) >= self.policy.budget
        {
            self.state = State::Detached;
            return Next::GiveUp { attempts: attempt };
        }
        let attempt = attempt + 1;
        self.state = State::Retrying { attempt, since };
        let after = self
            .policy
            .jittered(self.policy.delay(attempt), spread)
            // Never wait past the budget only to give up.
            .min(
                self.policy
                    .budget
                    .saturating_sub(now.saturating_duration_since(since)),
            );
        Next::Attempt { attempt, after }
    }

    /// The session is back.
    pub fn connected(&mut self) {
        self.state = State::Connected;
    }

    fn start(&mut self, now: Instant) -> Next {
        self.state = State::Retrying {
            attempt: 1,
            since: now,
        };
        Next::Attempt {
            attempt: 1,
            after: Duration::ZERO,
        }
    }
}

/// A value in `[-1, 1]` that differs from call to call, for jitter. Not for
/// anything that needs real randomness.
pub(crate) fn spread() -> f64 {
    use std::hash::{BuildHasher, Hasher};
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos()),
    );
    // 53 bits fit an f64's mantissa exactly.
    let unit = (hasher.finish() >> 11) as f64 / (1_u64 << 53) as f64;
    unit.mul_add(2.0, -1.0)
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    fn secs(s: f64) -> Duration {
        Duration::from_secs_f64(s)
    }

    #[rstest]
    #[case(1, 0.0)]
    #[case(2, 0.5)]
    #[case(3, 1.0)]
    #[case(4, 2.0)]
    #[case(7, 16.0)]
    #[case(8, 30.0)]
    #[case(40, 30.0)]
    fn backoff_doubles_up_to_the_cap(#[case] attempt: u32, #[case] expected: f64) {
        assert_eq!(ReconnectPolicy::default().delay(attempt), secs(expected));
    }

    #[rstest]
    #[case(-1.0, 1.6)]
    #[case(0.0, 2.0)]
    #[case(1.0, 2.4)]
    #[case(5.0, 2.4)]
    fn jitter_spreads_the_wait(#[case] spread: f64, #[case] expected: f64) {
        let policy = ReconnectPolicy::default();
        let waited = policy.jittered(secs(2.0), spread);
        assert!((waited.as_secs_f64() - expected).abs() < 1e-9, "{waited:?}");
    }

    #[test]
    fn spread_stays_in_range() {
        for _ in 0..100 {
            let spread = spread();
            assert!((-1.0..=1.0).contains(&spread), "{spread}");
        }
    }

    #[test]
    fn a_drop_during_a_turn_retries_at_once() {
        let mut reconnect = Reconnect::new(ReconnectPolicy::default());
        assert_eq!(
            reconnect.dropped(true, Instant::now()),
            Next::Attempt {
                attempt: 1,
                after: Duration::ZERO
            }
        );
    }

    #[test]
    fn a_drop_while_idle_waits_until_needed() {
        let mut reconnect = Reconnect::new(ReconnectPolicy::default());
        let now = Instant::now();
        assert_eq!(reconnect.dropped(false, now), Next::WhenNeeded);
        assert_eq!(
            reconnect.needed(now),
            Next::Attempt {
                attempt: 1,
                after: Duration::ZERO
            }
        );
    }

    #[test]
    fn failures_back_off_then_give_up() {
        let mut reconnect = Reconnect::new(ReconnectPolicy::default());
        let now = Instant::now();
        reconnect.dropped(true, now);
        let waits: Vec<Next> = (0..8).map(|_| reconnect.failed(now, 0.0)).collect();
        let expected: Vec<Next> = [0.5, 1.0, 2.0, 4.0, 8.0, 16.0, 30.0]
            .into_iter()
            .zip(2..)
            .map(|(after, attempt)| Next::Attempt {
                attempt,
                after: secs(after),
            })
            .chain([Next::GiveUp { attempts: 8 }])
            .collect();
        assert_eq!(waits, expected);
    }

    #[test]
    fn gives_up_once_the_budget_is_spent() {
        let policy = ReconnectPolicy {
            budget: Duration::from_secs(10),
            ..ReconnectPolicy::default()
        };
        let mut reconnect = Reconnect::new(policy);
        let start = Instant::now();
        reconnect.dropped(true, start);
        // The wait is cut short to the budget's end rather than overshoot it.
        assert_eq!(
            reconnect.failed(start + Duration::from_millis(9_800), 0.0),
            Next::Attempt {
                attempt: 2,
                after: Duration::from_millis(200)
            }
        );
        assert_eq!(
            reconnect.failed(start + Duration::from_secs(10), 0.0),
            Next::GiveUp { attempts: 2 }
        );
    }

    #[test]
    fn coming_back_resets_the_count() {
        let mut reconnect = Reconnect::new(ReconnectPolicy::default());
        let now = Instant::now();
        reconnect.dropped(true, now);
        reconnect.failed(now, 0.0);
        reconnect.failed(now, 0.0);
        reconnect.connected();
        assert_eq!(
            reconnect.dropped(true, now),
            Next::Attempt {
                attempt: 1,
                after: Duration::ZERO
            }
        );
        assert_eq!(
            reconnect.failed(now, 0.0),
            Next::Attempt {
                attempt: 2,
                after: secs(0.5)
            }
        );
    }

    #[rstest]
    #[case(true)]
    #[case(false)]
    fn never_gives_up_at_once(#[case] running: bool) {
        let mut reconnect = Reconnect::new(ReconnectPolicy::NEVER);
        assert_eq!(
            reconnect.dropped(running, Instant::now()),
            Next::GiveUp { attempts: 0 }
        );
    }
}
