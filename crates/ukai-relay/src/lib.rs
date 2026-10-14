//! `ukai-relay`: a stdio ACP agent behind a server whose sessions outlive
//! client connections. [`hub`] is the protocol side, [`server`] the process,
//! and [`reports`] serves the reports the agent writes in its checkout, which
//! no client can read for itself.

pub mod hub;
pub mod reports;
pub mod server;
