//! `agent`: hand a self-contained task to a subagent.
//!
//! Ports Marv's `src/tools/agent.ts`. The subagent itself (`src/subagent.ts`, `src/agents.ts`, worktrees) is
//! marv-session's: this tool reaches it through [`crate::AgentHost`], which only the main agent's context has, so
//! subagents can't start subagents.
//!
//! - A fresh history (just the prompt), the type's system prompt, `SUBAGENT_TOOLS` ∩ the type's tools; never `agent`
//!   or `memory`. 50 steps. Only its final message returns, so the files it read never fill the parent's context.
//! - `parallel`: several agent calls in a row run at once (up to 4).
//! - `needs_approval` only with `isolation: "worktree"` (the one approval for it, scope `agent:worktree`); a
//!   shared-folder subagent asks before each change instead. `auto_safe` when that worktree is sandboxed.
//! - The preview makes every check `run_subagent` makes, so the user is never asked to approve one that can't run;
//!   it warns that uncommitted changes won't be in the worktree.
//! - Port note: the subagent's taint starts as the parent's (`TaintState::for_subagent`), without its approvals.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::types::{Preview, Scope, Tool, ToolContext, ToolError, ToolOutput};

/// The prompt shown in the approval, at most.
pub const PREVIEW_CHARS: usize = 400;
/// Longest task description (the transcript label and the branch name come from it).
pub const MAX_DESCRIPTION_CHARS: usize = 120;
pub const NO_SUBAGENTS: &str = "Subagents aren't available here.";
pub const NO_WORKTREES: &str = "Worktrees aren't available here. Start the agent without isolation.";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Isolation {
    Worktree,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentInput {
    #[serde(rename = "type")]
    pub agent_type: Option<String>,
    pub description: String,
    pub prompt: String,
    pub isolation: Option<Isolation>,
}

/// "general-purpose · Task 2: parser errors" (Marv's `agentLabel`, src/agent.ts).
pub fn agent_label(agent_type: Option<&str>, description: &str) -> String {
    format!("{} · {description}", agent_type.unwrap_or("general-purpose"))
}

pub struct Agent;

#[async_trait]
impl Tool for Agent {
    type Input = AgentInput;

    fn name(&self) -> &str {
        "agent"
    }
    fn description(&self) -> &str {
        "Hand a self-contained task to a subagent: a separate agent with a fresh context and its own tools, which does the task and returns only its final report. Several agent calls in a row run in parallel (up to 4 at once). The available types are listed in the system prompt."
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "type": {"type": "string", "description": "The agent type, from the list in the system prompt. Default \"general-purpose\"."},
                "description": {"type": "string", "minLength": 1, "maxLength": MAX_DESCRIPTION_CHARS, "description": "A short name for the task, 3-5 words, e.g. \"Task 2: parser errors\"."},
                "prompt": {"type": "string", "minLength": 1, "description": "The complete task. The subagent sees nothing else: include the goal, the relevant files, constraints, and what to report back."},
                "isolation": {"type": "string", "enum": ["worktree"], "description": "Run it in its own git worktree on a new branch, so it can change files and build while other agents do too. You merge its branch afterwards."}
            },
            "required": ["description", "prompt"],
            "additionalProperties": false
        })
    }
    fn kind(&self) -> marv_core::ToolKind {
        marv_core::ToolKind::Execute
    }
    fn provenance(&self) -> marv_core::Provenance {
        marv_core::Provenance::Subagent
    }
    fn parallel(&self) -> bool {
        true
    }
    fn check(&self, input: &AgentInput) -> Result<(), String> {
        let len = input.description.chars().count();
        if len == 0 || len > MAX_DESCRIPTION_CHARS {
            return Err(format!("description: must be 1 to {MAX_DESCRIPTION_CHARS} characters"));
        }
        if input.prompt.is_empty() {
            return Err("prompt: must not be empty".into());
        }
        Ok(())
    }
    fn label(&self, input: &AgentInput) -> String {
        agent_label(input.agent_type.as_deref(), &input.description)
    }
    fn needs_approval(&self, input: &AgentInput) -> Option<bool> {
        Some(input.isolation == Some(Isolation::Worktree))
    }
    fn scope(&self, _input: &AgentInput) -> Option<Scope> {
        Some(Scope { key: "agent:worktree".into(), description: "subagents in their own worktrees".into() })
    }
    /// Yolo: a sandboxed worktree is as confined as an agent gets (its branch is the review).
    fn auto_safe(&self, input: &AgentInput, ctx: &ToolContext) -> bool {
        input.isolation == Some(Isolation::Worktree) && ctx.sandbox.active()
    }

    async fn preview(&self, input: &AgentInput, ctx: &ToolContext) -> Result<Preview, ToolError> {
        let host = ctx.agent_host.as_ref().ok_or_else(|| ToolError::new(NO_SUBAGENTS))?;
        if !host.worktrees() {
            return Err(ToolError::new(NO_WORKTREES));
        }
        host.preview(input, ctx).await
    }

    async fn run(&self, input: &AgentInput, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let host = ctx.agent_host.as_ref().ok_or_else(|| ToolError::new(NO_SUBAGENTS))?;
        host.run(input, ctx).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asks_only_for_a_worktree_and_labels_by_type() {
        let mut input = AgentInput { agent_type: None, description: "Task 2".into(), prompt: "do it".into(), isolation: None };
        assert_eq!(Agent.label(&input), "general-purpose · Task 2");
        assert_eq!(Agent.needs_approval(&input), Some(false));
        input.isolation = Some(Isolation::Worktree);
        assert_eq!(Agent.needs_approval(&input), Some(true));
        input.description = "x".repeat(121);
        assert!(Agent.check(&input).is_err());
    }
}
