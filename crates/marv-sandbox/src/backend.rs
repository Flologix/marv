//! A sandbox backend: how one [`Plan`] becomes a command that can't do more than the plan allows (#163,
//! `docs/specs/2026-10-17-macos-sandbox.md` §5).
//!
//! Two: bubblewrap ([`crate::bwrap::Bwrap`]) on Linux, and Seatbelt ([`crate::seatbelt_backend::Seatbelt`],
//! `sandbox-exec`) on macOS (#166), built from the same plan: its profile builder (#164) renders the plan as an SBPL
//! profile, and `sandbox-exec` runs `marv __guard` (#165) around the command, so the command's environment, its
//! clean-up and the way it's killed all go through this trait rather than through callers that know bwrap. Where the
//! backend can't start a sandbox, [`SandboxBackend::available`] is false and commands run unsandboxed with the
//! warning.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

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
    /// The spawned sandbox program's pid, once [`Prepared::spawned`] said so.
    pid: Option<u32>,
}

impl Prepared {
    pub fn new(command: SandboxCommand, backend: &'static dyn SandboxBackend) -> Self {
        Self { command, backend, finished: false, pid: None }
    }

    /// The command is running as `pid`. Dropped before [`Prepared::finish`] (its owner went away while it ran: a
    /// process manager dropped), the backend kills it, and everything it started, before cleaning up: Seatbelt's
    /// private `TMPDIR` is never removed from under a running command (it could make the folder again and leave it).
    pub fn spawned(&mut self, pid: Option<u32>) {
        self.pid = pid;
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
        if !self.finished
            && let Some(pid) = self.pid.take()
        {
            self.backend.kill(pid);
        }
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
    /// After the sandbox program has exited (seen, not reaped yet, so its pid and the id of the group it led are still
    /// its own): ends everything the command left running, before the caller reaps it. `killed`: a timeout or a stop
    /// killed it first, so a process being created as that kill landed may still appear. `held`: true while the caller
    /// still holds the program unreaped; the sweep stops as soon as it's false (the group's id may then be another's).
    /// [`Self::kill`] unless the backend needs more (Seatbelt: the group is killed again and again, for seconds after a
    /// kill).
    fn sweep(&self, pid: u32, _killed: bool, held: &AtomicBool) {
        if held.load(Ordering::SeqCst) {
            self.kill(pid);
        }
    }
    /// Whether [`Self::sweep`] after a kill takes long (Seatbelt: [`GROUP_LINGER`]). The caller then doesn't wait for
    /// it: it returns at once, and reaps the program and cleans up when the sweep is done.
    fn lingers(&self) -> bool {
        false
    }
    /// Asks a running sandboxed command and everything it started to end (a process's stop, before [`Self::kill`]).
    fn terminate(&self, pid: u32);
    /// After the command has ended: removes what the sandbox left behind ([`SandboxCommand::leftovers`]).
    fn after(&self, command: &SandboxCommand);
    /// Variables this backend sets on top of [`Plan::env`], given the command's `TMPDIR` (none for bubblewrap).
    fn extra_env(&self, _tmp: &str) -> Vec<(String, String)> {
        Vec::new()
    }
    /// Whether the process door (background processes and their port relay) works in this backend's sandbox. Not
    /// yet on macOS (#152, spec §8 option C): the door isn't offered there.
    fn processes(&self) -> bool {
        true
    }
}

/// The backend for this platform: Seatbelt on macOS (#166), bubblewrap everywhere else.
pub fn backend() -> &'static dyn SandboxBackend {
    #[cfg(target_os = "macos")]
    {
        &crate::seatbelt_backend::Seatbelt
    }
    #[cfg(not(target_os = "macos"))]
    {
        &crate::bwrap::Bwrap
    }
}

