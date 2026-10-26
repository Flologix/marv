//! Diagnostics (`src/diagnostics/`): a project's own checker (tsc today) after a step that changed files, telling
//! the model only the errors that are new since the turn's baseline. The machinery ([`turn`]) is generic, so the
//! project checks ([`crate::checks`]) reuse it.
//!
//! - [`tsc`]: reading tsc's output, the note for the model (`tsc.ts`);
//! - [`turn`]: `Checker`, `TurnChecks`, `CheckOutcome`, `combine_checks`, notices (`turn.ts`), plus (marv-rs, #187)
//!   the per-kind policy: timeout, warm-up, cache, turning off after unreadable runs;
//! - [`cache`] (marv-rs, #187): the check cache, a writable folder per repository for checkers that keep a build cache.

pub mod cache;
pub mod tsc;
pub mod turn;

#[cfg(test)]
mod machinery_tests;

pub use tsc::diagnostics_note;
pub use turn::{CheckOutcome, CheckResult, Checker, TurnChecks, check_notice, combine_checks, notice};

/// One problem a checker reported: tsc's error today, and the shape every command-line checker's fits (#187).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    /// Relative to the project; absent for errors about the command line or the whole run.
    pub file: Option<String>,
    pub line: Option<u64>,
    pub column: Option<u64>,
    /// "TS2345".
    pub code: String,
    /// With its continuation lines (tsc's indented detail), joined by newlines.
    pub message: String,
}
