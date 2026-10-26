//! The per-kind policy (#187, `docs/specs/2026-10-25-lsp-checker.md` §6.1–§6.4) with a stand-in kind that has a
//! cache and a warm-up: the warm-up and pending turns, the per-key lock, turning off after unreadable runs, the cache
//! mount, the disk guards, and that nothing Marv does around a check runs anything on the host.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::FutureExt;
use futures::future::BoxFuture;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use super::Diagnostic;
use super::cache::{CheckCache, Space, repo_key};
use super::turn::testing::*;
use super::turn::*;
use crate::checks::Scratch;

const ERR_X: &str = "src/a.rs(1,1): error TS2322: mismatched types\n";
const GB: u64 = 1024 * 1024 * 1024;

/// A checker that keeps a build cache and warms it up (what cargo check will be), reading tsc's format.
struct Cached {
    warm: bool,
    max_unreadable: Option<u32>,
}

impl CheckKind<Diagnostic> for Cached {
    fn label(&self) -> &'static str {
        "fake check"
    }
    fn source(&self) -> Option<&'static str> {
        Some("fake")
    }
    fn cache(&self) -> bool {
        true
    }
    fn warm_up(&self, _cache: &Path) -> bool {
        self.warm
    }
    fn max_unreadable(&self) -> Option<u32> {
        self.max_unreadable
    }
    fn prepare(&self, root: &Path) -> bool {
        root.join("Fake.toml").exists()
    }
    fn pass(&self, _root: &Path, cache: Option<&Path>, _changed: Option<&[String]>) -> Option<CheckPass<Diagnostic>> {
        Some(CheckPass {
            command: format!("fake-check --target-dir {}/target", cache?.display()),
            covers: None,
            read_only: Vec::new(),
        })
    }
    fn parse(&self, output: &str, exit_code: Option<i32>) -> Option<Vec<Diagnostic>> {
        super::tsc::parse_tsc(output, exit_code)
    }
    fn missing(&self, _output: &str, _exit_code: Option<i32>) -> Option<String> {
        None
    }
    fn key(&self, problem: &Diagnostic) -> String {
        super::tsc::ts_error_key(problem)
    }
    fn note(&self, added: &[Diagnostic]) -> String {
        super::tsc::diagnostics_note(added)
    }
}

/// A warm-up run is the one with the long timeout.
fn is_warm_up(run: &CheckRun) -> bool {
    run.timeout > CHECK_TIMEOUT
}

/// A runner whose answers may wait: `answer(n, run, cancel)`, n counting every run (warm-ups too).
struct Runs {
    runs: Arc<Mutex<Vec<CheckRun>>>,
    run: RunCheck,
}

impl Runs {
    fn new(
        answer: impl Fn(usize, CheckRun, CancellationToken) -> BoxFuture<'static, CommandResult> + Send + Sync + 'static,
    ) -> Self {
        let runs: Arc<Mutex<Vec<CheckRun>>> = Default::default();
        let kept = runs.clone();
        let run: RunCheck = Arc::new(move |run: CheckRun, cancel| {
            let n = {
                let mut runs = kept.lock().unwrap();
                runs.push(run.clone());
                runs.len() - 1
            };
            answer(n, run, cancel)
        });
        Self { runs, run }
    }
    /// Checks answer in order (then `ok("")`); warm-ups answer `ok("")` at once.
    fn checks(answers: Vec<CommandResult>) -> Self {
        let next = Arc::new(AtomicUsize::new(0));
        Self::new(move |_, run, _| {
            let result = if is_warm_up(&run) {
                ok("")
            } else {
                answers.get(next.fetch_add(1, Ordering::SeqCst)).cloned().unwrap_or_else(|| ok(""))
            };
            async move { result }.boxed()
        })
    }
    fn all(&self) -> Vec<CheckRun> {
        self.runs.lock().unwrap().clone()
    }
    fn checks_run(&self) -> usize {
        self.all().iter().filter(|r| !is_warm_up(r)).count()
    }
}

/// A scratch folder with a project the stand-in kind applies to, and a cache in it with plenty of free space.
fn setup() -> (Scratch, PathBuf, Arc<CheckCache>) {
    let dir = Scratch::new();
    dir.put("project/Fake.toml", "");
    let mut cache = CheckCache::new(dir.0.join("cache/checks"));
    cache.free_space = Arc::new(|_| Some(Space { available: 100 * GB, total: 200 * GB }));
    let project = dir.0.join("project");
    (dir, project, Arc::new(cache))
}

