//! `ukai-relay`: a stdio ACP agent behind a server whose sessions outlive
//! client connections. [`hub`] is the protocol side, [`server`] the process.

pub mod hub;
pub mod server;
