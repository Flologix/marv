//! A lock across Marv processes for one read-change-write of a shared file (`~/.marv/mcp-tokens.json`,
//! `~/.marv/mcp.json`). Ports Marv's `src/file-lock.ts`.
//!
//! Without it, two terminals read the same snapshot and the later write drops what the other just saved.
//! - `<file>.lock`: created with O_EXCL (atomic on every local filesystem), holding a token unique to this hold
//!   (`<pid> <random>`). Its mtime is refreshed every `stale / 3` while the work runs, so only a lock whose process
//!   died stops being touched and turns stale.
//! - `<file>.lock.takeover`: the guard, also O_EXCL, held only for one synchronous check-and-unlink, never across an
//!   await. Removing the lock (a stale takeover, or a holder's own release) happens only under it.
//!
//! Why the guard: "this lock is stale, remove it" is two steps (look, unlink). Without it, a waiter could look at the
//! stale lock, another waiter remove it meanwhile and a third process create a fresh one, and the first unlink the
//! fresh lock while its owner works: two holders. Under the guard nothing can remove the lock between the look and the
//! unlink, and nothing can create one either (it still exists), so the look is still true when we act. The same goes
//! for releasing: a holder unlinks the lock only if it still holds its own token, under the guard, so it never removes
//! a lock that was taken over and recreated by someone else.
//!
//! The guard itself is taken over when its mtime is older than 3 × `stale`: that only happens if a process died
//! inside a few microseconds of synchronous code. (That takeover is the plain look-and-unlink, so two waiters racing
//! over a dead guard could, in theory, both get it; it needs a crash in that instant first.)
//!
//! In this process, calls for one file are queued, so they don't poll against each other.
//!
//! marv-rs: a hold whose future is dropped mid-work (a cancelled turn) still releases its lock, from a spawned task;
//! Marv's `finally` covered the same case for a rejected promise.

use std::collections::HashMap;
use std::ffi::OsString;
use std::fs::{FileTimes, OpenOptions};
use std::future::Future;
use std::io::{ErrorKind, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant, SystemTime};

use crate::private_file::{random_hex_bytes, random_u64};

/// A lock untouched this long belongs to a process that died.
pub const STALE_LOCK: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LockOptions {
    /// A lock untouched for this long belongs to a process that died (holders refresh it every `stale / 3`).
    pub stale: Duration,
    /// How long to wait for another process's lock before giving up: long enough to outlast a stale one.
    pub wait: Duration,
}

impl LockOptions {
    /// `stale`, waiting `stale` + 5 s (Marv's default `waitMs`).
    pub fn stale(stale: Duration) -> Self {
        Self { stale, wait: stale + Duration::from_secs(5) }
    }
}