/// Kills the process group `pid` leads: SIGSTOP to the group, then SIGKILL (nothing for a pid that doesn't fit or a
/// group that's gone). Never for a pid of 1 or below: `killpg(0)` is the caller's own group, and `killpg(1)` init's.
///
/// Stopped first (#247): macOS delivers a group signal to one member after another, so a SIGKILL alone can end a
/// child before its parent, and the parent, woken by that on another core, runs on until its own SIGKILL comes: long
/// enough for a shell to start its next command (`(sleep 2; touch late)` touched `late`), which the kill, already
/// past, then misses. A stopped process can't run on, and SIGKILL ends it all the same.
pub fn kill_group(pid: u32) {
    if let Ok(pid) = i32::try_from(pid)
        && pid > 1
    {
        let _ = stop_and_kill(nix::unistd::Pid::from_raw(pid));
    }
}

/// [`kill_group`]'s signals: what the SIGKILL `killpg` returned.
fn stop_and_kill(group: nix::unistd::Pid) -> nix::Result<()> {
    let _ = nix::sys::signal::killpg(group, nix::sys::signal::Signal::SIGSTOP);
    nix::sys::signal::killpg(group, nix::sys::signal::Signal::SIGKILL)
}

/// How long [`kill_group_for`] keeps killing a group whose leader exited by itself, for what it left running: a few
/// rounds, so a process its jobs were forking as the first round landed is reached by the next.
pub const GROUP_SWEEP: Duration = Duration::from_millis(5);

/// The fewest rounds [`kill_group_for`] makes, however long they take (#269): a window counted in time alone can pass
/// in one round when the sweeping thread isn't scheduled (a loaded macOS runner), and a process being forked as that
/// round landed then escapes. Counted in rounds, a slow machine gets a longer sweep, which is when creation is slow.
const GROUP_MIN_ROUNDS: u32 = 5;

/// How long [`kill_group_for`] keeps killing a group after a timeout or a stop killed its leader (#247): a process the
/// leader (or the command's shell) was creating as the kill landed appears only once its creation is done, and on a
/// loaded Intel Mac that was seen to take well over 50 ms after its parent had died.
pub const GROUP_LINGER: Duration = Duration::from_secs(3);

/// [`kill_group`] on the process group `pid` leads, again and again for `window` (every ms for the first 30 ms, then
/// every 10 ms) and at least [`GROUP_MIN_ROUNDS`] times, until the group is gone or `held` turns false; refused for a
/// pid of 1 or below, as [`kill_group`].
///
/// One group kill isn't enough on macOS (#247): it reaches the members it finds, and a process still being created
/// isn't found (`ps` doesn't list it either). When the kill lands on a process in the middle of a `fork` or a
/// `posix_spawn` (the guard starting the command, the command's shell starting a job), the parent dies but the child
/// is still created, and goes on running in the sandbox with no one left to stop it. Once created it's a member, and
/// the next round gets it.
///
/// **A bounded window, not a guarantee.** macOS has no PID namespace to take everything down at once, nor a way to see
/// a process before its creation is done: one that takes longer than `window` to appear escapes the sweep (still
/// inside the sandbox). [`GROUP_LINGER`] is well beyond what was seen. Not "until empty" either: on macOS the unreaped
/// leader (a zombie) still counts as a member, so a group never looks empty while it's held.
///
/// `held` is the caller's: true while it holds the leader unreaped, so the group's id can't be another's. Each round
/// checks it first.
pub fn kill_group_for(pid: u32, window: Duration, held: &AtomicBool) {
    let Some(group) = i32::try_from(pid).ok().filter(|p| *p > 1).map(nix::unistd::Pid::from_raw) else { return };
    let started = Instant::now();
    let mut rounds = 0;
    while (rounds < GROUP_MIN_ROUNDS || started.elapsed() < window) && held.load(Ordering::SeqCst) {
        if stop_and_kill(group) == Err(nix::errno::Errno::ESRCH) {
            return;
        }
        rounds += 1;
        let pause = if started.elapsed() < Duration::from_millis(30) { 1 } else { 10 };
        std::thread::sleep(Duration::from_millis(pause));
    }
}

