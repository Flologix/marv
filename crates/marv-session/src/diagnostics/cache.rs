//! The check cache (marv-rs, #187; `docs/specs/2026-10-25-lsp-checker.md` §6.3): a writable folder per repository
//! for the checkers that keep a build cache (cargo's target dir, go's build cache). New in marv-rs: TypeScript Marv
//! has only tsc, which needs none.
//!
//! - **Where:** `<root>/<key>/`, the root being `$XDG_CACHE_HOME/marv/checks` (default `~/.cache/marv/checks`) on
//!   Linux and `~/Library/Caches/marv/checks` on macOS ([`default_root`]).
//! - **The key is per repository** ([`repo_key`]): a worktree's is its repository's common git folder, read from its
//!   `.git` file and `commondir` the way `git_guard::worktree_git_dirs` checks them, without running git; anything else
//!   (no `.git`, a forged one) is the project's own path key. Worktrees of one repository share the dependencies' build.
//! - **Mounted for one check only, exactly `<root>/<key>`** (never `<root>`): a check can write only its own key folder.
//!   The lock (`<key>.lock`) and the record (`<key>.json`: when it was last used, the projects that used it, its size)
//!   sit in `<root>`, outside the mount. The root and each key folder are created 0700, and a key folder that is a
//!   symlink, isn't a folder or isn't the user's is refused ([`CheckCache::key_dir`]).
//! - **Bounded:** [`MAX_CHECK_CACHE`] in all with the least recently used evicted first (never one whose lock is held),
//!   [`MAX_KEY_CACHE`] per key (more turns that checker off and deletes the folder), no run below [`MIN_FREE`] or
//!   [`MIN_FREE_PERCENT`] free, and pruning of folders unused for [`MAX_AGE`] or whose projects are all gone.
//! - **No symlink is followed**, by sizing, pruning or clearing: a sandboxed build script can plant `target/x -> /`.
//!   Sizing counts a link as itself and never descends into one; deleting first checks that the key folder is a real
//!   folder directly under the root, then removes it with `std::fs::remove_dir_all`, which unlinks links rather than
//!   following them (a test pins it).
//! - **Nothing runs on the host:** every step here only stats, reads, writes and deletes files (`docs/SECURITY.md`).

use std::fs::{DirBuilder, File, OpenOptions};
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

const GB: u64 = 1024 * 1024 * 1024;
/// All key folders together; past it, the least recently used go first.
pub const MAX_CHECK_CACHE: u64 = 20 * GB;
/// One key folder; past it, that checker is off for the session and the folder is deleted (a build script filling
/// its cache on purpose included).
pub const MAX_KEY_CACHE: u64 = 10 * GB;
/// No warm-up or check starts with less free than this on the cache's filesystem (a full disk stalls everything else
/// on the machine, btrfs especially)…
pub const MIN_FREE: u64 = 5 * GB;
/// …or less than this percentage of it.
pub const MIN_FREE_PERCENT: u64 = 5;
/// Key folders unused this long are pruned.
pub const MAX_AGE: Duration = Duration::from_secs(14 * 24 * 60 * 60);
/// A key folder is sized at most this often (a big target dir has hundreds of thousands of files).
pub const SIZE_EVERY: Duration = Duration::from_secs(60);
/// The project roots a key's record remembers (for orphan pruning).
const MAX_ROOTS: usize = 32;

/// A filesystem's free and total bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Space {
    pub available: u64,
    pub total: u64,
}

/// How free space is learned: `statvfs` ([`statvfs`]); tests pass a stand-in.
pub type FreeSpace = Arc<dyn Fn(&Path) -> Option<Space> + Send + Sync>;

/// The real one.
#[allow(clippy::unnecessary_cast)] // the field types differ between Linux and macOS
pub fn statvfs(path: &Path) -> Option<Space> {
    let s = nix::sys::statvfs::statvfs(path).ok()?;
    let block = s.fragment_size() as u64;
    Some(Space { available: s.blocks_available() as u64 * block, total: s.blocks() as u64 * block })
}

