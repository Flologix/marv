//! `marv __guard <parent-pid> -- <command…>` (hidden): the process supervisor for a sandbox backend without a PID
//! namespace (#165, `docs/specs/2026-10-17-macos-sandbox.md` §6.10). New in marv-rs; Marv had no such thing, since
//! bwrap's PID namespace and `--die-with-parent` did this job.
//!
//! Seatbelt (`sandbox-exec`) runs the guard around each sandboxed command (#166 wires it in). The guard:
//! - **makes a session** (`setsid`): a new session and process group whose id is the guard's pid. `sandbox-exec`
//!   execs the guard, so that's also the pid Marv spawned, and Marv's `killpg(pid, SIGKILL)` (a timeout, a stop,
//!   `kill_running`) takes down the guard, the command and everything still in the group at once. No controlling
//!   terminal comes with it. Without a session of its own the command doesn't run (exit 127): a guard spawned as a
//!   group leader (`process_group(0)`) can't `setsid`, and would otherwise leave the command in Marv's session, with
//!   Marv's controlling terminal.
//! - **watches its parent**: every [`POLL`] it checks `getppid()` against the pid it was given. When that changes
//!   (Marv exited, panicked or was SIGKILLed, and the guard was re-parented to init/launchd), it SIGKILLs its whole
//!   group, itself included. Polling is portable and needs nothing unsafe; `PR_SET_PDEATHSIG` isn't used because it
//!   fires when the *thread* that spawned the guard exits, and Marv spawns from Tokio's worker threads, which come
//!   and go. If the parent is already gone when the guard starts, the command never runs.
//! - **passes SIGTERM, SIGINT and SIGHUP on to the group** (a process's stop asks before it kills), and every other
//!   signal whose default would end the guard and leave the command unwatched ([`PASSED_ON`]). They're caught, not
//!   ignored (an ignored signal stays ignored across exec). The guard is in the group, so it gets its own copy back:
//!   that echo is skipped, once per signal passed on.
//! - **exits with the command's code**, 128 + n when signal n ended it (`marv_tools::relay::exit_code`, as bash's
//!   results report it), 127 when it couldn't start.
//!
//! What it doesn't do: kill what the command left running in the background after a normal exit. bwrap's namespace
//! does that; here the caller does it, `killpg(pid, SIGKILL)` after the guard has been waited for (a process group's
//! id isn't reused while the group has members). **Known limit** (spec §11, decision 5): a process that leaves the
//! group itself (`setsid`, `setpgid`) and double-forks is out of the guard's reach and can outlive the command. It
//! is still inside the sandbox (Seatbelt is inherited and can't be dropped). There's no process-tree sweep. The same
//! goes for the guard itself: it shares the command's sandbox and group, so the command can SIGKILL or SIGSTOP it,
//! and the group then outlives a Marv that crashes or is SIGKILLed (a Marv that exits normally or times the command
//! out still `killpg`s it).

use std::ffi::OsString;
use std::process::ExitCode;
use std::time::Duration;

use nix::sys::signal::{Signal, killpg};
use nix::unistd::{Pid, getpgrp, getpid, getppid, getsid, setsid};

/// How often the guard checks that its parent is still the one it was given.
pub const POLL: Duration = Duration::from_millis(200);

/// The signals passed on to the group: the stops, then every other one whose default action ends the process and that
/// can be caught (SIGKILL and SIGSTOP can't; SIGPIPE is ignored by Rust, and SIGSEGV, SIGBUS, SIGILL, SIGFPE, SIGTRAP,
/// SIGABRT and SIGSYS are the guard's own faults).
pub const PASSED_ON: [Signal; 11] = [
    Signal::SIGTERM,
    Signal::SIGINT,
    Signal::SIGHUP,
    Signal::SIGQUIT,
    Signal::SIGUSR1,
    Signal::SIGUSR2,
    Signal::SIGALRM,
    Signal::SIGVTALRM,
    Signal::SIGPROF,
    Signal::SIGXCPU,
    Signal::SIGXFSZ,
];

/// `marv __guard <parent> -- <command…>`: runs the command in a session of its own, and exits with its code.
pub fn main(parent: u32, command: &[OsString]) -> ExitCode {
    let code = run(parent, command);
    ExitCode::from(u8::try_from(code).unwrap_or(1))
}

