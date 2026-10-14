//! `ukai-relay`: a stdio ACP agent behind a server whose sessions outlive
//! client connections. [`hub`] is the protocol side, [`server`] the process,
//! and [`reports`] serves the files the agent wrote in its working directory.

pub mod hub;
pub mod reports;
pub mod server;
