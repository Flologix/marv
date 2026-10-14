//! Where a session's checkpoints go (Marv #16). Ports Marv's `src/checkpoints/store.ts`.
//!
//! Git commits of the project's files, written through a temporary index so the user's index, HEAD, branches and
//! stash are never touched. In a repository they go into its own object store under a hidden ref,
//! `refs/marv/checkpoints/<session>`, which no branch list, push, fetch or status shows; outside one, into a bare git
//! dir of Marv's own (`~/.marv/checkpoints/<project>/git`). Each checkpoint's parent is the one before it, so the ref
//! keeps the whole chain reachable and one `update-ref -d` lets it go.
//!
//! Every call goes through [`run_git_async`] (no hooks or fsmonitor, no inherited `GIT_*`, a timeout, a stop), and
//! the store's writes (snapshot, `drop_to`, `prune`) run one at a time.
//!
//! Port notes: paths come back from git as UTF-8 (lossy); a path that isn't UTF-8 keeps what `add -A` stored. The
//! TS tests replaced `snapshot` and `chain` on the instance; here a test sets [`CheckpointStore::fake`].

use std::collections::{HashMap, HashSet};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use tokio_util::sync::CancellationToken;

use crate::git::{GitAsyncResult, GitOptions, run_git_async};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CheckpointKind {
    Before,
    After,
    PreUndo,
    Undo,
}

