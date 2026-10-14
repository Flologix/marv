//! The checks after a step's file changes, per turn and per session. Ports Marv's `src/diagnostics/turn.ts`.
//!
//! A [`CheckKind`] says what to run (the baseline, then a step's changed files), how to parse it, and what to tell the
//! model; a [`Checker`] holds what lasts across turns (timeouts, notices given) and makes each turn's [`TurnChecks`]:
//! - `before_change`: the baseline before the turn's first change, started (not awaited) by the edit tools' preview
//!   and awaited before writing, so it's right in yolo too. Later calls wait for the same one.
//! - `after_step`: after a step that changed files, the new problems only (counted, matched without line numbers),
//!   as a note appended as a user message from Marv; each check becomes the next baseline, so a problem is told once.
//!
//! What runs is the project's own code (its compiler, its check scripts), which a cloned repository could have
//! planted, so it runs only sandboxed with the project read-only and without network ([`sandboxed_options`]); without
//! the sandbox it doesn't run at all (said once: `Off { NoSandbox }`). 60 s per check (TypeScript 7 checks Marv's 24k
//! lines in half a second; TypeScript 5 is about ten times slower); two timeouts turn a kind off for the session. Main
//! agent only.
//!
//! Port notes: Marv's `RunCheck` took `{root, signal, timeoutMs}`; here it takes a [`CheckRun`], which also carries
//! the pass's extra read-only mounts (the `marv` binary for the project checks' runner). A runner that throws (Marv)
//! is one whose output can't be read (here): a spawn failure comes back as `[couldn't start the command: …]`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration, Instant};

use futures::FutureExt;
use futures::future::BoxFuture;
use marv_agent::ToolRun;
use marv_tools::bash::{MAX_CAPTURE, RunOptions};
use tokio_util::sync::CancellationToken;

use super::tsc::{TsError, format_error};

pub const CHECK_TIMEOUT: Duration = Duration::from_secs(60);
/// Timeouts after which a session stops checking: a huge project would otherwise add a minute to every step.
pub const MAX_TIMEOUTS: u32 = 2;
/// Errors the transcript shows (the model gets up to `MAX_REPORTED`).
const NOTICE_LINES: usize = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailReason {
    Timeout,
    Unreadable,
    /// The checker's interpreter isn't in the sandbox (tsc's `node` from a hidden toolchain folder).
    MissingInterpreter,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OffReason {
    NoSandbox,
    Timeouts,
}

/// How a check went, for one kind of problem `E`.
#[derive(Debug, Clone, PartialEq)]
pub enum CheckOutcome<E> {
    /// It ran. `added`: the problems the model was told about.
    Done { ms: u64, before: usize, errors: usize, added: Vec<E> },
    /// It couldn't tell what's new, so nothing was said. `notify`: the session's first such result.
    Failed { ms: u64, reason: FailReason, tool: Option<String>, notify: bool },
    /// No checks from here on (said once per session).
    Off { reason: OffReason },
}

/// The typecheck's result.
pub type CheckResult = CheckOutcome<TsError>;

/// A finished check command.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommandResult {
    pub output: String,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    /// Stopped by the turn's stop (Esc): not a failure.
    pub aborted: bool,
}

/// One check command to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckRun {
    pub command: String,
    pub root: PathBuf,
    /// Extra paths the command may read (the pass's [`CheckPass::read_only`]).
    pub read_only: Vec<PathBuf>,
    pub timeout: Duration,
}

/// Runs a check command; tests pass a stand-in. The default is [`sandboxed`].
pub type RunCheck = Arc<dyn Fn(CheckRun, CancellationToken) -> BoxFuture<'static, CommandResult> + Send + Sync>;

/// How a check command runs: in the sandbox, the project read-only, no network, and the repository's `.git` (a
/// worktree's common one too) read-only, so a check can ask git what changed. The guard test holds this to it.
pub fn sandboxed_options(run: &CheckRun) -> RunOptions {
    let mut read_only = marv_sandbox::git_guard::worktree_git(&run.root);
    read_only.extend(run.read_only.iter().cloned());
    RunOptions {
        command: run.command.clone(),
        root: run.root.clone(),
        sandbox: true,
        network: false,
        timeout: run.timeout,
        read_only,
        writable: Vec::new(),
        placeholders: Vec::new(),
        project_read_only: true,
        env: None,
        forwards: Vec::new(),
    }
}

/// The real runner: [`sandboxed_options`] through `marv_tools::bash::run_command`.
pub fn sandboxed() -> RunCheck {
    Arc::new(|run: CheckRun, cancel: CancellationToken| {
        async move {
            let result =
                marv_tools::bash::run_command(sandboxed_options(&run), &marv_sandbox::Sandbox::from_env(true), &cancel).await;
            CommandResult {
                output: result.output,
                exit_code: result.exit_code,
                timed_out: result.timed_out,
                aborted: result.aborted,
            }
        }
        .boxed()
    })
}

/// Which problems a pass re-checks (the rest of the baseline stands).
pub type Covers<E> = Box<dyn Fn(&E) -> bool + Send + Sync>;

/// One pass of a check: the command, and which problems it can report (all, when `covers` is `None`).
pub struct CheckPass<E> {
    pub command: String,
    pub covers: Option<Covers<E>>,
    /// Paths outside the project the command must read, mounted read-only (the `marv` binary for the project checks).
    pub read_only: Vec<PathBuf>,
}

