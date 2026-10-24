//! Per-file serialisation of read-change-write, in this process and across Marv processes.
//!
//! Ports Marv's `src/tools/file-lock.ts` (the in-process queue; the cross-process `src/file-lock.ts` with O_EXCL lock
//! files belongs to marv-session's private files).
//!
//! `write_file` and `edit_file` read a file, change it, and write the whole file back. Two calls on one file at the
//! same time (parallel tool calls, subagents sharing the project folder) would both read the same original, and the
//! later write would drop the earlier one's change: two appends would keep only one. Each file's read-change-write runs
//! after the previous one on the same path has finished. Entries are forgotten once nothing is queued behind them.
//!
//! marv-rs: the same holds across processes (two Marv sessions in one folder, a TUI and the desktop). Inside the queue
//! a call also takes an advisory lock (`flock`) on a per-file lock file in `ToolContext::file_locks` (the runtime
//! folder's `file-locks/`, or `~/.marv/file-locks/` where there's no runtime folder), so the edit tools' check that
//! the file is still as the session last saw it ([`crate::file_versions`]) and their write are one step: two writers
//! can't both pass the check.
//! - Both locks are keyed on the file's real path ([`lock_key`]), after the path is resolved like the tools resolve it
//!   (`~/`, `..`, symlinks): every spelling of one file is one lock. On a case-insensitive filesystem (macOS, Windows)
//!   the file's name is case-folded, so `A.txt` and `a.txt` are one lock too.
//! - Held for one call's check, write and format only, never across a model step or a turn, and released when the
//!   call ends however it ends (the lock goes with the file handle, and the OS drops it when a process dies, so
//!   there are no stale locks to take over). Waiting for it ends at Esc.
//! - The runtime folder and `~/.marv` are hidden in every sandbox, so a sandboxed command can't hold a lock to stall
//!   the edit tools.
//! - A lock file is named after the hash of the lock key. Lock files are never removed: unlinking one while a waiter
//!   has it open would let two holders lock two different files. Each is an empty file, one per file path ever
//!   written. The runtime folder is cleared at logout or reboot; `~/.marv/file-locks/` keeps them.
//! - Waits up to [`LOCK_WAIT`] (longer than the formatter's timeout) and then fails the call, writing nothing.
//! - Without a lock folder (`file_locks` None), only the in-process queue applies.
//! - Known limits: two hard links to one file are two keys (and two locks); and two Marv processes with different lock
//!   folders (a different `MARV_RUNTIME_DIR`, or one with a runtime folder and one without) don't see each other's
//!   locks. The content check ([`crate::file_versions`]) still catches the change, except in the instant between one's
//!   check and its write.

use std::collections::HashMap;
use std::fs::{File, OpenOptions, TryLockError};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use marv_sandbox::paths::normalize;

use crate::types::{ToolContext, ToolError};

/// How long a call waits for another process's lock on the same file: longer than one call's write and format.
pub const LOCK_WAIT: Duration = Duration::from_secs(20);
const LOCK_POLL: Duration = Duration::from_millis(25);

/// The runtime folder's subfolder the lock files live in.
pub const LOCK_FOLDER: &str = "file-locks";

fn locks() -> &'static Mutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>> {
    static LOCKS: OnceLock<Mutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>> = OnceLock::new();
    LOCKS.get_or_init(Default::default)
}

/// A file's real path, also when it doesn't exist yet (its folder's real path and its name): every spelling of one
/// file, symlinks included, is one key.
pub fn real_path(path: &Path) -> PathBuf {
    let path = normalize(path);
    if let Ok(real) = std::fs::canonicalize(&path) {
        return real;
    }
    match (path.parent().and_then(|p| std::fs::canonicalize(p).ok()), path.file_name()) {
        (Some(parent), Some(name)) => parent.join(name),
        _ => path,
    }
}

/// Whether file names are case-insensitive here (the platform's default filesystem: APFS, NTFS).
const CASE_INSENSITIVE: bool = cfg!(any(target_os = "macos", windows));

