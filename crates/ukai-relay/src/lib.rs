//! `ukai-relay`: a stdio ACP agent behind a server whose sessions outlive
//! client connections. [`hub`] is the protocol side, [`server`] the process,
//! [`reports`] serves the reports the agent writes in its checkout, which
//! no client can read for itself, [`ports`] lists and forwards to the
//! servers it runs in the pod, [`checkout`] lists the checkout's files and
//! runs the user's commands in it, [`terminal`] opens a shell there for a
//! terminal pane, [`sessions`] answers the gateway about the sessions for a
//! secretary agent, [`github`] hands git and `gh` tokens of the gateway's
//! GitHub App, and [`mcp`] tells the agent about reports and previews.
//! As its container's first process, the relay also does what an init
//! would (`init`, on Linux).

pub mod checkout;
pub mod github;
pub mod hub;
#[cfg(target_os = "linux")]
pub mod init;
pub mod mcp;
pub mod metrics;
pub mod ports;
pub mod reports;
pub mod review;
pub mod server;
pub mod sessions;
pub mod terminal;
