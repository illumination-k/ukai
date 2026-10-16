//! `ukai-relay [--listen ADDR] -- AGENT [ARGS...]` runs AGENT as a stdio
//! ACP agent and serves it at `/acp` (HTTP/SSE and WebSocket) with sessions
//! that outlive client connections. The token clients must present comes
//! from `UKAI_RELAY_TOKEN`, never an argument, so it stays out of `ps`.
//!
//! `ukai-relay mcp [--root DIR]` is the MCP server the relay hands the agent
//! for each session (see `ukai_relay::mcp`), on stdin/stdout.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand};
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
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "ukai_relay=info".into()),
        )
        .init();
    let cli = Cli::parse();
    if let Some(Mode::Mcp { root }) = cli.mode {
        return match ukai_relay::mcp::serve(&root) {
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
        token: std::env::var("UKAI_RELAY_TOKEN")
            .ok()
            .filter(|t| !t.is_empty()),
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
            matches!(cli.mode, Some(Mode::Mcp { root }) if root == std::path::Path::new("/workspace"))
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