fn checker(kind: Cached, runs: &Runs, cache: &Arc<CheckCache>) -> Arc<Checker<Diagnostic>> {
    checker_of(Arc::new(kind), runs, cache)
}

fn checker_of(kind: Arc<dyn CheckKind<Diagnostic>>, runs: &Runs, cache: &Arc<CheckCache>) -> Arc<Checker<Diagnostic>> {
    let mut checker = Checker::new(kind, Some(runs.run.clone()));
    checker.sandbox_works = Some(Arc::new(|| true));
    checker.cache = Some(cache.clone());
    checker.baseline_wait = Duration::from_millis(200);
    Arc::new(checker)
}

fn turn(checker: &Arc<Checker<Diagnostic>>, project: &Path) -> TurnLog<Diagnostic> {
    TurnLog::new(checker, project, true, CancellationToken::new())
}

fn plain() -> Cached {
    Cached { warm: false, max_unreadable: None }
}

/// Waits (bounded) until `done()`.
async fn until(done: impl Fn() -> bool) {
    for _ in 0..1000 {
        if done() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting");
}

fn pending(r: &CheckResult, notify: bool) -> bool {
    matches!(r, CheckOutcome::Failed { reason: FailReason::Pending, notify: n, .. } if *n == notify)
}

// --- the warm-up and pending turns

/// A read-only session never runs the project's build: no warm-up, no cache folder, without a change.
#[tokio::test]
async fn no_warm_up_in_a_session_without_edits() {
    let (_dir, project, cache) = setup();
    let runs = Runs::checks(vec![]);
    let checker = checker(Cached { warm: true, max_unreadable: None }, &runs, &cache);
    for _ in 0..3 {
        let t = turn(&checker, &project);
        assert!(t.checks.is_some());
        assert_eq!(t.after_step(vec![read()]).await, None);
    }
    assert_eq!((checker.warm_ups(), runs.all().len()), (0, 0));
    assert!(!cache.root.exists(), "the cache was created without an edit");
}

#[tokio::test]
async fn the_warm_up_starts_at_the_first_change_once_per_session() {
    let (_dir, project, cache) = setup();
    let runs = Runs::checks(vec![]);
    let checker = checker(Cached { warm: true, max_unreadable: None }, &runs, &cache);
    let t = turn(&checker, &project);
    t.before_change().await;
    assert_eq!(checker.warm_ups(), 1);
    until(|| !checker.warming()).await;
    turn(&checker, &project).before_change().await;
    assert_eq!(checker.warm_ups(), 1);
    let all = runs.all();
    let warm = all.iter().find(|r| is_warm_up(r)).expect("a warm-up ran");
    // The warm-up runs the baseline's command in the key's folder, mounted for it.
    let key_dir = cache.root.join(repo_key(&project));
    assert_eq!(warm.writable, vec![key_dir.clone()]);
    assert!(warm.command.contains(&key_dir.display().to_string()));
    assert!(warm.timeout <= WARM_UP_TIMEOUT);
}

/// A warm-up that's still running: the baseline waits, then the turn is pending (no note, one notice a session, not a
/// timeout); the turn after it finished runs as usual.
#[tokio::test]
async fn a_baseline_due_during_the_warm_up_waits_then_the_turn_is_pending() {
    let (_dir, project, cache) = setup();
    let gate = Arc::new(Semaphore::new(0));
    let checks = Arc::new(AtomicUsize::new(0));
    let (g, c) = (gate.clone(), checks.clone());
    let runs = Runs::new(move |_, run, cancel| {
        let (gate, checks) = (g.clone(), c.clone());
        async move {
            if is_warm_up(&run) {
                tokio::select! {
                    _ = gate.acquire() => ok(""),
                    _ = cancel.cancelled() => aborted(),
                }
            } else if checks.fetch_add(1, Ordering::SeqCst) == 0 {
                ok("")
            } else {
                failed(ERR_X)
            }
        }
        .boxed()
    });
    let checker = checker(Cached { warm: true, max_unreadable: None }, &runs, &cache);
    let started = std::time::Instant::now();
    let first = turn(&checker, &project);
    first.before_change().await;
    assert!(started.elapsed() >= checker.baseline_wait);
    assert_eq!(first.after_step(vec![edited(Some("src/a.rs"))]).await, None);
    let results = first.results();
    assert!(results.len() == 1 && pending(&results[0], true), "{results:?}");
    assert!(notice("fake check", &results[0]).unwrap().contains("still building its cache"));
    // Still warming: pending again, without repeating the notice, and never counted as a timeout.
    for _ in 0..3 {
        let again = turn(&checker, &project);
        assert!(again.checks.is_some(), "turned off");
        again.before_change().await;
        assert!(pending(&again.results()[0], false), "{:?}", again.results());
    }
    assert_eq!(runs.checks_run(), 0);
    assert!(checker.state().contains("building its cache"));
    gate.add_permits(1);
    until(|| !checker.warming()).await;
    let next = turn(&checker, &project);
    next.before_change().await;
    let note = next.after_step(vec![edited(Some("src/a.rs"))]).await.unwrap();
    assert!(note.contains("TS2322 mismatched types"), "{note}");
    assert_eq!(checker.warm_ups(), 1);
}

/// The warm-up belongs to the session: the turn's Esc doesn't stop it; `stop()` (a switch to off, the session's end)
/// and dropping the checker do.
#[tokio::test]
async fn the_warm_up_is_stopped_by_off_and_the_sessions_end_not_by_esc() {
    let (_dir, project, cache) = setup();
    let cancelled = Arc::new(AtomicUsize::new(0));
    let seen = cancelled.clone();
    let runs = Runs::new(move |_, run, cancel| {
        let seen = seen.clone();
        async move {
            if is_warm_up(&run) {
                cancel.cancelled().await;
                seen.fetch_add(1, Ordering::SeqCst);
                aborted()
            } else {
                ok("")
            }
        }
        .boxed()
    });
    let checker = checker(Cached { warm: true, max_unreadable: None }, &runs, &cache);
    let esc = CancellationToken::new();
    let t = TurnLog::new(&checker, &project, true, esc.clone());
    let hook = t.checks.clone().unwrap().before_change;
    let waiting = tokio::spawn(async move { hook.before_change().await });
    until(|| checker.warming()).await;
    esc.cancel();
    let _ = waiting.await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(checker.warming(), "Esc stopped the warm-up");
    assert_eq!(cancelled.load(Ordering::SeqCst), 0);
    // /diagnostics off.
    checker.stop();
    until(|| cancelled.load(Ordering::SeqCst) == 1).await;
    assert!(!checker.warming());
    // The next first change after it starts a new one; the session's end (the checker dropped) stops that.
    turn(&checker, &project).before_change().await;
    assert_eq!(checker.warm_ups(), 2);
    until(|| checker.warming()).await;
    drop(t);
    drop(checker);
    until(|| cancelled.load(Ordering::SeqCst) == 2).await;
}

// --- the per-key lock

/// Two sessions on one key: the second's baseline waits for the lock (and runs once it's free, its timeout counted
/// from then); a wait that runs out is pending, never a timeout.
#[tokio::test]
async fn a_baseline_waits_for_another_sessions_lock_and_the_wait_is_never_a_timeout() {
    let (_dir, project, cache) = setup();
    let key = repo_key(&project);
    cache.key_dir(&key).unwrap();
    // The other session: its own handle on the same cache, as another process has.
    let other = CheckCache::new(cache.root.clone());
    let runs = Runs::checks(vec![]);
    let checker = checker(plain(), &runs, &cache);
    let held = other.try_lock_briefly(&key).unwrap();
    for _ in 0..3 {
        let t = turn(&checker, &project);
        assert!(t.checks.is_some(), "a lock wait turned checking off");
        t.before_change().await;
        assert!(pending(&t.results()[0], true) || pending(&t.results()[0], false), "{:?}", t.results());
    }
    assert_eq!(runs.checks_run(), 0);
    // Freed while the next baseline waits: it runs, with its whole timeout.
    let t = turn(&checker, &project);
    let release = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        drop(held);
    });
    t.before_change().await;
    release.await.unwrap();
    assert_eq!(runs.checks_run(), 1);
    assert_eq!(runs.all()[0].timeout, CHECK_TIMEOUT);
    assert!(t.results().is_empty(), "{:?}", t.results());
}

