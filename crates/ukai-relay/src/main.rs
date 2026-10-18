//! `ukai-relay [--listen ADDR] -- AGENT [ARGS...]` runs AGENT as a stdio
//! ACP agent and serves it at `/acp` (HTTP/SSE and WebSocket) with sessions
//! that outlive client connections. The token clients must present comes
//! from `UKAI_RELAY_TOKEN`, never an argument, so it stays out of `ps`.
//!
//! `ukai-relay mcp [--root DIR] [--gateway URL] [--previews FILE]` is the MCP
//! server the relay hands the agent for each session (see `ukai_relay::mcp`),
//! on stdin/stdout; with a gateway, and the relay's token in
//! `UKAI_RELAY_TOKEN`, it also serves the knowledge kept there for its
//! project, and with a previews file it lets the agent open its servers in a
//! pane.
//!
//! In a gateway's pod, `UKAI_GATEWAY_URL` is where the gateway is reached.
//! There the relay is PID 1, so it runs itself again as its child and stays
//! as that child's init (`ukai_relay::init`).

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand};
use ukai_acp::knowledge::KnowledgeEndpoint;
use ukai_relay::server::{Options, run};

#[derive(Debug, Parser)]
#[command(
    version,
    about,
    args_conflicts_with_subcommands = true,
    subcommand_negates_reqs = true
)]
struct Cli {
    #[command(subcommand)]
    mode: Option<Mode>,
    /// Where to serve `/acp`, `/reports` and `/health`.
    #[arg(long, default_value = "0.0.0.0:7070")]
    listen: SocketAddr,
    /// Exit after this many seconds with no client connected and no turn
    /// running.
    #[arg(long, env = "UKAI_IDLE_TIMEOUT", default_value_t = 1800)]
    idle_timeout: u64,
    /// A file to write after every finished turn, for the workspace sidecar
    /// to take a snapshot.
    #[arg(long, env = "UKAI_SNAPSHOT_TRIGGER")]
    snapshot_trigger: Option<PathBuf>,
    /// The gateway that started this relay's pod, whose knowledge of the
    /// project the MCP server hands the agent.
    #[arg(long, env = "UKAI_GATEWAY_URL")]
    gateway: Option<String>,
    /// The agent and its arguments.
    #[arg(last = true, required = true, value_name = "AGENT")]
    command: Vec<String>,
}

#[derive(Debug, Subcommand)]
enum Mode {
    /// Serve the relay's MCP server on stdin/stdout.
    Mcp {
        /// The checkout whose reports it describes.
        #[arg(long, default_value = ".")]
        root: PathBuf,
        /// The gateway keeping the project's knowledge.
        #[arg(long)]
        gateway: Option<String>,
        /// The file the relay reads previews from, which the agent can then
        /// ask for.
        #[arg(long)]
        previews: Option<PathBuf>,
    },
}

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "ukai_relay=info".into()),
        )
        .init();
    let cli = Cli::parse();
    // Before the runtime starts threads that would take the signals init
    // forwards.
    #[cfg(target_os = "linux")]
    if cli.mode.is_none() && ukai_relay::init::is_init() {
        return match ukai_relay::init::supervise_self() {
            Ok(exit) => {
                tracing::info!(code = exit.code, orphans = exit.orphans, "relay exited");
                ExitCode::from(u8::try_from(exit.code).unwrap_or(1))
            }
            Err(error) => {
                tracing::error!(%error, "could not run the relay as init");
                ExitCode::FAILURE
            }
        };
    }
    match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime.block_on(serve(cli)),
        Err(error) => {
            tracing::error!(%error, "could not start the runtime");
            ExitCode::FAILURE
        }
    }
}

async fn serve(cli: Cli) -> ExitCode {
    let token = std::env::var(ukai_relay::mcp::TOKEN_ENV)
        .ok()
        .filter(|t| !t.is_empty());
    if let Some(Mode::Mcp {
        root,
        gateway,
        previews,
    }) = cli.mode
    {
        let notes = match (gateway, &token) {
            (Some(gateway), Some(token)) => {
                match KnowledgeEndpoint::for_workspace(&gateway, token) {
                    Ok(endpoint) => Some(endpoint),
                    Err(error) => {
                        tracing::warn!(%error, "cannot reach the gateway's knowledge");
                        None
                    }
                }
            }
            _ => None,
        };
        return match ukai_relay::mcp::serve(&root, notes, previews).await {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                tracing::error!(%error, "mcp server failed");
                ExitCode::FAILURE
            }
        };
    }
    let mut command = cli.command.into_iter();
    let Some(program) = command.next() else {
        return ExitCode::FAILURE;
    };
    let options = Options {
        listen: cli.listen,
        token,
        gateway: cli.gateway.filter(|url| !url.is_empty()),
        idle_timeout: Duration::from_secs(cli.idle_timeout),
        command: program,
        args: command.collect(),
        snapshot_trigger: cli.snapshot_trigger,
    };
    match run(options).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!(%error, "relay failed");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;
    use rstest::rstest;

    use super::*;

    #[test]
    fn the_cli_is_well_formed() {
        Cli::command().debug_assert();
    }

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("ukai-relay").chain(args.iter().copied()))
    }

    #[test]
    fn takes_the_agent_after_the_separator() {
        let cli = parse(&[
            "--listen",
            "0.0.0.0:7070",
            "--",
            "npx",
            "-y",
            "agent",
            "--acp",
        ])
        .unwrap_or_else(|e| panic!("{e}"));
        assert!(cli.mode.is_none());
        assert_eq!(
            cli.listen,
            "0.0.0.0:7070".parse().unwrap_or_else(|e| panic!("{e}"))
        );
        assert_eq!(cli.command, ["npx", "-y", "agent", "--acp"]);
    }

    #[test]
    fn mcp_needs_no_agent() {
        let cli = parse(&["mcp", "--root", "/workspace"]).unwrap_or_else(|e| panic!("{e}"));
        assert!(
            matches!(cli.mode, Some(Mode::Mcp { root, gateway: None, previews: None }) if root == std::path::Path::new("/workspace"))
        );
        let cli =
            parse(&["mcp", "--gateway", "http://ukai-gateway"]).unwrap_or_else(|e| panic!("{e}"));
        assert!(
            matches!(cli.mode, Some(Mode::Mcp { gateway: Some(url), .. }) if url == "http://ukai-gateway")
        );
    }

    #[rstest]
    #[case::no_agent(&[])]
    #[case::no_separator(&["npx", "agent"])]
    #[case::bad_address(&["--listen", "nowhere", "--", "agent"])]
    #[case::mcp_and_an_agent(&["mcp", "--", "agent"])]
    fn refuses(#[case] args: &[&str]) {
        assert!(parse(args).is_err());
    }
}
