//! A sandbox backend: how one [`Plan`] becomes a command that can't do more than the plan allows (#163,
//! `docs/specs/2026-10-17-macos-sandbox.md` §5).
//!
//! Today there is one, bubblewrap ([`crate::bwrap::Bwrap`]), on every platform: where `bwrap` doesn't run (macOS),
//! [`SandboxBackend::available`] is false and commands run unsandboxed with the warning, as before. Seatbelt
//! (`sandbox-exec`, macOS) is the second, built from the same plan: its profile builder (#164) renders the plan as an
//! SBPL profile, and `sandbox-exec` runs `marv __guard` (#165) around the command, so the command's environment, its
//! clean-up and the way it's killed all go through this trait rather than through callers that know bwrap.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::SandboxError;
use crate::fs::FsProbe;
use crate::plan::{Bind, Plan};

/// A sandboxed command ready to spawn: the argv, the whole environment to spawn it with (after `env_clear()`), and
/// what [`SandboxBackend::after`] removes once it has ended. The caller spawns it in the plan's project folder.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SandboxCommand {
    /// `bwrap <args> -- inner…` (or, on macOS, `/usr/bin/sandbox-exec -p … -- marv __guard … -- inner…`).
    pub argv: Vec<OsString>,
    /// The environment the sandbox program itself gets. bwrap sets the command's own (`--clearenv`, `--setenv`) and
    /// needs only `PATH` to be found; `sandbox-exec` passes its environment on, so there it's the plan's whole list.
    pub env: Vec<(OsString, OsString)>,
    /// Paths the sandbox may leave behind: bwrap's empty placeholder mount points (a private `TMPDIR` for Seatbelt).
    pub leftovers: Vec<PathBuf>,
}

impl SandboxCommand {
    /// The command to spawn, in `root`: [`SandboxCommand::argv`] with **only** [`SandboxCommand::env`], nothing
    /// inherited from Marv (no API keys, no `DYLD_*`). Always spawn through this: `sandbox-exec` passes its environment
    /// on to the command, so a spawn that kept Marv's would hand it all to the sandbox. (`tokio::process::Command`
    /// converts from it.) Stdio and the rest are the caller's.
    pub fn to_command(&self, root: &Path) -> std::process::Command {
        let mut cmd = std::process::Command::new(&self.argv[0]);
        cmd.args(&self.argv[1..]).env_clear().envs(self.env.iter().map(|(k, v)| (k, v))).current_dir(root);
        cmd
    }
}

/// A [`SandboxCommand`] that cleans up after itself: [`Prepared::finish`] runs the backend's
/// [`SandboxBackend::after`] once the command has ended, and dropping it does so if nothing did, so a spawn that fails
/// or a caller that returns early leaves nothing behind (bwrap's placeholder mount points, Seatbelt's private
/// `TMPDIR`). Finish it explicitly where the order matters: bash removes the placeholders before it scans for
/// protected files a command created.
#[derive(Debug)]
pub struct Prepared {
    command: SandboxCommand,
    backend: &'static dyn SandboxBackend,
    finished: bool,
}

impl Prepared {
    pub fn new(command: SandboxCommand, backend: &'static dyn SandboxBackend) -> Self {
        Self { command, backend, finished: false }
    }

    /// The command's clean-up, now (only the first call does anything). Call it once the command has ended.
    pub fn finish(&mut self) {
        if !self.finished {
            self.finished = true;
            self.backend.after(&self.command);
        }
    }
}

impl std::ops::Deref for Prepared {
    type Target = SandboxCommand;
    fn deref(&self) -> &SandboxCommand {
        &self.command
    }
}

impl Drop for Prepared {
    fn drop(&mut self) {
        self.finish();
    }
}