#[tokio::test]
async fn a_steps_check_skips_quietly_while_another_session_holds_the_lock() {
    let (_dir, project, cache) = setup();
    let runs = Runs::checks(vec![ok(""), failed(ERR_X)]);
    let checker = checker(plain(), &runs, &cache);
    let t = turn(&checker, &project);
    t.before_change().await;
    let other = CheckCache::new(cache.root.clone());
    let held = other.try_lock_briefly(&repo_key(&project)).unwrap();
    assert_eq!(t.after_step(vec![edited(Some("src/a.rs"))]).await, None);
    assert!(pending(&t.results()[0], false), "{:?}", t.results());
    assert_eq!(runs.checks_run(), 1);
    drop(held);
    // The baseline stands: the next step is compared with it.
    assert!(t.after_step(vec![edited(Some("src/a.rs"))]).await.unwrap().contains("TS2322"));
}

// --- unreadable runs

#[tokio::test]
async fn two_unreadable_runs_in_a_row_turn_the_kind_off_said_once() {
    let (_dir, project, cache) = setup();
    let runs = Runs::checks(vec![ok(""), failed("Segmentation fault\n"), failed("Segmentation fault\n")]);
    let checker = checker(Cached { warm: false, max_unreadable: Some(MAX_UNREADABLE) }, &runs, &cache);
    let first = turn(&checker, &project);
    first.before_change().await;
    assert_eq!(first.after_step(vec![edited(None)]).await, None);
    let second = turn(&checker, &project);
    second.before_change().await;
    assert_eq!(second.results().last(), Some(&CheckOutcome::Off { reason: OffReason::Unreadable }));
    assert!(notice("fake check", &CheckOutcome::Off { reason: OffReason::Unreadable }).unwrap().contains("twice in a row"));
    assert!(turn(&checker, &project).checks.is_none());
    assert_eq!(runs.checks_run(), 3);
    assert!(checker.state().contains("couldn't be read"));
    // Turned off and on again: it runs again.
    checker.reset();
    assert!(turn(&checker, &project).checks.is_some());
}