/// The cache root for this platform: `~/Library/Caches/marv/checks` on macOS; `$XDG_CACHE_HOME/marv/checks` (when
/// it's an absolute path) or `~/.cache/marv/checks` elsewhere.
pub fn default_root(xdg_cache_home: Option<&str>, home: &Path) -> PathBuf {
    if cfg!(target_os = "macos") {
        return home.join("Library/Caches/marv/checks");
    }
    let base = xdg_cache_home.map(PathBuf::from).filter(|p| p.is_absolute()).unwrap_or_else(|| home.join(".cache"));
    base.join("marv/checks")
}

/// [`default_root`] from Marv's own environment.
pub fn default_root_from_env() -> PathBuf {
    default_root(std::env::var("XDG_CACHE_HOME").ok().as_deref(), &crate::paths::home_dir())
}

/// The cache key of the project at `root`: its repository's, so worktrees share one. Read from the files only, never
/// by running git: `.git` as a real folder is the repository itself; a `.git` file counts only when it points at a
/// worktree record that points back at it ([`marv_sandbox::git_guard::worktree_git_dirs`]). Anything else (no
/// repository, a forged `.git` file, a `.git` symlink) is the project's own path key.
pub fn repo_key(root: &Path) -> String {
    let git = root.join(".git");
    let common = match std::fs::symlink_metadata(&git) {
        Ok(m) if m.file_type().is_dir() => std::fs::canonicalize(&git).ok(),
        Ok(m) if m.file_type().is_file() => marv_sandbox::git_guard::worktree_git_dirs(&git).pop(),
        _ => None,
    };
    match common {
        Some(common) => crate::paths::project_key(&common),
        None => crate::paths::project_key(root),
    }
}

/// The bytes of the files under `path`, links counted as themselves and never followed (folders count nothing).
pub fn size_of(path: &Path) -> u64 {
    let Ok(meta) = std::fs::symlink_metadata(path) else { return 0 };
    if !meta.file_type().is_dir() {
        return meta.len();
    }
    let mut total = 0;
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            // `DirEntry::metadata` doesn't follow a symlink.
            let Ok(meta) = entry.metadata() else { continue };
            if meta.file_type().is_dir() {
                stack.push(entry.path());
            } else {
                total += meta.len();
            }
        }
    }
    total
}

/// `1.2 GB`, `340 MB`, `12 KB`.
pub fn human_size(bytes: u64) -> String {
    let mb = 1024.0 * 1024.0;
    let b = bytes as f64;
    if b >= GB as f64 {
        format!("{:.1} GB", b / GB as f64)
    } else if b >= mb {
        format!("{:.0} MB", b / mb)
    } else {
        format!("{:.0} KB", b / 1024.0)
    }
}

/// A key's lock, held while a check or warm-up uses its folder (and while Marv deletes it). Dropping it unlocks.
#[derive(Debug)]
pub struct KeyLock(#[allow(dead_code)] File);

/// What a key's record says.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Record {
    pub last_used: Option<u64>,
    pub roots: Vec<String>,
    pub size: Option<u64>,
    pub sized_at: Option<u64>,
}

/// The cache: its root, its bounds, and how free space is learned.
pub struct CheckCache {
    pub root: PathBuf,
    pub total_cap: u64,
    pub key_cap: u64,
    pub min_free: u64,
    pub min_free_percent: u64,
    pub max_age: Duration,
    pub size_every: Duration,
    pub free_space: FreeSpace,
    pruned: AtomicBool,
}

