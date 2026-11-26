//! `ukai-store`: keeps what a gateway's agents and their people write
//! outside the repositories they work on: the notes about the code (see
//! "ナレッジ" in `docs/design.md`, [`knowledge`]), and which session
//! published which report (see "セッションのレポート", [`reports`]), and
//! what each turn of their sessions took (see "ターンのメトリクス", [`metrics`]).
//!
//! It serves `ukai.knowledge.v1.KnowledgeStore`,
//! `ukai.reports.v1.ReportStore`, and `ukai.metrics.v1.MetricsStore` over
//! gRPC to the gateway alone, and keeps them in SurrealDB ([`db`]). The gateway tells who is asking; this service
//! trusts it, so it answers only requests bearing the token the two share,
//! and nothing but the gateway should be able to reach it (a NetworkPolicy
//! in `deploy/gateway`). `grpc.health.v1.Health` says whether the store is
//! reached.
//!
//! Environment:
//! - `UKAI_STORE_TOKEN`: the token the gateway presents (required)
//! - `UKAI_STORE_LISTEN`: address to serve (default `0.0.0.0:50051`)
//! - `UKAI_STORE_DB`: the SurrealDB server, `ws://host:port` (required)
//! - `UKAI_STORE_DB_USER`, `UKAI_STORE_DB_PASS`: its root user

mod db;
mod knowledge;
mod metrics;
mod reports;

use std::net::SocketAddr;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::OnceCell;
use tonic_health::ServingStatus;
use tonic_health::server::HealthReporter;
use ukai_proto::knowledge::v1::knowledge_store_server::KnowledgeStoreServer;
use ukai_proto::metrics::v1::metrics_store_server::MetricsStoreServer;
use ukai_proto::reports::v1::report_store_server::ReportStoreServer;

use crate::db::StoreConfig;
use crate::knowledge::service::{self, Service};
use crate::knowledge::store::Knowledge;
use crate::metrics::Metrics;
use crate::reports::Reports;

/// How long to wait between attempts to reach the store.
const RETRY: Duration = Duration::from_secs(10);

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "ukai_store=info".into()),
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
    let token = env("UKAI_STORE_TOKEN")
        .ok_or("set UKAI_STORE_TOKEN: only the gateway, which holds it too, may ask")?;
    let listen: SocketAddr = env("UKAI_STORE_LISTEN")
        .unwrap_or_else(|| "0.0.0.0:50051".to_owned())
        .parse()
        .map_err(|error| format!("UKAI_STORE_LISTEN: {error}"))?;
    let config = StoreConfig {
        url: env("UKAI_STORE_DB").ok_or("set UKAI_STORE_DB")?,
        credentials: env("UKAI_STORE_DB_USER").zip(env("UKAI_STORE_DB_PASS")),
    };
    tracing::info!(%listen, store = %config.url, "store starting");

    let knowledge = Arc::new(OnceCell::new());
    let reports = Arc::new(OnceCell::new());
    let turns = Arc::new(OnceCell::new());
    let (reporter, health) = tonic_health::server::health_reporter();
    reporter
        .set_service_status("", ServingStatus::NotServing)
        .await;
    tokio::spawn(reach(
        knowledge.clone(),
        reports.clone(),
        turns.clone(),
        config,
        reporter,
    ));
    tonic::transport::Server::builder()
        .add_service(health)
        .add_service(KnowledgeStoreServer::with_interceptor(
            Service::new(knowledge),
            service::authorize(token.clone()),
        ))
        .add_service(ReportStoreServer::with_interceptor(
            reports::Service::new(reports),
            service::authorize(token.clone()),
        ))
        .add_service(MetricsStoreServer::with_interceptor(
            metrics::Service::new(turns),
            service::authorize(token),
        ))
        .serve_with_shutdown(listen, shutdown())
        .await?;
    Ok(())
}

/// Reach the store, trying until it answers; the services are UNAVAILABLE,
/// and not serving to the health check, until then.
async fn reach(
    knowledge: Arc<OnceCell<Knowledge>>,
    reports: Arc<OnceCell<Reports>>,
    turns: Arc<OnceCell<Metrics>>,
    config: StoreConfig,
    reporter: HealthReporter,
) {
    loop {
        let reached = async {
            Ok::<_, String>((
                Knowledge::connect(&config)
                    .await
                    .map_err(|error| error.to_string())?,
                Reports::connect(&config)
                    .await
                    .map_err(|error| error.to_string())?,
                Metrics::connect(&config)
                    .await
                    .map_err(|error| error.to_string())?,
            ))
        };
        match reached.await {
            Ok((notes, publications, measured)) => {
                tracing::info!(url = %config.url, "store reached");
                let _ = knowledge.set(notes);
                let _ = reports.set(publications);
                let _ = turns.set(measured);
                reporter
                    .set_service_status("", ServingStatus::Serving)
                    .await;
                reporter
                    .set_serving::<KnowledgeStoreServer<Service>>()
                    .await;
                reporter
                    .set_serving::<ReportStoreServer<reports::Service>>()
                    .await;
                reporter
                    .set_serving::<MetricsStoreServer<metrics::Service>>()
                    .await;
                return;
            }
            Err(error) => {
                tracing::warn!(url = %config.url, %error, "cannot reach the store; trying again");
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