/// What one kind of check is.
pub trait CheckKind<E>: Send + Sync {
    /// Whether anything can run in this project this turn (a compiler, some checks).
    fn prepare(&self, root: &Path) -> bool;
    /// `None` = the baseline (everything); `Some(files)` = a step that changed those (project-relative). `None` back:
    /// nothing it checks could have changed, which costs nothing.
    fn pass(&self, root: &Path, changed: Option<&[String]>) -> Option<CheckPass<E>>;
    fn parse(&self, output: &str, exit_code: Option<i32>) -> Option<Vec<E>>;
    fn missing(&self, output: &str, exit_code: Option<i32>) -> Option<String>;
    fn key(&self, problem: &E) -> String;
    fn note(&self, added: &[E]) -> String;
}

/// The problems in `now` that `before` didn't have, matched by `key`; counted, so a second copy of an old one is new.
pub fn new_problems<E: Clone>(before: &[E], now: &[E], key: impl Fn(&E) -> String) -> Vec<E> {
    let mut left: std::collections::HashMap<String, usize> = Default::default();
    for e in before {
        *left.entry(key(e)).or_default() += 1;
    }
    now.iter()
        .filter(|e| match left.get_mut(&key(e)) {
            Some(n) if *n > 0 => {
                *n -= 1;
                false
            }
            _ => true,
        })
        .cloned()
        .collect()
}

/// Whether a tool's result changed a file (it has a diff).
fn has_diff(run: &ToolRun) -> bool {
    run.display.as_ref().and_then(|d| d.downcast_ref::<crate::runner::ToolDisplay>()).is_some_and(|d| d.diff.is_some())
}

/// The files a step's results changed, or `None` when a change doesn't say which (then everything is checked).
pub fn changed_files(runs: &[ToolRun]) -> Option<Vec<String>> {
    let mut files = Vec::new();
    for run in runs.iter().filter(|r| has_diff(r)) {
        files.push(run.changed.clone()?);
    }
    Some(files)
}

/// What the session hands the tools (`before_change`) and the loop (`after_step`) for one turn.
#[derive(Clone)]
pub struct TurnChecks {
    pub before_change: Arc<dyn marv_tools::BeforeChange>,
    pub after_step: marv_agent::agent::AfterStep,
}

/// Turn options: the session's sandbox setting, the turn's stop (must cancel when the turn ends, however it ends: a
/// baseline started by a declined call's preview would otherwise run on and its timeout count against the session),
/// and where status ("Checking…") and results go.
pub struct TurnOptions<E> {
    pub root: PathBuf,
    pub sandbox: bool,
    pub cancel: CancellationToken,
    pub on_status: Arc<dyn Fn(bool) + Send + Sync>,
    pub on_result: Arc<dyn Fn(CheckOutcome<E>) + Send + Sync>,
}

/// One per kind of check and session: survives `clear()` and `resume()`.
pub struct Checker<E> {
    pub kind: Arc<dyn CheckKind<E>>,
    /// `None`: [`sandboxed`].
    pub run: Option<RunCheck>,
    pub timeout: Duration,
    /// Whether bwrap works here; `None`: `marv_sandbox::sandbox_available`. Tests pass a stand-in.
    pub sandbox_works: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
    timeouts: AtomicU32,
    told_no_sandbox: AtomicBool,
    told_unreadable: AtomicBool,
}

impl<E: Clone + Send + Sync + 'static> Checker<E> {
    pub fn new(kind: Arc<dyn CheckKind<E>>, run: Option<RunCheck>) -> Self {
        Self {
            kind,
            run,
            timeout: CHECK_TIMEOUT,
            sandbox_works: None,
            timeouts: Default::default(),
            told_no_sandbox: Default::default(),
            told_unreadable: Default::default(),
        }
    }

    /// Checking was turned off and on again: forget what made the session give up, and the notices given.
    pub fn reset(&self) {
        self.timeouts.store(0, Ordering::SeqCst);
        self.told_no_sandbox.store(false, Ordering::SeqCst);
        self.told_unreadable.store(false, Ordering::SeqCst);
    }

    /// This turn's hooks; `None` when nothing can run (no compiler, no checks, two timeouts already).
    pub fn turn(self: &Arc<Self>, options: TurnOptions<E>) -> Option<TurnChecks> {
        if self.timeouts.load(Ordering::SeqCst) >= MAX_TIMEOUTS || !self.kind.prepare(&options.root) {
            return None;
        }
        let works = self.sandbox_works.as_ref().map_or_else(marv_sandbox::sandbox_available, |f| f());
        if !options.sandbox || !works {
            // Said when it would have mattered (the first change), not on every turn of a project nobody edits.
            return Some(TurnChecks {
                before_change: Arc::new(NoSandbox { checker: self.clone(), on_result: options.on_result }),
                after_step: Arc::new(|_| async { None }.boxed()),
            });
        }
        let turn = Arc::new(Turn {
            checker: self.clone(),
            run: self.run.clone().unwrap_or_else(sandboxed),
            options,
            unavailable: AtomicBool::new(false),
            baseline: tokio::sync::Mutex::new(Baseline::NotTaken),
        });
        let after = turn.clone();
        Some(TurnChecks {
            before_change: Arc::new(BaselineHook(turn)),
            after_step: Arc::new(move |runs| {
                let turn = after.clone();
                async move { turn.after_step(runs).await }.boxed()
            }),
        })
    }
}

/// Calls a UI callback: whatever it does, a check never panics into the turn.
fn quietly(f: impl FnOnce()) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
}

/// Without the sandbox: nothing runs, and the session's first change says so once.
struct NoSandbox<E> {
    checker: Arc<Checker<E>>,
    on_result: Arc<dyn Fn(CheckOutcome<E>) + Send + Sync>,
}

#[async_trait::async_trait]
impl<E: Send + Sync + 'static> marv_tools::BeforeChange for NoSandbox<E> {
    async fn before_change(&self) {
        if !self.checker.told_no_sandbox.swap(true, Ordering::SeqCst) {
            quietly(|| (self.on_result)(CheckOutcome::Off { reason: OffReason::NoSandbox }));
        }
    }
}