/// One way to sandbox a command. Selected by [`backend`]; held by [`crate::Sandbox`].
pub trait SandboxBackend: Send + Sync + std::fmt::Debug {
    /// "bubblewrap" (or "Seatbelt"), for notes and `/sandbox`.
    fn name(&self) -> &'static str;
    /// Whether it can start a sandbox here (probed once per process).
    fn available(&self) -> bool;
    /// The command that runs `inner` in the plan's sandbox, checked against `fs`. An error means a mount (or rule)
    /// can't be made safely, and the command must not start.
    fn command(&self, plan: &Plan, inner: &[OsString], fs: &dyn FsProbe) -> Result<SandboxCommand, SandboxError>;
    /// Where a bind (or the mesh plug, [`Plan::mesh_bind`]) appears inside: `bind.dest` for bubblewrap, `bind.source`
    /// for a backend that can't mount. Callers put this path in what runs inside (the relay's spec, its binary).
    fn guest_path(&self, bind: &Bind) -> PathBuf;
    /// Whether every sandbox of this backend hides `dir` from the commands in it, so the mesh may keep sockets
    /// there (`crate::hidden_in_sandbox` for bubblewrap).
    fn hides(&self, dir: &Path, home: &Path) -> bool;
    /// Ends a running sandboxed command and everything it started, at once (`pid`: the spawned sandbox program's).
    fn kill(&self, pid: u32);
    /// Asks a running sandboxed command and everything it started to end (a process's stop, before [`Self::kill`]).
    fn terminate(&self, pid: u32);
    /// After the command has ended: removes what the sandbox left behind ([`SandboxCommand::leftovers`]).
    fn after(&self, command: &SandboxCommand);
}

/// The backend for this platform: bubblewrap. (Seatbelt takes macOS in #166.)
pub fn backend() -> &'static dyn SandboxBackend {
    &crate::bwrap::Bwrap
}

/// Sends `signal` to `pid` (nothing for a pid that doesn't fit, or a process that's gone).
pub(crate) fn send(pid: u32, signal: nix::sys::signal::Signal) {
    // Never 0 or below: kill(2) would signal a whole process group, or every process the user may signal.
    if let Ok(pid) = i32::try_from(pid)
        && pid > 0
    {
        let _ = nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), signal);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A backend that only counts its clean-ups.
    #[derive(Debug)]
    struct Counting(AtomicUsize);
    impl SandboxBackend for Counting {
        fn name(&self) -> &'static str {
            "counting"
        }
        fn available(&self) -> bool {
            false
        }
        fn command(&self, _: &Plan, _: &[OsString], _: &dyn FsProbe) -> Result<SandboxCommand, SandboxError> {
            Ok(SandboxCommand::default())
        }
        fn guest_path(&self, bind: &Bind) -> PathBuf {
            bind.source.clone()
        }
        fn hides(&self, _: &Path, _: &Path) -> bool {
            false
        }
        fn kill(&self, _: u32) {}
        fn terminate(&self, _: u32) {}
        fn after(&self, _: &SandboxCommand) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn the_spawned_command_gets_only_the_backends_environment() {
        let command = SandboxCommand {
            argv: vec!["bwrap".into(), "--".into(), "true".into()],
            env: vec![("PATH".into(), "/usr/bin".into())],
            leftovers: vec![],
        };
        let cmd = command.to_command(Path::new("/work"));
        assert_eq!(cmd.get_program(), "bwrap");
        assert_eq!(cmd.get_args().collect::<Vec<_>>(), ["--", "true"]);
        assert_eq!(cmd.get_current_dir(), Some(Path::new("/work")));
        // The backend's environment and nothing else set (env_clear() itself is checked by the guards, which spawn this).
        let envs: Vec<_> = cmd.get_envs().collect();
        assert_eq!(envs, [(std::ffi::OsStr::new("PATH"), Some(std::ffi::OsStr::new("/usr/bin")))]);
    }

    #[test]
    fn a_prepared_command_cleans_up_once_finished_or_dropped() {
        static FINISHED: Counting = Counting(AtomicUsize::new(0));
        let mut prepared = Prepared::new(SandboxCommand::default(), &FINISHED);
        prepared.finish();
        prepared.finish();
        drop(prepared);
        assert_eq!(FINISHED.0.load(Ordering::SeqCst), 1, "finished once, not again on drop");
        static DROPPED: Counting = Counting(AtomicUsize::new(0));
        drop(Prepared::new(SandboxCommand::default(), &DROPPED));
        assert_eq!(DROPPED.0.load(Ordering::SeqCst), 1, "a spawn that failed or an early return still cleans up");
    }
}
