//! `ukai-relay [--listen ADDR] -- COMMAND [ARGS...]`
//!
//! Runs COMMAND as a stdio ACP agent and serves it at `/acp` (HTTP/SSE and
//! WebSocket) with sessions that outlive client connections. The token clients
//! must present comes from `UKAI_RELAY_TOKEN`, the idle timeout in seconds from
//! `UKAI_IDLE_TIMEOUT` (default 1800), and `UKAI_SNAPSHOT_TRIGGER` names a file
//! to write after every finished turn.

use std::process::ExitCode;
use std::time::Duration;

use ukai_relay::server::{Options, run};

const USAGE: &str = "usage: ukai-relay [--listen ADDR] -- COMMAND [ARGS...]";

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "ukai_relay=info".into()),
        )
        .init();
    let options = match options(std::env::args().skip(1).collect()) {
        Ok(options) => options,
        Err(message) => {
            tracing::error!("{message}\n{USAGE}");
            return ExitCode::FAILURE;
        }
    };
    match run(options).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!(%error, "relay failed");
            ExitCode::FAILURE
        }
    }
}

fn options(args: Vec<String>) -> Result<Options, String> {
    let mut listen = "0.0.0.0:7070".to_owned();
    let mut args = args.into_iter();
    let mut command = Vec::new();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--listen" => listen = args.next().ok_or("--listen needs an address")?,
            "--" => {
                command = args.by_ref().collect();
                break;
            }
            other => return Err(format!("unexpected argument `{other}`")),
        }
    }
    let mut command = command.into_iter();
    let program = command.next().ok_or("no agent command given")?;
    let idle = std::env::var("UKAI_IDLE_TIMEOUT")
        .ok()
        .map(|value| {
            value
                .parse::<u64>()
                .map_err(|_| format!("UKAI_IDLE_TIMEOUT `{value}` is not seconds"))
        })
        .transpose()?
        .unwrap_or(1800);
    Ok(Options {
        listen: listen
            .parse()
            .map_err(|_| format!("`{listen}` is not an address"))?,
        token: std::env::var("UKAI_RELAY_TOKEN")
            .ok()
            .filter(|t| !t.is_empty()),
        idle_timeout: Duration::from_secs(idle),
        command: program,
        args: command.collect(),
        snapshot_trigger: std::env::var_os("UKAI_SNAPSHOT_TRIGGER").map(Into::into),
    })
}
