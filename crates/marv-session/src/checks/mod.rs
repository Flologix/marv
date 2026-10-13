//! Project checks (`src/checks/`, issue #41): a project's rules as code, `.marv/checks/<name>.yaml|yml|json|ts`,
//! run in the sandbox after the model's edits through the diagnostics machinery ([`crate::diagnostics::turn`]).
//!
//! - [`load`]: reading the rule files (`load.ts`);
//! - [`kind`]: the check kind, the runner's output, the note for the model (`kind.ts`);
//! - [`runner`]: what runs inside the sandbox (`runner.ts`).

pub mod kind;
pub mod load;
pub mod runner;

pub use kind::{ProjectCheckResult, checks_note, project_check_notice};
pub use load::{CHECKS_DIR, ProjectCheck, Violation, load_checks};
