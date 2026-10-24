//! The agent loop, its repetition guards, and compaction. Ports `src/agent.ts`,
//! `src/repetition.ts` and `src/compact.ts` from Marv.
//!
//! - [`agent`]: [`run_agent`] yields [`LoopEvent`]s; tools run through a [`ToolRunner`] that
//!   `marv-session` implements. UI-free and vendor-free.
//! - [`repetition`]: text and tool-call loop detection.
//! - [`compact`]: the summary request and the compacted history (and, planned, compaction v2, #63).
//! - [`announce`]: #90, marv-rs only — the announced-next-step detector and its continue note.
//! - [`send_now`]: marv-rs only — the person's messages delivered at the next step boundary.

pub mod agent;
pub mod announce;
pub mod compact;
pub mod repetition;
pub mod send_now;
pub mod suggest;

#[cfg(test)]
pub(crate) mod testing;

pub use agent::{AgentOptions, ApprovalRecord, History, LimitAnswer, LoopEvent, ToolRun, ToolRunner, run_agent};
