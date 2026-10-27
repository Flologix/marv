//! The kind of turn a call runs in ([`CallMode`]), and the least a call must ask in it ([`approval_floor`]).
//!
//! marv-rs only. Who started a turn used to reach the approval flow as a boolean (`no_auto_approval`) plus the
//! subagent's filtered tool list. It is now one value on [`ToolContext`], set by the session for each turn
//! (`run_turn`, `subagent_context`) and by nothing else: no tool, no model text. [`approval_floor`] turns it and the
//! call into the least the call must ask; `run_tool` starts there, and the later checks (taint, protected paths, a
//! file change in the home folder) can only raise it.
//!
//! One exception lowers a call below its floor, on purpose: in a tainted conversation, a web request under a URL
//! prefix the person approved ("always" on a tainted fetch, `TaintState::approve_prefix`) runs without asking. It's a
//! remembered answer, like an "always", so it applies only in a turn with automatic approval
//! ([`CallMode::auto_approval`]), never in a woken or radio turn.
//!
//! The change was a refactor with no change in behaviour: `tests/guards/golden/approvals.txt` (every example call ×
//! kind of turn, clean and tainted, yolo off and on) was written before it and is unchanged by it.

use marv_core::ToolKind;

use crate::types::{DynTool, ToolContext};

/// The built-in tools only the main agent has: a subagent never gets them, whatever its type lists. Never `agent` (no
/// subagents of subagents: no runaway recursion) or `memory` (memory outlives the session, so only the main agent
/// changes it, with the user's approval); the plan and the background processes are the main agent's.
pub const MAIN_AGENT_ONLY: [&str; 5] = ["process", "memory", "plan", "agent", "recall"];

/// Who started the turn a call runs in. Set by the session; nothing a tool or the model sends changes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum CallMode {
    /// The person typed it, at a terminal or over the SDK.
    #[default]
    Person,
    /// A one-shot session: `marv -p`, or an SDK session made `one_shot`. No one at a prompt: what asks is answered by
    /// the client's approver (`--approve` for `marv -p`). Its approvals are a person's turn's.
    Headless,
    /// Another agent's message woke the session (the mesh inbox, #78): automatic approval is off.
    Woken,
    /// A radio message started it, or it's a turn that handles radio requests (#124): automatic approval is off.
    Radio,
    /// A subagent's call. `unattended`: the turn that started it had no automatic approval (woken, radio), and
    /// neither has the subagent.
    Subagent { unattended: bool },
}

impl CallMode {
    /// Whether automatic approval applies: yolo, a remembered "always", an approved URL prefix, a worktree subagent's
    /// automatic yes. Only in a turn the person at this Marv started (#74, #124): another agent's message is never a
    /// reason to approve anything automatically.
    pub fn auto_approval(self) -> bool {
        matches!(self, Self::Person | Self::Headless | Self::Subagent { unattended: false })
    }

    /// Whether `approveSandboxed` applies: the person's turn, or one a mesh wake started (read-only there, see
    /// [`Self::sandboxed_unasked_project_read_only`]). Not `marv -p` (its `--approve` flags are the contract), and not
    /// the radio's turns.
    pub fn runs_sandboxed_unasked(self) -> bool {
        matches!(self, Self::Person | Self::Woken | Self::Subagent { unattended: false })
    }

    /// Whether a command that runs unasked under `approveSandboxed` sees the project read-only in this mode. The one
    /// place that decides it (a downstream fork with its own task turns overrides it here). A woken turn is not a
    /// person's: another agent's message led to it (#74), so it may inspect, build into the sandbox's /tmp and run
    /// read-only checks unasked, and a command that has to change the project asks like it did before
    /// `approveSandboxed` (`git_write`, or `edit_file`/`write_file`, which ask). The person's turn keeps it writable.
    pub fn sandboxed_unasked_project_read_only(self) -> bool {
        matches!(self, Self::Woken)
    }

    /// The mode of a subagent started in this mode (subagents don't nest).
    pub fn subagent(self) -> Self {
        Self::Subagent { unattended: !self.auto_approval() }
    }