impl CheckpointKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Before => "before",
            Self::After => "after",
            Self::PreUndo => "pre-undo",
            Self::Undo => "undo",
        }
    }
    fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "before" => Self::Before,
            "after" => Self::After,
            "pre-undo" => Self::PreUndo,
            "undo" => Self::Undo,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checkpoint {
    pub sha: String,
    pub tree: String,
    pub turn: u32,
    pub kind: CheckpointKind,
    /// The turn's request, first line ("add the parser").
    pub label: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapshotFailure {
    TooLarge,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SnapshotResult {
    Ok { sha: String, tree: String, files: usize, ms: u64 },
    Failed { reason: SnapshotFailure, message: String, ms: u64 },
}

impl SnapshotResult {
    pub fn ok(&self) -> bool {
        matches!(self, Self::Ok { .. })
    }
    pub fn ms(&self) -> u64 {
        match self {
            Self::Ok { ms, .. } | Self::Failed { ms, .. } => *ms,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocationKind {
    /// The user's repository (the project, a subfolder of one, or a git worktree).
    Repo,
    /// Marv's own bare git dir, the project as its work tree.
    Own,
}

/// Where snapshots are written: git's dir (refs, objects), the work tree's top, and the project's place in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Location {
    pub kind: LocationKind,
    pub git_dir: PathBuf,
    pub top: PathBuf,
    /// "pkg/" for a subfolder project; "" at the top (always, in Marv's own git dir).
    pub prefix: String,
}

/// Outside a repository nothing like .gitignore keeps a home folder's worth of files out: past these, checkpoints stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub files: usize,
    pub bytes: u64,
}
pub const OWN_LIMITS: Limits = Limits { files: 20_000, bytes: 200 * 1024 * 1024 };

/// Marv's own identity on every checkpoint, so a repository without user.email works, and they're easy to tell apart.
const IDENTITY: [(&str, &str); 4] = [
    ("GIT_AUTHOR_NAME", "Marv"),
    ("GIT_AUTHOR_EMAIL", "marv@localhost"),
    ("GIT_COMMITTER_NAME", "Marv"),
    ("GIT_COMMITTER_EMAIL", "marv@localhost"),
];
pub const REF_PREFIX: &str = "refs/marv/checkpoints/";
const DAY_MS: u64 = 86_400_000;
pub const NOT_A_REPOSITORY: &str = "the project isn't in a git repository";

/// An index lock older than this was left by a git that was killed (run_git_async's timeouts are far shorter).
const STALE_LOCK: Duration = Duration::from_secs(10 * 60);
/// A ref lock older than this was left by a killed git: update-ref holds one for milliseconds.
const STALE_REF_LOCK: Duration = Duration::from_secs(60);

/// A checkpoint's commit message: "marv checkpoint: turn 3 before", then the turn's request on its own line.
pub fn checkpoint_message(turn: u32, kind: CheckpointKind, label: &str) -> String {
    let line: String = label.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(80).collect();
    let body = if line.is_empty() { String::new() } else { format!("\n\n{line}") };
    format!("marv checkpoint: turn {turn} {}{body}\n", kind.as_str())
}

/// Turn, kind and label of a checkpoint's message; None for anything else.
pub fn parse_message(message: &str) -> Option<(u32, CheckpointKind, String)> {
    let (subject, rest) = message.split_once('\n').unwrap_or((message, ""));
    let words = subject.trim().strip_prefix("marv checkpoint: turn ")?;
    let (turn, kind) = words.split_once(' ')?;
    if turn.is_empty() || !turn.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((turn.parse().ok()?, CheckpointKind::parse(kind)?, rest.trim().to_string()))
}

pub struct StoreOptions {
    /// The project root.
    pub root: PathBuf,
    /// `~/.marv/checkpoints/<projectKey>`: Marv's own git dir, for a project that isn't in a repository. Without it,
    /// such a project gets none.
    pub own_dir: Option<PathBuf>,
    /// The session id: one ref per session.
    pub session: String,
    pub limits: Option<Limits>,
    /// The checkpoints' commit time in ms (tests).
    pub now: Option<Arc<dyn Fn() -> u64 + Send + Sync>>,
}

impl StoreOptions {
    pub fn new(root: impl Into<PathBuf>, own_dir: Option<PathBuf>, session: impl Into<String>) -> Self {
        Self { root: root.into(), own_dir, session: session.into(), limits: None, now: None }
    }
}

/// Stand-ins for tests (Marv's tests replaced `snapshot` and `chain` on the instance).
#[derive(Default)]
pub struct Fake {
    /// Answers a snapshot of this kind instead of taking it (None: take it).
    pub snapshot: Option<Arc<dyn Fn(CheckpointKind) -> Option<SnapshotResult> + Send + Sync>>,
    /// Waits this long before each snapshot.
    pub delay: Option<Duration>,
    /// `chain()` fails with this.
    pub chain_error: Option<String>,
}

pub struct SnapshotOptions<'a> {
    pub turn: u32,
    pub kind: CheckpointKind,
    pub label: &'a str,
    pub cancel: Option<&'a CancellationToken>,
}

/// The ms since `started`.
fn elapsed(started: Instant) -> u64 {
    started.elapsed().as_millis() as u64
}

fn now_ms() -> u64 {
    crate::time::now_ms()
}

/// Whether `path` was last changed more than `age` ago.
fn older_than(path: &Path, age: Duration) -> bool {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| SystemTime::now().duration_since(t).ok())
        .is_some_and(|d| d > age)
}

/// Locks a killed git left behind (run_git_async SIGKILLs on a timeout or a stop), which would fail every later
/// snapshot. Each goes only once it's old enough that no live git can hold it: another Marv may have resumed the same
/// session, and Marv's own index outside a repository is shared by every Marv on the project.
fn clear_stale_locks(where_: &Location, reference: &str) {
    let ref_lock = where_.git_dir.join(format!("{reference}.lock"));
    if older_than(&ref_lock, STALE_REF_LOCK) {
        let _ = std::fs::remove_file(&ref_lock);
    }
    let index_lock = where_.git_dir.join("marv-index.lock");
    if where_.kind == LocationKind::Own && older_than(&index_lock, STALE_LOCK) {
        let _ = std::fs::remove_file(&index_lock);
    }
}

/// Removes a temporary index's folder however a snapshot ends.
struct TempFolder(Option<PathBuf>);
impl Drop for TempFolder {
    fn drop(&mut self) {
        if let Some(dir) = &self.0 {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

pub struct CheckpointStore {
    options: StoreOptions,
    /// Why checkpoints are off for this project this session (too large outside a repository), once it is so.
    off: Mutex<Option<String>>,
    session: Mutex<String>,
    located: tokio::sync::OnceCell<Result<Location, String>>,
    /// Snapshots, ref moves and pruning, one at a time: a prune's gc running beside a snapshot would collect the tree
    /// it just wrote before the commit points at it, and two snapshots would race for the ref.
    queue: tokio::sync::Mutex<()>,
    /// The working bytes' blob of each file whose stored blob git's filters changed (`keep_own_bytes`), with the
    /// file's stat when it was hashed: a big LFS asset is read once a session while it doesn't change, not every turn.
    own_bytes: Mutex<HashMap<String, (String, String)>>,
    /// Turns undone whose `undo` marker couldn't be written: without one the chain still shows the turn as the newest
    /// undoable one, and a second /undo would put its files back again over what's there now.
    undone_turns: Mutex<HashSet<u32>>,
    pub fake: Mutex<Fake>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl CheckpointStore {
    pub fn new(options: StoreOptions) -> Self {
        let session = options.session.clone();
        Self {
            options,
            off: Mutex::default(),
            session: Mutex::new(session),
            located: tokio::sync::OnceCell::new(),
            queue: tokio::sync::Mutex::new(()),
            own_bytes: Mutex::default(),
            undone_turns: Mutex::default(),
            fake: Mutex::default(),
        }
    }

    /// Why checkpoints are off for this project (too large outside a repository), once they are.
    pub fn off(&self) -> Option<String> {
        lock(&self.off).clone()
    }

    pub fn undone_turns(&self) -> HashSet<u32> {
        lock(&self.undone_turns).clone()
    }

    pub(crate) fn mark_undone(&self, turn: u32) {
        lock(&self.undone_turns).insert(turn);
    }

    /// A new conversation (clear) or an old one (resume): its own chain.
    pub fn set_session(&self, id: &str) {
        *lock(&self.session) = id.to_string();
        lock(&self.undone_turns).clear();
    }

    /// This session's ref, `refs/marv/checkpoints/<session>`.
    pub fn reference(&self) -> String {
        format!("{REF_PREFIX}{}", lock(&self.session))
    }

    /// Found once per store: the repository around the project, or Marv's own git dir (made on first use).
    pub async fn locate(&self) -> Result<Location, String> {
        self.located.get_or_init(|| self.find()).await.clone()
    }

    /// git with the checkpoint's git dir and work tree pinned, run in the work tree's top.
    pub async fn git(
        &self,
        args: &[&str],
        index: Option<&Path>,
        cancel: Option<&CancellationToken>,
        input: Option<&[u8]>,
    ) -> GitAsyncResult {
        self.git_with(args, index, cancel, input, crate::git::GIT_TIMEOUT).await
    }

    async fn git_with(
        &self,
        args: &[&str],
        index: Option<&Path>,
        cancel: Option<&CancellationToken>,
        input: Option<&[u8]>,
        timeout: Duration,
    ) -> GitAsyncResult {
        let where_ = match self.locate().await {
            Ok(w) => w,
            Err(error) => return GitAsyncResult { ok: false, out: error, bytes: Vec::new(), timed_out: false },
        };
        let mut env: Vec<(String, String)> = IDENTITY.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        env.push(("GIT_DIR".into(), where_.git_dir.to_string_lossy().into_owned()));
        env.push(("GIT_WORK_TREE".into(), where_.top.to_string_lossy().into_owned()));
        if let Some(now) = &self.options.now {
            let time = format!("{} +0000", now() / 1000);
            env.push(("GIT_AUTHOR_DATE".into(), time.clone()));
            env.push(("GIT_COMMITTER_DATE".into(), time));
        }
        if let Some(index) = index {
            env.push(("GIT_INDEX_FILE".into(), index.to_string_lossy().into_owned()));
        }
        run_git_async(&where_.top, args, GitOptions { env: &env, timeout, cancel, input }).await
    }

    /// The project's files, as a pathspec in the work tree ("." or ":(literal)pkg/").
    pub async fn pathspec(&self) -> String {
        match self.locate().await {
            Ok(w) if !w.prefix.is_empty() => format!(":(literal){}", w.prefix),
            _ => ".".into(),
        }
    }

    /// The newest checkpoint of this session, or None.
    pub async fn head(&self) -> Option<String> {
        let found =
            self.git(&["rev-parse", "--verify", "--quiet", &format!("{}^{{commit}}", self.reference())], None, None, None).await;
        found.ok.then(|| found.out.trim().to_string())
    }

    /// Snapshots the project's files (what `git add -A` would stage: tracked plus untracked, non-ignored ones) as a
    /// new checkpoint on this session's chain. Never fails: a failure is a result.
    pub async fn snapshot(&self, options: SnapshotOptions<'_>) -> SnapshotResult {
        let (fake, delay) = {
            let fake = lock(&self.fake);
            (fake.snapshot.clone(), fake.delay)
        };
        if let Some(delay) = delay {
            tokio::time::sleep(delay).await;
        }
        if let Some(answer) = fake.and_then(|f| f(options.kind)) {
            return answer;
        }
        let _one_at_a_time = self.queue.lock().await;
        self.write(options).await
    }

    async fn write(&self, SnapshotOptions { turn, kind, label, cancel }: SnapshotOptions<'_>) -> SnapshotResult {
        let started = Instant::now();
        let fail = |message: &str, reason: SnapshotFailure| SnapshotResult::Failed {
            reason,
            message: message.trim().to_string(),
            ms: elapsed(started),
        };
        let failed = |out: &GitAsyncResult, what: &str| {
            let message = if out.timed_out {
                format!("{what} timed out")
            } else if out.out.trim().is_empty() {
                format!("{what} failed")
            } else {
                out.out.clone()
            };
            fail(&message, SnapshotFailure::Failed)
        };
        if let Some(off) = self.off() {
            return fail(&off, SnapshotFailure::TooLarge);
        }
        let where_ = match self.locate().await {
            Ok(w) => w,
            Err(error) => return fail(&error, SnapshotFailure::Failed),
        };
        let reference = self.reference();
        clear_stale_locks(&where_, &reference);
        let mut temp = TempFolder(None);
        let index = if where_.kind == LocationKind::Repo {
            // A copy of the user's index: its stat cache makes `add -A` rehash only what changed, and it's what says a
            // force-added or skip-worktree file is tracked (from an empty index those would look new, and /undo would
            // delete them). The user's own index is only read.
            let dir = std::env::temp_dir().join(format!("marv-checkpoint-{}", marv_fs::random_hex()));
            if let Err(err) = std::fs::create_dir_all(&dir) {
                return fail(&err.to_string(), SnapshotFailure::Failed);
            }
            temp.0 = Some(dir.clone());
            let index = dir.join("index");
            let user_index = where_.git_dir.join("index");
            // A new repository has none: start empty. The copy keeps the index's own modification time: git treats an
            // entry whose file changed in the same timestamp tick as the index was written as "racy" and re-reads it,
            // and a copy stamped now would hide that, so a same-size edit could be missed.
            if std::fs::copy(&user_index, &index).is_ok()
                && let Ok(modified) = std::fs::metadata(&user_index).and_then(|m| m.modified())
            {
                let _ = std::fs::File::options().write(true).open(&index).and_then(|f| f.set_modified(modified));
            }
            index
        } else {
            let index = where_.git_dir.join("marv-index"); // Marv's own: kept, for its stat cache
            if let Some(big) = self.too_large(&index, cancel).await {
                *lock(&self.off) = Some(big.clone());
                return fail(&big, SnapshotFailure::TooLarge);
            }
            index
        };
        let spec = self.pathspec().await;
        let added = self.git(&["add", "-A", "--", &spec], Some(&index), cancel, None).await;
        if !added.ok {
            return failed(&added, "git add");
        }
        let written = self.git(&["write-tree"], Some(&index), cancel, None).await;
        if !written.ok {
            return failed(&written, "git write-tree");
        }
        let tree = match self.keep_own_bytes(&where_, &index, written.out.trim(), cancel).await {
            Ok(tree) => tree,
            Err(message) => return fail(&message, SnapshotFailure::Failed),
        };
        let listed = self.git(&["ls-files", "-z", "--", &spec], Some(&index), cancel, None).await;
        let files = if listed.ok { listed.out.split('\0').filter(|p| !p.is_empty()).count() } else { 0 };
        let previous = self.head().await;
        let message = checkpoint_message(turn, kind, label);
        let mut args = vec!["commit-tree", "--no-gpg-sign", tree.as_str()];
        if let Some(previous) = &previous {
            args.extend(["-p", previous.as_str()]);
        }
        args.extend(["-m", message.as_str()]);
        let commit = self.git(&args, None, cancel, None).await;
        if !commit.ok {
            return failed(&commit, "git commit-tree");
        }
        let sha = commit.out.trim().to_string();
        // Compare-and-swap: a ref that moved meanwhile fails loudly rather than losing a checkpoint.
        let old = previous.unwrap_or_default();
        let moved = self.git(&["update-ref", "-m", "marv checkpoint", &reference, &sha, &old], None, cancel, None).await;
        if !moved.ok {
            return failed(&moved, "git update-ref");
        }
        SnapshotResult::Ok { sha, tree, files, ms: elapsed(started) }
    }

    /// `add -A` stores what git's clean side makes of a file: an LFS pointer, LF for a CRLF file (eol, autocrlf), a
    /// collapsed `$Id$`, UTF-8 for a re-encoded file. Undo writes blobs back without running checkout's smudge side (a
    /// program, maybe a download), so the snapshot must hold the file's own bytes: each blob whose size differs from
    /// the file's (an eol conversion only ever drops CRs) or whose path has a `filter`, `ident` or
    /// `working-tree-encoding` attribute (a filter may keep the size) is hashed again from the file with
    /// `--no-filters` and put in the temporary index. Returns the tree to commit.
    async fn keep_own_bytes(
        &self,
        where_: &Location,
        index: &Path,
        tree: &str,
        cancel: Option<&CancellationToken>,
    ) -> Result<String, String> {
        let or = |r: &GitAsyncResult, what: &str| if r.out.trim().is_empty() { format!("{what} failed") } else { r.out.clone() };
        if !self.may_convert(where_, index, cancel).await {
            return Ok(tree.to_string());
        }
        let listed = self.git(&["ls-tree", "-r", "-l", "-z", tree], None, cancel, None).await;
        if !listed.ok {
            return Err(or(&listed, "git ls-tree"));
        }
        struct Blob {
            mode: String,
            size: u64,
            path: String,
        }
        let mut blobs = Vec::new();
        for entry in listed.out.split('\0') {
            let Some((meta, path)) = entry.split_once('\t') else { continue };
            let fields: Vec<&str> = meta.split(' ').filter(|f| !f.is_empty()).collect();
            let [mode, kind, _sha, size] = fields[..] else { continue };
            // Symlinks and submodules go through no filter; a path with a newline, or starting with a quote (which
            // --stdin-paths unquotes C-style), can't go through --stdin-paths: it keeps add's blob.
            if kind != "blob"
                || mode == "120000"
                || !path.starts_with(&where_.prefix)
                || path.contains('\n')
                || path.starts_with('"')
            {
                continue;
            }
            blobs.push(Blob { mode: mode.into(), size: size.parse().unwrap_or(0), path: path.into() });
        }
        if blobs.is_empty() {
            return Ok(tree.to_string());
        }
        let input: String = blobs.iter().map(|b| format!("{}\0", b.path)).collect();
        let attrs = self
            .git(
                &["check-attr", "-z", "--stdin", "filter", "ident", "working-tree-encoding"],
                Some(index),
                cancel,
                Some(input.as_bytes()),
            )
            .await;
        if !attrs.ok {
            return Err(or(&attrs, "git check-attr"));
        }
        let fields: Vec<&str> = attrs.out.split('\0').collect();
        let filtered: HashSet<&str> =
            fields.chunks(3).filter(|c| c.len() == 3 && c[2] != "unspecified" && c[2] != "unset").map(|c| c[0]).collect();

        // The files' sizes as `add -A` just saw them: the index's stat data, which it refreshed (stat'ing every file
        // from here took most of the snapshot's time). A path it doesn't give is stat'ed below.
        let spec = self.pathspec().await;
        let debug = self.git(&["ls-files", "--debug", "-s", "-z", "--", &spec], Some(index), cancel, None).await;
        let mut sizes: HashMap<&str, u64> = HashMap::new();
        if debug.ok {
            let pattern = regex::Regex::new(r"\d{6} [0-9a-f]+ \d\t([^\x00]*)\x00[^\x00]*?size: (\d+)\t").expect("valid");
            for m in pattern.captures_iter(&debug.out) {
                if let (Some(path), Some(size)) = (m.get(1), m.get(2)) {
                    sizes.insert(path.as_str(), size.as_str().parse().unwrap_or(0));
                }
            }
        }
        let candidates: Vec<&Blob> =
            blobs.iter().filter(|b| filtered.contains(b.path.as_str()) || sizes.get(b.path.as_str()) != Some(&b.size)).collect();
        struct Changed {
            mode: String,
            path: String,
            stamp: String,
        }
        let mut changed = Vec::new();
        for blob in candidates {
            // Not a file here (skip-worktree, deleted meanwhile), or the same size after all: git's blob is the file.
            let Ok(info) = std::fs::symlink_metadata(where_.top.join(&blob.path)) else { continue };
            if !info.is_file() || (info.size() == blob.size && !filtered.contains(blob.path.as_str())) {
                continue;
            }
            let stamp = format!(
                "{}:{}:{}:{}",
                info.size(),
                info.mtime() as i128 * 1_000_000_000 + info.mtime_nsec() as i128,
                info.ctime() as i128 * 1_000_000_000 + info.ctime_nsec() as i128,
                info.ino()
            );
            changed.push(Changed { mode: blob.mode.clone(), path: blob.path.clone(), stamp });
        }
        if changed.is_empty() {
            return Ok(tree.to_string());
        }
        // Hashed before and unchanged since: reused, if the object is still there (a dropped pair's may be collected).
        let known: Vec<(String, String)> = {
            let own = lock(&self.own_bytes);
            changed
                .iter()
                .filter_map(|c| {
                    own.get(&c.path).filter(|(stamp, _)| *stamp == c.stamp).map(|(_, sha)| (c.path.clone(), sha.clone()))
                })
                .collect()
        };
        let mut shas: HashMap<String, String> = HashMap::new();
        if !known.is_empty() {
            let input: String = known.iter().map(|(_, sha)| format!("{sha}\n")).collect();
            let checked = self.git(&["cat-file", "--batch-check=%(objectname)"], None, cancel, Some(input.as_bytes())).await;
            if checked.ok {
                let present: HashSet<&str> =
                    checked.out.lines().filter(|l| l.bytes().all(|b| b.is_ascii_hexdigit()) && !l.is_empty()).collect();
                for (path, sha) in &known {
                    if present.contains(sha.as_str()) {
                        shas.insert(path.clone(), sha.clone());
                    }
                }
            }
        }
        let fresh: Vec<&Changed> = changed.iter().filter(|c| !shas.contains_key(&c.path)).collect();
        if !fresh.is_empty() {
            let input: String = fresh.iter().map(|c| format!("{}\n", c.path)).collect();
            let hashed =
                self.git(&["hash-object", "-w", "--no-filters", "--stdin-paths"], None, cancel, Some(input.as_bytes())).await;
            if !hashed.ok {
                return Err(or(&hashed, "git hash-object"));
            }
            let lines: Vec<&str> = hashed.out.lines().filter(|l| !l.is_empty()).collect();
            if lines.len() != fresh.len() {
                return Err("git hash-object answered for fewer files than asked".into());
            }
            let mut own = lock(&self.own_bytes);
            for (c, sha) in fresh.iter().zip(lines) {
                shas.insert(c.path.clone(), sha.to_string());
                own.insert(c.path.clone(), (c.stamp.clone(), sha.to_string()));
            }
        }
        let input: String = changed.iter().map(|c| format!("{} {}\t{}\0", c.mode, shas[&c.path], c.path)).collect();
        let updated = self.git(&["update-index", "-z", "--index-info"], Some(index), cancel, Some(input.as_bytes())).await;
        if !updated.ok {
            return Err(or(&updated, "git update-index"));
        }
        let rewritten = self.git(&["write-tree"], Some(index), cancel, None).await;
        if rewritten.ok { Ok(rewritten.out.trim().to_string()) } else { Err(or(&rewritten, "git write-tree")) }
    }

    /// Whether git could store anything but a file's own bytes here: core.autocrlf is on, or some attributes file (the
    /// tree's .gitattributes, info/attributes, the user's and the system's) names an attribute that converts. Read as
    /// text, so a comment or a macro using one counts too: a needless check costs time, never bytes. Most projects
    /// have none, and the check (check-attr over every path) is most of a snapshot's time on a big tree.
    async fn may_convert(&self, where_: &Location, index: &Path, cancel: Option<&CancellationToken>) -> bool {
        // Every .gitattributes in the work tree (a parent folder's applies to a subfolder project too).
        let (config, listed, info) = tokio::join!(
            self.git(&["config", "-z", "--type=path", "--get-regexp", r"^core\.(autocrlf|attributesfile)$"], None, cancel, None),
            self.git(&["ls-files", "-z", "--full-name", "--", ":(top,glob)**/.gitattributes"], Some(index), cancel, None),
            self.git(&["rev-parse", "--git-path", "info/attributes"], None, cancel, None),
        );
        let config_home = std::env::var_os("XDG_CONFIG_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| crate::paths::home_dir().join(".config"));
        let mut user_file = config_home.join("git").join("attributes");
        if config.ok {
            for pair in config.out.split('\0') {
                let (key, value) = pair.split_once('\n').unwrap_or((pair, ""));
                if key == "core.autocrlf" && ["true", "input"].contains(&value.trim().to_lowercase().as_str()) {
                    return true;
                }
                if key == "core.attributesfile" {
                    user_file = where_.top.join(value);
                }
            }
        }
        let mut files: Vec<PathBuf> = if listed.ok {
            listed.out.split('\0').filter(|p| !p.is_empty()).map(|p| where_.top.join(p)).collect()
        } else {
            Vec::new()
        };
        files.push(user_file);
        files.push("/etc/gitattributes".into());
        if info.ok {
            files.push(where_.top.join(info.out.trim()));
        }
        let converting = regex::Regex::new(r"\b(filter|ident|working-tree-encoding|text|eol|crlf)\b").expect("valid");
        files.iter().any(|file| std::fs::read_to_string(file).is_ok_and(|text| converting.is_match(&text)))
    }

    /// Moves the ref back to `sha` (a pair that changed nothing), or deletes it (None).
    pub async fn drop_to(&self, sha: Option<&str>) -> bool {
        let _one_at_a_time = self.queue.lock().await;
        let Some(current) = self.head().await else { return sha.is_none() };
        let reference = self.reference();
        let result = match sha {
            Some(sha) => self.git(&["update-ref", &reference, sha, &current], None, None, None).await,
            None => self.git(&["update-ref", "-d", &reference, &current], None, None, None).await,
        };
        result.ok
    }

    /// This session's checkpoints, newest first (at most 1000). A chain that exists but can't be read is an error, not
    /// an empty chain: numbering from 0 would collide with its turns.
    pub async fn chain(&self) -> Result<Vec<Checkpoint>, String> {
        if let Some(error) = lock(&self.fake).chain_error.clone() {
            return Err(error);
        }
        let Some(head) = self.head().await else { return Ok(Vec::new()) };
        let listed = self.git(&["rev-list", "--max-count=1000", "--format=%x1e%H%x00%T%x00%B", &head], None, None, None).await;
        if !listed.ok {
            let why = if listed.timed_out {
                "git timed out".to_string()
            } else if listed.out.trim().is_empty() {
                "git rev-list failed".to_string()
            } else {
                listed.out.trim().to_string()
            };
            return Err(format!("the checkpoints can't be read ({why})"));
        }
        let trailer = regex::Regex::new(r"\ncommit [0-9a-f]+\s*$").expect("valid");
        let mut chain = Vec::new();
        for chunk in listed.out.split('\x1e').skip(1) {
            let mut parts = chunk.splitn(3, '\0');
            let (sha, tree, message) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""), parts.next().unwrap_or(""));
            if let Some((turn, kind, label)) = parse_message(&trailer.replace(message, "")) {
                chain.push(Checkpoint { sha: sha.trim().into(), tree: tree.trim().into(), turn, kind, label });
            }
        }
        Ok(chain)
    }

    /// Keeps the newest `keep` sessions' chains of this project and anything updated in the last `days`; deletes the
    /// other refs (never this session's). In the user's repository that's all (their next gc takes the objects); in
    /// Marv's own git dir, Marv collects them, at most once a day.
    pub async fn prune(&self, keep: usize, days: u64) {
        let _one_at_a_time = self.queue.lock().await;
        let Ok(where_) = self.locate().await else { return };
        let listed = self
            .git(
                &["for-each-ref", "--sort=-committerdate", "--format=%(refname)%00%(committerdate:unix)", REF_PREFIX],
                None,
                None,
                None,
            )
            .await;
        if !listed.ok {
            return;
        }
        let now = self.options.now.as_ref().map_or_else(now_ms, |now| now());
        let mine = self.reference();
        for (i, line) in listed.out.lines().filter(|l| !l.is_empty()).enumerate() {
            let (name, time) = line.split_once('\0').unwrap_or((line, "0"));
            let ms = time.trim().parse::<u64>().unwrap_or(0) * 1000;
            if i < keep || name == mine || now.saturating_sub(ms) < days * DAY_MS {
                continue;
            }
            self.git(&["update-ref", "-d", name], None, None, None).await;
        }
        let (LocationKind::Own, Some(own_dir)) = (where_.kind, &self.options.own_dir) else { return };
        let stamp = own_dir.join("gc-stamp");
        if !older_than(&stamp, Duration::from_millis(DAY_MS)) && stamp.exists() {
            return;
        }
        let _ = std::fs::write(&stamp, "");
        // Not --prune=now: another Marv in the same project may be between writing a tree and committing it.
        self.git_with(&["gc", "--prune=1.hour.ago", "--quiet"], None, None, None, Duration::from_secs(120)).await;
    }

    async fn find(&self) -> Result<Location, String> {
        let root = &self.options.root;
        let found = run_git_async(
            root,
            &["rev-parse", "--absolute-git-dir", "--show-toplevel", "--show-prefix"],
            GitOptions { timeout: Duration::from_secs(10), ..Default::default() },
        )
        .await;
        if found.ok {
            let mut lines = found.out.split('\n');
            let (git_dir, top, prefix) = (lines.next().unwrap_or(""), lines.next().unwrap_or(""), lines.next().unwrap_or(""));
            if !git_dir.is_empty() && !top.is_empty() {
                return Ok(Location {
                    kind: LocationKind::Repo,
                    git_dir: git_dir.into(),
                    top: top.into(),
                    prefix: prefix.into(),
                });
            }
        }
        // Not a repository (or an unusable one): Marv's own bare git dir, with the project as its work tree.
        let own_dir = self.options.own_dir.as_ref().ok_or_else(|| NOT_A_REPOSITORY.to_string())?;
        let git_dir = own_dir.join("git");
        std::fs::create_dir_all(&git_dir).map_err(|e| e.to_string())?;
        let init = run_git_async(
            own_dir,
            &["init", "--bare", "-q", &git_dir.to_string_lossy()],
            GitOptions { timeout: Duration::from_secs(10), ..Default::default() },
        )
        .await;
        if !init.ok {
            return Err(if init.out.trim().is_empty() { "git init failed".into() } else { init.out.trim().into() });
        }
        // No .gitignore applies here: the file listing's skip list keeps dependencies and build output out.
        std::fs::create_dir_all(git_dir.join("info")).map_err(|e| e.to_string())?;
        let exclude: String = marv_tools::files::IGNORED_DIRS.iter().map(|name| format!("{name}/\n")).collect();
        std::fs::write(git_dir.join("info").join("exclude"), exclude).map_err(|e| e.to_string())?;
        let top = std::path::absolute(root).unwrap_or_else(|_| root.clone());
        Ok(Location { kind: LocationKind::Own, git_dir, top, prefix: String::new() })
    }

    /// Outside a repository: why this project is too large to snapshot (files, or new content), or None.
    async fn too_large(&self, index: &Path, cancel: Option<&CancellationToken>) -> Option<String> {
        let Limits { files, bytes } = self.options.limits.unwrap_or(OWN_LIMITS);
        let all = self.git(&["ls-files", "-z", "--cached", "--others", "--exclude-standard"], Some(index), cancel, None).await;
        if !all.ok {
            return None; // add -A reports what's wrong
        }
        let count = all.out.split('\0').filter(|p| !p.is_empty()).count();
        if count > files {
            return Some(format!(
                "the project is too large for checkpoints outside a git repository ({count} files, the limit is {files})"
            ));
        }
        let changed =
            self.git(&["ls-files", "-z", "--others", "--modified", "--exclude-standard"], Some(index), cancel, None).await;
        if !changed.ok {
            return None;
        }
        let mut total = 0;
        for path in changed.out.split('\0').filter(|p| !p.is_empty()) {
            total += std::fs::metadata(self.options.root.join(path)).map(|m| if m.is_file() { m.len() } else { 0 }).unwrap_or(0);
            if total > bytes {
                return Some(format!(
                    "the project is too large for checkpoints outside a git repository (over {} MB of new content)",
                    (bytes as f64 / 1024.0 / 1024.0).round() as u64
                ));
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    // Ports tests/checkpoints.test.ts: locate, snapshot, stale locks, the chain.
    use super::*;
    use crate::checkpoints::test_support::{Temp, git, make_repo, read, temp, tree_files, write};
    use std::os::unix::fs::PermissionsExt;

    pub(crate) fn store_in(root: &Path, session: &str, own: &Temp) -> CheckpointStore {
        CheckpointStore::new(StoreOptions::new(root, Some(own.0.clone()), session))
    }

    async fn snap(s: &CheckpointStore, turn: u32, kind: CheckpointKind, label: &str) -> SnapshotResult {
        s.snapshot(SnapshotOptions { turn, kind, label, cancel: None }).await
    }

    fn sha_of(r: &SnapshotResult) -> String {
        match r {
            SnapshotResult::Ok { sha, .. } => sha.clone(),
            SnapshotResult::Failed { message, .. } => panic!("snapshot failed: {message}"),
        }
    }

    /// The user's git state: HEAD, the index's bytes, branches, stash, status.
    fn state(repo: &Path) -> (String, Vec<u8>, String, String, String) {
        (
            git(repo, &["rev-parse", "HEAD"]),
            std::fs::read(repo.join(".git/index")).unwrap(),
            git(repo, &["branch", "-a"]),
            git(repo, &["stash", "list"]),
            git(repo, &["status", "--porcelain"]),
        )
    }

    fn age(path: &Path, secs: u64) {
        let file = std::fs::File::options().write(true).open(path).unwrap();
        file.set_modified(SystemTime::now() - Duration::from_secs(secs)).unwrap();
    }

    #[tokio::test]
    async fn locate_a_repositorys_top_a_subfolder_no_repository() {
        let repo = make_repo(true);
        let own = temp("cp-own");
        let found = store_in(&repo.0, "s1", &own).locate().await.unwrap();
        assert_eq!(
            found,
            Location { kind: LocationKind::Repo, git_dir: repo.0.join(".git"), top: repo.0.clone(), prefix: "".into() }
        );
        std::fs::create_dir(repo.0.join("pkg")).unwrap();
        let sub = store_in(&repo.0.join("pkg"), "s1", &own).locate().await.unwrap();
        assert_eq!((sub.kind, sub.top, sub.prefix.as_str()), (LocationKind::Repo, repo.0.clone(), "pkg/"));
        let plain = temp("cp-plain");
        let own = temp("cp-own");
        let located = store_in(&plain.0, "s1", &own).locate().await.unwrap();
        assert_eq!(
            located,
            Location { kind: LocationKind::Own, git_dir: own.0.join("git"), top: plain.0.clone(), prefix: "".into() }
        );
        assert!(own.0.join("git/HEAD").exists());
        assert!(read(own.0.join("git/info/exclude")).contains("node_modules/"));
    }

    #[tokio::test]
    async fn locate_a_git_worktree_its_own_git_dir_for_the_index() {
        let repo = make_repo(true);
        let parent = temp("cp-wt");
        let wt = parent.0.join("wt");
        git(&repo.0, &["worktree", "add", "-q", "-b", "side", &wt.to_string_lossy()]);
        let located = store_in(&wt, "s1", &temp("cp-own")).locate().await.unwrap();
        assert_eq!((located.kind, located.top.clone(), located.prefix.as_str()), (LocationKind::Repo, wt.clone(), ""));
        assert!(located.git_dir.starts_with(repo.0.join(".git/worktrees")), "{:?}", located.git_dir);
    }

    #[tokio::test]
    async fn snapshot_tracked_modified_untracked_and_force_added_not_ignored_the_users_state_untouched() {
        let repo = make_repo(true);
        let r = &repo.0;
        write(r.join("forced.txt"), "f\n");
        write(r.join(".gitignore"), "ignored.txt\nbuild/\nforced.txt\n");
        git(r, &["add", "-f", "forced.txt", ".gitignore"]);
        git(r, &["commit", "-q", "-m", "forced"]);
        write(r.join("a.txt"), "changed\n");
        write(r.join("new.txt"), "new\n");
        write(r.join("ignored.txt"), "secret\n");
        let before = state(r);
        let cp_own = temp("cp-own");
        let s = store_in(r, "s1", &cp_own);
        let result = snap(&s, 1, CheckpointKind::Before, "do it").await;
        let sha = sha_of(&result);
        assert_eq!(tree_files(r, &sha), [".gitignore", "a.txt", "forced.txt", "new.txt"]);
        assert_eq!(git(r, &["show", &format!("{sha}:a.txt")]), "changed");
        assert_eq!(state(r), before);
        assert_eq!(git(r, &["rev-parse", "refs/marv/checkpoints/s1"]), sha);

        let second = sha_of(&snap(&s, 1, CheckpointKind::After, "do it").await);
        assert_eq!(git(r, &["rev-parse", &format!("{second}^")]), sha);
        assert_eq!(state(r), before);
    }

    #[tokio::test]
    async fn a_subfolder_projects_tree_holds_only_its_prefix() {
        let repo = make_repo(true);
        write(repo.0.join("pkg/x.txt"), "x\n");
        write(repo.0.join("outside.txt"), "o\n");
        let sha = sha_of(&snap(&store_in(&repo.0.join("pkg"), "s1", &temp("cp-own")), 1, CheckpointKind::Before, "").await);
        let files = tree_files(&repo.0, &sha);
        assert!(files.contains(&"pkg/x.txt".to_string()));
        assert!(!files.contains(&"outside.txt".to_string()));
    }

    #[tokio::test]
    async fn works_without_user_email() {
        let repo = make_repo(false);
        assert!(snap(&store_in(&repo.0, "s1", &temp("cp-own")), 1, CheckpointKind::Before, "").await.ok());
        assert_eq!(git(&repo.0, &["log", "-1", "--format=%an <%ae>", "refs/marv/checkpoints/s1"]), "Marv <marv@localhost>");
    }

    #[tokio::test]
    async fn never_runs_a_hook_or_fsmonitor() {
        let repo = make_repo(true);
        let outside = temp("cp-marker");
        let marker = outside.0.join("ran");
        let script = repo.0.join(".git/touch.sh");
        let body = format!("#!/bin/sh\ntouch {}\n", marker.display());
        write(&script, &body);
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        for hook in ["pre-commit", "post-commit", "reference-transaction"] {
            let path = repo.0.join(".git/hooks").join(hook);
            write(&path, &body);
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        git(&repo.0, &["config", "core.fsmonitor", &script.to_string_lossy()]);
        write(repo.0.join("a.txt"), "x\n");
        assert!(snap(&store_in(&repo.0, "s1", &temp("cp-own")), 1, CheckpointKind::Before, "").await.ok());
        assert!(!marker.exists());
    }

    #[tokio::test]
    async fn own_git_dir_the_skip_list_applies_the_index_persists_too_large_stops() {
        let plain = temp("cp-plain");
        let own = temp("cp-own");
        write(plain.0.join("node_modules/x.js"), "x");
        write(plain.0.join("a.txt"), "a\n");
        let s = store_in(&plain.0, "s1", &own);
        let sha = sha_of(&snap(&s, 1, CheckpointKind::Before, "").await);
        assert_eq!(tree_files(&own.0.join("git"), &sha), ["a.txt"]);
        assert!(own.0.join("git/marv-index").exists());

        let big = temp("cp-big");
        for i in 0..6 {
            write(big.0.join(format!("f{i}.txt")), "x");
        }
        let cp_own = temp("cp-own");
        let small = CheckpointStore::new(StoreOptions {
            limits: Some(Limits { files: 5, bytes: 1_000_000_000 }),
            ..StoreOptions::new(&big.0, Some(cp_own.0.clone()), "s1")
        });
        let too_large = snap(&small, 1, CheckpointKind::Before, "").await;
        assert!(matches!(too_large, SnapshotResult::Failed { reason: SnapshotFailure::TooLarge, .. }), "{too_large:?}");
        assert!(small.off().unwrap().contains("too large"));
        let own2 = temp("cp-own");
        let sized = CheckpointStore::new(StoreOptions {
            limits: Some(Limits { files: 100, bytes: 3 }),
            ..StoreOptions::new(&big.0, Some(own2.0.clone()), "s1")
        });
        assert!(matches!(
            snap(&sized, 1, CheckpointKind::Before, "").await,
            SnapshotResult::Failed { reason: SnapshotFailure::TooLarge, .. }
        ));
    }

    #[tokio::test]
    async fn stale_locks_this_sessions_ref_lock_and_an_old_marv_index_lock_go_a_fresh_index_lock_stays() {
        let plain = temp("cp-plain");
        let own = temp("cp-own");
        write(plain.0.join("a.txt"), "a\n");
        let s = store_in(&plain.0, "s1", &own);
        assert!(snap(&s, 1, CheckpointKind::Before, "").await.ok());
        let git_dir = own.0.join("git");
        let ref_lock = git_dir.join("refs/marv/checkpoints/s1.lock");
        let index_lock = git_dir.join("marv-index.lock");
        write(&ref_lock, "");
        write(&index_lock, "");
        age(&index_lock, 3600);
        age(&ref_lock, 3600);
        assert!(snap(&s, 1, CheckpointKind::After, "").await.ok());
        assert!(!ref_lock.exists() && !index_lock.exists());

        write(&index_lock, "");
        assert!(!snap(&s, 2, CheckpointKind::Before, "").await.ok());
        assert!(index_lock.exists());
    }

    #[tokio::test]
    async fn in_a_repository_this_sessions_ref_lock_is_cleared_once_its_old() {
        let repo = make_repo(true);
        let cp_own = temp("cp-own");
        let s = store_in(&repo.0, "s1", &cp_own);
        assert!(snap(&s, 1, CheckpointKind::Before, "").await.ok());
        let lock = repo.0.join(".git/refs/marv/checkpoints/s1.lock");
        write(&lock, "");
        // A fresh one may be a live git's (another Marv resumed this session): left alone, so this snapshot fails.
        assert!(!snap(&s, 1, CheckpointKind::After, "").await.ok());
        age(&lock, 3600);
        assert!(snap(&s, 1, CheckpointKind::After, "").await.ok());
    }

    #[tokio::test]
    async fn drop_to_moves_the_ref_back_chain_parses_turn_and_kind_newest_first() {
        let repo = make_repo(true);
        let cp_own = temp("cp-own");
        let s = store_in(&repo.0, "s1", &cp_own);
        let one = sha_of(&snap(&s, 1, CheckpointKind::Before, "add the parser").await);
        let two = sha_of(&snap(&s, 1, CheckpointKind::After, "add the parser").await);
        let chain = s.chain().await.unwrap();
        let shape: Vec<(u32, CheckpointKind, &str)> = chain.iter().map(|c| (c.turn, c.kind, c.label.as_str())).collect();
        assert_eq!(shape, [(1, CheckpointKind::After, "add the parser"), (1, CheckpointKind::Before, "add the parser")]);
        assert_eq!(chain[0].sha, two);
        s.drop_to(Some(&one)).await;
        assert_eq!(s.chain().await.unwrap().len(), 1);
        s.drop_to(None).await;
        assert!(s.chain().await.unwrap().is_empty());
        assert_eq!(git(&repo.0, &["for-each-ref", "refs/marv/"]), "");
    }

    #[test]
    fn parse_message_reads_marvs_subject() {
        assert_eq!(
            parse_message("marv checkpoint: turn 3 pre-undo\n\nfix it\n"),
            Some((3, CheckpointKind::PreUndo, "fix it".into()))
        );
        assert_eq!(parse_message("something else"), None);
        assert_eq!(
            checkpoint_message(2, CheckpointKind::Before, "  add\n the   parser "),
            "marv checkpoint: turn 2 before\n\nadd the parser\n"
        );
        assert_eq!(checkpoint_message(2, CheckpointKind::Undo, ""), "marv checkpoint: turn 2 undo\n");
    }

    #[tokio::test]
    async fn prune_keeps_the_newest_sessions_and_the_recent_ones() {
        let repo = make_repo(true);
        let now = crate::time::now_ms();
        // Five sessions: two old, three recent. Keep 2 + anything newer than 14 days.
        for (i, age) in [40u64, 30, 3, 2, 1].into_iter().enumerate() {
            let cp_own = temp("cp-own");
            let s = CheckpointStore::new(StoreOptions {
                now: Some(Arc::new(move || now - age * DAY_MS)),
                ..StoreOptions::new(&repo.0, Some(cp_own.0.clone()), format!("s{i}"))
            });
            assert!(snap(&s, 1, CheckpointKind::Before, "").await.ok());
        }
        store_in(&repo.0, "s4", &temp("cp-own")).prune(2, 14).await;
        let mut left: Vec<String> = git(&repo.0, &["for-each-ref", "--format=%(refname:lstrip=3)", "refs/marv/checkpoints/"])
            .lines()
            .map(String::from)
            .collect();
        left.sort();
        assert_eq!(left, ["s2", "s3", "s4"]);
    }

    #[tokio::test]
    async fn prune_in_the_own_git_dir_runs_gc_at_most_once_a_day() {
        let plain = temp("cp-plain");
        let own = temp("cp-own");
        write(plain.0.join("a.txt"), "a");
        let s = store_in(&plain.0, "s1", &own);
        snap(&s, 1, CheckpointKind::Before, "").await;
        s.prune(20, 14).await;
        let stamp = std::fs::metadata(own.0.join("gc-stamp")).unwrap().modified().unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        s.prune(20, 14).await;
        assert_eq!(std::fs::metadata(own.0.join("gc-stamp")).unwrap().modified().unwrap(), stamp);
    }
}
