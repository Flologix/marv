//! A lock across Marv processes for one read-change-write of a shared file (`~/.marv/mcp-tokens.json`,
//! `~/.marv/mcp.json`). Ports Marv's `src/file-lock.ts`.
//!
//! Without it, two terminals read the same snapshot and the later write drops what the other just saved.
//! - `<file>.lock`: created with O_EXCL, holding a token unique to this hold (`<pid> <random>`); its mtime is
//!   refreshed every `stale_ms / 3` while the work runs, so only a lock whose process died turns stale.
//! - `<file>.lock.takeover`: the guard, also O_EXCL, held only for one synchronous check-and-unlink. "This lock is
//!   stale, remove it" is two steps; without the guard a waiter could unlink a *fresh* lock another process just
//!   made, and two would hold it. A holder releases only if the lock still holds its own token, under the guard.
//! - The guard itself is taken over when older than 3 × `stale_ms` (a crash within microseconds of sync code).
//! - In this process, calls for one file are queued, so they don't poll against each other.
//!
//! Port note: marv-mcp needs this (`oauth_store::update`, `personal_file`), and can't depend on marv-session. It
//! should move, with `private_file`, to a small crate both use; until then the body is todo.

use std::future::Future;
use std::path::Path;
use std::time::Duration;

/// A lock untouched this long belongs to a process that died.
pub const STALE_LOCK: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy)]
pub struct LockOptions {
    pub stale: Duration,
    /// How long to wait for another process's lock: long enough to outlast a stale one (stale + 5 s).
    pub wait: Duration,
}

impl Default for LockOptions {
    fn default() -> Self {
        Self { stale: STALE_LOCK, wait: STALE_LOCK + Duration::from_secs(5) }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum LockError {
    /// `"<lock> is held by another Marv process: try again in a moment."`
    #[error("{0} is held by another Marv process: try again in a moment.")]
    Busy(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Runs `work` holding `<path>.lock`: one holder at a time, in this process and across processes.
pub async fn with_file_lock<T, F, Fut>(_path: &Path, _options: LockOptions, _work: F) -> Result<T, LockError>
where
    F: FnOnce() -> Fut + Send,
    Fut: Future<Output = T> + Send,
{
    todo!("port: withFileLock (in-process queue per path, O_EXCL lock with token, heartbeat, guarded takeover/release)")
}
