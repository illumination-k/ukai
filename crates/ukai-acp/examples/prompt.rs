//! Send one prompt to an agent declared in `ukai.toml` and print the session
//! events: a headless check of a real agent without the desktop app.
//!
//! ```sh
//! cargo run -p ukai-acp --example prompt -- claude "Reply with one word: hi"
//! ```
//!
//! Permission requests are declined, so the agent cannot change anything.

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
    let transport = ukai_acp::resolve(spec, |name| std::env::var(name).ok())?;
    let (session, mut events) = ukai_acp::spawn_session(
        transport,
        SessionOptions {
            cwd: root.clone(),
            fs_root: FsRoot::new(Path::new(&root))?,
            restore: None,
        },
    );

    let mut prompted = false;
    while let Some(event) = events.recv().await {
        match &event {
            SessionEvent::AgentMessage { text } => print!("{text}"),
            SessionEvent::Ready { .. } if !prompted => {
                println!("{event:?}");
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