/// The turn's baseline: not taken yet, or taken (`None`: it couldn't be, so this turn says nothing).
enum Baseline<E> {
    NotTaken,
    Taken(Option<Vec<E>>),
}

/// One turn of one kind of check.
struct Turn<E> {
    checker: Arc<Checker<E>>,
    run: RunCheck,
    options: TurnOptions<E>,
    /// A check that failed (not one Esc stopped) makes checking unavailable for the rest of this turn.
    unavailable: AtomicBool,
    /// Held while the baseline is taken, so later callers wait for the same one, and while a step is checked.
    baseline: tokio::sync::Mutex<Baseline<E>>,
}

struct BaselineHook<E>(Arc<Turn<E>>);

#[async_trait::async_trait]
impl<E: Clone + Send + Sync + 'static> marv_tools::BeforeChange for BaselineHook<E> {
    /// Takes the baseline before the turn's first change; later calls wait for the same one. Never fails.
    async fn before_change(&self) {
        let turn = &self.0;
        let mut baseline = turn.baseline.lock().await;
        if let Baseline::NotTaken = *baseline {
            let taken = match turn.checker.kind.pass(&turn.options.root, None) {
                Some(pass) => turn.check(&pass).await.map(|(errors, _)| errors),
                None => Some(Vec::new()),
            };
            *baseline = Baseline::Taken(taken);
        }
    }
}

/// Ends "checking" however the step's check ends, a dropped future included.
struct Checking<'a>(&'a (dyn Fn(bool) + Send + Sync));

impl Drop for Checking<'_> {
    fn drop(&mut self) {
        quietly(|| (self.0)(false));
    }
}

impl<E: Clone + Send + Sync + 'static> Turn<E> {
    fn tell(&self, result: CheckOutcome<E>) {
        quietly(|| (self.options.on_result)(result));
    }

    /// The session's first unreadable-or-worse result asks for a notice; later ones don't repeat it.
    fn notify_once(&self) -> bool {
        !self.checker.told_unreadable.swap(true, Ordering::SeqCst)
    }

    /// Runs a pass's command: its problems and how long it took, or `None` (stopped, failed: then it was told).
    async fn check(&self, pass: &CheckPass<E>) -> Option<(Vec<E>, u64)> {
        let (checker, cancel) = (&self.checker, &self.options.cancel);
        let started = Instant::now();
        let run = CheckRun {
            command: pass.command.clone(),
            root: self.options.root.clone(),
            read_only: pass.read_only.clone(),
            timeout: checker.timeout,
        };
        let result = (self.run)(run, cancel.clone()).await;
        let ms = started.elapsed().as_millis() as u64;
        if result.aborted || cancel.is_cancelled() {
            return None; // Esc: the turn is ending, nothing to say
        }
        if result.timed_out {
            let timeouts = checker.timeouts.fetch_add(1, Ordering::SeqCst) + 1;
            self.unavailable.store(true, Ordering::SeqCst);
            self.tell(CheckOutcome::Failed { ms, reason: FailReason::Timeout, tool: None, notify: false });
            if timeouts == MAX_TIMEOUTS {
                self.tell(CheckOutcome::Off { reason: OffReason::Timeouts });
            }
            return None;
        }
        // A `#!/usr/bin/env node` launcher whose node the sandbox can't see (hidden home, e.g. mise's): that's worth
        // saying plainly, not as an unreadable output.
        let missing = checker.kind.missing(&result.output, result.exit_code);
        // Output at the capture limit was cut off, maybe mid-list: what's left would make problems look fixed or new.
        let errors = if missing.is_none() && result.output.len() < MAX_CAPTURE {
            // A bug in reading the output must never end the turn.
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| checker.kind.parse(&result.output, result.exit_code)))
                .ok()
                .flatten()
        } else {
            None
        };
        let Some(errors) = errors else {
            self.unavailable.store(true, Ordering::SeqCst);
            let reason = if missing.is_some() { FailReason::MissingInterpreter } else { FailReason::Unreadable };
            self.tell(CheckOutcome::Failed { ms, reason, tool: missing, notify: self.notify_once() });
            return None;
        };
        Some((errors, ms))
    }

    /// After a step: the note for the model when its file changes added problems, otherwise `None`.
    async fn after_step(&self, runs: Vec<ToolRun>) -> Option<String> {
        if !runs.iter().any(has_diff) {
            return None;
        }
        let mut baseline = self.baseline.lock().await;
        let before = match &*baseline {
            Baseline::Taken(Some(before)) => before.clone(),
            // No baseline this turn (nothing was changed through the edit tools), or it couldn't be taken.
            _ => return None,
        };
        let checker = &self.checker;
        if self.unavailable.load(Ordering::SeqCst)
            || self.options.cancel.is_cancelled()
            || checker.timeouts.load(Ordering::SeqCst) >= MAX_TIMEOUTS
        {
            return None;
        }
        let kind = &checker.kind;
        // Nothing this kind checks could have changed.
        let pass = kind.pass(&self.options.root, changed_files(&runs).as_deref())?;
        // The status goes back after the result is told, so the client sees checking, the check, then running.
        quietly(|| (self.options.on_status)(true));
        let _checking = Checking(&*self.options.on_status);
        let (now, ms) = self.check(&pass).await?;
        if self.options.cancel.is_cancelled() {
            return None;
        }
        let covers = |e: &E| pass.covers.as_ref().is_none_or(|c| c(e));
        // Each problem is told once, when it first appears; what this pass didn't re-check stands.
        let checked: Vec<E> = before.iter().filter(|e| covers(e)).cloned().collect();
        let added = new_problems(&checked, &now, |e| kind.key(e));
        let count = before.len();
        let mut next: Vec<E> = before.iter().filter(|e| !covers(e)).cloned().collect();
        next.extend(now);
        let errors = next.len();
        *baseline = Baseline::Taken(Some(next));
        self.tell(CheckOutcome::Done { ms, before: count, errors, added: added.clone() });
        if added.is_empty() {
            return None;
        }
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| kind.note(&added))).ok()
    }
}

