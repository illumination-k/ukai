//! `ukai-relay [--listen ADDR] -- AGENT [ARGS...]` runs AGENT as a stdio
//! ACP agent and serves it at `/acp` (HTTP/SSE and WebSocket) with sessions
//! that outlive client connections. The token clients must present comes
//! from `UKAI_RELAY_TOKEN`, never an argument, so it stays out of `ps`.
//!
//! `ukai-relay mcp [--root DIR] [--gateway URL] [--previews FILE] [--review FILE]` is the MCP
//! server the relay hands the agent for each session (see `ukai_relay::mcp`),
//! on stdin/stdout; with a gateway, and the relay's token in
//! `UKAI_RELAY_TOKEN`, it also serves the knowledge kept there for its
//! project, and with a previews file it lets the agent open its servers in a
//! pane.
//!
//! `ukai-relay git-credential <get|store|erase>` is git's credential helper
//! in a pod whose agent uses the gateway's GitHub App (`ukai_relay::github`):
//! it answers `get` for the repository's host with an installation token
//! from the gateway at `UKAI_GITHUB_GATEWAY`, asked with `UKAI_RELAY_TOKEN`.
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
    /// The agent is the gateway's secretary, which reads and prompts the
    /// other workspaces.
    #[arg(long, env = "UKAI_SECRETARY")]
    secretary: bool,
    /// The gateway minting GitHub tokens for this pod's repository, which
    /// the relay keeps `gh` holding a fresh one of in `GH_CONFIG_DIR`.
    #[arg(long, env = ukai_relay::github::GATEWAY_ENV)]
    github_gateway: Option<String>,
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
        /// The file the relay keeps the agent's review of its changes in.
        #[arg(long)]
        review: Option<PathBuf>,
        /// The file the relay records which session each server serves in,
        /// for publishing reports through the gateway.
        #[arg(long)]
        sessions: Option<PathBuf>,
        /// Offer the secretary's tools, through the gateway.
        #[arg(long)]
        secretary: bool,
    },
    /// Answer git's credential helper protocol with a GitHub token from the
    /// gateway.
    GitCredential {
        /// What git asks: `get`, `store`, or `erase`; only `get` is answered.
        action: String,
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
    if let Some(Mode::GitCredential { action }) = cli.mode {
        return git_credential(&action, token.as_deref()).await;
    }
    if let Some(Mode::Mcp {
        root,
        gateway,
        previews,
        review,
        sessions,
        secretary,
    }) = cli.mode
    {
        let publishing = match (&gateway, &token, sessions) {
            (Some(gateway), Some(token), Some(sessions)) => {
                match ukai_acp::reports::PublishEndpoint::for_workspace(gateway, token) {
                    Ok(endpoint) => Some(ukai_relay::mcp::Publishing {
                        publisher: Box::new(endpoint),
                        sessions,
                        key: std::env::var(ukai_relay::hub::SESSION_KEY_ENV)
                            .ok()
                            .filter(|key| !key.is_empty()),
                    }),
                    Err(error) => {
                        tracing::warn!(%error, "cannot publish reports through the gateway");
                        None
                    }
                }
            }
            _ => None,
        };
        let notes = match (&gateway, &token) {
            (Some(gateway), Some(token)) => {
                match KnowledgeEndpoint::for_workspace(gateway, token) {
                    Ok(endpoint) => Some(endpoint),
                    Err(error) => {
                        tracing::warn!(%error, "cannot reach the gateway's knowledge");
                        None
                    }
                }
            }
            _ => None,
        };
        let secretary = match (secretary, &gateway, &token) {
            (true, Some(gateway), Some(token)) => {
                match ukai_acp::secretary::SecretaryEndpoint::for_workspace(gateway, token) {
                    Ok(endpoint) => {
                        Some(Box::new(endpoint) as Box<dyn ukai_relay::mcp::secretary::Secretary>)
                    }
                    Err(error) => {
                        tracing::warn!(%error, "cannot reach the gateway for the secretary's tools");
                        None
                    }
                }
            }
            _ => None,
        };
        return match ukai_relay::mcp::serve(&root, notes, previews, review, publishing, secretary)
            .await
        {
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
        secretary: cli.secretary,
        github: cli.github_gateway.filter(|url| !url.is_empty()),
    };
    match run(options).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!(%error, "relay failed");
            ExitCode::FAILURE
        }
    }
}

/// git's credential helper: a token for the repository's host on `get`,
/// and nothing for anything else, so git asks its next helper.
async fn git_credential(action: &str, token: Option<&str>) -> ExitCode {
    use std::io::{Read, Write};
    use ukai_relay::github;
    if action != "get" {
        return ExitCode::SUCCESS;
    }
    let mut input = String::new();
    if let Err(error) = std::io::stdin().read_to_string(&mut input) {
        tracing::error!(%error, "cannot read what git asks");
        return ExitCode::FAILURE;
    }
    let gateway = std::env::var(github::GATEWAY_ENV)
        .ok()
        .filter(|url| !url.is_empty());
    let (Some(gateway), Some(token)) = (gateway, token) else {
        tracing::error!(
            "no GitHub App in this pod: {} and {} are not set",
            github::GATEWAY_ENV,
            ukai_relay::mcp::TOKEN_ENV
        );
        return ExitCode::FAILURE;
    };
    let endpoint = match ukai_acp::github::TokenEndpoint::for_workspace(&gateway, token) {
        Ok(endpoint) => endpoint,
        Err(error) => {
            tracing::error!(%error, "cannot reach the gateway for a GitHub token");
            return ExitCode::FAILURE;
        }
    };
    let minted = match endpoint.token().await {
        Ok(minted) => minted,
        Err(error) => {
            tracing::error!(%error, "cannot get a GitHub token");
            return ExitCode::FAILURE;
        }
    };
    if !github::answers(&github::asked(&input), &minted.host) {
        return ExitCode::SUCCESS;
    }
    let mut stdout = std::io::stdout().lock();
    match stdout
        .write_all(github::credential(&minted).as_bytes())
        .and_then(|()| stdout.flush())
    {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!(%error, "cannot answer git");
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
            matches!(cli.mode, Some(Mode::Mcp { root, gateway: None, previews: None, review: None, sessions: None, secretary: false }) if root == std::path::Path::new("/workspace"))
        );
        let cli =
            parse(&["mcp", "--gateway", "http://ukai-gateway"]).unwrap_or_else(|e| panic!("{e}"));
        assert!(
            matches!(cli.mode, Some(Mode::Mcp { gateway: Some(url), .. }) if url == "http://ukai-gateway")
        );
    }

    #[test]
    fn git_credential_needs_no_agent() {
        let cli = parse(&["git-credential", "get"]).unwrap_or_else(|e| panic!("{e}"));
        assert!(matches!(cli.mode, Some(Mode::GitCredential { action }) if action == "get"));
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
