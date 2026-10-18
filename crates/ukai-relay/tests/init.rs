//! `init` reaps every child of the process, so its checks run one after
//! another in a binary of their own, where no other test's child is taken.
#![cfg(target_os = "linux")]

use std::time::{Duration, Instant};

use nix::sys::prctl::set_child_subreaper;
use nix::sys::pthread::pthread_kill;
use nix::sys::signal::Signal;
use ukai_relay::init::{Exit, Supervisor};

fn sh(script: &str) -> std::io::Result<Supervisor> {
    Supervisor::start("sh".as_ref(), ["-c", script])
}

fn run(script: &str) -> std::io::Result<Exit> {
    sh(script)?.wait()
}

#[test]
fn acts_as_init() {
    // Orphans go to the nearest subreaper, which is what PID 1 is.
    assert!(set_child_subreaper(true).is_ok());

    // The subshell exits at once, handing its `true` to this process.
    let exit = run("( true & ); sleep 1; exit 3");
    assert!(
        matches!(exit, Ok(Exit { code: 3, orphans }) if orphans >= 1),
        "{exit:?}"
    );

    let exit = run("kill -KILL $$");
    assert!(matches!(exit, Ok(Exit { code: 137, .. })), "{exit:?}");

    // Well before the sleep ends: the shell must get the signal at once.
    let started = Instant::now();
    let supervisor =
        sh("trap 'kill $!; exit 7' TERM; sleep 10 & wait").unwrap_or_else(|e| panic!("{e}"));
    // Give the shell time to set its trap.
    std::thread::sleep(Duration::from_millis(500));
    assert!(pthread_kill(supervisor.forwarder(), Signal::SIGTERM).is_ok());
    let exit = supervisor.wait();
    assert!(matches!(exit, Ok(Exit { code: 7, .. })), "{exit:?}");
    assert!(started.elapsed() < Duration::from_secs(5));

    assert!(Supervisor::start("/nonexistent".as_ref(), [""; 0]).is_err());
}