/// Blocks until `pid`, a child of this process, has exited, **without reaping it**: while it's a zombie its pid (and
/// the id of the group it led) can't be taken by another process, so a [`SandboxBackend::kill`] right after this
/// reaches only what it left behind. Linux: `waitid(WNOWAIT)`; macOS: a kqueue `NOTE_EXIT` (macOS has `WNOWAIT` only
/// on `waitid`, which nix doesn't offer there). False where it can't tell (another platform, or an error), and then
/// it returns at once: the caller must not take that for an exit.
pub fn wait_exited(pid: u32) -> bool {
    let Some(pid) = i32::try_from(pid).ok().filter(|p| *p > 1) else { return false };
    wait_exited_os(pid)
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn wait_exited_os(pid: i32) -> bool {
    use nix::sys::wait::{Id, WaitPidFlag, waitid};
    loop {
        match waitid(Id::Pid(nix::unistd::Pid::from_raw(pid)), WaitPidFlag::WEXITED | WaitPidFlag::WNOWAIT) {
            Err(nix::errno::Errno::EINTR) => continue,
            Ok(_) => return true,
            // ECHILD: reaped by someone else already; it has exited either way.
            Err(nix::errno::Errno::ECHILD) => return true,
            Err(_) => return false,
        }
    }
}

#[cfg(target_os = "macos")]
fn wait_exited_os(pid: i32) -> bool {
    use nix::sys::event::{EvFlags, EventFilter, FilterFlag, KEvent, Kqueue};
    let Ok(kq) = Kqueue::new() else { return false };
    let change =
        KEvent::new(pid as usize, EventFilter::EVFILT_PROC, EvFlags::EV_ADD | EvFlags::EV_ONESHOT, FilterFlag::NOTE_EXIT, 0, 0);
    let mut events = [KEvent::new(0, EventFilter::EVFILT_PROC, EvFlags::empty(), FilterFlag::empty(), 0, 0)];
    let mut changes: &[KEvent] = std::slice::from_ref(&change);
    loop {
        match kq.kevent(changes, &mut events, None) {
            Err(nix::errno::Errno::EINTR) => changes = &[],
            // ESRCH: it has exited already (a zombie can't be watched), and it's ours, so it isn't reaped.
            Err(nix::errno::Errno::ESRCH) => return true,
            Err(_) => return false,
            Ok(0) => changes = &[],
            Ok(_) if events[0].flags().contains(EvFlags::EV_ERROR) => return events[0].data() == nix::libc::ESRCH as isize,
            Ok(_) => return events[0].fflags().contains(FilterFlag::NOTE_EXIT),
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos")))]
fn wait_exited_os(_pid: i32) -> bool {
    false
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

    /// A backend that only counts its clean-ups, its kills, and the kills there had been when it last cleaned up.
    #[derive(Debug)]
    struct Counting(AtomicUsize, AtomicUsize, AtomicUsize);
    impl Counting {
        const fn new() -> Self {
            Self(AtomicUsize::new(0), AtomicUsize::new(0), AtomicUsize::new(0))
        }
    }
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
        fn kill(&self, _: u32) {
            self.1.fetch_add(1, Ordering::SeqCst);
        }
        fn terminate(&self, _: u32) {}
        fn after(&self, _: &SandboxCommand) {
            self.2.store(self.1.load(Ordering::SeqCst), Ordering::SeqCst);
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
        static FINISHED: Counting = Counting::new();
        let mut prepared = Prepared::new(SandboxCommand::default(), &FINISHED);
        prepared.spawned(Some(4242));
        prepared.finish();
        prepared.finish();
        drop(prepared);
        assert_eq!(FINISHED.0.load(Ordering::SeqCst), 1, "finished once, not again on drop");
        assert_eq!(FINISHED.1.load(Ordering::SeqCst), 0, "finished after it ended: nothing to kill");
        static DROPPED: Counting = Counting::new();
        drop(Prepared::new(SandboxCommand::default(), &DROPPED));
        assert_eq!(DROPPED.0.load(Ordering::SeqCst), 1, "a spawn that failed or an early return still cleans up");
        assert_eq!(DROPPED.1.load(Ordering::SeqCst), 0, "never spawned: nothing to kill");
    }

    /// Dropped while its command still runs (a process manager dropped): killed first, then cleaned up.
    #[test]
    fn a_running_command_dropped_is_killed_before_its_clean_up() {
        static RUNNING: Counting = Counting::new();
        let mut prepared = Prepared::new(SandboxCommand::default(), &RUNNING);
        prepared.spawned(Some(4242));
        drop(prepared);
        assert_eq!((RUNNING.1.load(Ordering::SeqCst), RUNNING.0.load(Ordering::SeqCst)), (1, 1));
        assert_eq!(RUNNING.2.load(Ordering::SeqCst), 1, "killed before the clean-up, not after");
    }

    #[test]
    fn the_group_kill_refuses_pids_that_would_reach_too_far() {
        // 0 and 1 would be this test's own group and init's: nothing is sent (this test would die otherwise).
        kill_group(0);
        kill_group(1);
        kill_group(u32::MAX);
        assert!(!wait_exited(0) && !wait_exited(1));
    }

    /// A group whose leader has exited (unreaped), with a job still running and another one forking all the time:
    /// nothing in it survives the sweep, which takes about [`GROUP_SWEEP`].
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn a_group_is_killed_again_and_again_until_nothing_in_it_survives() {
        use std::os::unix::process::CommandExt;
        let dir = std::env::temp_dir().join(format!("marv-sweep-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut leader = std::process::Command::new("bash")
            .args(["-c", "(while :; do (sleep 0.5; touch late) & sleep 0.01; done) & (sleep 0.5; touch late) & sleep 0.3"])
            .current_dir(&dir)
            .process_group(0)
            .spawn()
            .unwrap();
        assert!(wait_exited(leader.id()));
        let started = Instant::now();
        kill_group_for(leader.id(), GROUP_SWEEP, &AtomicBool::new(true));
        // Short, as a sweep after a normal exit must be (it holds the call up), but not bounded tightly: a loaded runner
        // stretches the rounds, which is the point (#269).
        assert!(started.elapsed() < GROUP_LINGER / 3, "{:?}", started.elapsed());
        leader.wait().unwrap();
        std::thread::sleep(Duration::from_millis(800));
        let survived = dir.join("late").exists();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(!survived, "something in the group survived the sweep");
    }

    /// A window that has passed before the first round still gets [`GROUP_MIN_ROUNDS`] rounds (#269): the thread
    /// sweeping may not be scheduled for the whole window.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn a_sweep_makes_its_rounds_even_when_its_window_has_passed() {
        use std::os::unix::process::CommandExt;
        let mut leader = std::process::Command::new("sleep").arg("5").process_group(0).spawn().unwrap();
        let (pid, started) = (leader.id(), Instant::now());
        // Held by this test, unreaped: a zombie leader keeps the group alive, so no round ends it early.
        kill_group(pid);
        assert!(wait_exited(pid));
        let started_sweep = Instant::now();
        kill_group_for(pid, Duration::ZERO, &AtomicBool::new(true));
        let took = started_sweep.elapsed();
        leader.wait().unwrap();
        assert!(started.elapsed() < Duration::from_secs(5));
        // One pause of at least 1 ms after each round but the last.
        assert!(took >= Duration::from_millis(u64::from(GROUP_MIN_ROUNDS - 1)), "{took:?}");
    }

    /// The exit is seen while the child is still a zombie: it hasn't been reaped, so its pid stays taken.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn an_exit_is_seen_without_reaping_the_child() {
        let mut child = std::process::Command::new("sleep").arg("0.2").spawn().unwrap();
        assert!(wait_exited(child.id()));
        assert!(nix::sys::signal::kill(nix::unistd::Pid::from_raw(child.id() as i32), None).is_ok(), "still a zombie");
        assert!(child.wait().unwrap().success());
        // Exited before the watch began: seen all the same.
        let mut quick = std::process::Command::new("true").spawn().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(wait_exited(quick.id()));
        assert!(quick.wait().unwrap().success());
    }
}
