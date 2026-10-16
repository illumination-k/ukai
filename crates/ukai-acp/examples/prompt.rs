//! Send one prompt to an agent declared in `ukai.toml` and print the session
//! events: a headless check of a real agent without the desktop app.
//!
//! ```sh
//! cargo run -p ukai-acp --example prompt -- claude "Reply with one word: hi"
//! ```
//!
//! Permission requests are declined, so the agent cannot change anything.
//!
//! To check that a remote agent (e.g. behind `ukai-relay`) keeps its turn
//! going across a disconnect: `UKAI_DETACH_AFTER=N` drops the connection after
//! N message chunks and prints the session id; `UKAI_RESTORE=<id>` reconnects
//! to that session instead of prompting (the prompt argument is ignored) and
//! prints the replay and the rest of the turn. For a gateway agent,
//! `UKAI_POD_KEY=<key>` asks for a pod of that key's own, as a pane would.

#![allow(clippy::print_stdout, clippy::print_stderr)]

use std::path::Path;
use std::process::ExitCode;

use ukai_acp::{FsRoot, SessionEvent, SessionOptions};

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [agent, prompt] = args.as_slice() else {
        eprintln!("usage: prompt <agent-id> <prompt>");
        return ExitCode::FAILURE;
    };
    match run(agent, prompt).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn run(agent: &str, prompt: &str) -> Result<(), Box<dyn std::error::Error>> {
    let root = std::env::current_dir()?;
    let config = ukai_config::load(&root.join(ukai_config::FILE_NAME))?;
    let spec = config
        .agents
        .get(agent)
        .ok_or_else(|| format!("agent `{agent}` is not declared"))?;
    let workspace = std::env::var("UKAI_POD_KEY")
        .ok()
        .map(|key| ukai_acp::PaneWorkspace {
            key,
            scope: ukai_acp::WorkspaceScope::Pod,
        });
    let transport = ukai_acp::resolve(spec, ukai_acp::system_lookup, workspace.as_ref())?;
    let restore = std::env::var("UKAI_RESTORE").ok();
    let detach_after = std::env::var("UKAI_DETACH_AFTER")
        .ok()
        .and_then(|n| n.parse::<usize>().ok());
    let (session, mut events) = ukai_acp::spawn_session(
        transport,
        SessionOptions {
            cwd: root.clone(),
            fs_root: Some(FsRoot::new(Path::new(&root))?),
            restore: restore.clone(),
            update_check: None,
        },
    );

    let mut prompted = restore.is_some();
    let mut chunks = 0;
    let mut session_id = None;
    while let Some(event) = events.recv().await {
        match &event {
            SessionEvent::AgentMessage { text } => {
                print!("{text}");
                chunks += 1;
                if detach_after.is_some_and(|n| chunks >= n) {
                    println!(
                        "\n[detaching from session {}]",
                        session_id.as_deref().unwrap_or("?")
                    );
                    return Ok(());
                }
            }
            SessionEvent::Ready { session_id: id, .. } if !prompted => {
                println!("{event:?}");
                session_id = Some(id.clone());
                prompted = session.prompt(prompt);
            }
            SessionEvent::PermissionRequested { id, .. } => {
                println!("\n{event:?} -> declined");
                session.resolve_permission(*id, None);
            }
            SessionEvent::TurnEnded { .. } | SessionEvent::PromptFailed { .. } => {
                println!("\n{event:?}");
                break;
            }
            SessionEvent::Closed { error } => {
                return Err(error.clone().unwrap_or_else(|| "closed".into()).into());
            }
            _ => println!("\n{event:?}"),
        }
    }
    Ok(())
}