impl CheckCache {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            total_cap: MAX_CHECK_CACHE,
            key_cap: MAX_KEY_CACHE,
            min_free: MIN_FREE,
            min_free_percent: MIN_FREE_PERCENT,
            max_age: MAX_AGE,
            size_every: SIZE_EVERY,
            free_space: Arc::new(statvfs),
            pruned: AtomicBool::new(false),
        }
    }

    /// The key's folder (whether it exists or not): what a check mounts writable.
    pub fn dir(&self, key: &str) -> PathBuf {
        self.root.join(key)
    }

    fn record_path(&self, key: &str) -> PathBuf {
        self.root.join(format!("{key}.json"))
    }

    fn lock_path(&self, key: &str) -> PathBuf {
        self.root.join(format!("{key}.lock"))
    }

    /// The key's folder, created 0700 (the root too) when it's missing. Refused, with the reason, when the root or
    /// the folder is a symlink, isn't a folder, or isn't the user's: then the check doesn't run.
    pub fn key_dir(&self, key: &str) -> Result<PathBuf, String> {
        if key.is_empty() || key.contains('/') || key.starts_with('.') {
            return Err(format!("{key} isn't a cache key"));
        }
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&self.root)
            .map_err(|e| format!("couldn't create {}: {e}", self.root.display()))?;
        plain_own_dir(&self.root)?;
        let dir = self.dir(key);
        match DirBuilder::new().mode(0o700).create(&dir) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(format!("couldn't create {}: {e}", dir.display())),
        }
        plain_own_dir(&dir)?;
        Ok(dir)
    }

    /// The free space when it's below the guard ([`MIN_FREE`] or [`MIN_FREE_PERCENT`]), else `None`. Unknown free
    /// space doesn't stop anything.
    pub fn low_on_space(&self) -> Option<Space> {
        let at = if self.root.exists() { self.root.clone() } else { nearest_existing(&self.root) };
        let space = (self.free_space)(&at)?;
        let low = space.available < self.min_free
            || u128::from(space.available) * 100 < u128::from(space.total) * u128::from(self.min_free_percent);
        low.then_some(space)
    }

    /// The key's lock if nobody holds it (another session's check, warm-up or deletion), else `None`. The root must
    /// exist ([`CheckCache::key_dir`] made it).
    pub fn try_lock(&self, key: &str) -> Option<KeyLock> {
        let file = OpenOptions::new().create(true).truncate(false).write(true).open(self.lock_path(key)).ok()?;
        file.try_lock().ok()?;
        Some(KeyLock(file))
    }

    /// The key's lock, waiting at most `wait` for it (or until `cancel`).
    pub async fn lock(&self, key: &str, wait: Duration, cancel: &CancellationToken) -> Option<KeyLock> {
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            if let Some(lock) = self.try_lock(key) {
                return Some(lock);
            }
            if cancel.is_cancelled() || tokio::time::Instant::now() >= deadline {
                return None;
            }
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(50)) => {}
                _ = cancel.cancelled() => return None,
            }
        }
    }

    /// The key's record (empty when there's none or it can't be read).
    pub fn record(&self, key: &str) -> Record {
        let Ok(text) = std::fs::read_to_string(self.record_path(key)) else { return Record::default() };
        let Ok(value) = serde_json::from_str::<Value>(&text) else { return Record::default() };
        Record {
            last_used: value["lastUsed"].as_u64(),
            roots: value["roots"]
                .as_array()
                .map(|a| a.iter().filter_map(|r| r.as_str().map(String::from)).collect())
                .unwrap_or_default(),
            size: value["size"].as_u64(),
            sized_at: value["sizedAt"].as_u64(),
        }
    }

    fn write_record(&self, key: &str, record: &Record) {
        let value = json!({
            "lastUsed": record.last_used,
            "roots": record.roots,
            "size": record.size,
            "sizedAt": record.sized_at,
        });
        // Written beside and renamed over: a reader never sees half a record.
        let tmp = self.root.join(format!("{key}.json.{}", marv_fs::random_hex()));
        if std::fs::write(&tmp, value.to_string()).is_ok() && std::fs::rename(&tmp, self.record_path(key)).is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
    }

    /// A run used the key, for `project`: its record's last use and projects.
    pub fn touch(&self, key: &str, project: &Path, now_ms: u64) {
        let mut record = self.record(key);
        record.last_used = Some(now_ms);
        let project = project.to_string_lossy().into_owned();
        record.roots.retain(|r| *r != project);
        record.roots.push(project);
        let extra = record.roots.len().saturating_sub(MAX_ROOTS);
        record.roots.drain(..extra);
        self.write_record(key, &record);
    }

    /// Whether the key is due for sizing ([`CheckCache::size_every`] since the last).
    pub fn due_for_sizing(&self, key: &str, now_ms: u64) -> bool {
        self.record(key).sized_at.is_none_or(|at| now_ms.saturating_sub(at) >= self.size_every.as_millis() as u64)
    }

    /// Sizes the key's folder and records it. Over [`CheckCache::key_cap`]: deletes it (unless its lock is held) and
    /// says so (`true`).
    pub fn measure(&self, key: &str, now_ms: u64) -> (u64, bool) {
        let size = size_of(&self.dir(key));
        let mut record = self.record(key);
        record.size = Some(size);
        record.sized_at = Some(now_ms);
        self.write_record(key, &record);
        let too_big = size > self.key_cap;
        if too_big {
            self.remove(key);
        }
        (size, too_big)
    }

    /// The key's last recorded size.
    pub fn recorded_size(&self, key: &str) -> Option<u64> {
        self.record(key).size
    }

    /// The key folders: (key, record, size as recorded or measured now, last use or the folder's change time).
    fn keys(&self, now_ms: u64) -> Vec<(String, Record, u64, u64)> {
        let Ok(entries) = std::fs::read_dir(&self.root) else { return Vec::new() };
        let mut keys = Vec::new();
        for entry in entries.flatten() {
            let Ok(meta) = entry.metadata() else { continue };
            // Only real folders: Marv creates nothing else here but the records and locks.
            if !meta.file_type().is_dir() {
                continue;
            }
            let Some(key) = entry.file_name().to_str().map(String::from) else { continue };
            let record = self.record(&key);
            let size = record.size.unwrap_or_else(|| size_of(&entry.path()));
            let changed = meta.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok());
            let used = record.last_used.or(changed.map(|d| d.as_millis() as u64)).unwrap_or(now_ms);
            keys.push((key, record, size, used));
        }
        keys
    }

    /// Evicts the least recently used key folders (never `keep`, never one whose lock is held) until all of them
    /// together are within [`CheckCache::total_cap`]. The keys deleted.
    pub fn evict(&self, keep: &str, now_ms: u64) -> Vec<String> {
        let mut keys = self.keys(now_ms);
        let mut total: u64 = keys.iter().map(|k| k.2).sum();
        keys.sort_by_key(|k| k.3);
        let mut gone = Vec::new();
        for (key, _, size, _) in keys {
            if total <= self.total_cap {
                break;
            }
            if key != keep && self.remove(&key) {
                total = total.saturating_sub(size);
                gone.push(key);
            }
        }
        gone
    }

    /// Prunes key folders unused for [`CheckCache::max_age`], and those whose recorded projects are all gone (deleted
    /// worktrees); never `keep`, never a locked one. The keys deleted.
    pub fn prune(&self, keep: &str, now_ms: u64) -> Vec<String> {
        let max_age = self.max_age.as_millis() as u64;
        let mut gone = Vec::new();
        for (key, record, _, used) in self.keys(now_ms) {
            let old = now_ms.saturating_sub(used) > max_age;
            let orphan = !record.roots.is_empty() && record.roots.iter().all(|r| !Path::new(r).exists());
            if key != keep && (old || orphan) && self.remove(&key) {
                gone.push(key);
            }
        }
        gone
    }

    /// [`CheckCache::prune`], once per cache (a session's first change).
    pub fn prune_once(self: &Arc<Self>, keep: &str) {
        if self.pruned.swap(true, Ordering::SeqCst) {
            return;
        }
        let (cache, keep) = (self.clone(), keep.to_string());
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn_blocking(move || cache.prune(&keep, crate::time::now_ms()));
        }
    }

    /// `/diagnostics clear`: deletes the key's folder. `Ok(Some(bytes))` freed, `Ok(None)` when there was none, an
    /// error when a check is using it or it isn't Marv's plain folder.
    pub fn clear(&self, key: &str) -> Result<Option<u64>, String> {
        let dir = self.dir(key);
        match std::fs::symlink_metadata(&dir) {
            Err(_) => return Ok(None),
            Ok(m) if !m.file_type().is_dir() => {
                return Err(format!("{} isn't a folder; Marv leaves it alone", dir.display()));
            }
            Ok(_) => {}
        }
        let size = size_of(&dir);
        let Some(_lock) = self.try_lock(key) else {
            return Err("a check is using it right now; try again when it's done".into());
        };
        self.remove_locked(key).map(|()| Some(size))
    }

    /// Deletes the key's folder and record if its lock is free; whether it did.
    fn remove(&self, key: &str) -> bool {
        match self.try_lock(key) {
            Some(_lock) => self.remove_locked(key).is_ok(),
            None => false,
        }
    }

    /// Deletes the key's folder (a real folder directly under the root, links inside unlinked, never followed) and
    /// its record. The lock file stays: unlinking one a waiter has open would let two holders lock two files.
    fn remove_locked(&self, key: &str) -> Result<(), String> {
        let dir = self.dir(key);
        if dir.parent() != Some(self.root.as_path()) || key.is_empty() || key.starts_with('.') || key.contains('/') {
            return Err(format!("{} isn't a cache folder", dir.display()));
        }
        match std::fs::symlink_metadata(&dir) {
            Ok(m) if m.file_type().is_dir() => {}
            Ok(_) => return Err(format!("{} isn't a folder", dir.display())),
            Err(_) => return Ok(()),
        }
        std::fs::remove_dir_all(&dir).map_err(|e| format!("couldn't delete {}: {e}", dir.display()))?;
        let _ = std::fs::remove_file(self.record_path(key));
        Ok(())
    }
}

