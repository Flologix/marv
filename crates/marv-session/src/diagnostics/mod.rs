//! Diagnostics (`src/diagnostics/`): the typecheck after a step that changed files, in a TypeScript project, telling
//! the model only the errors that are new since the turn's baseline. The machinery ([`turn`]) is generic, so the
//! project checks ([`crate::checks`]) reuse it.
//!
//! - [`tsc`]: reading tsc's output, the note for the model (`tsc.ts`);
//! - [`turn`]: `Checker`, `TurnChecks`, `CheckOutcome`, `combine_checks`, notices (`turn.ts`).

pub mod tsc;
pub mod turn;

pub use tsc::{TsError, diagnostics_note};
pub use turn::{CheckOutcome, CheckResult, Checker, TurnChecks, check_notice, combine_checks};
