//! The gRPC services between ukai's parts, generated from `proto/`: nothing
//! but the generated messages, clients, and servers. What the messages mean
//! in the rest of ukai, and the conversions to the types it works with, are
//! where those types are (`ukai_acp::knowledge`).

/// `ukai.knowledge.v1`: the notes `ukai-knowledge` keeps for the gateway.
pub mod knowledge {
    pub mod v1 {
        #![allow(clippy::all, clippy::pedantic, missing_docs)]
        tonic::include_proto!("ukai.knowledge.v1");
    }
}

/// `ukai.gateway.v1`: what the gateway answers its clients and agent pods.
pub mod gateway {
    pub mod v1 {
        #![allow(clippy::all, clippy::pedantic, missing_docs)]
        tonic::include_proto!("ukai.gateway.v1");
    }
}