/// Refuses a path that is a symlink, not a folder, or not the user's.
fn plain_own_dir(path: &Path) -> Result<(), String> {
    let meta = std::fs::symlink_metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if meta.file_type().is_symlink() {
        return Err(format!("{} is a symlink", path.display()));
    }
    if !meta.file_type().is_dir() {
        return Err(format!("{} isn't a folder", path.display()));
    }
    if meta.uid() != nix::unistd::getuid().as_raw() {
        return Err(format!("{} isn't yours", path.display()));
    }
    Ok(())
}

/// The nearest existing parent (where free space can be asked before the root exists).
fn nearest_existing(path: &Path) -> PathBuf {
    path.ancestors().find(|p| p.exists()).map_or_else(|| PathBuf::from("/"), Path::to_path_buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checks::Scratch;

    fn cache(dir: &Scratch) -> CheckCache {
        let mut cache = CheckCache::new(dir.0.join("cache/checks"));
        cache.free_space = Arc::new(|_| Some(Space { available: 100 * GB, total: 200 * GB }));
        cache
    }

    fn put(path: &Path, bytes: usize) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, vec![b'x'; bytes]).unwrap();
    }

    #[test]
    fn the_root_follows_the_platform() {
        let home = Path::new("/home/me");
        if cfg!(target_os = "macos") {
            assert_eq!(default_root(Some("/x"), home), PathBuf::from("/home/me/Library/Caches/marv/checks"));
        } else {
            assert_eq!(default_root(None, home), PathBuf::from("/home/me/.cache/marv/checks"));
            assert_eq!(default_root(Some("/xdg"), home), PathBuf::from("/xdg/marv/checks"));
            // A relative XDG_CACHE_HOME is ignored, as the spec says.
            assert_eq!(default_root(Some("rel"), home), PathBuf::from("/home/me/.cache/marv/checks"));
        }
    }

    /// A repository with a worktree whose records point back at each other, as git writes them.
    fn repo_with_worktree(dir: &Scratch) -> (PathBuf, PathBuf) {
        let repo = dir.0.join("repo");
        let tree = dir.0.join("tree");
        let admin = repo.join(".git/worktrees/tree");
        std::fs::create_dir_all(&admin).unwrap();
        std::fs::create_dir_all(&tree).unwrap();
        std::fs::write(tree.join(".git"), format!("gitdir: {}\n", admin.display())).unwrap();
        std::fs::write(admin.join("commondir"), "../..\n").unwrap();
        std::fs::write(admin.join("gitdir"), format!("{}\n", tree.join(".git").display())).unwrap();
        (repo, tree)
    }

    #[test]
    fn worktrees_of_one_repository_share_a_key_and_a_forged_git_file_gets_the_path_key() {
        let dir = Scratch::new();
        let (repo, tree) = repo_with_worktree(&dir);
        assert_eq!(repo_key(&repo), repo_key(&tree));
        assert_ne!(repo_key(&repo), crate::paths::project_key(&tree));
        // A .git file pointing at another repository's record that doesn't point back: the path key.
        let forged = dir.0.join("forged");
        std::fs::create_dir_all(&forged).unwrap();
        std::fs::write(forged.join(".git"), format!("gitdir: {}\n", repo.join(".git/worktrees/tree").display())).unwrap();
        assert_eq!(repo_key(&forged), crate::paths::project_key(&forged));
        // A .git symlink to another repository: the path key too.
        let linked = dir.0.join("linked");
        std::fs::create_dir_all(&linked).unwrap();
        std::os::unix::fs::symlink(repo.join(".git"), linked.join(".git")).unwrap();
        assert_eq!(repo_key(&linked), crate::paths::project_key(&linked));
        // No repository at all.
        let plain = dir.0.join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        assert_eq!(repo_key(&plain), crate::paths::project_key(&plain));
    }

    #[test]
    fn key_folders_are_created_private_and_a_symlinked_or_odd_one_is_refused() {
        let dir = Scratch::new();
        let cache = cache(&dir);
        let made = cache.key_dir("k1").unwrap();
        assert_eq!(made, cache.root.join("k1"));
        assert_eq!(std::fs::metadata(&made).unwrap().mode() & 0o777, 0o700);
        assert_eq!(std::fs::metadata(&cache.root).unwrap().mode() & 0o777, 0o700);
        // Again: fine.
        assert_eq!(cache.key_dir("k1").unwrap(), made);
        // A symlink in its place (to another key's folder, or anywhere): refused.
        std::os::unix::fs::symlink(&made, cache.root.join("k2")).unwrap();
        assert!(cache.key_dir("k2").unwrap_err().contains("symlink"));
        std::fs::write(cache.root.join("k3"), "").unwrap();
        assert!(cache.key_dir("k3").unwrap_err().contains("isn't a folder"));
        assert!(cache.key_dir("../k").is_err() && cache.key_dir("").is_err() && cache.key_dir(".k").is_err());
    }

    #[test]
    fn a_folder_someone_else_owns_is_refused() {
        // `/` is root's: the user running the tests isn't root in CI or here (skip if they are).
        if nix::unistd::getuid().is_root() {
            return;
        }
        assert!(plain_own_dir(Path::new("/")).unwrap_err().contains("isn't yours"));
    }

    #[test]
    fn the_lock_is_one_holder_at_a_time() {
        let dir = Scratch::new();
        let cache = cache(&dir);
        cache.key_dir("k").unwrap();
        let held = cache.try_lock("k").unwrap();
        assert!(cache.try_lock("k").is_none());
        drop(held);
        assert!(cache.try_lock("k").is_some());
    }

    #[tokio::test]
    async fn waiting_for_the_lock_is_bounded() {
        let dir = Scratch::new();
        let cache = cache(&dir);
        cache.key_dir("k").unwrap();
        let held = cache.try_lock("k").unwrap();
        let started = std::time::Instant::now();
        assert!(cache.lock("k", Duration::from_millis(200), &CancellationToken::new()).await.is_none());
        assert!(started.elapsed() >= Duration::from_millis(200));
        drop(held);
        assert!(cache.lock("k", Duration::from_millis(200), &CancellationToken::new()).await.is_some());
    }

    #[test]
    fn the_free_space_guard_is_5_gb_or_5_percent() {
        let dir = Scratch::new();
        let mut cache = cache(&dir);
        let at = |available, total| -> FreeSpace { Arc::new(move |_| Some(Space { available, total })) };
        cache.free_space = at(100 * GB, 200 * GB);
        assert_eq!(cache.low_on_space(), None);
        cache.free_space = at(4 * GB, 50 * GB);
        assert!(cache.low_on_space().is_some());
        cache.free_space = at(40 * GB, 1000 * GB); // 4%
        assert!(cache.low_on_space().is_some());
        cache.free_space = Arc::new(|_| None);
        assert_eq!(cache.low_on_space(), None);
    }

    #[test]
    fn the_total_cap_evicts_the_least_recently_used_never_a_locked_one() {
        let dir = Scratch::new();
        let mut cache = cache(&dir);
        cache.total_cap = 2500;
        for (key, used) in [("old", 1), ("older", 0), ("new", 3), ("mid", 2)] {
            let d = cache.key_dir(key).unwrap();
            put(&d.join("f"), 1000);
            cache.touch(key, &dir.0, used);
        }
        let held = cache.try_lock("older").unwrap();
        // 4000+ bytes in all: "older" is the least recent but locked, so "old" and "mid" go.
        let gone = cache.evict("new", 10);
        assert_eq!(gone, ["old", "mid"]);
        assert!(cache.dir("older").exists() && cache.dir("new").exists());
        assert!(!cache.dir("old").exists() && !cache.root.join("old.json").exists());
        drop(held);
    }

    #[test]
    fn the_per_key_cap_deletes_the_folder() {
        let dir = Scratch::new();
        let mut cache = cache(&dir);
        cache.key_cap = 500;
        let d = cache.key_dir("k").unwrap();
        put(&d.join("target/a"), 100);
        assert!(!cache.measure("k", 1).1);
        assert!(cache.recorded_size("k").unwrap() >= 100);
        put(&d.join("target/b"), 1000);
        assert!(cache.measure("k", 2).1);
        assert!(!d.exists());
    }

    #[test]
    fn sizing_is_at_most_once_a_minute() {
        let dir = Scratch::new();
        let cache = cache(&dir);
        cache.key_dir("k").unwrap();
        assert!(cache.due_for_sizing("k", 0));
        cache.measure("k", 1_000);
        assert!(!cache.due_for_sizing("k", 30_000));
        assert!(cache.due_for_sizing("k", 61_000));
    }

    #[test]
    fn pruning_takes_old_and_orphaned_folders_never_the_kept_or_a_locked_one() {
        let dir = Scratch::new();
        let cache = cache(&dir);
        let day = 24 * 60 * 60 * 1000;
        let now = 100 * day;
        let alive = dir.0.join("alive");
        std::fs::create_dir_all(&alive).unwrap();
        for (key, used, root) in [
            ("stale", now - 15 * day, &alive),
            ("fresh", now - day, &alive),
            ("orphan", now - day, &dir.0.join("deleted-worktree")),
            ("kept", now - 20 * day, &alive),
            ("locked", now - 20 * day, &alive),
        ] {
            cache.key_dir(key).unwrap();
            cache.touch(key, root, used);
        }
        let held = cache.try_lock("locked").unwrap();
        let mut gone = cache.prune("kept", now);
        gone.sort();
        assert_eq!(gone, ["orphan", "stale"]);
        assert!(cache.dir("fresh").exists() && cache.dir("kept").exists() && cache.dir("locked").exists());
        drop(held);
    }

    #[test]
    fn clearing_deletes_the_folder_unless_a_check_holds_it() {
        let dir = Scratch::new();
        let cache = cache(&dir);
        assert_eq!(cache.clear("k"), Ok(None));
        let d = cache.key_dir("k").unwrap();
        put(&d.join("target/a"), 100);
        let held = cache.try_lock("k").unwrap();
        assert!(cache.clear("k").unwrap_err().contains("using it"));
        drop(held);
        assert!(cache.clear("k").unwrap().unwrap() >= 100);
        assert!(!d.exists());
    }

    /// `target/x -> /` and `target/y -> <another key>` inside a key folder: sizing counts the links only, and pruning,
    /// eviction and clearing leave what they point at untouched.
    #[test]
    fn no_step_follows_a_symlink() {
        let dir = Scratch::new();
        let mut cache = cache(&dir);
        let other = cache.key_dir("other").unwrap();
        put(&other.join("precious"), 4000);
        let outside = dir.0.join("outside");
        put(&outside.join("precious"), 4000);
        let d = cache.key_dir("k").unwrap();
        put(&d.join("target/real"), 10);
        std::os::unix::fs::symlink("/", d.join("target/x")).unwrap();
        std::os::unix::fs::symlink(&other, d.join("target/y")).unwrap();
        std::os::unix::fs::symlink(&outside, d.join("target/z")).unwrap();
        let size = size_of(&d);
        assert!(size < 4000, "followed a link: {size}");
        cache.measure("k", 1);
        assert!(cache.recorded_size("k").unwrap() < 4000);
        // Eviction (the cap forces it), with the link targets alive afterwards.
        cache.total_cap = 4000;
        cache.touch("k", &dir.0, 0);
        cache.touch("other", &dir.0, 5);
        assert_eq!(cache.evict("other", 10), ["k"]);
        assert!(other.join("precious").exists() && outside.join("precious").exists());
        // Clearing and pruning the same way.
        for remove in [0, 1] {
            let d = cache.key_dir("k").unwrap();
            std::os::unix::fs::symlink("/", d.join("x")).unwrap();
            std::os::unix::fs::symlink(&other, d.join("y")).unwrap();
            std::os::unix::fs::symlink(&outside, d.join("z")).unwrap();
            if remove == 0 {
                cache.clear("k").unwrap();
            } else {
                cache.touch("k", &dir.0.join("gone"), 1);
                assert_eq!(cache.prune("other", 2), ["k"]);
            }
            assert!(!d.exists());
            assert!(other.join("precious").exists() && outside.join("precious").exists());
        }
        // A key folder replaced by a symlink is never deleted through.
        std::os::unix::fs::symlink(&outside, cache.root.join("k")).unwrap();
        assert!(cache.clear("k").is_err());
        assert!(outside.join("precious").exists());
    }

    /// The fact the deletes rely on: `remove_dir_all` unlinks a symlink to a folder instead of emptying the folder.
    #[test]
    fn remove_dir_all_doesnt_follow_symlinks() {
        let dir = Scratch::new();
        let target = dir.0.join("target-dir");
        put(&target.join("keep"), 1);
        let doomed = dir.0.join("doomed");
        std::fs::create_dir_all(&doomed).unwrap();
        std::os::unix::fs::symlink(&target, doomed.join("link")).unwrap();
        std::fs::remove_dir_all(&doomed).unwrap();
        assert!(target.join("keep").exists());
    }
}