/// What both locks are keyed on: the real path, its file name case-folded where names are case-insensitive (a new
/// file has no real spelling yet, so `A.txt` and `a.txt` would otherwise be two locks on one file).
pub fn lock_key(path: &Path) -> PathBuf {
    let real = real_path(path);
    match (CASE_INSENSITIVE, real.parent(), real.file_name()) {
        (true, Some(parent), Some(name)) => parent.join(name.to_string_lossy().to_lowercase()),
        _ => real,
    }
}

/// The lock file for `file` (an absolute path) in the lock folder `dir`.
pub fn lock_path(dir: &Path, file: &Path) -> PathBuf {
    let hash = crate::file_versions::content_hash(lock_key(file).as_os_str().as_encoded_bytes());
    let name: String = hash[..16].iter().map(|b| format!("{b:02x}")).collect();
    dir.join(format!("{name}.lock"))
}

/// Opens (or makes) the lock file and waits for this process's lock on it.
async fn lock_across_processes(ctx: &ToolContext, dir: &Path, file: &Path, shown: &str) -> Result<File, ToolError> {
    let failed = |e: std::io::Error| ToolError(format!("Couldn't lock {shown} against other Marv processes: {e}"));
    let path = lock_path(dir, file);
    #[cfg(unix)]
    let handle = {
        use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir).map_err(failed)?;
        OpenOptions::new().write(true).create(true).truncate(false).mode(0o600).open(&path).map_err(failed)?
    };
    #[cfg(not(unix))]
    let handle = {
        std::fs::create_dir_all(dir).map_err(failed)?;
        OpenOptions::new().write(true).create(true).truncate(false).open(&path).map_err(failed)?
    };
    let deadline = Instant::now() + LOCK_WAIT;
    loop {
        match handle.try_lock() {
            Ok(()) => return Ok(handle),
            Err(TryLockError::WouldBlock) if Instant::now() < deadline => {
                tokio::select! {
                    () = tokio::time::sleep(LOCK_POLL) => {}
                    () = ctx.cancel.cancelled() => {
                        return Err(ToolError(crate::edit_file::INTERRUPTED_BEFORE_WRITE.into()));
                    }
                }
            }
            Err(TryLockError::WouldBlock) => {
                return Err(ToolError(format!(
                    "{shown} is being changed by another Marv process, so nothing was written. Try again in a moment."
                )));
            }
            Err(TryLockError::Error(e)) => return Err(failed(e)),
        }
    }
}