#[tokio::test]
async fn a_readable_run_in_between_resets_the_count() {
    let (_dir, project, cache) = setup();
    let runs = Runs::checks(vec![failed("Segmentation fault\n"), ok(""), failed("Segmentation fault\n"), ok("")]);
    let checker = checker(Cached { warm: false, max_unreadable: Some(MAX_UNREADABLE) }, &runs, &cache);
    for _ in 0..3 {
        turn(&checker, &project).before_change().await;
    }
    assert!(turn(&checker, &project).checks.is_some());
}

/// tsc keeps its behaviour: no limit, so unreadable runs never turn it off.
#[test]
fn tsc_has_no_unreadable_limit_no_cache_and_no_warm_up() {
    let tsc = Typecheck;
    assert_eq!((tsc.max_unreadable(), tsc.cache(), tsc.warm_up(Path::new("/x"))), (None, false, false));
    assert_eq!((tsc.timeout(), tsc.label(), tsc.source()), (CHECK_TIMEOUT, "tsc", None));
}

// --- the mount

/// A check mounts exactly its key's folder writable, never the cache root; the run is otherwise the guarded one.
#[tokio::test]
async fn the_run_mounts_exactly_the_key_folder() {
    let (_dir, project, cache) = setup();
    let runs = Runs::checks(vec![ok(""), ok("")]);
    let checker = checker(plain(), &runs, &cache);
    let t = turn(&checker, &project);
    t.before_change().await;
    t.after_step(vec![edited(None)]).await;
    let key_dir = cache.root.join(repo_key(&project));
    for run in runs.all() {
        assert_eq!(run.writable, vec![key_dir.clone()]);
        let options = sandboxed_options(&run);
        assert_eq!(options.writable, vec![key_dir.clone()]);
        assert!(!options.writable.contains(&cache.root));
        assert!(options.sandbox && options.project_read_only && !options.network);
    }
    // The lock and the record sit beside it, outside the mount.
    assert!(cache.root.join(format!("{}.lock", repo_key(&project))).exists());
    assert!(cache.root.join(format!("{}.json", repo_key(&project))).exists());
}

/// The typecheck mounts nothing writable.
#[tokio::test]
async fn a_kind_without_a_cache_mounts_nothing() {
    let dir = Scratch::new();
    dir.put("tsconfig.json", "{}");
    dir.put("node_modules/.bin/tsc", "");
    let runs = Runs::checks(vec![]);
    let mut c = Checker::new(Arc::new(Typecheck), Some(runs.run.clone()));
    c.sandbox_works = Some(Arc::new(|| true));
    let t = turn(&Arc::new(c), &dir.0);
    t.before_change().await;
    assert_eq!(runs.all().len(), 1);
    assert!(runs.all()[0].writable.is_empty());
}

