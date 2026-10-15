//! The tool registry: what the model is offered, and how its calls are run.
//!
//! Ports Marv's `src/tools/index.ts` (named `registry` here: `lib.rs` is the crate's index).
//!
//! - Tool specs are built once, so every request sends byte-identical definitions in a stable order (anything else
//!   breaks the prompt cache). Note: `serde_json` without `preserve_order` sorts object keys, so a schema's key order
//!   differs from Marv's (zod's insertion order); it's still byte-stable, which is what the cache needs.
//! - `skill` is offered only when skills exist, `plan` unless the session turned it off (#15), `github` only where the
//!   session has it (#47) (`tool_specs_for`).
//! - `run_tool` never fails: bad JSON, invalid input and `ToolError`s become error results the model can recover from.
//!   `available` is what this agent was offered (a subagent gets fewer tools); anything else is unknown, even if the
//!   model names it.
//! - Flow: parse JSON (else `repair_json_args`) → schema-guided repair (#65, [`schema_repaired`]) → the tool's
//!   `repair` → validate → label → if gated: preview (a
//!   `ToolError` fails the call before anyone is asked) → aborted? interrupted → the tool's own prompt
//!   (`approve_itself`, #77; never for a protected path) → else yolo + `auto_safe`: run confined → no approver: "no
//!   one to ask" → approve → either answer: unavailable / no (aborted → interrupted) / yes / always → run.
//! - Right before any run (ungated, yolo or approved), the turn's checkpoint (#16) when the tool takes one
//!   (`Tool::checkpoint`); a stop while it's taken returns "interrupted" instead of running.
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
use crate::schema_repair::{RepairMode, repair_args, repair_note};
use crate::types::{Approval, ApprovalRequest, Decision, DynTool, ToolContext, ToolOutput};

/// A tool as the registry holds it.
pub type ToolRef = Arc<dyn DynTool>;

/// What the model reads for a call stopped before it ran. Must equal marv-agent's `NOT_RUN.aborted`.
pub const INTERRUPTED: &str = "Interrupted by the user before this tool ran.";
/// What the model reads when the user said no.
pub const DECLINED: &str = "The user declined this. Don't retry it: stop and wait for them to say how to proceed.";

