//! Running a subagent: the same agent loop started again, with a fresh conversation, its own system prompt and its
//! own tools. Ports Marv's `src/subagent.ts`.
//!
//! Only its final message goes back to the agent that started it, so the files it read never fill the parent's
//! context. Marv's own lines in the report (why it stopped, the branch) are prefixed `Marv:` and bracketed, so the
//! subagent's text can't pass for them. Its usage counts toward `/cost`.
//!
//! Its tool context is built from scratch ([`subagent_context`]), so nothing is inherited by accident: no agent host
//! (no nesting), no memory, no call id. Yolo carries over. marv-rs: its taint is `TaintState::for_subagent` and it
//! **doesn't inherit network, web or mesh approvals** (docs/SECURITY.md): its "always" scopes start empty.

use std::sync::Arc;

use async_trait::async_trait;
use marv_tools::{ApprovalRequest, Approver, Decision, ToolContext, ToolError, ToolOutput};

use crate::agents::{AgentType, find_agent};
use crate::usage::tokens;
use crate::worktree::{Worktree, worktree_env};

/// Implementing a task takes more steps than answering a question.
pub const SUBAGENT_MAX_STEPS: u32 = marv_agent::agent::SUBAGENT_MAX_STEPS;
/// Finished steps kept per subagent for ctrl+o: labels and summaries only.
pub const MAX_STEPS_KEPT: usize = 20;

pub use marv_tools::agent::{NO_SUBAGENTS, NO_WORKTREES};

/// Why a subagent stopped, as its report says it.
pub fn stopped_text(reason: &str) -> String {
    match reason {
        "aborted" => "Interrupted by the user before it finished.".into(),
        // Its history is gone once it ends, so "continue" can't resume it.
        "max_steps" => format!(
            "It hit its {SUBAGENT_MAX_STEPS}-step limit before finishing. To go on, start a new subagent with what remains."
        ),
        "length" => "Its last reply was cut off by the model's output limit.".into(),
        "error" => "It stopped on an error:".into(),
        other => format!("It stopped ({other})."),
    }
}

pub fn resolve_type<'a>(agents: &'a [AgentType], name: Option<&str>) -> Result<&'a AgentType, ToolError> {
    let name = name.unwrap_or("general-purpose");
    find_agent(agents, name).ok_or_else(|| {
        ToolError::new(format!(
            "There's no agent type \"{name}\". Available: {}.",
            agents.iter().map(|a| a.name.as_str()).collect::<Vec<_>>().join(", ")
        ))
    })
}

/// How a subagent's changes get approved. In a sandboxed worktree they run without asking (the worktree is the
/// boundary, merging its branch is the review), except anything that reaches the network, and anything a person
/// must answer (`must_ask`: tainted egress, protected paths). Otherwise the user is asked, labeled with which
/// subagent asks. Without an approver there's no auto-yes either.
pub struct SubagentApprover {
    pub inner: Arc<dyn Approver>,
    /// "implementer · Task 2"
    pub agent: String,
    pub auto: bool,
}

#[async_trait]
impl Approver for SubagentApprover {
    async fn approve(&self, request: ApprovalRequest) -> Decision {
        if self.auto && !request.network && !request.must_ask {
            return Decision::Yes;
        }
        self.inner.approve(ApprovalRequest { agent: Some(self.agent.clone()), ..request }).await
    }
}

/// The tool context a subagent's tools run with, built from scratch. In a worktree it works in the worktree's folder,
/// bash sees the shared `.git` read-only, and Marv's own git calls are pinned to the worktree.
pub fn subagent_context(
    parent: &ToolContext,
    who: &str,
    worktree: Option<&Worktree>,
    approver: Option<Arc<dyn Approver>>,
) -> ToolContext {
    let sandboxed = parent.sandbox.active();
    let mut ctx = ToolContext::new(worktree.map_or_else(|| parent.root.clone(), |w| w.work_dir.clone()));
    ctx.home = parent.home.clone();
    ctx.cancel = parent.cancel.clone();
    ctx.sandbox = parent.sandbox.clone();
    ctx.yolo = parent.yolo;
    ctx.skills = parent.skills.clone();
    ctx.taint = Arc::new(parent.taint.for_subagent());
    if let Some(wt) = worktree {
        ctx.read_only = if wt.prefix.is_empty() { vec![wt.git_dir.clone()] } else { vec![wt.dir.clone(), wt.git_dir.clone()] };
        ctx.git_env = worktree_env(wt);
    }
    ctx.approver = approver.map(|inner| {
        Arc::new(SubagentApprover { inner, agent: who.to_string(), auto: worktree.is_some() && sandboxed }) as Arc<dyn Approver>
    });
    ctx
}

/// Marv's own lines in a report, marked so the subagent's text can't pass for them.
pub fn from_marv(text: &str) -> String {
    format!("Marv: {text}")
}

fn plural(n: usize, word: &str) -> String {
    format!("{n} {word}{}", if n == 1 { "" } else { "s" })
}