/// The typecheck: the project's own tsgo or tsc (Marv's `typecheck` kind).
pub struct Typecheck;

impl CheckKind<TsError> for Typecheck {
    fn prepare(&self, root: &Path) -> bool {
        super::tsc::detect_checker(root).is_some()
    }
    fn pass(&self, root: &Path, _changed: Option<&[String]>) -> Option<CheckPass<TsError>> {
        super::tsc::detect_checker(root).map(|bin| CheckPass {
            command: super::tsc::check_command(&bin),
            covers: None,
            read_only: Vec::new(),
        })
    }
    fn parse(&self, output: &str, exit_code: Option<i32>) -> Option<Vec<TsError>> {
        super::tsc::parse_tsc(output, exit_code)
    }
    fn missing(&self, output: &str, exit_code: Option<i32>) -> Option<String> {
        super::tsc::missing_interpreter(output, exit_code)
    }
    fn key(&self, problem: &TsError) -> String {
        super::tsc::ts_error_key(problem)
    }
    fn note(&self, added: &[TsError]) -> String {
        super::tsc::diagnostics_note(added)
    }
}

/// Several kinds as one: their baselines taken together, their after-step checks side by side, their notes joined
/// into one message. The status is "checking" while any of them runs.
pub fn combine_checks(parts: Vec<TurnChecks>) -> Option<TurnChecks> {
    match parts.len() {
        0 => None,
        1 => parts.into_iter().next(),
        _ => {
            let befores: Vec<_> = parts.iter().map(|p| p.before_change.clone()).collect();
            let afters: Vec<_> = parts.into_iter().map(|p| p.after_step).collect();
            Some(TurnChecks {
                before_change: Arc::new(Together(befores)),
                after_step: Arc::new(move |runs: Vec<ToolRun>| {
                    let steps: Vec<_> = afters.iter().map(|after| after(runs.clone())).collect();
                    async move {
                        let notes: Vec<String> = futures::future::join_all(steps).await.into_iter().flatten().collect();
                        (!notes.is_empty()).then(|| notes.join("\n\n"))
                    }
                    .boxed()
                }),
            })
        }
    }
}

/// Several baselines, taken together.
struct Together(Vec<Arc<dyn marv_tools::BeforeChange>>);

#[async_trait::async_trait]
impl marv_tools::BeforeChange for Together {
    async fn before_change(&self) {
        futures::future::join_all(self.0.iter().map(|b| b.before_change())).await;
    }
}

/// What the transcript shows for a typecheck, or `None`. Error messages quote the project's code: printable.
pub fn check_notice(result: &CheckResult) -> Option<String> {
    use marv_core::printable;
    match result {
        CheckOutcome::Off { reason: OffReason::NoSandbox } => Some("✻ Marv typechecks after the model's edits only in the sandbox (it runs the project's own compiler), so it won't here.".into()),
        CheckOutcome::Off { reason: OffReason::Timeouts } => Some("✻ The typecheck timed out twice, so Marv stopped running it for this session.".into()),
        CheckOutcome::Failed { reason: FailReason::MissingInterpreter, tool, notify: true, .. } => Some(format!(
            "✻ tsc needs {}, which the sandbox can't see (Marv's sandbox hides the home folder except known toolchains). Marv can't tell the model about new type errors.",
            printable(tool.as_deref().unwrap_or("node"))
        )),
        CheckOutcome::Failed { reason: FailReason::Unreadable, notify: true, .. } => Some("✻ Marv couldn't read the typecheck's output here, so it can't tell the model about new errors.".into()),
        CheckOutcome::Failed { .. } => None,
        CheckOutcome::Done { added, .. } if added.is_empty() => None,
        CheckOutcome::Done { added, .. } => {
            let n = added.len();
            let mut lines = vec![format!("✻ Typecheck: {n} new error{}, told the model:", if n == 1 { "" } else { "s" })];
            lines.extend(added.iter().take(NOTICE_LINES).map(|e| printable(&format_error(e))));
            if n > NOTICE_LINES {
                lines.push(format!("… and {} more", n - NOTICE_LINES));
            }
            Some(lines.join("\n"))
        }
    }
}

/// What tests share: tool runs with and without a change, and a stand-in runner.
#[cfg(test)]
pub(crate) mod testing {
    use super::*;

    /// A step's result that changed `path` (it has a diff).
    pub fn edited(path: Option<&str>) -> ToolRun {
        let mut run = ToolRun::new(
            marv_core::ToolResult { output: "Edited".into(), summary: Some("+1 −1".into()), is_error: false },
            "a.ts",
        );
        run.changed = path.map(String::from);
        run.display = Some(Arc::new(crate::runner::ToolDisplay { diff: Some(Default::default()), plan: None }));
        run
    }

    /// A step's result that changed nothing.
    pub fn read() -> ToolRun {
        ToolRun::new(marv_core::ToolResult { output: "contents".into(), summary: Some("1 line".into()), is_error: false }, "a.ts")
    }

