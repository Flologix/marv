//! The agent loop, its repetition guards, and compaction. Ports `src/agent.ts`,
//! `src/repetition.ts` and `src/compact.ts` from Marv.
//!
//! - [`agent`]: [`run_agent`] yields [`LoopEvent`]s; tools run through a [`ToolRunner`] that
//!   `marv-session` implements. UI-free and vendor-free.
//! - [`repetition`]: text and tool-call loop detection.
//! - [`compact`]: the summary request and the compacted history (and, planned, compaction v2, #63).

pub mod agent;
pub mod compact;
pub mod repetition;

#[cfg(test)]
pub(crate) mod testing;

pub use agent::{AgentOptions, ApprovalRecord, History, LimitAnswer, LoopEvent, ToolRun, ToolRunner, run_agent};
