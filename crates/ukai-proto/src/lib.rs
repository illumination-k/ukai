//! The gRPC services between ukai's parts, generated from `proto/`: nothing
//! but the generated messages, clients, and servers. What the messages mean
//! in the rest of ukai, and the conversions to the types it works with, are
//! where those types are (`ukai_acp::knowledge`).

/// `ukai.knowledge.v1`: the notes `ukai-store` keeps for the gateway.
pub mod knowledge {
    pub mod v1 {
        #![allow(clippy::all, clippy::pedantic, missing_docs)]
        tonic::include_proto!("ukai.knowledge.v1");
    }
}

/// `ukai.reports.v1`: which session published which report, as
/// `ukai-store` keeps it for the gateway.
pub mod reports {
    pub mod v1 {
        #![allow(clippy::all, clippy::pedantic, missing_docs)]
        tonic::include_proto!("ukai.reports.v1");
    }
}

/// `ukai.metrics.v1`: what each prompt turn took, as `ukai-store` keeps it
/// for the gateway.
pub mod metrics {
    pub mod v1 {
        #![allow(clippy::all, clippy::pedantic, missing_docs)]
        tonic::include_proto!("ukai.metrics.v1");
    }
}

/// `ukai.gateway.v1`: what the gateway answers its clients and agent pods.
pub mod gateway {
    pub mod v1 {
        #![allow(clippy::all, clippy::pedantic, missing_docs)]
        tonic::include_proto!("ukai.gateway.v1");
    }
}
