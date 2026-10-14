//! marv-session: the UI-free engine. Marv's `MarvSession` and everything around it: config, the system prompt,
//! sessions on disk, trajectories, usage, memory, skills, agent types, subagents, worktrees, git, project checks,
//! diagnostics, slash commands (engine side), and the SDK.
//!
//! One module per Marv TypeScript file (see `docs/subsystems/session.md`), plus the marv-rs seams:
//! [`runner`] (the agent loop's `ToolRunner`, the "always" approver, the `AgentHost`), [`mcp_tools`] (MCP tools as
//! Marv tools), [`mesh`] (the switchboard and mesh tools), [`provenance`] (fencing untrusted text), [`time`].
//!
//! What clients use:
//! - [`MarvSession`] (`new`, `send` → [`Turn`] (a `Stream` of [`SessionEvent`]), `interrupt`, `compact`, `clear`,
//!   `resume`, `configure`, `rate`, `save`, `flush`, `close`, getters), [`SessionInit`];
//! - [`SessionBuilder`] / [`create_session`] (the SDK's way);
//! - [`config`] (presets, `resolve_config`, `ConfigStore`), [`commands`] (`run_command` → `CommandAction`),
//!   [`types::Message`] (the transcript), [`usage`] (`/cost`, the status bar), [`tool_draft`] (the waiting line).
//!
//! The engine never imports the TUI. It never writes to the terminal: clients run model text through
//! `marv_core::printable()`.

pub mod agents;
pub mod approvals;
pub mod checkpoints;
pub mod checks;
pub mod commands;
pub mod config;
pub mod diagnostics;
pub mod event_queue;
pub mod feedback;
pub mod frontmatter;
pub mod git;
pub mod hooks;
pub mod instructions;
pub mod mcp_install;
pub mod mcp_tools;
pub mod memory;
pub mod mesh;
pub mod paths;
pub mod processes;
pub mod prompt;
pub mod provenance;
pub mod runner;
pub mod sdk;
pub mod session;
pub mod sessions;
pub mod skills;
pub mod sources;
pub mod subagent;
pub mod time;
pub mod tool_draft;
pub mod trajectory;
pub mod trajectory_stats;
pub mod types;
pub mod usage;
pub mod worktree;

pub use checkpoints::undo::{UndoFile, UndoPlan, UndoRefusal, UndoResult, undo_request, undo_summary};
pub use runner::{AlwaysScopes, ApprovalRecord, SessionAgentHost, SessionApprover, SessionToolRunner, ToolDisplay};
pub use sdk::{SessionBuilder, SessionOptions, create_session};
pub use session::{
    CheckpointProblem, CompactFailure, CompactResult, Configure, Feedback, MarvSession, ProviderChoice, RateOutcome,
    ResumeTarget, Resumed, SendOptions, SessionError, SessionEvent, SessionInit, SystemPrompt, Turn, TurnEndReason, TurnStatus,
    UsageSnapshot,
};
pub use types::{Message, Role};
pub use usage::Totals;