/// Marv's built-in tools, in Marv's order (the order of the specs the model sees):
/// read_file, glob, grep, web_fetch, github, skill, edit_file, write_file, bash, process, memory, plan, agent.
/// `github` is offered only where the session has it ([`tool_specs_for`]). `apply_patch` (#66) isn't registered until
/// it's built.
pub fn builtin_tools() -> &'static [ToolRef] {
    static TOOLS: OnceLock<Vec<ToolRef>> = OnceLock::new();
    TOOLS.get_or_init(|| {
        vec![
            Arc::new(crate::read_file::ReadFile),
            Arc::new(crate::glob::Glob),
            Arc::new(crate::grep::Grep),
            Arc::new(crate::web_fetch::WebFetch::default()),
            Arc::new(crate::github::Github::default()),
            Arc::new(crate::skill::SkillTool),
            Arc::new(crate::edit_file::EditFile),
            Arc::new(crate::write_file::WriteFile),
            Arc::new(crate::bash::Bash),
            Arc::new(crate::process::ProcessTool),
            Arc::new(crate::memory::Memory),
            Arc::new(crate::plan::Plan),
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

/// The specs for a session: the skill tool only when there are skills to load, the plan tool unless the session
/// turned it off (#15; decided at construction, since it changes the tool list), the github tool only where gh is
/// there and logged in (#47). The process tool is in it: a session without the process door (Marv #51) drops it. Same
/// values every time (cache).
pub fn tool_specs_for(has_skills: bool, plan: bool, github: bool) -> Vec<ToolSpec> {
    tool_specs()
        .iter()
        .filter(|s| (has_skills || s.name != "skill") && (plan || s.name != "plan") && (github || s.name != "github"))
        .cloned()
        .collect()
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

/// `raw` repaired against `tool`'s schema (#65), and the repairs' `Marv:` note. The value to parse, never the call's
/// stored arguments: `ToolCall::arguments` stays as the model sent it (prompt cache).
pub fn schema_repaired(tool: &dyn DynTool, raw: Value) -> (Value, Option<String>) {
    // What a built-in tool already accepts isn't a mistake, even where it strays from the schema (`null` for an
    // `Option` field): a note there would be noise. An MCP tool's input is any object, so its parse can't tell.
    if tool.repair_mode() == RepairMode::Builtin && tool.parse(raw.clone()).is_ok() {
        return (raw, None);
    }
    let (value, fixes) = repair_args(&tool.spec().parameters, raw, tool.repair_mode());
    (value, repair_note(&fixes))
}

/// Whether a call is a file change (a `Write` tool with the "files" scope: edit_file, write_file) in a project that is
/// the home folder or holds it ([`marv_sandbox::home_project`]). Such a call is asked `once`, every time.
pub fn home_file_write(kind: ToolKind, scope_key: &str, ctx: &ToolContext) -> bool {
    kind == ToolKind::Write
        && scope_key == "files"
        && !ctx.home.as_os_str().is_empty()
        && marv_sandbox::home_project(&ctx.root, &ctx.home)
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
    // Generic schema mistakes (a number as a string, another harness's field), then the tool's own repair of what
    // it can read with certainty, before validation. Both repair this value: `call` is never touched (prompt cache).
    let (raw, schema_note) = schema_repaired(tool.as_ref(), raw);
    let (input_value, tool_note) = match tool.repair(&raw, ctx) {
        Some(repaired) => (repaired.input, repaired.note),
        None => (raw, None),
    };
    let note = [json_note, schema_note, tool_note].into_iter().flatten().collect::<Vec<_>>().join("\n\n");
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
    let interrupted = |approval: Approval| {
        let mut result = ToolOutput::ok(INTERRUPTED, "interrupted");
        result.declined = true;
        result.approval = Some(approval);
        result.door = door;
        noted(ToolOutcome { result, label: label.clone() })
    };
    // The turn's checkpoint, before anything that can change files runs (approved, yolo, or a tool that asks no one):
    // whatever the call does, bash included, /undo can put back. It never fails the call; a stop meanwhile does.
    let checkpointed = || async {
        if let Some(checkpoint) = ctx.checkpoint.as_ref().filter(|_| tool.checkpoint()) {
            checkpoint.checkpoint().await;
        }
        !ctx.aborted()
    };
    if !gated {
        if !checkpointed().await {
            return interrupted(Approval::Interrupted);
        }
        return finish(tool.run(input, &per_call).await, None);
    }

    // The preview runs first, so a call that can't succeed fails here instead of being approved.
    let preview = match tool.preview(input, ctx).await {
        Ok(preview) => preview,
        Err(err) => return noted(fail(err.0, &label)),
    };
    // Stopped while the preview was being made: don't raise a prompt for a run that's over.
    if ctx.aborted() {
        return interrupted(Approval::Interrupted);
    }
    let no_one_to_ask = || noted(fail(format!("{} needs the user's approval, and there's no one to ask.", call.name), &label));
    // #77: a call the tool asks a person about itself, in one prompt (a mesh call and the new wire it needs). Never
    // for a protected path, whose warning only the registry's prompt carries.
    let own = match protected {
        None => tool.approve_itself(input, &per_call, &preview).await,
        Some(_) => None,
    };
    let decision = match own {
        Some(Err(err)) => return noted(fail(err.0, &label)),
        Some(Ok(decision)) => decision,
        None => {
            let scope = tool
                .scope(input, ctx)
                .unwrap_or_else(|| crate::types::Scope { key: call.name.clone(), description: call.name.clone() });
            // In the home folder (the project is `~` or holds it), a file change asks each time: the edit tools write
            // outside the sandbox, and a plain file there (a script on PATH, another repository's Makefile) can run on
            // the host later. So no "always" is offered, no remembered or config-seeded "files" covers it, and yolo
            // (which the session refuses there anyway) doesn't run it.
            let home_write = home_file_write(tool.kind(), &scope.key, ctx);
            let once = tool.once(input, ctx) || home_write;
            // Yolo: a call the tool vouches for runs without asking, confined (bash then can't touch .git).
            if ctx.yolo && !must_ask && !home_write && tool.auto_safe(input, ctx) {
                if !checkpointed().await {
                    return interrupted(Approval::Interrupted);
                }
                let confined = ToolContext { confined: true, ..per_call };
                return finish(tool.run(input, &confined).await, Some(Approval::Auto));
            }
            let Some(approver) = &ctx.approver else { return no_one_to_ask() };
            let request = ApprovalRequest {
                tool: call.name.clone(),
                label: label.clone(),
                preview,
                scope,
                agent: None,
                network,
                once,
                must_ask,
                protected,
                door,
            };
            approver.approve(request).await
        }
    };
    match decision {
        Decision::Unavailable => no_one_to_ask(),
        // Esc at the prompt answers "no" and then aborts the run: an interrupt, not "don't retry".
        Decision::No if ctx.aborted() => interrupted(Approval::Interrupted),
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
            if !checkpointed().await {
                return interrupted(Approval::Decided(decision));
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
        // "self yes" / "self no" / "self err": the tool's own prompt answers (#77).
        async fn approve_itself(
            &self,
            input: &EchoInput,
            _ctx: &ToolContext,
            preview: &Preview,
        ) -> Option<Result<Decision, ToolError>> {
            assert_eq!(preview.title, "Echo", "the registry's preview is handed over");
            match input.text.as_str() {
                "self yes" => Some(Ok(Decision::Yes)),
                "self no" => Some(Ok(Decision::No)),
                "self err" => Some(Err(ToolError::new("the far end said no"))),
                _ => None,
            }
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

    /// #65 through `run_tool`, with Marv's own tools: repaired calls run and say so, valid ones get no note, an
    /// ambiguous one fails as it did before, and the stored call is the model's bytes.
    #[tokio::test]
    async fn schema_mistakes_are_repaired_with_a_note_and_the_call_is_left_as_sent() {
        let root = crate::test_support::project("schema-repair");
        let ctx = ToolContext::new(root.path());
        let run = |name: &str, args: &str| {
            let call = ToolCall { id: "c".into(), name: name.into(), arguments: args.into() };
            let ctx = ctx.clone();
            async move {
                let out = run_tool(&call, &ctx, builtin_tools()).await;
                (out, call)
            }
        };
        // #73 multi-ask: grep with another harness's head_limit.
        let (out, call) = run("grep", r#"{"pattern": "greet", "head_limit": 30}"#).await;
        assert!(!out.result.is_error, "{}", out.result.output);
        assert!(out.result.output.contains("src/greet.ts"));
        assert!(out.result.output.ends_with(
            "\n\nMarv: the arguments didn't match the tool's schema and were repaired before the call: unknown field \"head_limit\" was dropped. Send them as the schema says next time."
        ));
        assert_eq!(call.arguments, r#"{"pattern": "greet", "head_limit": 30}"#); // prompt cache: never rewritten
        let (out, _) = run("read_file", r#"{"path": "src/app.ts", "limit": "1", "offset": null}"#).await;
        assert!(!out.result.is_error, "{}", out.result.output);
        assert!(out.result.output.contains("\"limit\" was the string \"1\": read as the integer 1"));
        assert!(out.result.output.contains("\"offset\" was null: left out (it's optional)"));
        // Valid: no note at all.
        let (out, _) = run("read_file", r#"{"path": "src/app.ts", "limit": 1}"#).await;
        assert!(!out.result.output.contains("Marv:"), "{}", out.result.output);
        // Ambiguous: today's error, no note.
        let (out, _) = run("read_file", r#"{"path": "src/app.ts", "limit": "20abc"}"#).await;
        assert!(out.result.is_error);
        assert!(out.result.output.starts_with("Invalid input for read_file:\ninvalid type: string \"20abc\", expected"));
        assert!(!out.result.output.contains("Marv:"));
        // Schema repair and a tool's own repair both apply (a leading-slash path), notes in that order.
        let (out, _) = run("read_file", r#"{"path": "/src/app.ts", "limit": "1"}"#).await;
        assert!(!out.result.is_error, "{}", out.result.output);
        let (schema, path) = (out.result.output.find("schema").unwrap(), out.result.output.find("leading slash").unwrap());
        assert!(schema < path);
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

    /// #77: a call the tool asks about itself isn't asked again by the registry, tainted or in yolo; its answer is the
    /// call's (no declines and stops, an error fails the call), and it's recorded as decided.
    #[tokio::test]
    async fn a_call_the_tool_asks_about_itself_is_asked_once() {
        let t = tools(ToolKind::Egress, true);
        let approver = Arc::new(Answer(Decision::Yes, Mutex::new(vec![])));
        let asking = ToolContext { approver: Some(approver.clone()), yolo: true, ..ctx() };
        asking.taint.mark(Provenance::Mesh);
        let yes = run_tool(&call(r#"{"text": "self yes"}"#), &asking, &t).await;
        assert_eq!(yes.result.output, "self yes", "not confined: not yolo's");
        assert_eq!(yes.result.approval, Some(Approval::Decided(Decision::Yes)));
        let no = run_tool(&call(r#"{"text": "self no"}"#), &asking, &t).await;
        assert!(no.result.declined);
        assert_eq!(no.result.output, DECLINED);
        let err = run_tool(&call(r#"{"text": "self err"}"#), &asking, &t).await;
        assert!(err.result.is_error);
        assert_eq!(err.result.output, "the far end said no");
        assert!(approver.1.lock().unwrap().is_empty(), "the registry asked no one");
        // Any other call is asked as before.
        run_tool(&call(r#"{"text": "x"}"#), &asking, &t).await;
        assert_eq!(approver.1.lock().unwrap().len(), 1);
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
    fn builtin_specs_are_in_marvs_order_and_skill_and_plan_are_optional() {
        let names: Vec<&str> = tool_specs().iter().map(|s| s.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "read_file",
                "glob",
                "grep",
                "web_fetch",
                "github",
                "skill",
                "edit_file",
                "write_file",
                "bash",
                "process",
                "memory",
                "plan",
                "agent"
            ]
        );
        assert!(!tool_specs_for(false, true, true).iter().any(|s| s.name == "skill"));
        assert_eq!(tool_specs_for(true, true, true).len(), 13);
        assert!(!tool_specs_for(true, false, true).iter().any(|s| s.name == "plan"));
        assert_eq!(tool_specs_for(true, false, true).len(), 12);
        // github only where it's offered (#47), the same values every time (cache).
        assert!(!tool_specs_for(true, true, false).iter().any(|s| s.name == "github"));
        assert_eq!(tool_specs_for(true, true, true), tool_specs_for(true, true, true));
        let agent = ToolCall { id: "1".into(), name: "agent".into(), arguments: "{}".into() };
        assert!(is_parallel_call(&agent, builtin_tools()));
    }

    // Ports tests/tools.test.ts "the turn's checkpoint" (#16).
    mod checkpoint {
        use super::*;
        use crate::test_support::project;
        use crate::types::Checkpoint;

        type Order = Arc<Mutex<Vec<&'static str>>>;

        struct Counting(Order);
        #[async_trait]
        impl Checkpoint for Counting {
            async fn checkpoint(&self) {
                self.0.lock().unwrap().push("checkpoint");
            }
        }
        struct Saying(Order, Decision);
        #[async_trait]
        impl Approver for Saying {
            async fn approve(&self, _request: ApprovalRequest) -> Decision {
                self.0.lock().unwrap().push("approve");
                self.1
            }
        }

        fn named(name: &str, args: Value) -> ToolCall {
            ToolCall { id: "c1".into(), name: name.into(), arguments: args.to_string() }
        }
        fn counted(root: &std::path::Path, order: &Order, answer: Option<Decision>) -> ToolContext {
            let mut ctx = ToolContext::new(root);
            ctx.sandbox = Arc::new(marv_sandbox::Sandbox::from_env(false));
            ctx.checkpoint = Some(Arc::new(Counting(order.clone())));
            ctx.approver = answer.map(|a| Arc::new(Saying(order.clone(), a)) as Arc<dyn Approver>);
            ctx
        }
        fn without_approvals(order: &Order) -> Vec<&'static str> {
            order.lock().unwrap().iter().copied().filter(|e| *e != "approve").collect()
        }

        #[tokio::test]
        async fn taken_before_a_change_runs_approved_and_in_yolo() {
            let root = project("checkpoint-yes");
            let order: Order = Arc::default();
            let ctx = counted(root.path(), &order, Some(Decision::Yes));
            run_tool(&named("write_file", serde_json::json!({"path": "new.txt", "content": "x\n"})), &ctx, builtin_tools()).await;
            assert_eq!(*order.lock().unwrap(), ["approve", "checkpoint"]);
            order.lock().unwrap().clear();
            let yolo = ToolContext { yolo: true, approver: None, ..ctx.clone() };
            let edit = named("edit_file", serde_json::json!({"path": "src/app.ts", "old_string": "main", "new_string": "start"}));
            let edited = run_tool(&edit, &yolo, builtin_tools()).await;
            assert_eq!(edited.result.approval, Some(Approval::Auto), "{}", edited.result.output);
            assert_eq!(*order.lock().unwrap(), ["checkpoint"]);
            run_tool(&named("bash", serde_json::json!({"command": "true"})), &ctx, builtin_tools()).await;
            assert_eq!(without_approvals(&order), ["checkpoint", "checkpoint"]);
        }

        #[tokio::test]
        async fn not_for_reads_and_not_when_declined_invalid_or_aborted() {
            let root = project("checkpoint-no");
            let order: Order = Arc::default();
            let ctx = counted(root.path(), &order, None);
            for (name, args) in [
                ("read_file", serde_json::json!({"path": "src/app.ts"})),
                ("glob", serde_json::json!({"pattern": "**/*.ts"})),
                ("grep", serde_json::json!({"pattern": "greet"})),
            ] {
                run_tool(&named(name, args), &ctx, builtin_tools()).await;
            }
            let write = named("write_file", serde_json::json!({"path": "a.txt", "content": "x"}));
            run_tool(&write, &counted(root.path(), &order, Some(Decision::No)), builtin_tools()).await;
            let yes = counted(root.path(), &order, Some(Decision::Yes));
            run_tool(&named("write_file", serde_json::json!({"nope": true})), &yes, builtin_tools()).await;
            yes.cancel.cancel();
            run_tool(&write, &yes, builtin_tools()).await;
            assert!(without_approvals(&order).is_empty());
        }

        /// A tool that changes nothing in the project (an MCP server's, memory, a subagent).
        struct Outsider;
        #[async_trait]
        impl Tool for Outsider {
            type Input = Value;
            fn name(&self) -> &str {
                "outsider"
            }
            fn description(&self) -> &str {
                ""
            }
            fn schema(&self) -> Value {
                serde_json::json!({"type": "object"})
            }
            fn kind(&self) -> ToolKind {
                ToolKind::Execute
            }
            fn checkpoint(&self) -> bool {
                false
            }
            fn label(&self, _input: &Value) -> String {
                "x".into()
            }
            async fn run(&self, _input: &Value, _ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
                Ok(ToolOutput::ok("ok", "ok"))
            }
        }

        #[tokio::test]
        async fn not_for_a_tool_that_changes_nothing_in_the_project() {
            let root = project("checkpoint-outsider");
            let order: Order = Arc::default();
            let ctx = counted(root.path(), &order, Some(Decision::Yes));
            let out = run_tool(&named("outsider", serde_json::json!({})), &ctx, &[Arc::new(Outsider) as ToolRef]).await;
            assert_eq!(out.result.output, "ok");
            assert!(without_approvals(&order).is_empty());
        }

        #[test]
        fn the_default_is_any_kind_that_changes_files_and_memory_and_agent_opt_out() {
            let takes = |name: &str| builtin_tools().iter().find(|t| t.name() == name).unwrap().checkpoint();
            for name in ["edit_file", "write_file", "bash"] {
                assert!(takes(name), "{name}");
            }
            for name in ["read_file", "glob", "grep", "web_fetch", "skill", "memory", "plan", "agent"] {
                assert!(!takes(name), "{name}");
            }
        }

        /// Marv's "a failing checkpoint doesn't fail the call": here `Checkpoint::checkpoint` can't fail (it returns
        /// nothing), so what's left to check is that a stop while it runs doesn't run the call.
        struct StopsMeanwhile(tokio_util::sync::CancellationToken);
        #[async_trait]
        impl Checkpoint for StopsMeanwhile {
            async fn checkpoint(&self) {
                self.0.cancel();
            }
        }

        #[tokio::test]
        async fn a_stop_while_checkpointing_is_an_interrupt_and_writes_nothing() {
            let root = project("checkpoint-stop");
            let order: Order = Arc::default();
            let mut ctx = counted(root.path(), &order, Some(Decision::Yes));
            ctx.checkpoint = Some(Arc::new(StopsMeanwhile(ctx.cancel.clone())));
            let out =
                run_tool(&named("write_file", serde_json::json!({"path": "b.txt", "content": "x\n"})), &ctx, builtin_tools())
                    .await;
            assert_eq!(out.result.output, INTERRUPTED);
            // Marv keeps the answer the call got: it was approved, then stopped.
            assert_eq!(out.result.approval, Some(Approval::Decided(Decision::Yes)));
            assert!(!root.path().join("b.txt").exists());
        }
    }
}