fn run(parent: u32, command: &[OsString]) -> i32 {
    let Some((program, args)) = command.split_first() else {
        eprintln!("marv __guard: nothing to run");
        return 127;
    };
    let Some(parent) = i32::try_from(parent).ok().filter(|p| *p > 1).map(Pid::from_raw) else {
        eprintln!("marv __guard: {parent} isn't a parent to watch");
        return 127;
    };
    // `setsid` refuses a group leader: then only a session the guard already leads will do. A group of its own in
    // Marv's session would keep Marv's controlling terminal, so the command doesn't run.
    if setsid().is_err() && getsid(None).ok() != Some(getpid()) {
        eprintln!("marv __guard: couldn't start a session of its own");
        return 127;
    }
    // Checked after the session exists: from here, a parent that dies is noticed by the watch below.
    if getppid() != parent {
        eprintln!("marv __guard: its parent is gone; the command doesn't run");
        return 127;
    }
    let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("marv __guard: {e}");
            return 127;
        }
    };
    runtime.block_on(supervise(parent, program, args))
}

async fn supervise(parent: Pid, program: &OsString, args: &[OsString]) -> i32 {
    use tokio::signal::unix::{SignalKind, signal};
    // Before the spawn, so a stop that comes right after it isn't lost. The child gets the default dispositions back
    // (exec resets caught signals) and an empty signal mask (std resets it).
    let (arrived, mut signals) = tokio::sync::mpsc::unbounded_channel();
    for passed in PASSED_ON {
        let Ok(mut stream) = signal(SignalKind::from_raw(passed as i32)) else { continue };
        let arrived = arrived.clone();
        tokio::spawn(async move { while stream.recv().await.is_some() && arrived.send(passed).is_ok() {} });
    }
    let mut child = match tokio::process::Command::new(program).args(args).spawn() {
        Ok(child) => child,
        Err(e) => {
            eprintln!("marv __guard: couldn't start {}: {e}", program.to_string_lossy());
            return 127;
        }
    };
    let group = getpgrp();
    let mut echoes = Echoes::default();
    let mut watch = tokio::time::interval(POLL);
    loop {
        tokio::select! {
            status = child.wait() => {
                return match status {
                    Ok(status) => crate::relay::exit_code(status),
                    Err(_) => 1,
                };
            }
            _ = watch.tick() => {
                if getppid() != parent {
                    // Marv is gone: nothing it started may outlive it. This ends the guard too. Stopped first: macOS
                    // signals a group one member at a time, and a member woken by another's death could run on.
                    let _ = killpg(group, Signal::SIGSTOP);
                    let _ = killpg(group, Signal::SIGKILL);
                    return 137;
                }
            }
            Some(signal) = signals.recv() => echoes.pass_on(group, signal),
        }
    }
}

/// Signals passed on to the group whose copy hasn't come back to the guard yet.
#[derive(Debug, Default)]
struct Echoes {
    pending: Vec<Signal>,
}

impl Echoes {
    /// A signal arrived: the guard's own copy of one it passed on is dropped; anything else goes to the whole group
    /// (which sends a copy back here). Signals of a kind merge while pending, so one passed on that arrives while its
    /// echo is pending isn't passed on twice: the group has it already.
    fn pass_on(&mut self, group: Pid, signal: Signal) {
        if self.take_echo(signal) {
            return;
        }
        if killpg(group, signal).is_ok() {
            self.pending.push(signal);
        }
    }

    fn take_echo(&mut self, signal: Signal) -> bool {
        match self.pending.iter().position(|s| *s == signal) {
            Some(i) => {
                self.pending.swap_remove(i);
                true
            }
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_signals_echo_is_dropped_once() {
        let mut echoes = Echoes { pending: vec![Signal::SIGTERM] };
        assert!(!echoes.take_echo(Signal::SIGINT));
        assert!(echoes.take_echo(Signal::SIGTERM));
        assert!(!echoes.take_echo(Signal::SIGTERM), "the next SIGTERM is a new one");
    }

    #[test]
    fn nothing_to_run_or_no_parent_to_watch_is_refused() {
        assert_eq!(run(std::process::id(), &[]), 127);
        assert_eq!(run(0, &["true".into()]), 127);
        assert_eq!(run(1, &["true".into()]), 127);
        assert_eq!(run(u32::MAX, &["true".into()]), 127);
    }
}
