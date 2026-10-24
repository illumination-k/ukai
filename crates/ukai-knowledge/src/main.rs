//! `ukai-knowledge`: keeps the notes a gateway's agents and their people
//! write about the code they work on, outside its repositories (see
//! "ナレッジ" in `docs/design.md`).
//!
//! It serves `ukai.knowledge.v1.KnowledgeStore` over gRPC to the gateway
//! alone ([`service`]), and keeps the notes in SurrealDB ([`store`]). The
//! gateway tells who is asking; this service trusts it, so it answers only
//! requests bearing the token the two share, and nothing but the gateway
//! should be able to reach it (a NetworkPolicy in `deploy/gateway`).
//! `grpc.health.v1.Health` says whether the store is reached.
//!
//! Environment:
//! - `UKAI_KNOWLEDGE_TOKEN`: the token the gateway presents (required)
//! - `UKAI_KNOWLEDGE_LISTEN`: address to serve (default `0.0.0.0:50051`)
//! - `UKAI_KNOWLEDGE_STORE`: the SurrealDB server, `ws://host:port` (required)
//! - `UKAI_KNOWLEDGE_STORE_USER`, `UKAI_KNOWLEDGE_STORE_PASS`: its root user

mod service;
mod store;

use std::net::SocketAddr;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::OnceCell;
use tonic_health::ServingStatus;
use tonic_health::server::HealthReporter;
use ukai_proto::knowledge::v1::knowledge_store_server::KnowledgeStoreServer;

use crate::service::Service;
use crate::store::{Knowledge, StoreConfig};

/// How long to wait between attempts to reach the store.
const RETRY: Duration = Duration::from_secs(10);

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "ukai_knowledge=info".into()),
        )
        .init();
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!("{error}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let env = |name: &str| std::env::var(name).ok().filter(|value| !value.is_empty());
    let token = env("UKAI_KNOWLEDGE_TOKEN")
        .ok_or("set UKAI_KNOWLEDGE_TOKEN: only the gateway, which holds it too, may ask")?;
    let listen: SocketAddr = env("UKAI_KNOWLEDGE_LISTEN")
        .unwrap_or_else(|| "0.0.0.0:50051".to_owned())
        .parse()
        .map_err(|error| format!("UKAI_KNOWLEDGE_LISTEN: {error}"))?;
    let config = StoreConfig {
        url: env("UKAI_KNOWLEDGE_STORE").ok_or("set UKAI_KNOWLEDGE_STORE")?,
        credentials: env("UKAI_KNOWLEDGE_STORE_USER").zip(env("UKAI_KNOWLEDGE_STORE_PASS")),
    };
    tracing::info!(%listen, store = %config.url, "knowledge starting");

    let store = Arc::new(OnceCell::new());
    let (reporter, health) = tonic_health::server::health_reporter();
    reporter
        .set_service_status("", ServingStatus::NotServing)
        .await;
    tokio::spawn(reach(store.clone(), config, reporter));
    tonic::transport::Server::builder()
        .add_service(health)
        .add_service(KnowledgeStoreServer::with_interceptor(
            Service::new(store),
            service::authorize(token),
        ))
        .serve_with_shutdown(listen, shutdown())
        .await?;
    Ok(())
}

/// Reach the store, trying until it answers; the service is UNAVAILABLE,
/// and not serving to its health check, until then.
async fn reach(store: Arc<OnceCell<Knowledge>>, config: StoreConfig, reporter: HealthReporter) {
    loop {
        match Knowledge::connect(&config).await {
            Ok(knowledge) => {
                tracing::info!(url = %config.url, "knowledge store reached");
                let _ = store.set(knowledge);
                reporter
                    .set_service_status("", ServingStatus::Serving)
                    .await;
                reporter
                    .set_serving::<KnowledgeStoreServer<Service>>()
                    .await;
                return;
            }
            Err(error) => {
                tracing::warn!(url = %config.url, %error, "cannot reach the knowledge store; trying again");
                tokio::time::sleep(RETRY).await;
            }
        }
    }
}

/// Until the pod is told to stop.
async fn shutdown() {
    #[cfg(unix)]
    {
        if let Ok(mut terminate) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            tokio::select! {
                _ = terminate.recv() => {}
                _ = tokio::signal::ctrl_c() => {}
            }
            return;
        }
    }
    let _ = tokio::signal::ctrl_c().await;
}