/// Runs `f` after every earlier `with_file_lock` on the same file has finished, holding the cross-process lock on it
/// when `ctx.file_locks` names a folder. `path` is resolved as the tools resolve it (a path outside the project fails
/// here with their error). Fails, without running `f`, when the path is refused or the lock can't be had (Esc
/// included).
pub async fn with_file_lock<T, F, Fut>(ctx: &ToolContext, path: &str, f: F) -> Result<T, ToolError>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<T, ToolError>>,
{
    let key = lock_key(&crate::files::resolve_in_project(ctx, path)?);
    let lock = locks().lock().unwrap_or_else(|e| e.into_inner()).entry(key.clone()).or_default().clone();
    let result = {
        let _held = lock.lock().await;
        match &ctx.file_locks {
            Some(dir) => match lock_across_processes(ctx, dir, &key, path).await {
                // Released when this arm ends: the call's check, write and format, and nothing after.
                Ok(_across) => f().await,
                Err(e) => Err(e),
            },
            None => f().await,
        }
    };
    // Forget the path once nothing else holds or waits for it (the map's copy and ours).
    let mut map = locks().lock().unwrap_or_else(|e| e.into_inner());
    if Arc::strong_count(&lock) == 2 {
        map.remove(&key);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempDir;

    #[tokio::test]
    async fn calls_on_one_path_run_one_after_another() {
        let order = Arc::new(Mutex::new(Vec::new()));
        let root = TempDir::new("lock-order");
        let ctx = ToolContext::new(root.path());
        let a = {
            let order = order.clone();
            with_file_lock(&ctx, "a.txt", move || async move {
                tokio::time::sleep(Duration::from_millis(20)).await;
                order.lock().unwrap().push("first");
                Ok(())
            })
        };
        let b = {
            let order = order.clone();
            with_file_lock(&ctx, "./a.txt", move || async move {
                order.lock().unwrap().push("second");
                Ok(())
            })
        };
        let (a, b) = tokio::join!(a, b);
        assert!(a.is_ok() && b.is_ok());
        assert_eq!(*order.lock().unwrap(), ["first", "second"]);
        assert!(!locks().lock().unwrap().contains_key(&root.path().join("a.txt")));
    }

    #[tokio::test]
    async fn a_call_waits_for_another_handles_lock_and_releases_its_own() {
        let root = TempDir::new("lock-across");
        let dir = root.path().join("locks");
        let mut ctx = ToolContext::new(root.path());
        ctx.file_locks = Some(dir.clone());
        let file = root.path().join("a.txt");
        // Another process's lock: a second open file handle conflicts with flock exactly as another process would.
        std::fs::create_dir_all(&dir).unwrap();
        let other = File::create(lock_path(&dir, &file)).unwrap();
        other.lock().unwrap();
        let ran = Arc::new(Mutex::new(false));
        let call = {
            let ran = ran.clone();
            with_file_lock(&ctx, "a.txt", move || async move {
                *ran.lock().unwrap() = true;
                Ok(())
            })
        };
        let release = async {
            tokio::time::sleep(Duration::from_millis(100)).await;
            assert!(!*ran.lock().unwrap(), "ran while another handle held the lock");
            other.unlock().unwrap();
        };
        let (result, ()) = tokio::join!(call, release);
        assert!(result.is_ok() && *ran.lock().unwrap());
        // Nothing holds it once the call is over. (Within a moment: a command another test starts meanwhile shares
        // the lock's open file between its fork and its exec, which closes it.)
        let lock = File::open(lock_path(&dir, &file)).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while lock.try_lock().is_err() {
            assert!(Instant::now() < deadline, "the call kept its lock");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn every_spelling_of_a_file_has_one_lock() {
        let root = TempDir::new("lock-path");
        root.write("a.txt", "x");
        let dir = Path::new("/locks");
        assert_eq!(lock_path(dir, &root.path().join("a.txt")), lock_path(dir, &root.path().join("x/../a.txt")));
        assert_eq!(lock_path(dir, &root.path().join("new.txt")), lock_path(dir, &root.path().join("./new.txt")));
        assert_ne!(lock_path(dir, &root.path().join("a.txt")), lock_path(dir, &root.path().join("new.txt")));
    }

    #[tokio::test]
    async fn esc_ends_the_wait_for_another_processs_lock() {
        let root = TempDir::new("lock-esc");
        let dir = root.path().join("locks");
        let mut ctx = ToolContext::new(root.path());
        ctx.file_locks = Some(dir.clone());
        std::fs::create_dir_all(&dir).unwrap();
        let other = File::create(lock_path(&dir, &root.path().join("a.txt"))).unwrap();
        other.lock().unwrap();
        let cancel = ctx.cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            cancel.cancel();
        });
        let started = Instant::now();
        let out = with_file_lock(&ctx, "a.txt", || async { Ok(()) }).await;
        assert_eq!(out.unwrap_err().0, crate::edit_file::INTERRUPTED_BEFORE_WRITE);
        assert!(started.elapsed() < Duration::from_secs(5), "waited past Esc");
    }

    #[tokio::test]
    async fn a_home_spelling_of_a_project_file_is_the_same_lock() {
        let home = TempDir::new("lock-home");
        home.write("proj/a.txt", "x");
        let root = home.path().join("proj");
        let ctx = ToolContext { home: home.path().to_path_buf(), ..ToolContext::new(&root) };
        let key = lock_key(&crate::files::resolve_in_project(&ctx, "~/proj/a.txt").unwrap());
        assert_eq!(key, lock_key(&crate::files::resolve_in_project(&ctx, "a.txt").unwrap()));
        assert!(with_file_lock(&ctx, "../outside.txt", || async { Ok(()) }).await.is_err(), "refused like the tools");
    }

    #[cfg(any(target_os = "macos", windows))]
    #[test]
    fn a_new_files_name_is_case_folded() {
        let root = TempDir::new("lock-case");
        assert_eq!(lock_key(&root.path().join("New.TXT")), lock_key(&root.path().join("new.txt")));
    }
}
