//! What a container's first process owes the rest: in a gateway's pod the
//! relay is PID 1, so every process whose parent exits is handed to it, and
//! one nobody reaps stays a zombie. Tools that daemonize (Bazel's server, a
//! build's `nohup`) then find their dead server "still running" by its pid
//! and cannot replace it.
//!
//! So as PID 1 the relay starts itself again as its only child and stays
//! behind as a small init, like `tini`: it forwards signals to that child,
//! reaps every orphan, and exits as the child does.

use std::ffi::{CString, OsStr};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::thread::JoinHandleExt;
use std::thread::JoinHandle;

use nix::errno::Errno;
use nix::spawn::{PosixSpawnAttr, PosixSpawnFileActions, PosixSpawnFlags, posix_spawnp};
use nix::sys::pthread::Pthread;
use nix::sys::signal::{SigSet, Signal, kill};
use nix::sys::wait::{WaitStatus, waitpid};
use nix::unistd::Pid;

/// Passed on to the child; a container runtime stops a pod with `SIGTERM`.
const FORWARDED: [Signal; 6] = [
    Signal::SIGTERM,
    Signal::SIGINT,
    Signal::SIGHUP,
    Signal::SIGQUIT,
    Signal::SIGUSR1,
    Signal::SIGUSR2,
];

/// How the child ended, and how many orphans were reaped meanwhile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Exit {
    /// The child's exit code, or `128 + signal` when a signal killed it.
    pub code: i32,
    pub orphans: usize,
}

/// Whether this process is its PID namespace's init.
pub fn is_init() -> bool {
    std::process::id() == 1
}

/// A child run the way init runs one.
pub struct Supervisor {
    child: Pid,
    forwarder: JoinHandle<()>,
}

impl Supervisor {
    /// Start `program` (looked up in `PATH`) with `args`, and forward
    /// [`FORWARDED`] signals to it from a thread of their own. They are
    /// blocked first in the calling thread, which the forwarder inherits, so
    /// a process-directed signal waits for it as long as no other thread
    /// takes it: call this before starting any.
    pub fn start<I, S>(program: &OsStr, args: I) -> io::Result<Self>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut signals = SigSet::empty();
        for signal in FORWARDED {
            signals.add(signal);
        }
        signals.thread_block()?;
        let child = spawn(program, args)?;
        let forwarder = std::thread::Builder::new()
            .name("forward-signals".into())
            .spawn(move || {
                while let Ok(signal) = signals.wait() {
                    // It may have just ended; `wait` sees to that.
                    let _ = kill(child, signal);
                }
            })?;
        Ok(Self { child, forwarder })
    }

    /// The thread forwarding signals, for tests to signal directly.
    pub fn forwarder(&self) -> Pthread {
        self.forwarder.as_pthread_t()
    }

    /// Reap every child that ends, orphans handed to this process included,
    /// until the one started has.
    pub fn wait(self) -> io::Result<Exit> {
        let mut orphans = 0;
        loop {
            let status = match waitpid(None, None) {
                Err(Errno::EINTR) => continue,
                Err(error) => return Err(error.into()),
                Ok(status) => status,
            };
            let code = match status {
                WaitStatus::Exited(_, code) => code,
                WaitStatus::Signaled(_, signal, _) => 128 + signal as i32,
                // Stops and continues are reported only on request.
                _ => continue,
            };
            if status.pid() == Some(self.child) {
                return Ok(Exit { code, orphans });
            }
            orphans += 1;
        }
    }
}

/// Spawn with no signal blocked: a child inherits its parent's mask, and
/// `std::process::Command` does not reset it.
fn spawn<I, S>(program: &OsStr, args: I) -> io::Result<Pid>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let c_string = |s: &OsStr| CString::new(s.as_bytes()).map_err(io::Error::other);
    let program = c_string(program)?;
    let mut argv = vec![program.clone()];
    for arg in args {
        argv.push(c_string(arg.as_ref())?);
    }
    let env = std::env::vars_os()
        .map(|(key, value)| {
            let mut pair = key;
            pair.push("=");
            pair.push(value);
            c_string(&pair)
        })
        .collect::<io::Result<Vec<_>>>()?;
    let mut attr = PosixSpawnAttr::init()?;
    attr.set_sigmask(&SigSet::empty())?;
    attr.set_flags(PosixSpawnFlags::POSIX_SPAWN_SETSIGMASK)?;
    let actions = PosixSpawnFileActions::init()?;
    Ok(posix_spawnp(&program, &actions, &attr, &argv, &env)?)
}

/// Run this program again with the same arguments as init would; the child
/// is not PID 1, so it does the relay's work.
pub fn supervise_self() -> io::Result<Exit> {
    let program = std::env::current_exe()?;
    Supervisor::start(program.as_os_str(), std::env::args_os().skip(1))?.wait()
}