/// How a subagent's run went, for its report.
#[derive(Debug, Clone, Default)]
pub struct SubagentRun {
    /// Its last reply.
    pub text: String,
    /// The loop's done reason ("end", "declined", "aborted", "max_steps", …).
    pub reason: String,
    pub error: Option<String>,
    /// The step the user said no to ("write_file a.txt").
    pub declined_step: Option<String>,
    /// The worktree's line (already `Marv:`-prefixed).
    pub branch_line: Option<String>,
    pub tool_count: usize,
    pub tokens_used: u64,
}

/// The report the parent agent reads: the subagent's own text first, then Marv's lines.
pub fn subagent_report(run: &SubagentRun) -> ToolOutput {
    let stats = format!("{} · {} tokens", plural(run.tool_count, "tool"), tokens(run.tokens_used));
    let stopped = if run.reason == "end" || run.reason == "declined" {
        String::new()
    } else {
        format!("[{}{}]", from_marv(&stopped_text(&run.reason)), run.error.as_ref().map(|e| format!(" {e}")).unwrap_or_default())
    };
    // A "no" that came with an interrupt (Esc declines what's waiting, then stops) is reported as the interrupt.
    let declined = run.declined_step.is_some() && run.reason != "aborted";
    // Without this line the parent reads only the subagent's last words ("I'll update a.txt now.").
    let declined_line = match (&run.declined_step, declined) {
        (Some(step), true) => format!(
            "[{}]",
            from_marv(&format!(
                "The user declined its {step}, so it stopped there. Don't retry: wait for the user to say how to proceed."
            ))
        ),
        _ => String::new(),
    };
    let text = run.text.trim();
    let output = [
        if text.is_empty() { "(The subagent gave no report.)" } else { text },
        &stopped,
        &declined_line,
        run.branch_line.as_deref().unwrap_or(""),
    ]
    .into_iter()
    .filter(|s| !s.is_empty())
    .collect::<Vec<_>>()
    .join("\n\n");
    let mut out = ToolOutput::ok(output, "");
    out.provenance = marv_core::Provenance::Subagent;
    if declined {
        out.summary = format!("declined · {stats}");
        out.declined = true;
    } else if !stopped.is_empty() {
        out.summary = format!("stopped · {stats}");
        out.is_error = true;
    } else {
        let first: String = text.lines().next().unwrap_or("").chars().take(80).collect();
        out.summary = if first.is_empty() { format!("done · {stats}") } else { format!("done · {stats} · \"{first}\"") };
    }
    out
}

/// Runs a subagent (Marv's `runSubagent`): resolves its type, makes the worktree, runs `marv_agent::run_agent` with
/// `SUBAGENT_MAX_STEPS`, forwards every loop event to the host's sink (`SessionEvent::Subagent`), reports progress,
/// finishes the worktree however it ended, and returns [`subagent_report`].
pub async fn run_subagent(
    _host: &crate::runner::SessionAgentHost,
    _input: &marv_tools::agent::AgentInput,
    _ctx: &ToolContext,
) -> Result<ToolOutput, ToolError> {
    todo!(
        "port: runSubagent (src/subagent.ts): type, worktree, available tools ∩ SUBAGENT_TOOLS, subagent_prompt, run_agent loop, progress, finish_worktree, report"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_finished_report_quotes_its_first_line() {
        let out = subagent_report(&SubagentRun {
            text: "Found it.\nDetails".into(),
            reason: "end".into(),
            tool_count: 1,
            tokens_used: 1200,
            ..Default::default()
        });
        assert_eq!(out.output, "Found it.\nDetails");
        assert_eq!(out.summary, "done · 1 tool · 1.2k tokens · \"Found it.\"");
        assert_eq!(out.provenance, marv_core::Provenance::Subagent);
    }

    #[test]
    fn marvs_lines_follow_the_subagents_text_and_are_marked() {
        let out = subagent_report(&SubagentRun {
            text: "".into(),
            reason: "max_steps".into(),
            branch_line: Some(from_marv("Branch marv/x-1: 1 commit on abc.")),
            ..Default::default()
        });
        assert!(out.output.starts_with("(The subagent gave no report.)\n\n[Marv: It hit its 50-step limit"));
        assert!(out.output.ends_with("Marv: Branch marv/x-1: 1 commit on abc."));
        assert!(out.is_error);
    }

    #[test]
    fn a_decline_says_not_to_retry_but_an_interrupt_wins() {
        let out = subagent_report(&SubagentRun {
            reason: "declined".into(),
            declined_step: Some("write_file a.txt".into()),
            ..Default::default()
        });
        assert!(out.declined && out.output.contains("The user declined its write_file a.txt"));
        let out = subagent_report(&SubagentRun {
            reason: "aborted".into(),
            declined_step: Some("bash x".into()),
            ..Default::default()
        });
        assert!(!out.declined && out.output.contains("Interrupted by the user"));
    }
}