#[tokio::test]
async fn a_symlinked_key_folder_is_refused_and_said_once() {
    let (dir, project, cache) = setup();
    std::fs::create_dir_all(&cache.root).unwrap();
    let elsewhere = dir.0.join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    std::os::unix::fs::symlink(&elsewhere, cache.root.join(repo_key(&project))).unwrap();
    let runs = Runs::checks(vec![]);
    let checker = checker(Cached { warm: true, max_unreadable: None }, &runs, &cache);
    let t = turn(&checker, &project);
    t.before_change().await;
    assert!(
        matches!(&t.results()[..], [CheckOutcome::Failed { reason: FailReason::CacheRefused, notify: true, tool: Some(why), .. }] if why.contains("symlink")),
        "{:?}",
        t.results()
    );
    assert!(notice("fake check", &t.results()[0]).unwrap().contains("won't use"));
    assert_eq!(t.after_step(vec![edited(None)]).await, None);
    assert_eq!((runs.all().len(), checker.warm_ups()), (0, 0));
    let again = turn(&checker, &project);
    again.before_change().await;
    assert!(matches!(&again.results()[..], [CheckOutcome::Failed { reason: FailReason::CacheRefused, notify: false, .. }]));
}

/// No cache configured (`SessionInit::check_cache` `None`): a kind that needs one doesn't run, quietly.
#[tokio::test]
async fn without_a_cache_a_kind_that_needs_one_doesnt_run() {
    let (_dir, project, _cache) = setup();
    let runs = Runs::checks(vec![]);
    let mut c = Checker::new(Arc::new(plain()), Some(runs.run.clone()));
    c.sandbox_works = Some(Arc::new(|| true));
    assert!(turn(&Arc::new(c), &project).checks.is_none());
}

// --- disk

#[tokio::test]
async fn low_free_space_skips_the_warm_up_and_the_check_with_one_notice() {
    let (_dir, project, cache) = setup();
    let mut low = CheckCache::new(cache.root.clone());
    low.free_space = Arc::new(|_| Some(Space { available: GB, total: 500 * GB }));
    let low = Arc::new(low);
    let runs = Runs::checks(vec![]);
    let checker = checker(Cached { warm: true, max_unreadable: None }, &runs, &low);
    let first = turn(&checker, &project);
    first.before_change().await;
    assert!(matches!(&first.results()[..], [CheckOutcome::Failed { reason: FailReason::LowDisk, notify: true, .. }]));
    assert_eq!(notice("fake check", &first.results()[0]).unwrap(), "✻ Less than 5 GB free; Marv isn't running fake check.");
    let second = turn(&checker, &project);
    second.before_change().await;
    assert!(matches!(&second.results()[..], [CheckOutcome::Failed { reason: FailReason::LowDisk, notify: false, .. }]));
    assert_eq!((runs.all().len(), checker.warm_ups()), (0, 0));
}

/// A key folder past its cap after a run: deleted, and the kind off for the session, said at the next change.
#[tokio::test]
async fn the_per_key_cap_turns_the_kind_off_and_deletes_the_folder() {
    let (_dir, project, cache) = setup();
    let mut small = CheckCache::new(cache.root.clone());
    small.free_space = cache.free_space.clone();
    small.key_cap = 1000;
    small.size_every = Duration::ZERO;
    let small = Arc::new(small);
    // A build script filling its cache.
    let runs = Runs::new(|_, run, _| {
        std::fs::write(run.writable[0].join("big"), vec![b'x'; 5000]).unwrap();
        async { ok("") }.boxed()
    });
    let checker = checker(plain(), &runs, &small);
    let t = turn(&checker, &project);
    t.before_change().await;
    checker.settle().await;
    let key_dir = small.root.join(repo_key(&project));
    assert!(!key_dir.exists(), "the folder over its cap is still there");
    // The rest of this turn checks nothing.
    assert_eq!(t.after_step(vec![edited(None)]).await, None);
    assert_eq!(runs.all().len(), 1);
    let next = turn(&checker, &project);
    next.before_change().await;
    assert_eq!(next.results(), [CheckOutcome::Off { reason: OffReason::CacheTooBig }]);
    assert!(notice("fake check", &next.results()[0]).unwrap().contains("passed its size limit"));
    assert!(turn(&checker, &project).checks.is_none());
    assert_eq!(runs.all().len(), 1);
}

/// The session's first change prunes the cache (old and orphaned folders), never its own key.
#[tokio::test]
async fn the_first_change_prunes_the_cache() {
    let (dir, project, cache) = setup();
    cache.key_dir("orphan").unwrap();
    cache.touch("orphan", &dir.0.join("deleted-worktree"), crate::time::now_ms());
    let runs = Runs::checks(vec![]);
    let checker = checker(plain(), &runs, &cache);
    turn(&checker, &project).before_change().await;
    until(|| !cache.root.join("orphan").exists()).await;
    assert!(cache.root.join(repo_key(&project)).exists());
}

// --- nothing runs on the host