    pub fn ok(output: &str) -> CommandResult {
        CommandResult { output: output.into(), exit_code: Some(0), ..Default::default() }
    }
    pub fn failed(output: &str) -> CommandResult {
        CommandResult { output: output.into(), exit_code: Some(2), ..Default::default() }
    }
    pub fn timed_out() -> CommandResult {
        CommandResult { timed_out: true, ..Default::default() }
    }
    pub fn aborted() -> CommandResult {
        CommandResult { aborted: true, ..Default::default() }
    }

    /// A runner that answers each run with `answer(n)` (n from 0), counting the runs and keeping what they ran.
    pub struct Fake {
        pub runs: Arc<std::sync::Mutex<Vec<CheckRun>>>,
        pub run: RunCheck,
    }

    impl Fake {
        pub fn new(answer: impl Fn(usize, &CheckRun, &CancellationToken) -> CommandResult + Send + Sync + 'static) -> Self {
            let runs: Arc<std::sync::Mutex<Vec<CheckRun>>> = Default::default();
            let kept = runs.clone();
            let run: RunCheck = Arc::new(move |run: CheckRun, cancel: CancellationToken| {
                let n = {
                    let mut runs = kept.lock().unwrap();
                    runs.push(run.clone());
                    runs.len() - 1
                };
                let result = answer(n, &run, &cancel);
                async move { result }.boxed()
            });
            Self { runs, run }
        }
        /// Answers in order, then `ok("")`.
        pub fn answers(answers: Vec<CommandResult>) -> Self {
            Self::new(move |n, _, _| answers.get(n).cloned().unwrap_or_else(|| ok("")))
        }
        pub fn count(&self) -> usize {
            self.runs.lock().unwrap().len()
        }
    }

    /// One turn's checks and what they reported.
    pub struct TurnLog<E> {
        pub checks: Option<TurnChecks>,
        pub results: Arc<std::sync::Mutex<Vec<CheckOutcome<E>>>>,
        pub statuses: Arc<std::sync::Mutex<Vec<bool>>>,
    }

