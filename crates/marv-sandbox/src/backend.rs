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
