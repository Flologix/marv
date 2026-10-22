//! The table of Marv's built-in tools, generated from the code: `docs/TOOLS.md`.
//!
//! marv-rs only (Marv has no such table). Every property a reviewer needs to see in one place (kind, where the output
//! comes from, whether a call asks and when, the network, what outlives the conversation) lives on the [`Tool`] trait,
//! so the table is printed from the trait rather than kept by hand: a hand-kept table drifts. A guard test
//! (`tests/guards/tests/tools.rs`) fails when the checked-in file differs from [`tools_table`], so a tool change
//! shows up in review as a table diff.
//!
//! Most properties are per call (bash with `network: true`, a GitHub write), so the table has two parts: the tools,
//! and a set of example calls ([`examples`]) with what each one does. Every built-in tool has at least one example.
//!
//! The examples are evaluated in a fixed context ([`example_context`]): the sandbox off (so the table doesn't depend
//! on whether this machine can run it), no doors, nothing tainted. What taint and protected paths add on top is
//! described below the table, not computed: they depend on the session, not on the call.
//!
//! [`Tool`]: crate::Tool

use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::Arc;

use serde::Serialize;
use serde_json::{Value, json};

use crate::floor::{CallMode, Floor, approval_floor};
use crate::registry::{ToolRef, builtin_tools};
use crate::types::ToolContext;

/// One example call of a built-in tool, for the table and the approval goldens.
#[derive(Debug, Clone)]
pub struct Example {
    pub tool: &'static str,
    pub input: Value,
}

fn ex(tool: &'static str, input: Value) -> Example {
    Example { tool, input }
}

/// Representative calls of every built-in tool, in the registry's order: one per distinct way a call asks, reaches
/// the network or persists.
pub fn examples() -> Vec<Example> {
    vec![
        ex("read_file", json!({"path": "src/main.rs"})),
        ex("glob", json!({"pattern": "**/*.rs"})),
        ex("grep", json!({"pattern": "fn main"})),
        ex("web_fetch", json!({"url": "https://example.com/"})),
        ex("web_fetch", json!({"url": "https://example.com/", "offset": 2000})),
        ex("github", json!({"args": ["issue", "list"]})),
        ex("github", json!({"args": ["api", "repos/o/r/actions/secrets"]})),
        ex("github", json!({"args": ["issue", "comment", "1", "--body", "done"]})),
        ex("skill", json!({"name": "release"})),
        ex("edit_file", json!({"path": "src/main.rs", "old_string": "a", "new_string": "b"})),
        ex("write_file", json!({"path": "notes.txt", "content": "hi"})),
        ex("write_file", json!({"path": ".git/hooks/pre-commit", "content": "true"})),
        ex("bash", json!({"command": "cargo test"})),
        ex("bash", json!({"command": "cargo fetch", "network": true})),
        ex("bash", json!({"command": "git commit -m x", "git_write": true})),
        ex("process", json!({"action": "start", "command": "npm run dev"})),
        ex("process", json!({"action": "start", "command": "npm run dev", "network": true})),
        ex("process", json!({"action": "list"})),
        ex("memory", json!({"action": "add", "scope": "project", "text": "Tests run with cargo nextest."})),
        ex("memory", json!({"action": "remove", "scope": "project", "text": "nextest"})),
        ex("plan", json!({"items": [{"text": "Write the test", "status": "in_progress"}]})),
        ex("agent", json!({"description": "find the bug", "prompt": "Find it."})),
        ex("agent", json!({"description": "fix the bug", "prompt": "Fix it.", "isolation": "worktree"})),
    ]
}

/// The fixed context the examples are evaluated in: a project at `/project`, a home at `/home/person` (not holding
/// the project), the sandbox off, no doors, no approver, nothing tainted.
pub fn example_context() -> ToolContext {
    let mut ctx = ToolContext::new("/project");
    let mut sandbox = marv_sandbox::Sandbox::from_env(false);
    sandbox.home = PathBuf::from("/home/person");
    ctx.home = sandbox.home.clone();
    ctx.sandbox = Arc::new(sandbox);
    ctx
}

/// The built-in tool an example calls.
pub fn example_tool(example: &Example) -> &'static ToolRef {
    builtin_tools().iter().find(|t| t.name() == example.tool).expect("an example of a built-in tool")
}

/// An example as the table shows it: the tool and its arguments.
pub fn example_label(example: &Example) -> String {
    format!("{} {}", example.tool, serde_json::to_string(&example.input).unwrap_or_default())
}

/// A serde enum's wire name (`Read` → `read`).
fn name_of(value: impl Serialize) -> String {
    serde_json::to_value(value).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default()
}

/// The kinds of turn the table has a column for, in its order.
pub const MODES: [CallMode; 5] =
    [CallMode::Person, CallMode::Headless, CallMode::Woken, CallMode::Radio, CallMode::Subagent { unattended: false }];

