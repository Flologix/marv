//! The checks after a step's file changes, per turn and per session. Ports Marv's `src/diagnostics/turn.ts`.
//!
//! A [`CheckKind`] says what to run (the baseline, then a step's changed files), how to parse it, and what to tell the
//! model; a [`Checker`] holds what lasts across turns (timeouts, notices given) and makes each turn's [`TurnChecks`]:
//! - `before_change`: the baseline before the turn's first change, started (not awaited) by the edit tools' preview
//!   and awaited before writing, so it's right in yolo too. Later calls wait for the same one.
//! - `after_step`: after a step that changed files, the new problems only (counted, matched without line numbers),
//!   as a note appended as a user message from Marv; each check becomes the next baseline, so a problem is told once.
//!
//! Checks are the project's own code, so they run only sandboxed with the project read-only and without network;
//! without the sandbox they don't run (said once: `Off { NoSandbox }`). 60 s per check; two timeouts turn a kind off
//! for the session. Main agent only.

use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use marv_agent::ToolRun;

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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandResult {
    pub output: String,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
}

/// Runs a check command in the sandbox (project read-only, no network, `.git` read-only). Tests pass a stand-in.
pub type RunCheck = Arc<
    dyn Fn(String, std::path::PathBuf, tokio_util::sync::CancellationToken, Duration) -> BoxFuture<'static, CommandResult>
        + Send
        + Sync,
>;

/// Which problems a pass re-checks (the rest of the baseline stands).
pub type Covers<E> = Box<dyn Fn(&E) -> bool + Send + Sync>;

/// One pass of a check: the command, and which problems it can report (all, when `covers` is `None`).
pub struct CheckPass<E> {
    pub command: String,
    pub covers: Option<Covers<E>>,
}

/// What one kind of check is.
pub trait CheckKind<E>: Send + Sync {
    /// Whether anything can run in this project this turn (a compiler, some checks).
    fn prepare(&self, root: &std::path::Path) -> bool;
    /// `None` = the baseline (everything); `Some(files)` = a step that changed those (project-relative). `None` back:
    /// nothing it checks could have changed, which costs nothing.
    fn pass(&self, root: &std::path::Path, changed: Option<&[String]>) -> Option<CheckPass<E>>;
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

/// The files a step's results changed, or `None` when a change doesn't say which (then everything is checked).
pub fn changed_files(runs: &[ToolRun]) -> Option<Vec<String>> {
    let mut files = Vec::new();
    for run in runs.iter().filter(|r| {
        r.display.as_ref().and_then(|d| d.downcast_ref::<crate::runner::ToolDisplay>()).is_some_and(|d| d.diff.is_some())
    }) {
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
    pub root: std::path::PathBuf,
    pub sandbox: bool,
    pub cancel: tokio_util::sync::CancellationToken,
    pub on_status: Arc<dyn Fn(bool) + Send + Sync>,
    pub on_result: Arc<dyn Fn(CheckOutcome<E>) + Send + Sync>,
}

/// One per kind of check and session: survives `clear()` and `resume()`.
pub struct Checker<E> {
    pub kind: Arc<dyn CheckKind<E>>,
    pub run: Option<RunCheck>,
    pub timeout: Duration,
    timeouts: std::sync::atomic::AtomicU32,
    told_no_sandbox: std::sync::atomic::AtomicBool,
    told_unreadable: std::sync::atomic::AtomicBool,
}

impl<E: Clone + Send + Sync + 'static> Checker<E> {
    pub fn new(kind: Arc<dyn CheckKind<E>>, run: Option<RunCheck>) -> Self {
        Self {
            kind,
            run,
            timeout: CHECK_TIMEOUT,
            timeouts: Default::default(),
            told_no_sandbox: Default::default(),
            told_unreadable: Default::default(),
        }
    }

    /// Checking was turned off and on again: forget what made the session give up, and the notices given.
    pub fn reset(&self) {
        use std::sync::atomic::Ordering::Relaxed;
        self.timeouts.store(0, Relaxed);
        self.told_no_sandbox.store(false, Relaxed);
        self.told_unreadable.store(false, Relaxed);
    }

    /// This turn's hooks; `None` when nothing can run (no compiler, no checks, two timeouts already).
    pub fn turn(self: &Arc<Self>, _options: TurnOptions<E>) -> Option<TurnChecks> {
        todo!("port: Checker.turn (baseline once per turn, sandbox check, pass per step, new_problems, timeouts, notices)")
    }
}

/// The typecheck: the project's own tsgo or tsc (Marv's `typecheck` kind).
pub struct Typecheck;

impl CheckKind<TsError> for Typecheck {
    fn prepare(&self, root: &std::path::Path) -> bool {
        super::tsc::detect_checker(root).is_some()
    }
    fn pass(&self, root: &std::path::Path, _changed: Option<&[String]>) -> Option<CheckPass<TsError>> {
        super::tsc::detect_checker(root).map(|bin| CheckPass { command: super::tsc::check_command(&bin), covers: None })
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
        _ => todo!("port: combineChecks (join before_change; run after_step in parallel and join notes with blank lines)"),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_problems_are_counted_not_a_set() {
        let key = |s: &&str| s.to_string();
        assert_eq!(new_problems(&["a", "b"], &["a", "a", "c"], key), vec!["a", "c"]);
        assert!(new_problems(&["a"], &["a"], key).is_empty());
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