    pub fn is_subagent(self) -> bool {
        matches!(self, Self::Subagent { .. })
    }
}

/// The least a call must ask, before taint, protected paths and the home folder raise it. Ordered: a later check
/// moves a call up, never down.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Floor {
    /// It runs without asking.
    NeverAsks,
    /// It asks; yolo or a remembered "always" may answer.
    Asks,
    /// It asks every time: no "always", no yolo.
    AsksEveryTime,
    /// Not offered in this mode (a main-agent tool in a subagent).
    Refused,
}

/// Whether a built-in tool named `name` is offered in `mode`. Tools that aren't built in (MCP, mesh, doors) are
/// offered as the session decides. The session builds each agent's tool list with it (`subagent.rs`), and `run_tool`
/// refuses a call it says no to whatever list the call came in (the floor's [`Floor::Refused`]): a main-agent tool
/// in a subagent's context is unknown there, before the tool's own refusal (no process door, no agent host).
pub fn offered_in_mode(name: &str, mode: CallMode) -> bool {
    !(mode.is_subagent() && MAIN_AGENT_ONLY.contains(&name))
}

/// Whether a call that asks is asked every time in `ctx`: the tool says so (a host command, a sensitive GitHub
/// read), or the turn has no automatic approval.
pub fn asked_every_time(tool: &dyn DynTool, input: &(dyn std::any::Any + Send + Sync), ctx: &ToolContext) -> bool {
    tool.once(input, ctx) || !ctx.mode.auto_approval()
}

/// The least this call must ask in `ctx.mode`. The table in `docs/TOOLS.md` is this function, printed per mode.
///
/// Calls `needs_approval` once: `run_tool` takes its answer from here (web_fetch's records a cached read it allows).
/// The approved-URL-prefix exception (module docs) is `run_tool`'s, after this.
pub fn approval_floor(tool: &dyn DynTool, input: &(dyn std::any::Any + Send + Sync), ctx: &ToolContext) -> Floor {
    if !offered_in_mode(tool.name(), ctx.mode) {
        return Floor::Refused;
    }
    if !tool.needs_approval(input).unwrap_or(tool.kind() != ToolKind::Read) {
        return Floor::NeverAsks;
    }
    if asked_every_time(tool, input, ctx) { Floor::AsksEveryTime } else { Floor::Asks }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_turn_the_person_started_has_automatic_approval_and_subagents_inherit_it() {
        assert!(CallMode::Person.auto_approval() && CallMode::Headless.auto_approval());
        assert!(!CallMode::Woken.auto_approval() && !CallMode::Radio.auto_approval());
        assert_eq!(CallMode::Person.subagent(), CallMode::Subagent { unattended: false });
        assert_eq!(CallMode::Radio.subagent(), CallMode::Subagent { unattended: true });
        assert!(CallMode::Person.subagent().auto_approval() && !CallMode::Woken.subagent().auto_approval());
    }

    #[test]
    fn a_subagent_is_never_offered_the_main_agents_tools() {
        for name in MAIN_AGENT_ONLY {
            assert!(offered_in_mode(name, CallMode::Person) && !offered_in_mode(name, CallMode::Person.subagent()));
        }
        assert!(offered_in_mode("bash", CallMode::Woken.subagent()) && offered_in_mode("mesh_send", CallMode::Person.subagent()));
    }

    #[test]
    fn only_a_woken_turns_unasked_sandboxed_commands_see_the_project_read_only() {
        assert!(CallMode::Woken.runs_sandboxed_unasked() && CallMode::Woken.sandboxed_unasked_project_read_only());
        assert!(CallMode::Person.runs_sandboxed_unasked() && !CallMode::Person.sandboxed_unasked_project_read_only());
        assert!(!CallMode::Woken.subagent().runs_sandboxed_unasked() && !CallMode::Radio.runs_sandboxed_unasked());
        assert!(!CallMode::Headless.runs_sandboxed_unasked());
    }
}
