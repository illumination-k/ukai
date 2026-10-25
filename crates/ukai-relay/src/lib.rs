//! `ukai-relay`: a stdio ACP agent behind a server whose sessions outlive
//! client connections. [`hub`] is the protocol side, [`server`] the process,
//! [`reports`] serves the reports the agent writes in its checkout, which
//! no client can read for itself, [`ports`] lists and forwards to the
//! servers it runs in the pod, [`checkout`] lists the checkout's files and
//! runs the user's commands in it, and [`mcp`] tells the agent about
//! reports and previews.
//! As its container's first process, the relay also does what an init
//! would (`init`, on Linux).

pub mod checkout;
pub mod hub;
#[cfg(target_os = "linux")]
pub mod init;
pub mod mcp;
pub mod ports;
pub mod reports;
pub mod server;