/// The marker PATH of §6.4: fake `cargo`, `go`, `rustup`, `pyright`, `basedpyright` and `git` that leave a marker
/// when run. Detection, the key, warm-up scheduling, sizing, pruning, eviction and clearing for a Rust, a Go and a
/// Python project (each a worktree) leave none. Runs in a child process of this test binary, so its PATH is the
/// fakes' alone without touching the other tests'.
///
/// Proven: made `repo_key` run `git rev-parse --git-common-dir` → the `git` marker appeared, failed.
#[test]
fn nothing_marv_does_around_a_check_runs_anything_on_the_host() {
    if let Some(markers) = std::env::var_os("MARV_HOST_SPAWN_MARKERS") {
        return host_spawn_probe(Path::new(&markers));
    }
    let dir = Scratch::new();
    let (bin, markers) = (dir.0.join("bin"), dir.0.join("markers"));
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(&markers).unwrap();
    for tool in ["cargo", "go", "rustup", "pyright", "basedpyright", "git"] {
        let path = bin.join(tool);
        std::fs::write(&path, format!("#!/bin/sh\n: > '{}/{tool}'\n", markers.display())).unwrap();
        std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    }
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "diagnostics::machinery_tests::nothing_marv_does_around_a_check_runs_anything_on_the_host"])
        .args(["--nocapture", "--test-threads=1"])
        .env("PATH", &bin)
        .env("MARV_HOST_SPAWN_MARKERS", &markers)
        .output()
        .unwrap();
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(out.status.success(), "{text}");
    assert!(text.contains("1 passed"), "the probe didn't run: {text}");
}

fn host_spawn_probe(markers: &Path) {
    let left = || -> Vec<String> {
        std::fs::read_dir(markers).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect()
    };
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    runtime.block_on(async {
        let dir = Scratch::new();
        for (name, marker) in [("rust", "Cargo.toml"), ("go", "go.mod"), ("python", "pyproject.toml")] {
            // A worktree of a repository, as Marv's own workflow has them.
            let (repo, tree) = (dir.0.join(format!("{name}-repo")), dir.0.join(format!("{name}-tree")));
            let admin = repo.join(".git/worktrees/tree");
            std::fs::create_dir_all(&admin).unwrap();
            std::fs::create_dir_all(&tree).unwrap();
            std::fs::write(tree.join(".git"), format!("gitdir: {}\n", admin.display())).unwrap();
            std::fs::write(admin.join("commondir"), "../..\n").unwrap();
            std::fs::write(admin.join("gitdir"), format!("{}\n", tree.join(".git").display())).unwrap();
            for project in [&repo, &tree] {
                std::fs::write(project.join(marker), "").unwrap();
                std::fs::write(project.join("Fake.toml"), "").unwrap();
                std::fs::write(project.join("rust-toolchain.toml"), "[toolchain]\nchannel = \"nightly\"\n").unwrap();
                std::fs::write(project.join("Cargo.lock"), "version = 3\n").unwrap();
            }
            assert_eq!(repo_key(&repo), repo_key(&tree));
            let mut cache = CheckCache::new(dir.0.join("cache"));
            cache.size_every = Duration::ZERO;
            cache.total_cap = 1;
            let cache = Arc::new(cache);
            let runs = Runs::checks(vec![ok(""), failed(ERR_X)]);
            let checker = checker(Cached { warm: true, max_unreadable: None }, &runs, &cache);
            let t = turn(&checker, &tree);
            t.before_change().await;
            until(|| !checker.warming()).await;
            let _ = t.after_step(vec![edited(Some("src/a.rs"))]).await;
            checker.settle().await;
            let key = repo_key(&tree);
            cache.key_dir(&key).unwrap();
            cache.measure(&key, 1);
            cache.prune("none", crate::time::now_ms());
            cache.evict("none", crate::time::now_ms());
            let _ = cache.clear(&key);
            let _ = super::tsc::detect_checker(&tree);
            // The real Rust kind: detection, its command, the warm-up and a step's check (the runner a stand-in).
            assert_eq!(super::cargo::Cargo.prepare(&tree), name == "rust");
            if name == "rust" {
                let runs = Runs::checks(vec![ok(""), CommandResult { exit_code: Some(127), ..Default::default() }]);
                let checker = checker_of(Arc::new(super::cargo::Cargo), &runs, &cache);
                let t = turn(&checker, &tree);
                t.before_change().await;
                until(|| !checker.warming()).await;
                let _ = t.after_step(vec![edited(Some("src/lib.rs"))]).await;
                checker.settle().await;
                let _ = checker.state();
                assert!(runs.all().iter().any(|r| r.command.contains("cargo check")));
            }
        }
    });
    assert!(left().is_empty(), "ran on the host: {:?}", left());
    // The control: the fakes do leave a marker when something runs them.
    let _ = std::process::Command::new("git").arg("--version").status();
    assert_eq!(left(), ["git"], "the marker PATH doesn't work");
}

