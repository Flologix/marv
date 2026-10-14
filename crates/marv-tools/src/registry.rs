//! The tool registry: what the model is offered, and how its calls are run.
//!
//! Ports Marv's `src/tools/index.ts` (named `registry` here: `lib.rs` is the crate's index).
//!
//! - Tool specs are built once, so every request sends byte-identical definitions in a stable order (anything else
//!   breaks the prompt cache). Note: `serde_json` without `preserve_order` sorts object keys, so a schema's key order
//!   differs from Marv's (zod's insertion order); it's still byte-stable, which is what the cache needs.
//! - `skill` is offered only when skills exist (`tool_specs_for`).
//! - `run_tool` never fails: bad JSON, invalid input and `ToolError`s become error results the model can recover from.
//!   `available` is what this agent was offered (a subagent gets fewer tools); anything else is unknown, even if the
//!   model names it.
//! - Flow: parse JSON (else `repair_json_args`) → the tool's `repair` → validate → label → if gated: preview (a
//!   `ToolError` fails the call before anyone is asked) → aborted? interrupted → yolo + `auto_safe`: run confined →
//!   no approver: "no one to ask" → approve: unavailable / no (aborted → interrupted) / yes / always → run.
//! - Every repaired call's result carries the repair notes, however the call ended.
//!
//! marv-rs additions (docs/SECURITY.md): a tainted session's egress (`ToolKind::Egress` or `uses_network`) and any
//! write to a protected path are gated, skip yolo, and reach the approver with `must_ask` set; a result whose
//! provenance taints marks the session. A web request (`egress_url`) under a URL prefix the user approved after the
//! taint runs as approved; "always" on a tainted one approves its prefix (`web_fetch::url_prefix`), not the site.

use std::path::MAIN_SEPARATOR;
use std::sync::{Arc, OnceLock};

use marv_core::{ToolCall, ToolKind, ToolSpec};
use serde_json::Value;

use crate::repair::{repair_json_args, with_note};
use crate::types::{Approval, ApprovalRequest, Decision, DynTool, ToolContext, ToolOutput};

/// A tool as the registry holds it.
pub type ToolRef = Arc<dyn DynTool>;

/// What the model reads for a call stopped before it ran. Must equal marv-agent's `NOT_RUN.aborted`.
pub const INTERRUPTED: &str = "Interrupted by the user before this tool ran.";
/// What the model reads when the user said no.
pub const DECLINED: &str = "The user declined this. Don't retry it: stop and wait for them to say how to proceed.";

/// Marv's built-in tools, in Marv's order (the order of the specs the model sees):
/// read_file, glob, grep, web_fetch, skill, edit_file, write_file, bash, memory, agent.
/// `apply_patch` (#66) isn't registered until it's built.
pub fn builtin_tools() -> &'static [ToolRef] {
    static TOOLS: OnceLock<Vec<ToolRef>> = OnceLock::new();
    TOOLS.get_or_init(|| {
        vec![
            Arc::new(crate::read_file::ReadFile),
            Arc::new(crate::glob::Glob),
            Arc::new(crate::grep::Grep),
            Arc::new(crate::web_fetch::WebFetch::default()),
            Arc::new(crate::skill::SkillTool),
            Arc::new(crate::edit_file::EditFile),
            Arc::new(crate::write_file::WriteFile),
            Arc::new(crate::bash::Bash),
            Arc::new(crate::memory::Memory),
            Arc::new(crate::agent::Agent),
        ]
    })
}

/// Whether a call may run at the same time as its neighbours (subagents).
pub fn is_parallel_call(call: &ToolCall, available: &[ToolRef]) -> bool {
    available.iter().any(|t| t.name() == call.name && t.parallel())
}

/// The built-in specs, built once (same values every time: the cache).
pub fn tool_specs() -> &'static [ToolSpec] {
    static SPECS: OnceLock<Vec<ToolSpec>> = OnceLock::new();
    SPECS.get_or_init(|| builtin_tools().iter().map(|t| t.spec()).collect())
}