impl Default for LockOptions {
    fn default() -> Self {
        Self::stale(STALE_LOCK)
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

/// Per-file queues in this process: a lock per (absolute) path, dropped once nobody holds or waits on it.
static QUEUES: LazyLock<Mutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>> = LazyLock::new(Default::default);

/// Runs `work` holding `<path>.lock`: one holder at a time, in this process and across processes.
pub async fn with_file_lock<T, F, Fut>(path: &Path, options: LockOptions, work: F) -> Result<T, LockError>
where
    F: FnOnce() -> Fut + Send,
    Fut: Future<Output = T> + Send,
{
    let key = std::path::absolute(path)?;
    let queue = QUEUES.lock().unwrap_or_else(|e| e.into_inner()).entry(key.clone()).or_default().clone();
    let result = {
        let _held = queue.lock().await;
        hold(&key, options, work).await
    };
    // Forget the file once nobody else queues on it, so the map doesn't grow.
    let mut queues = QUEUES.lock().unwrap_or_else(|e| e.into_inner());
    if Arc::strong_count(&queue) <= 2 {
        queues.remove(&key);
    }
    result
}

/// `<path><suffix>` (`f.json` → `f.json.lock`).
fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut s = OsString::from(path.as_os_str());
    s.push(suffix);
    PathBuf::from(s)
}

/// Sleeps `min` plus up to `spread` (jitter, so waiters don't poll in step).
async fn jitter(min_ms: u64, spread_ms: u64) {
    tokio::time::sleep(Duration::from_millis(min_ms + random_u64() % spread_ms.max(1))).await;
}

/// Holds the lock across processes (no in-process queue: [`with_file_lock`] queues first).
async fn hold<T, F, Fut>(path: &Path, options: LockOptions, work: F) -> Result<T, LockError>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = T>,
{
    let LockOptions { stale, wait } = options;
    let lock = with_suffix(path, ".lock");
    if let Some(dir) = path.parent() {
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    }
    let token = format!("{} {}", std::process::id(), random_hex_bytes(9));
    let started = Instant::now();
    while !create(&lock, &token)? {
        take_over_stale(&lock, stale).await?;
        if started.elapsed() > wait {
            return Err(LockError::Busy(lock.display().to_string()));
        }
        jitter(5, 20).await;
    }
    let mut held = Held { lock, token, stale, heartbeat: None, released: false };
    // A slow update (a slow disk, a busy machine) mustn't look like a dead one.
    let (beat_lock, beat_token) = (held.lock.clone(), held.token.clone());
    held.heartbeat = Some(tokio::spawn(async move {
        let mut every = tokio::time::interval(stale / 3);
        every.tick().await; // the first tick is now
        loop {
            every.tick().await;
            touch_if_ours(&beat_lock, &beat_token);
        }
    }));
    let out = work().await;
    held.release().await?;
    Ok(out)
}

/// A hold in progress: stops the heartbeat, and releases the lock if the work never finished (its future dropped).
struct Held {
    lock: PathBuf,
    token: String,
    stale: Duration,
    heartbeat: Option<tokio::task::JoinHandle<()>>,
    released: bool,
}

impl Held {
    async fn release(&mut self) -> std::io::Result<()> {
        if let Some(beat) = self.heartbeat.take() {
            beat.abort();
        }
        self.released = true;
        release_lock(&self.lock, &self.token, self.stale).await
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        if let Some(beat) = self.heartbeat.take() {
            beat.abort();
        }
        if !self.released
            && let Ok(runtime) = tokio::runtime::Handle::try_current()
        {
            let (lock, token, stale) = (self.lock.clone(), std::mem::take(&mut self.token), self.stale);
            runtime.spawn(async move { release_lock(&lock, &token, stale).await });
        }
    }
}

/// Creates the lock with our token, or false when another holder has it.
fn create(lock: &Path, token: &str) -> std::io::Result<bool> {
    let mut file = match OpenOptions::new().write(true).create_new(true).mode(0o600).open(lock) {
        Ok(file) => file,
        Err(e) if e.kind() == ErrorKind::AlreadyExists => return Ok(false),
        Err(e) => return Err(e),
    };
    // Empty until written: harmless, staleness goes by mtime and only the holder compares its token.
    file.write_all(token.as_bytes())?;
    Ok(true)
}

/// Whether `path`'s mtime is older than `stale` (an error when it's gone).
fn is_stale(path: &Path, stale: Duration) -> std::io::Result<bool> {
    let modified = std::fs::metadata(path)?.modified()?;
    Ok(SystemTime::now().duration_since(modified).is_ok_and(|age| age > stale))
}

fn touch_if_ours(lock: &Path, token: &str) {
    // Gone or not ours: release will find out.
    if std::fs::read_to_string(lock).is_ok_and(|t| t == token)
        && let Ok(file) = OpenOptions::new().write(true).open(lock)
    {
        let now = SystemTime::now();
        let _ = file.set_times(FileTimes::new().set_accessed(now).set_modified(now));
    }
}

/// Runs `work` (synchronous: the guard is never held across an await) holding `<lock>.takeover`; `None` when the
/// guard couldn't be had in time.
async fn under_guard<R>(lock: &Path, stale: Duration, work: impl FnOnce() -> R) -> std::io::Result<Option<R>> {
    let guard = with_suffix(lock, ".takeover");
    let guard_stale = stale * 3;
    let started = Instant::now();
    loop {
        match OpenOptions::new().write(true).create_new(true).mode(0o600).open(&guard) {
            Ok(_) => break,
            Err(e) if e.kind() == ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
        // Released meanwhile: try again.
        if is_stale(&guard, guard_stale).unwrap_or(false) {
            let _ = std::fs::remove_file(&guard);
        }
        if started.elapsed() > guard_stale + stale {
            return Ok(None);
        }
        jitter(1, 4).await;
    }
    let out = work();
    // Taken over as stale: nothing to release.
    let _ = std::fs::remove_file(&guard);
    Ok(Some(out))
}

/// Removes the lock if it is stale, deciding under the guard: true when it removed one. A lock that is fresh by then
/// (the stale one was already taken over and someone created a new one) is left alone.
pub async fn take_over_stale(lock: &Path, stale: Duration) -> std::io::Result<bool> {
    // The cheap look first: a live lock is just waited for (and one released meanwhile is no lock).
    if !is_stale(lock, stale).unwrap_or(false) {
        return Ok(false);
    }
    let removed =
        under_guard(lock, stale, || is_stale(lock, stale).unwrap_or(false) && std::fs::remove_file(lock).is_ok()).await?;
    Ok(removed.unwrap_or(false))
}

/// Releases the lock only if it still holds `token`. If our lock was taken over (we looked dead) and someone else holds
/// it now, theirs stays. Without the guard the lock stays too: it goes stale and is taken over.
pub async fn release_lock(lock: &Path, token: &str, stale: Duration) -> std::io::Result<()> {
    under_guard(lock, stale, || {
        // Already gone: nothing to do.
        if std::fs::read_to_string(lock).is_ok_and(|t| t == token) {
            let _ = std::fs::remove_file(lock);
        }
    })
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Ported from tests/file-lock.test.ts (and tests/fixtures/file-lock-contender.ts).

    /// A fresh folder, removed when the returned guard drops: the caller keeps it for the whole test (#89: these
    /// were never removed, and a test run left hundreds of them in /tmp).
    fn dir() -> tempfile::TempDir {
        tempfile::Builder::new().prefix("marv-lock-").tempdir().unwrap()
    }

    fn age(path: &Path, ago: Duration) {
        let then = SystemTime::now() - ago;
        let file = OpenOptions::new().write(true).open(path).unwrap();
        file.set_times(FileTimes::new().set_accessed(then).set_modified(then)).unwrap();
    }

    fn read(path: &Path) -> String {
        std::fs::read_to_string(path).unwrap()
    }

    #[tokio::test]
    async fn held_during_the_work_gone_after_it_also_when_the_work_fails() {
        let tmp = dir();
        let path = tmp.path().join("f.json");
        let lock = with_suffix(&path, ".lock");
        with_file_lock(&path, LockOptions::default(), || async {
            let token = read(&lock);
            let (pid, random) = token.split_once(' ').unwrap();
            assert_eq!(pid, std::process::id().to_string());
            assert!(random.len() == 18 && random.chars().all(|c| c.is_ascii_hexdigit()));
        })
        .await
        .unwrap();
        assert!(!lock.exists());
        let failed: Result<Result<(), &str>, _> = with_file_lock(&path, LockOptions::default(), || async { Err("boom") }).await;
        assert_eq!(failed.unwrap(), Err("boom"));
        assert!(!lock.exists());
    }

    #[tokio::test]
    async fn a_dropped_hold_still_releases_its_lock() {
        let tmp = dir();
        let path = tmp.path().join("f.json");
        let lock = with_suffix(&path, ".lock");
        let hold = with_file_lock(&path, LockOptions::default(), std::future::pending::<()>);
        assert!(tokio::time::timeout(Duration::from_millis(50), hold).await.is_err());
        for _ in 0..100 {
            if !lock.exists() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("the lock was left behind");
    }

    #[tokio::test]
    async fn release_never_removes_another_holders_lock() {
        let tmp = dir();
        let path = tmp.path().join("f.json");
        let lock = with_suffix(&path, ".lock");
        with_file_lock(&path, LockOptions::default(), || async {
            // This hold looked dead: it was taken over, and another process holds the lock now.
            std::fs::write(&lock, "222 someone-else").unwrap();
        })
        .await
        .unwrap();
        assert_eq!(read(&lock), "222 someone-else");
        // And release_lock itself checks the token.
        release_lock(&lock, "111 mine", STALE_LOCK).await.unwrap();
        assert_eq!(read(&lock), "222 someone-else");
        release_lock(&lock, "222 someone-else", STALE_LOCK).await.unwrap();
        assert!(!lock.exists());
    }

    #[tokio::test]
    async fn a_slow_holder_keeps_its_lock_fresh_so_it_isnt_taken_over_as_stale() {
        let tmp = dir();
        let path = tmp.path().join("f.json");
        let options = LockOptions::stale(Duration::from_millis(120));
        let order = Arc::new(Mutex::new(Vec::new()));
        let slow = {
            let (path, order) = (path.clone(), order.clone());
            tokio::spawn(async move {
                with_file_lock(&path, options, || async {
                    order.lock().unwrap().push("slow in");
                    tokio::time::sleep(Duration::from_millis(400)).await;
                    order.lock().unwrap().push("slow out");
                })
                .await
                .unwrap();
                Instant::now()
            })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        // A second Marv process: same file, its own queue (none: `hold` directly), so it really contends for the lock.
        let other = hold(&path, options, || async { Instant::now() }).await.unwrap();
        let out = slow.await.unwrap();
        assert!(other >= out, "the other holder got in while the slow one was still working");
        assert_eq!(*order.lock().unwrap(), ["slow in", "slow out"]);
    }

    #[tokio::test]
    async fn a_stale_lock_is_removed_a_fresh_one_is_left_alone() {
        let tmp = dir();
        let lock = tmp.path().join("f.json.lock");
        std::fs::write(&lock, "111 dead").unwrap();
        age(&lock, Duration::from_secs(60));
        assert!(take_over_stale(&lock, Duration::from_secs(1)).await.unwrap());
        assert!(!lock.exists());
        std::fs::write(&lock, "222 alive").unwrap();
        assert!(!take_over_stale(&lock, Duration::from_secs(1)).await.unwrap());
        assert_eq!(read(&lock), "222 alive");
    }

    #[tokio::test]
    async fn a_lock_created_by_a_third_contender_during_a_takeover_is_left_alone() {
        let tmp = dir();
        let lock = tmp.path().join("f.json.lock");
        let guard = with_suffix(&lock, ".takeover");
        std::fs::write(&lock, "111 dead").unwrap();
        age(&lock, Duration::from_secs(60));
        // Another waiter is in the middle of taking the stale lock over (it holds the guard)...
        std::fs::write(&guard, "").unwrap();
        // ...when this one sees the lock is stale too.
        let ours = {
            let lock = lock.clone();
            tokio::spawn(async move { take_over_stale(&lock, Duration::from_secs(1)).await.unwrap() })
        };
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(read(&lock), "111 dead"); // nothing happens without the guard
        // The other waiter removes the stale lock and lets go; a third process takes the lock, fresh.
        std::fs::remove_file(&lock).unwrap();
        std::fs::write(&lock, "333 fresh").unwrap();
        std::fs::remove_file(&guard).unwrap();
        assert!(!ours.await.unwrap());
        assert_eq!(read(&lock), "333 fresh");
    }

    #[tokio::test]
    async fn a_guard_left_by_a_process_that_died_is_taken_over_after_its_own_longer_timeout() {
        let tmp = dir();
        let lock = tmp.path().join("f.json.lock");
        let guard = with_suffix(&lock, ".takeover");
        std::fs::write(&lock, "111 dead").unwrap();
        age(&lock, Duration::from_secs(60));
        std::fs::write(&guard, "").unwrap();
        age(&guard, Duration::from_secs(60));
        assert!(take_over_stale(&lock, Duration::from_secs(1)).await.unwrap());
        assert!(!lock.exists());
        assert!(!guard.exists());
    }

    /// A contender of the next test: `<path> <count> <stale ms>` takes the lock on `<path>` `<count>` times. While
    /// holding it, it creates `<path>.holder` with O_EXCL: if another process holds the lock at the same moment, that
    /// fails and this process fails. Every few rounds it plays a crashed holder instead: it leaves a lock behind that
    /// is already stale, so the others keep taking stale locks over while live ones come and go.
    const CONTENDER: &str = "MARV_FS_TEST_CONTENDER";

    #[tokio::test]
    #[ignore = "a child process of several_processes_with_crashed_holders_never_hold_at_once"]
    async fn file_lock_contender_child() {
        let Ok(spec) = std::env::var(CONTENDER) else { return };
        let mut args = spec.split(' ');
        let (path, count, stale) = (PathBuf::from(args.next().unwrap()), args.next().unwrap(), args.next().unwrap());
        let options = LockOptions { stale: Duration::from_millis(stale.parse().unwrap()), wait: Duration::from_secs(20) };
        let (lock, holder) = (with_suffix(&path, ".lock"), with_suffix(&path, ".holder"));
        for i in 0..count.parse::<u32>().unwrap() {
            if i % 4 == 3 {
                // Someone holds it: no crash this round.
                if create(&lock, &format!("dead {} {i}", std::process::id())).unwrap() {
                    age(&lock, Duration::from_secs(60));
                }
            }
            with_file_lock(&path, options, || async {
                // Fails if another holder is inside.
                OpenOptions::new().write(true).create_new(true).open(&holder).expect("two holders at once");
                jitter(10, 20).await;
                std::fs::remove_file(&holder).unwrap();
            })
            .await
            .unwrap();
        }
    }

    #[test]
    fn several_processes_with_crashed_holders_never_hold_at_once() {
        let tmp = dir();
        let path = tmp.path().join("f.json");
        let children: Vec<_> = (0..4)
            .map(|_| {
                std::process::Command::new(std::env::current_exe().unwrap())
                    .args(["file_lock::tests::file_lock_contender_child", "--exact", "--ignored", "--quiet"])
                    .env(CONTENDER, format!("{} 16 150", path.display()))
                    .stdout(std::process::Stdio::null())
                    .spawn()
                    .unwrap()
            })
            .collect();
        for mut child in children {
            assert!(child.wait().unwrap().success());
        }
        assert!(!with_suffix(&path, ".holder").exists());
        assert!(!with_suffix(&path, ".lock.takeover").exists());
    }
}