/// A floor as the table shows it.
pub fn floor_name(floor: Floor) -> &'static str {
    match floor {
        Floor::NeverAsks => "never",
        Floor::Asks => "asks",
        Floor::AsksEveryTime => "every time",
        Floor::Refused => "refused",
    }
}

fn yes(b: bool) -> &'static str {
    if b { "yes" } else { "no" }
}

/// `docs/TOOLS.md`: the built-in tools and their example calls, from the code.
pub fn tools_table() -> String {
    let ctx = example_context();
    let mut out = String::new();
    out.push_str(
        "# Marv's built-in tools\n\n\
         <!-- Generated from the code (marv_tools::table). Don't edit: run\n\
         UPDATE_GOLDEN=1 cargo test -p marv-guards --test tools\n\
         to write it again. -->\n\n\
         Every built-in tool, in the order the model sees them, with the properties that decide how its calls are\n\
         approved and how its output is treated. A tool change shows up here as a diff.\n\n\
         ## The tools\n\n\
         - **Kind**: `read` runs without asking; `write`, `execute` and `egress` ask unless the call says otherwise.\n  \
         Once the conversation holds outside text, `egress` and any call that reaches the network ask every time.\n\
         - **Source**: where the output comes from, at least (a call may be less trusted: see the calls). Its class:\n  \
         `authority` (the person, Marv) is not fenced; `workspace` (the project) is quoted; `outside` is fenced as\n  \
         data and taints the conversation.\n\
         - **Checkpoint**: the turn's checkpoint is taken before a call runs, so /undo can put it back.\n\n\
         | Tool | Kind | Source | Class | Checkpoint | Parallel |\n\
         |---|---|---|---|---|---|\n",
    );
    for tool in builtin_tools() {
        let source = tool.provenance();
        let _ = writeln!(
            out,
            "| `{}` | {} | {} | {} | {} | {} |",
            tool.name(),
            name_of(tool.kind()),
            name_of(source),
            name_of(source.class()),
            yes(tool.checkpoint()),
            yes(tool.parallel()),
        );
    }
    out.push_str(
        "\n## Example calls\n\n\
         What each call does by itself, evaluated with the sandbox off and nothing tainted:\n\n\
         - **Person** to **Subagent**: the least the call asks in each kind of turn: `never` (it runs without asking),\n  \
         `asks` (yolo or \"don't ask again\" may answer), `every time` (no yolo, no \"don't ask again\"), `refused` (not\n  \
         offered). `person`: the person typed it; `headless`: `marv -p`; `woken`: another agent's message woke the\n  \
         session; `radio`: a radio message, or a turn handling radio requests; `subagent`: a subagent of the person's\n  \
         turn (a subagent of a woken or radio turn asks as that turn does).\n\
         - **Network**: whether it can reach the network.\n\
         - **Source**: where this call's output comes from (its class in brackets).\n\
         - **Persists**: whether it writes something later conversations load as the person's own. In a conversation\n  \
         holding outside text, such a call is asked every time.\n\
         - **Door**: the door out of the sandbox it goes through, if any.\n\n\
         On top of this, a write to a protected path (docs/SECURITY.md: `.git`, a lifecycle script in `package.json`, ...)\n\
         is asked every time, even in yolo, and so is any call to the network once the conversation holds outside text.\n\n\
         | Call | Person | Headless | Woken | Radio | Subagent | Network | Source | Persists | Door |\n\
         |---|---|---|---|---|---|---|---|---|---|\n",
    );
    for example in examples() {
        let tool = example_tool(&example);
        let parsed = tool.parse(example.input.clone()).expect("an example that parses");
        let input = parsed.as_ref();
        let floors: Vec<&str> = MODES
            .iter()
            .map(|mode| floor_name(approval_floor(tool.as_ref(), input, &ToolContext { mode: *mode, ..ctx.clone() })))
            .collect();
        let source = tool.provenance_in(input, &ctx);
        let door = tool.door(input, &ctx).map(name_of).unwrap_or_else(|| "-".into());
        let _ = writeln!(
            out,
            "| `{}` | {} | {} | {} ({}) | {} | {} |",
            example_label(&example),
            floors.join(" | "),
            yes(tool.uses_network(input, &ctx)),
            name_of(source),
            name_of(source.class()),
            yes(tool.persists(input, &ctx)),
            door,
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_builtin_tool_has_an_example_and_every_example_parses() {
        let examples = examples();
        for tool in builtin_tools() {
            assert!(examples.iter().any(|e| e.tool == tool.name()), "no example of {}", tool.name());
        }
        for example in &examples {
            let tool = example_tool(example);
            assert!(tool.parse(example.input.clone()).is_ok(), "{} doesn't parse", example_label(example));
        }
    }

    #[test]
    fn the_examples_follow_the_registry_order() {
        let order: Vec<&str> = builtin_tools().iter().map(|t| t.name()).collect();
        let positions: Vec<usize> =
            examples().iter().map(|e| order.iter().position(|n| *n == e.tool).expect("a built-in tool")).collect();
        assert!(positions.windows(2).all(|w| w[0] <= w[1]), "examples out of the registry's order");
    }
}