/// The specs for a session: the skill tool only when there are skills to load.
pub fn tool_specs_for(has_skills: bool) -> Vec<ToolSpec> {
    tool_specs().iter().filter(|s| has_skills || s.name != "skill").cloned().collect()
}

/// A finished call: the result and the transcript label.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOutcome {
    pub result: ToolOutput,
    pub label: String,
}

/// Models sometimes pass absolute paths; the transcript shows them relative to the project.
fn short_label(label: String, ctx: &ToolContext) -> String {
    let root = ctx.root.to_string_lossy();
    if label.starts_with('/') && label.starts_with(&format!("{root}{MAIN_SEPARATOR}")) {
        return marv_sandbox::project_path(&ctx.root, std::path::Path::new(&label));
    }
    label
}

/// Runs a call. Never fails: every failure becomes a result the model can read and recover from.
pub async fn run_tool(call: &ToolCall, ctx: &ToolContext, available: &[ToolRef]) -> ToolOutcome {
    let fail = |output: String, label: &str| ToolOutcome { result: ToolOutput::error(output), label: label.to_string() };
    let Some(tool) = available.iter().find(|t| t.name() == call.name) else {
        let names: Vec<&str> = available.iter().map(|t| t.name()).collect();
        return fail(format!("Unknown tool \"{}\". Available tools: {}.", call.name, names.join(", ")), &call.name);
    };

    let args = if call.arguments.is_empty() { "{}" } else { call.arguments.as_str() };
    let (raw, json_note) = match serde_json::from_str::<Value>(args) {
        Ok(raw) => (raw, None),
        // A weaker model puts literal tabs or newlines inside JSON strings (issue #10). Escaping those is
        // unambiguous; anything else stays an error the model can recover from.
        Err(_) => match repair_json_args(args) {
            Some(repaired) => (repaired.value, repaired.note),
            None => return fail(format!("The arguments for {} are not valid JSON: {}", call.name, call.arguments), &call.name),
        },
    };
    // A tool's own repair of what it can read with certainty, before validation.
    let (input_value, repair_note) = match tool.repair(&raw, ctx) {
        Some(repaired) => (repaired.input, repaired.note),
        None => (raw, None),
    };
    let note = [json_note, repair_note].into_iter().flatten().collect::<Vec<_>>().join("\n\n");
    let note = (!note.is_empty()).then_some(note);
    let noted = |mut outcome: ToolOutcome| {
        outcome.result.output = with_note(&outcome.result.output, note.as_deref());
        outcome
    };

    let input = match tool.parse(input_value) {
        Ok(input) => input,
        Err(err) => return noted(fail(format!("Invalid input for {}:\n{err}", call.name), &call.name)),
    };
    let input = input.as_ref();
    let label = short_label(tool.label(input), ctx);
    let per_call = ToolContext { call_id: Some(call.id.clone()), ..ctx.clone() };
    // A door out of the sandbox (Marv #47, #51, #52): the prompt shows its warning, the result and trajectory name
    // it, and a `once` call is never remembered (the session's approver holds that, whatever the client answers).
    let door = tool.door(input, ctx);
    let finish = |result: Result<ToolOutput, crate::types::ToolError>, approval: Option<Approval>| {
        let mut result = match result {
            Ok(result) => result,
            Err(err) => return noted(fail(err.0, &label)),
        };
        result.door = door;
        if result.provenance.taints() {
            ctx.taint.mark(result.provenance);
        }
        if approval.is_some() {
            result.approval = approval;
        }
        noted(ToolOutcome { result, label: label.clone() })
    };

    let network = tool.uses_network(input, ctx);
    let protected = tool.protected_target(input, ctx);
    // A web request under a URL prefix the user approved after the taint (docs/SECURITY.md) runs as approved.
    let egress_url = tool.egress_url(input).filter(|_| ctx.taint.is_tainted());
    let prefix_approved = egress_url.as_deref().is_some_and(|url| ctx.taint.prefix_approved(url));
    let must_ask = protected.is_some() || (ctx.taint.must_ask(tool.kind(), network) && !prefix_approved);
    let gated = must_ask || (!prefix_approved && tool.needs_approval(input).unwrap_or(tool.kind() != ToolKind::Read));
    if !gated {
        return finish(tool.run(input, &per_call).await, None);
    }

    // The preview runs first, so a call that can't succeed fails here instead of being approved.
    let preview = match tool.preview(input, ctx).await {
        Ok(preview) => preview,
        Err(err) => return noted(fail(err.0, &label)),
    };
    let interrupted = || {
        let mut result = ToolOutput::ok(INTERRUPTED, "interrupted");
        result.declined = true;
        result.approval = Some(Approval::Interrupted);
        result.door = door;
        noted(ToolOutcome { result, label: label.clone() })
    };
    // Stopped while the preview was being made: don't raise a prompt for a run that's over.
    if ctx.aborted() {
        return interrupted();
    }
    // Yolo: a call the tool vouches for runs without asking, confined (bash then can't touch .git).
    if ctx.yolo && !must_ask && tool.auto_safe(input, ctx) {
        let confined = ToolContext { confined: true, ..per_call };
        return finish(tool.run(input, &confined).await, Some(Approval::Auto));
    }
    let no_one_to_ask = || noted(fail(format!("{} needs the user's approval, and there's no one to ask.", call.name), &label));
    let Some(approver) = &ctx.approver else { return no_one_to_ask() };
    let scope =
        tool.scope(input, ctx).unwrap_or_else(|| crate::types::Scope { key: call.name.clone(), description: call.name.clone() });
    let request = ApprovalRequest {
        tool: call.name.clone(),
        label: label.clone(),
        preview,
        scope,
        agent: None,
        network,
        once: tool.once(input, ctx),
        must_ask,
        protected,
        door,
    };
    match approver.approve(request).await {
        Decision::Unavailable => no_one_to_ask(),
        // Esc at the prompt answers "no" and then aborts the run: an interrupt, not "don't retry".
        Decision::No if ctx.aborted() => interrupted(),
        Decision::No => {
            let mut result = ToolOutput::ok(DECLINED, "declined");
            result.declined = true;
            result.approval = Some(Approval::Decided(Decision::No));
            result.door = door;
            noted(ToolOutcome { result, label: label.clone() })
        }
        decision => {
            // "Always" on a tainted web request covers its URL prefix, not the whole site (marv-rs S6).
            if decision == Decision::Always
                && must_ask
                && let Some(prefix) = egress_url.as_deref().and_then(crate::web_fetch::url_prefix)
            {
                ctx.taint.approve_prefix(&prefix);
            }
            finish(tool.run(input, &per_call).await, Some(Approval::Decided(decision)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Approver, Preview, Tool, ToolError};
    use async_trait::async_trait;
    use marv_core::Provenance;
    use serde::Deserialize;
    use std::sync::Mutex;

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct EchoInput {
        text: String,
    }

    /// A write tool that echoes; `safe` decides `auto_safe`.
    struct Echo {
        kind: ToolKind,
        safe: bool,
        provenance: Provenance,
    }

    #[async_trait]
    impl Tool for Echo {
        type Input = EchoInput;
        fn name(&self) -> &str {
            "echo"
        }
        fn description(&self) -> &str {
            "Echo."
        }
        fn schema(&self) -> Value {
            serde_json::json!({"type": "object"})
        }
        fn kind(&self) -> ToolKind {
            self.kind
        }
        fn provenance(&self) -> Provenance {
            self.provenance
        }
        fn label(&self, input: &EchoInput) -> String {
            input.text.clone()
        }
        fn auto_safe(&self, _input: &EchoInput, _ctx: &ToolContext) -> bool {
            self.safe
        }
        // "host …" goes through the host door, asked every time.
        fn door(&self, input: &EchoInput, _ctx: &ToolContext) -> Option<crate::types::Door> {
            input.text.starts_with("host").then_some(crate::types::Door::Host)
        }
        fn once(&self, input: &EchoInput, _ctx: &ToolContext) -> bool {
            input.text.starts_with("host")
        }
        async fn preview(&self, input: &EchoInput, _ctx: &ToolContext) -> Result<Preview, ToolError> {
            if input.text == "impossible" {
                return Err(ToolError::new("can't"));
            }
            Ok(Preview { title: "Echo".into(), ..Default::default() })
        }
        async fn run(&self, input: &EchoInput, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
            let mut out = ToolOutput::ok(format!("{}{}", input.text, if ctx.confined { " (confined)" } else { "" }), "ok");
            out.provenance = self.provenance;
            Ok(out)
        }
    }

    struct Answer(Decision, Mutex<Vec<ApprovalRequest>>);
    #[async_trait]
    impl Approver for Answer {
        async fn approve(&self, request: ApprovalRequest) -> Decision {
            self.1.lock().unwrap().push(request);
            self.0
        }
    }

    fn call(args: &str) -> ToolCall {
        ToolCall { id: "c1".into(), name: "echo".into(), arguments: args.into() }
    }
    fn tools(kind: ToolKind, safe: bool) -> Vec<ToolRef> {
        vec![Arc::new(Echo { kind, safe, provenance: Provenance::Project })]
    }
    fn ctx() -> ToolContext {
        ToolContext::new("/tmp/marv-registry-test")
    }

    #[tokio::test]
    async fn unknown_tools_bad_json_and_invalid_input_are_errors_not_panics() {
        let t = tools(ToolKind::Read, false);
        let unknown = run_tool(&ToolCall { id: "1".into(), name: "nope".into(), arguments: "{}".into() }, &ctx(), &t).await;
        assert_eq!(unknown.result.output, "Unknown tool \"nope\". Available tools: echo.");
        assert!(run_tool(&call("{not json"), &ctx(), &t).await.result.output.contains("not valid JSON"));
        let invalid = run_tool(&call(r#"{"text": 1}"#), &ctx(), &t).await;
        assert!(invalid.result.is_error && invalid.result.output.starts_with("Invalid input for echo:"));
    }

    #[tokio::test]
    async fn read_tools_run_freely_and_raw_tabs_are_repaired_with_a_note() {
        let out = run_tool(&call("{\"text\": \"a\tb\"}"), &ctx(), &tools(ToolKind::Read, false)).await;
        assert!(!out.result.is_error);
        assert!(out.result.output.starts_with("a\tb\n\nMarv: the arguments contained 1 raw control character"));
        assert_eq!(out.label, "a\tb");
    }

    #[tokio::test]
    async fn a_door_call_names_its_door_on_the_approval_and_the_result() {
        let t = tools(ToolKind::Execute, false);
        let approver = Arc::new(Answer(Decision::Yes, Mutex::new(vec![])));
        let asking = ToolContext { approver: Some(approver.clone()), ..ctx() };
        let host = run_tool(&call(r#"{"text": "host ls"}"#), &asking, &t).await;
        assert_eq!(host.result.door, Some(crate::types::Door::Host));
        let plain = run_tool(&call(r#"{"text": "ls"}"#), &asking, &t).await;
        assert_eq!(plain.result.door, None);
        let asked: Vec<_> = approver.1.lock().unwrap().iter().map(|r| (r.door, r.once)).collect();
        assert_eq!(asked, [(Some(crate::types::Door::Host), true), (None, false)]);
        // Declined, it still says which door it would have gone through.
        let no = ToolContext { approver: Some(Arc::new(Answer(Decision::No, Mutex::new(vec![])))), ..ctx() };
        let declined = run_tool(&call(r#"{"text": "host rm"}"#), &no, &t).await;
        assert!(declined.result.declined);
        assert_eq!(declined.result.door, Some(crate::types::Door::Host));
    }

    #[tokio::test]
    async fn gated_calls_preview_then_ask() {
        let t = tools(ToolKind::Write, false);
        // No approver: refused as an error the model can work around.
        let alone = run_tool(&call(r#"{"text": "x"}"#), &ctx(), &t).await;
        assert_eq!(alone.result.output, "echo needs the user's approval, and there's no one to ask.");
        // A preview error means nobody is asked.
        let approver = Arc::new(Answer(Decision::Yes, Mutex::new(vec![])));
        let asking = ToolContext { approver: Some(approver.clone()), ..ctx() };
        let impossible = run_tool(&call(r#"{"text": "impossible"}"#), &asking, &t).await;
        assert_eq!(impossible.result.output, "can't");
        assert!(approver.1.lock().unwrap().is_empty());
        let yes = run_tool(&call(r#"{"text": "x"}"#), &asking, &t).await;
        assert_eq!(yes.result.output, "x");
        assert_eq!(yes.result.approval, Some(Approval::Decided(Decision::Yes)));
        assert_eq!(approver.1.lock().unwrap()[0].scope.key, "echo");
    }

    #[tokio::test]
    async fn no_declines_and_stops_and_no_after_abort_is_an_interrupt() {
        let t = tools(ToolKind::Write, false);
        let no = ToolContext { approver: Some(Arc::new(Answer(Decision::No, Mutex::new(vec![])))), ..ctx() };
        let declined = run_tool(&call(r#"{"text": "x"}"#), &no, &t).await;
        assert!(declined.result.declined);
        assert_eq!(declined.result.output, DECLINED);
        no.cancel.cancel();
        let interrupted = run_tool(&call(r#"{"text": "x"}"#), &no, &t).await;
        assert_eq!(interrupted.result.output, INTERRUPTED);
        assert_eq!(interrupted.result.approval, Some(Approval::Interrupted));
    }

    #[tokio::test]
    async fn yolo_runs_auto_safe_calls_confined_unless_tainted_egress() {
        let yolo = ToolContext { yolo: true, ..ctx() };
        let auto = run_tool(&call(r#"{"text": "x"}"#), &yolo, &tools(ToolKind::Write, true)).await;
        assert_eq!(auto.result.output, "x (confined)");
        assert_eq!(auto.result.approval, Some(Approval::Auto));
        // marv-rs: tainted egress asks even in yolo, with must_ask set.
        yolo.taint.mark(Provenance::Web);
        let approver = Arc::new(Answer(Decision::Yes, Mutex::new(vec![])));
        let asking = ToolContext { approver: Some(approver.clone()), ..yolo };
        let egress = run_tool(&call(r#"{"text": "x"}"#), &asking, &tools(ToolKind::Egress, true)).await;
        assert_eq!(egress.result.output, "x");
        assert!(approver.1.lock().unwrap()[0].must_ask);
    }

    #[tokio::test]
    async fn a_web_result_taints_the_session() {
        let t: Vec<ToolRef> = vec![Arc::new(Echo { kind: ToolKind::Read, safe: false, provenance: Provenance::Web })];
        let c = ctx();
        run_tool(&call(r#"{"text": "x"}"#), &c, &t).await;
        assert!(c.taint.is_tainted());
    }

    #[test]
    fn builtin_specs_are_in_marvs_order_and_skill_is_optional() {
        let names: Vec<&str> = tool_specs().iter().map(|s| s.name.as_str()).collect();
        assert_eq!(
            names,
            ["read_file", "glob", "grep", "web_fetch", "skill", "edit_file", "write_file", "bash", "memory", "agent"]
        );
        assert!(!tool_specs_for(false).iter().any(|s| s.name == "skill"));
        assert_eq!(tool_specs_for(true).len(), 10);
        let agent = ToolCall { id: "1".into(), name: "agent".into(), arguments: "{}".into() };
        assert!(is_parallel_call(&agent, builtin_tools()));
    }
}