/// `low_priority` runs its command with bash, not `sh` (dash on Debian and Ubuntu), and a `nice` or `ionice` that's
/// refused or talks is left out instead of putting a line in the check's output (stdout and stderr are one output in
/// a check): macOS's `nice` under Seatbelt prints "setpriority: Operation not permitted" and runs the command anyway;
/// some containers refuse `ionice`.
///
/// Proven: put `sh -c` back → failed (bash as `sh` runs in POSIX mode; dash has no `[[ ]]`); dropped the `nice` probe
/// → the talking `nice`'s warning came out; dropped the `ionice` probe → the refusing `ionice`'s "refused" came out.
#[test]
fn low_priority_runs_bash_and_survives_a_refused_nice_or_ionice() {
    let stand_ins = [
        // BSD's nice under Seatbelt: warns, then runs the command.
        ("nice", "#!/bin/sh\necho 'nice: setpriority: Operation not permitted' >&2\nshift 2\nexec \"$@\"\n"),
        ("nice", "#!/bin/sh\necho 'nice: refused' >&2\nexit 1\n"),
        ("ionice", "#!/bin/sh\necho 'ionice: refused'\nexit 1\n"),
    ];
    for (tool, script) in stand_ins {
        let dir = Scratch::new();
        dir.put(&format!("bin/{tool}"), script);
        std::fs::set_permissions(dir.0.join("bin").join(tool), std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        let command = low_priority(&[], "[[ -n \"$BASH_VERSION\" ]] && ! shopt -oq posix && echo bash-ran");
        let path = format!("{}:{}", dir.0.join("bin").display(), std::env::var("PATH").unwrap_or_default());
        let out = std::process::Command::new("/bin/sh").arg("-c").arg(&command).env("PATH", path).output().unwrap();
        let (stdout, stderr) = (String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        assert_eq!((stdout.as_ref(), stderr.as_ref()), ("bash-ran\n", ""), "{tool}: {script}");
        assert!(out.status.success(), "{tool}");
    }
}

/// `low_priority` runs the whole command, pipes and all, niced, with its environment.
#[test]
fn low_priority_wraps_the_whole_command() {
    let command = low_priority(&[("GREETING", "it's me".to_string())], "echo \"$GREETING\" | tr a-z A-Z && ps -o nice= -p $$");
    let out = std::process::Command::new("/bin/sh").arg("-c").arg(&command).output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.first().copied(), Some("IT'S ME"), "{text}");
    let niceness: i32 = lines.get(1).and_then(|n| n.trim().parse().ok()).unwrap_or_else(|| panic!("{text}"));
    // Checked on Linux, where the checks run under bwrap: the macOS runners' `ps` reported 0 for the niced shell.
    if cfg!(target_os = "linux") {
        assert!(niceness >= 10, "{text}");
    }
    assert!(build_jobs() >= 1);
}

/// The key folder is checked again once the lock is held, right before it's mounted: one swapped for a symlink while
/// the check waited is refused, and one evicted meanwhile is made again.
#[tokio::test]
async fn the_key_folder_is_checked_again_under_the_lock() {
    let (dir, project, cache) = setup();
    let key = repo_key(&project);
    let key_dir = cache.key_dir(&key).unwrap();
    let other = CheckCache::new(cache.root.clone());
    let runs = Runs::checks(vec![]);
    let checker = checker(plain(), &runs, &cache);
    for swap in [true, false] {
        let held = other.try_lock_briefly(&key).unwrap();
        let t = turn(&checker, &project);
        let hook = t.checks.clone().unwrap().before_change;
        let waiting = tokio::spawn(async move { hook.before_change().await });
        tokio::time::sleep(Duration::from_millis(50)).await;
        std::fs::remove_dir_all(&key_dir).unwrap();
        if swap {
            let elsewhere = dir.0.join("elsewhere");
            std::fs::create_dir_all(&elsewhere).unwrap();
            std::os::unix::fs::symlink(&elsewhere, &key_dir).unwrap();
        }
        drop(held);
        waiting.await.unwrap();
        if swap {
            assert!(
                matches!(&t.results()[..], [CheckOutcome::Failed { reason: FailReason::CacheRefused, .. }]),
                "{:?}",
                t.results()
            );
            assert_eq!(runs.all().len(), 0);
            std::fs::remove_file(&key_dir).unwrap();
            checker.reset();
        } else {
            assert_eq!(runs.all().len(), 1);
            assert_eq!(runs.all()[0].writable, vec![key_dir.clone()]);
            assert!(std::fs::symlink_metadata(&key_dir).unwrap().is_dir());
        }
    }
}

// --- the Rust kind (cargo check) through the machinery

/// A Rust project (the stand-in runner answers with real cargo output).
fn rust_setup() -> (Scratch, PathBuf, Arc<CheckCache>) {
    let (dir, project, cache) = setup();
    dir.put("project/Cargo.toml", "[package]\nname = \"fixture\"\n");
    dir.put("project/Cargo.lock", "version = 3\n");
    (dir, project, cache)
}

/// A Rust project the kind can't check (no `Cargo.lock`) runs nothing and says why in `/diagnostics` only.
#[tokio::test]
async fn a_rust_project_without_a_lockfile_runs_nothing_and_says_why_in_diagnostics() {
    let (_dir, project, cache) = rust_setup();
    std::fs::remove_file(project.join("Cargo.lock")).unwrap();
    let runs = Runs::checks(vec![]);
    let checker = checker_of(Arc::new(super::cargo::Cargo), &runs, &cache);
    assert!(turn(&checker, &project).checks.is_none());
    assert_eq!(runs.all().len(), 0);
    assert!(checker.state_at(&project).starts_with("skipped: no Cargo.lock"), "{}", checker.state_at(&project));
    std::fs::write(project.join("Cargo.lock"), "version = 3\n").unwrap();
    assert_eq!(checker.state_at(&project), "ready");
    assert!(turn(&checker, &project).checks.is_some());
}

/// The kind warms up at the first change, runs in its key folder (target dir there, never the project's), and tells
/// an error once: baseline clean, then an edit adds errors (lib and lib-test copies folded), the same output again
/// tells nothing, and fixing them tells nothing.
#[tokio::test]
async fn cargo_check_tells_new_errors_once_from_its_key_folder() {
    let (_dir, project, cache) = rust_setup();
    let warnings = include_str!("fixtures/cargo-warnings.jsonl");
    let errors = include_str!("fixtures/cargo-errors.jsonl");
    let runs = Runs::checks(vec![ok(warnings), failed(errors), failed(errors), ok(warnings)]);
    let checker = checker_of(Arc::new(super::cargo::Cargo), &runs, &cache);
    let t = turn(&checker, &project);
    t.before_change().await;
    assert_eq!(checker.warm_ups(), 1);
    let note = t.after_step(vec![edited(Some("src/lib.rs"))]).await.unwrap();
    assert!(note.contains("cargo check found 3 errors that weren't in the previous check."), "{note}");
    assert!(note.contains("src/lib.rs:2:5 E0308 mismatched types: expected `u32`, found `&str`"), "{note}");
    assert_eq!(t.after_step(vec![edited(Some("src/lib.rs"))]).await, None);
    assert_eq!(t.after_step(vec![edited(Some("src/lib.rs"))]).await, None);
    let key_dir = cache.root.join(repo_key(&project));
    for run in runs.all() {
        assert_eq!(run.writable, vec![key_dir.clone()]);
        let command = run.command.replace("'\\''", "'");
        assert!(command.contains(&format!("--target-dir '{}/target'", key_dir.display())), "{command}");
    }
    let notice = notice(super::cargo::LABEL, &t.results()[0]).unwrap();
    assert!(notice.starts_with("✻ cargo check: 3 new errors, told the model:"), "{notice}");
}

/// No cargo in the sandbox: the kind goes off for the session quietly (recorded, no notice), `/diagnostics` says
/// so, and switching diagnostics off and on tries again.
#[tokio::test]
async fn a_missing_cargo_turns_the_kind_off_quietly() {
    let (_dir, project, cache) = rust_setup();
    let missing = CommandResult { exit_code: Some(127), ..Default::default() };
    let runs = Runs::checks(vec![missing, ok("")]);
    let checker = checker_of(Arc::new(super::cargo::Cargo), &runs, &cache);
    let t = turn(&checker, &project);
    t.before_change().await;
    assert_eq!(t.results(), [CheckOutcome::Off { reason: OffReason::NotInstalled }]);
    assert_eq!(notice(super::cargo::LABEL, &t.results()[0]), None);
    assert_eq!(checker.state(), "not installed (Marv doesn't install toolchains)");
    assert!(turn(&checker, &project).checks.is_none());
    assert_eq!(runs.checks_run(), 1);
    checker.reset();
    let again = turn(&checker, &project);
    again.before_change().await;
    assert_eq!(runs.checks_run(), 2);
}
