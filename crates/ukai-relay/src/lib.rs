//! `ukai-relay`: a stdio ACP agent behind a server whose sessions outlive
//! client connections. [`hub`] is the protocol side, [`server`] the process,
//! [`reports`] serves the reports the agent writes in its checkout, which
//! no client can read for itself, [`flows`] the runs of the flows it starts, [`ports`] lists and forwards to the
//! servers it runs in the pod, [`checkout`] lists the checkout's files and
//! runs the user's commands in it, [`terminal`] opens a shell there for a
//! terminal pane, [`sessions`] answers the gateway about the sessions for a
//! secretary agent, and [`mcp`] tells the agent about
//! reports, previews and flows.
//! As its container's first process, the relay also does what an init
//! would (`init`, on Linux).

pub mod checkout;
pub mod flows;
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