    impl<E: Clone + Send + Sync + 'static> TurnLog<E> {
        pub fn new(checker: &Arc<Checker<E>>, root: &Path, sandbox: bool, cancel: CancellationToken) -> Self {
            let results: Arc<std::sync::Mutex<Vec<CheckOutcome<E>>>> = Default::default();
            let statuses: Arc<std::sync::Mutex<Vec<bool>>> = Default::default();
            let (r, st) = (results.clone(), statuses.clone());
            let checks = checker.turn(TurnOptions {
                root: root.to_path_buf(),
                sandbox,
                cancel,
                on_status: Arc::new(move |s| st.lock().unwrap().push(s)),
                on_result: Arc::new(move |x| r.lock().unwrap().push(x)),
            });
            Self { checks, results, statuses }
        }
        pub async fn before_change(&self) {
            if let Some(c) = &self.checks {
                c.before_change.before_change().await;
            }
        }
        pub async fn after_step(&self, runs: Vec<ToolRun>) -> Option<String> {
            match &self.checks {
                Some(c) => (c.after_step)(runs).await,
                None => None,
            }
        }
        pub fn results(&self) -> Vec<CheckOutcome<E>> {
            self.results.lock().unwrap().clone()
        }
        pub fn statuses(&self) -> Vec<bool> {
            self.statuses.lock().unwrap().clone()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use super::testing::*;
    use crate::checks::Scratch;

    const ERR_X: &str = "src/a.ts(1,1): error TS2304: Cannot find name 'x'.\n";
    const ERR_Y: &str = "src/b.ts(2,3): error TS2304: Cannot find name 'y'.\n";

    /// A TypeScript project with its own tsc (empty: the runner is a stand-in).
    fn project() -> Scratch {
        let dir = Scratch::new();
        dir.put("tsconfig.json", "{}");
        dir.put("node_modules/.bin/tsc", "");
        dir
    }

    fn diagnostics(run: &Fake, works: bool) -> Arc<Checker<TsError>> {
        let mut checker = Checker::new(Arc::new(Typecheck), Some(run.run.clone()));
        checker.sandbox_works = Some(Arc::new(move || works));
        Arc::new(checker)
    }

    fn turn(checker: &Arc<Checker<TsError>>, dir: &Scratch) -> TurnLog<TsError> {
        TurnLog::new(checker, &dir.0, true, CancellationToken::new())
    }

    fn notify(results: &[CheckResult]) -> Vec<bool> {
        results.iter().map(|r| matches!(r, CheckOutcome::Failed { notify: true, .. })).collect()
    }

    fn unreadable(r: &CheckResult) -> bool {
        matches!(r, CheckOutcome::Failed { reason: FailReason::Unreadable, notify: true, tool: None, .. })
    }

    #[tokio::test]
    async fn the_baseline_is_taken_once_however_many_changes_ask_for_it() {
        let dir = project();
        let run = Fake::answers(vec![ok("")]);
        let t = turn(&diagnostics(&run, true), &dir);
        tokio::join!(t.before_change(), t.before_change());
        t.before_change().await;
        assert_eq!(run.count(), 1);
    }

    #[tokio::test]
    async fn only_the_errors_a_step_added_are_told_and_they_become_the_next_baseline() {
        let dir = project();
        let both = format!("{ERR_X}{ERR_Y}");
        let run = Fake::answers(vec![failed(ERR_X), failed(&both), failed(&both)]);
        let t = turn(&diagnostics(&run, true), &dir);
        t.before_change().await;
        let note = t.after_step(vec![edited(Some("src/b.ts"))]).await.unwrap();
        assert!(note.contains("found 1 new error"), "{note}");
        assert!(note.contains("src/b.ts:2:3 TS2304 Cannot find name 'y'."));
        assert!(!note.contains("'x'"));
        match t.results().as_slice() {
            [CheckOutcome::Done { before: 1, errors: 2, added, .. }] => assert_eq!(added[0].file.as_deref(), Some("src/b.ts")),
            other => panic!("{other:?}"),
        }
        assert_eq!(t.statuses(), [true, false]);
        // The same errors again: nothing new.
        assert_eq!(t.after_step(vec![edited(Some("src/b.ts"))]).await, None);
    }

    #[tokio::test]
    async fn a_step_that_changed_no_file_isnt_checked() {
        let dir = project();
        let run = Fake::answers(vec![ok("")]);
        let t = turn(&diagnostics(&run, true), &dir);
        t.before_change().await;
        assert_eq!(t.after_step(vec![read()]).await, None);
        assert_eq!(run.count(), 1);
    }

    #[tokio::test]
    async fn without_a_baseline_this_turn_nothing_runs() {
        let dir = project();
        let run = Fake::answers(vec![]);
        let t = turn(&diagnostics(&run, true), &dir);
        assert_eq!(t.after_step(vec![edited(None)]).await, None);
        assert_eq!(run.count(), 0);
    }

    #[tokio::test]
    async fn not_a_typescript_project_nothing_runs() {
        let dir = project();
        std::fs::remove_file(dir.0.join("tsconfig.json")).unwrap();
        let run = Fake::answers(vec![]);
        let t = turn(&diagnostics(&run, true), &dir);
        assert!(t.checks.is_none());
        t.before_change().await;
        assert_eq!(t.after_step(vec![edited(None)]).await, None);
        assert_eq!((run.count(), t.results().len()), (0, 0));
    }

    #[tokio::test]
    async fn without_the_sandbox_nothing_runs_and_the_first_change_says_so_once_per_session() {
        let dir = project();
        let run = Fake::answers(vec![]);
        let checker = diagnostics(&run, true);
        let first = TurnLog::new(&checker, &dir.0, false, CancellationToken::new());
        first.before_change().await;
        first.before_change().await;
        assert_eq!(first.after_step(vec![edited(None)]).await, None);
        let second = TurnLog::new(&checker, &dir.0, false, CancellationToken::new());
        second.before_change().await;
        assert_eq!(run.count(), 0);
        let all: Vec<_> = first.results().into_iter().chain(second.results()).collect();
        assert_eq!(all, [CheckOutcome::Off { reason: OffReason::NoSandbox }]);
    }

    #[tokio::test]
    async fn bwrap_not_working_counts_as_no_sandbox() {
        let dir = project();
        let run = Fake::answers(vec![]);
        let t = turn(&diagnostics(&run, false), &dir);
        t.before_change().await;
        assert_eq!(run.count(), 0);
        assert_eq!(t.results(), [CheckOutcome::Off { reason: OffReason::NoSandbox }]);
    }

    #[tokio::test]
    async fn two_timeouts_turn_checking_off_for_the_session_said_once() {
        let dir = project();
        let run = Fake::answers(vec![timed_out(), timed_out()]);
        let checker = diagnostics(&run, true);
        let first = turn(&checker, &dir);
        first.before_change().await;
        assert_eq!(first.after_step(vec![edited(None)]).await, None); // no baseline: nothing to compare with
        let second = turn(&checker, &dir);
        second.before_change().await;
        let third = turn(&checker, &dir);
        assert!(third.checks.is_none());
        assert_eq!(run.count(), 2);
        let all: Vec<_> = first.results().into_iter().chain(second.results()).collect();
        assert!(matches!(
            all.as_slice(),
            [
                CheckOutcome::Failed { reason: FailReason::Timeout, notify: false, .. },
                CheckOutcome::Failed { reason: FailReason::Timeout, notify: false, .. },
                CheckOutcome::Off { reason: OffReason::Timeouts }
            ]
        ));
    }

    #[tokio::test]
    async fn output_that_cant_be_read_tells_the_model_nothing() {
        let dir = project();
        let run = Fake::answers(vec![ok(""), failed("Segmentation fault\n")]);
        let t = turn(&diagnostics(&run, true), &dir);
        t.before_change().await;
        assert_eq!(t.after_step(vec![edited(None)]).await, None);
        let results = t.results();
        assert!(results.len() == 1 && unreadable(&results[0]), "{results:?}");
    }

    #[tokio::test]
    async fn a_missing_interpreter_is_its_own_failure_and_counts_as_unreadable_for_the_repeat_notices() {
        let dir = project();
        let lost = CommandResult {
            output: "env: ‘node’: No such file or directory\n".into(),
            exit_code: Some(127),
            ..Default::default()
        };
        let run = Fake::answers(vec![ok(""), lost.clone(), lost]);
        let checker = diagnostics(&run, true);
        let t = turn(&checker, &dir);
        t.before_change().await;
        assert_eq!(t.after_step(vec![edited(None)]).await, None);
        assert!(matches!(
            t.results().as_slice(),
            [CheckOutcome::Failed { reason: FailReason::MissingInterpreter, tool: Some(node), notify: true, .. }] if node == "node"
        ));
        // The next turn of the same session: the notice was given.
        let again = turn(&checker, &dir);
        again.before_change().await;
        assert!(matches!(
            again.results().as_slice(),
            [CheckOutcome::Failed { reason: FailReason::MissingInterpreter, notify: false, .. }]
        ));
    }

    #[tokio::test]
    async fn only_the_sessions_first_unreadable_result_asks_for_a_notice() {
        let dir = project();
        let run = Fake::answers(vec![failed("Segmentation fault\n"), failed("Segmentation fault\n")]);
        let checker = diagnostics(&run, true);
        let first = turn(&checker, &dir);
        first.before_change().await;
        let second = turn(&checker, &dir);
        second.before_change().await;
        assert_eq!(notify(&[first.results(), second.results()].concat()), [true, false]);
    }

    #[tokio::test]
    async fn a_result_arriving_after_the_turn_was_stopped_isnt_reported_and_doesnt_use_up_the_notice() {
        let dir = project();
        let run = Fake::new(|n, _, cancel| {
            if n == 0 {
                cancel.cancel();
            }
            failed("Segmentation fault\n")
        });
        let checker = diagnostics(&run, true);
        let first = turn(&checker, &dir);
        first.before_change().await;
        assert_eq!(first.results(), []);
        // The same session, in a new turn: the stopped one didn't spend the notice.
        let again = turn(&checker, &dir);
        again.before_change().await;
        let results = again.results();
        assert!(results.len() == 1 && unreadable(&results[0]), "{results:?}");
    }

    #[tokio::test]
    async fn reset_brings_back_checking_after_timeouts_and_the_one_time_notices() {
        let dir = project();
        let run = Fake::answers(vec![timed_out(), timed_out(), failed("Segmentation fault\n")]);
        let checker = diagnostics(&run, true);
        for _ in 0..2 {
            turn(&checker, &dir).before_change().await;
        }
        assert_eq!(run.count(), 2);
        turn(&checker, &dir).before_change().await;
        assert_eq!(run.count(), 2); // off
        checker.reset();
        let t = turn(&checker, &dir);
        t.before_change().await;
        assert_eq!(run.count(), 3);
        let results = t.results();
        assert!(results.len() == 1 && unreadable(&results[0]), "{results:?}");
    }

    #[tokio::test]
    async fn output_at_the_capture_limit_may_be_cut_mid_list_so_its_unreadable() {
        let dir = project();
        let run = Fake::answers(vec![ok(""), failed(&ERR_Y.repeat(MAX_CAPTURE / ERR_Y.len() + 1))]);
        let t = turn(&diagnostics(&run, true), &dir);
        t.before_change().await;
        assert_eq!(t.after_step(vec![edited(None)]).await, None);
        assert!(unreadable(&t.results()[0]));
    }

    #[tokio::test]
    async fn a_check_stopped_by_esc_tells_the_model_nothing_and_isnt_a_failure() {
        let dir = project();
        let run = Fake::answers(vec![ok(""), aborted()]);
        let t = turn(&diagnostics(&run, true), &dir);
        t.before_change().await;
        assert_eq!(t.after_step(vec![edited(None)]).await, None);
        assert_eq!(t.results(), []);
        // The status still ends.
        assert_eq!(t.statuses(), [true, false]);
    }

    #[tokio::test]
    async fn a_runner_that_cant_start_counts_as_unreadable() {
        let dir = project();
        let run =
            Fake::answers(vec![CommandResult { output: "[couldn't start the command: boom]\n".into(), ..Default::default() }]);
        let t = turn(&diagnostics(&run, true), &dir);
        t.before_change().await;
        assert!(unreadable(&t.results()[0]));
    }

    #[tokio::test]
    async fn a_step_check_that_cant_be_read_leaves_the_rest_of_the_turn_unchecked_the_next_starts_fresh() {
        let dir = project();
        let run = Fake::answers(vec![ok(""), failed("Segmentation fault\n"), ok(""), failed(ERR_X)]);
        let checker = diagnostics(&run, true);
        let first = turn(&checker, &dir);
        first.before_change().await;
        assert_eq!(first.after_step(vec![edited(None)]).await, None);
        assert_eq!(first.after_step(vec![edited(None)]).await, None);
        assert_eq!(run.count(), 2);
        let second = turn(&checker, &dir);
        second.before_change().await;
        assert!(second.after_step(vec![edited(None)]).await.unwrap().contains("found 1 new error"));
        assert_eq!(run.count(), 4);
    }

    #[tokio::test]
    async fn after_a_step_check_times_out_no_more_checks_run_this_turn_the_next_starts_fresh() {
        let dir = project();
        let run = Fake::answers(vec![ok(""), timed_out(), ok(""), failed(ERR_X)]);
        let checker = diagnostics(&run, true);
        let first = turn(&checker, &dir);
        first.before_change().await;
        assert_eq!(first.after_step(vec![edited(None)]).await, None);
        assert_eq!(first.after_step(vec![edited(None)]).await, None);
        assert_eq!(run.count(), 2);
        let second = turn(&checker, &dir);
        second.before_change().await;
        assert!(second.after_step(vec![edited(None)]).await.is_some());
    }

    #[tokio::test]
    async fn the_second_timeout_of_a_session_in_a_step_check_turns_checking_off() {
        let dir = project();
        let run = Fake::answers(vec![timed_out(), ok(""), timed_out()]);
        let checker = diagnostics(&run, true);
        turn(&checker, &dir).before_change().await;
        let second = turn(&checker, &dir);
        second.before_change().await;
        assert_eq!(second.after_step(vec![edited(None)]).await, None);
        assert_eq!(second.results().last(), Some(&CheckOutcome::Off { reason: OffReason::Timeouts }));
        assert_eq!(second.after_step(vec![edited(None)]).await, None);
        assert_eq!(run.count(), 3);
    }

    #[tokio::test]
    async fn a_stop_before_the_step_nothing_runs_no_status() {
        let dir = project();
        let run = Fake::answers(vec![ok("")]);
        let stop = CancellationToken::new();
        let t = TurnLog::new(&diagnostics(&run, true), &dir.0, true, stop.clone());
        t.before_change().await;
        stop.cancel();
        assert_eq!(t.after_step(vec![edited(None)]).await, None);
        assert_eq!(run.count(), 1);
        assert!(t.statuses().is_empty());
    }

    #[tokio::test]
    async fn esc_right_as_a_check_finishes_nothing_is_told_or_emitted() {
        let dir = project();
        let run = Fake::new(|n, _, cancel| {
            if n == 0 {
                return ok("");
            }
            cancel.cancel();
            failed(ERR_X)
        });
        let t = turn(&diagnostics(&run, true), &dir);
        t.before_change().await;
        assert_eq!(t.after_step(vec![edited(None)]).await, None);
        assert_eq!(t.results(), []);
    }

    #[tokio::test]
    async fn callbacks_that_panic_cant_make_the_hooks_panic() {
        let dir = project();
        let both = format!("{ERR_X}{ERR_Y}");
        let run = Fake::answers(vec![failed(ERR_X), failed(&both)]);
        let checks = diagnostics(&run, true)
            .turn(TurnOptions {
                root: dir.0.clone(),
                sandbox: true,
                cancel: CancellationToken::new(),
                on_status: Arc::new(|_| panic!("ui")),
                on_result: Arc::new(|_| panic!("ui")),
            })
            .unwrap();
        checks.before_change.before_change().await;
        assert!((checks.after_step)(vec![edited(None)]).await.unwrap().contains("found 1 new error"));
    }

    #[tokio::test]
    async fn combined_baselines_go_together_and_notes_join_with_a_blank_line() {
        let dir = project();
        let (a, b) = (Fake::answers(vec![ok(""), failed(ERR_X)]), Fake::answers(vec![ok(""), failed(ERR_Y)]));
        let (ta, tb) = (turn(&diagnostics(&a, true), &dir), turn(&diagnostics(&b, true), &dir));
        let both = combine_checks(vec![ta.checks.clone().unwrap(), tb.checks.clone().unwrap()]).unwrap();
        both.before_change.before_change().await;
        assert_eq!((a.count(), b.count()), (1, 1));
        let note = (both.after_step)(vec![edited(None)]).await.unwrap();
        let parts: Vec<&str> = note.split("\n\n").collect();
        assert!(parts.len() >= 2 && note.contains("'x'") && note.contains("'y'"), "{note}");
        assert!(combine_checks(vec![]).is_none());
    }

    #[test]
    fn checks_run_sandboxed_read_only_without_network() {
        let run = CheckRun {
            command: "tsc".into(),
            root: "/project".into(),
            read_only: vec!["/opt/marv".into()],
            timeout: CHECK_TIMEOUT,
        };
        let options = sandboxed_options(&run);
        assert!(options.sandbox && options.project_read_only && !options.network);
        assert!(options.writable.is_empty());
        assert!(options.read_only.contains(&PathBuf::from("/opt/marv")));
    }

    #[test]
    fn new_problems_are_counted_not_a_set() {
        let key = |s: &&str| s.to_string();
        assert_eq!(new_problems(&["a", "b"], &["a", "a", "c"], key), vec!["a", "c"]);
        assert!(new_problems(&["a"], &["a"], key).is_empty());
    }

    fn ts(i: u64) -> TsError {
        TsError {
            file: Some("a.ts".into()),
            line: Some(i + 1),
            column: Some(1),
            code: "TS2304".into(),
            message: format!("Cannot find name 'v{i}'."),
        }
    }

    #[test]
    fn new_errors_the_first_five_then_how_many_more() {
        let text = check_notice(&CheckOutcome::Done { ms: 5, before: 0, errors: 7, added: (0..7).map(ts).collect() }).unwrap();
        let lines: Vec<&str> = text.split('\n').collect();
        assert_eq!(lines[0], "✻ Typecheck: 7 new errors, told the model:");
        assert_eq!(lines.len(), 1 + 5 + 1);
        assert!(text.contains("… and 2 more"));
    }

    #[test]
    fn nothing_for_a_clean_check_or_a_failure_one_line_when_checking_is_off() {
        let failed =
            |reason, tool: Option<&str>, notify| CheckOutcome::Failed { ms: 5, reason, tool: tool.map(String::from), notify };
        assert_eq!(check_notice(&failed(FailReason::Timeout, None, false)), None);
        assert_eq!(check_notice(&failed(FailReason::Unreadable, None, false)), None);
        assert_eq!(
            check_notice(&failed(FailReason::Unreadable, None, true)).as_deref(),
            Some("✻ Marv couldn't read the typecheck's output here, so it can't tell the model about new errors.")
        );
        assert_eq!(check_notice(&failed(FailReason::MissingInterpreter, Some("node"), false)), None);
        assert_eq!(
            check_notice(&failed(FailReason::MissingInterpreter, Some("node"), true)).as_deref(),
            Some(
                "✻ tsc needs node, which the sandbox can't see (Marv's sandbox hides the home folder except known toolchains). Marv can't tell the model about new type errors."
            )
        );
        assert!(check_notice(&CheckOutcome::Off { reason: OffReason::NoSandbox }).unwrap().contains("sandbox"));
        // Off after timeouts doesn't name a duration.
        assert_eq!(
            check_notice(&CheckOutcome::Off { reason: OffReason::Timeouts }).as_deref(),
            Some("✻ The typecheck timed out twice, so Marv stopped running it for this session.")
        );
    }

    #[test]
    fn notices() {
        assert!(check_notice(&CheckOutcome::Done { ms: 1, before: 0, errors: 0, added: vec![] }).is_none());
        let e = TsError { file: None, line: None, column: None, code: "TS1".into(), message: "m\u{1b}[2J".into() };
        let n = check_notice(&CheckOutcome::Done { ms: 1, before: 0, errors: 1, added: vec![e] }).unwrap();
        assert!(n.starts_with("✻ Typecheck: 1 new error, told the model:"));
        assert!(!n.contains('\u{1b}'));
    }
}
