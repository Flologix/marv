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

use std::panic::AssertUnwindSafe;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::{FutureExt, StreamExt};
use marv_agent::{AgentOptions, History, LoopEvent, run_agent};
use marv_core::{ChatTurn, StopReason};
use marv_tools::agent::{AgentInput, Isolation};
use marv_tools::{AgentProgress, ApprovalRequest, Approver, Decision, Preview, ToolContext, ToolError, ToolOutput, ToolRef};

use crate::agents::{AgentType, SUBAGENT_TOOLS, find_agent};
use crate::paths::{home_dir, shorten_home};
use crate::prompt::{Listed, SubagentPromptInput, subagent_prompt};
use crate::runner::{SessionAgentHost, SessionApprover, SessionToolRunner};
use crate::time::{iso_date, now_ms};
use crate::usage::tokens;
use crate::worktree::{NOT_A_REPO, Worktree, create_worktree, finish_worktree, inspect_repo, worktree_env};

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
    // Its sandboxed bash reaches the session's background processes like the main agent's (curl localhost:<port>);
    // starting one is the main agent's alone (no processes, no doors).
    ctx.forwards = parent.forwards.clone();
    // A shared-folder subagent edits the same files in the same turn: the turn's one checkpoint covers it (#16). A
    // worktree's changes land on its branch, which is already the way back.
    ctx.checkpoint = if worktree.is_some() { None } else { parent.checkpoint.clone() };
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

/// The prompt shown in the worktree dispatch's approval, at most.
pub use marv_tools::agent::PREVIEW_CHARS;

/// The worktree dispatch's approval (the preview in Marv's `src/tools/agent.ts`). It makes every check
/// [`run_subagent`] makes before it starts, so the user is never asked to approve one that can't run.
pub async fn preview(host: &SessionAgentHost, input: &AgentInput, ctx: &ToolContext) -> Result<Preview, ToolError> {
    if host.worktrees_dir.is_none() {
        return Err(ToolError::new(NO_WORKTREES));
    }
    let found = resolve_type(&host.agents, input.agent_type.as_deref())?;
    let repo = inspect_repo(&ctx.root).await.ok_or_else(|| ToolError::new(NOT_A_REPO))?;
    let auto = ctx.sandbox.active();
    let text = if input.prompt.chars().count() > PREVIEW_CHARS {
        format!("{}…", input.prompt.chars().take(PREVIEW_CHARS).collect::<String>())
    } else {
        input.prompt.clone()
    };
    let working = if repo.prefix.is_empty() { String::new() } else { format!(", working in {}", repo.prefix) };
    let asks = if auto {
        "edits and sandboxed commands run without asking inside its worktree; network commands still ask"
    } else {
        "asks before each change (no sandbox)"
    };
    Ok(Preview {
        title: format!("Start {} in its own worktree: {}", found.name, input.description),
        text: Some(text),
        note: Some(format!("new branch from {}{working} · {asks}", repo.base)),
        // git counts the whole repository's changes, even from a subfolder, so say which.
        warning: (repo.dirty > 0).then(|| {
            format!(
                "{} uncommitted change{} in the {} won't be in the worktree (it starts from {})",
                repo.dirty,
                if repo.dirty == 1 { "" } else { "s" },
                if repo.prefix.is_empty() { "project" } else { "repository" },
                repo.base
            )
        }),
        ..Preview::default()
    })
}

/// What a running subagent has done so far, for its progress line and its report.
struct Tally {
    call_id: Option<String>,
    sink: Arc<dyn crate::runner::SubagentSink>,
    /// "worktree marv/x-1a2b" or "shared folder".
    place: String,
    tool_count: usize,
    used: u64,
    current: Option<String>,
    steps: Vec<String>,
}

impl Tally {
    fn report(&self) {
        let Some(id) = &self.call_id else { return };
        let line =
            format!("{} · {} · {}", self.place, plural(self.tool_count, "tool"), self.current.as_deref().unwrap_or("thinking…"));
        let steps = self.steps[self.steps.len().saturating_sub(MAX_STEPS_KEPT)..].to_vec();
        self.sink.progress(id, AgentProgress { line, steps });
    }
}

fn reason_text(reason: StopReason) -> String {
    serde_json::to_value(reason).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_else(|| "error".into())
}

fn panic_text(panic: &(dyn std::any::Any + Send)) -> String {
    panic
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| panic.downcast_ref::<&str>().map(|s| s.to_string()))
        .unwrap_or_else(|| "it panicked".into())
}

/// Runs a subagent (Marv's `runSubagent`): resolves its type, makes the worktree, runs `marv_agent::run_agent` with
/// `SUBAGENT_MAX_STEPS`, forwards every loop event to the host's sink (`SessionEvent::Subagent`), reports progress,
/// finishes the worktree however it ended, and returns [`subagent_report`].
pub async fn run_subagent(host: &SessionAgentHost, input: &AgentInput, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
    let agent_type = resolve_type(&host.agents, input.agent_type.as_deref())?;
    let who = format!("{} · {}", agent_type.name, input.description);

    let worktree = match input.isolation {
        Some(Isolation::Worktree) => {
            let base_dir = host.worktrees_dir.as_ref().ok_or_else(|| ToolError::new(NO_WORKTREES))?;
            Some(create_worktree(&ctx.root, base_dir, &input.description).await?)
        }
        None => None,
    };

    let mut tally = Tally {
        call_id: ctx.call_id.clone(),
        sink: host.sink.clone(),
        place: worktree.as_ref().map_or_else(|| "shared folder".into(), |w| format!("worktree {}", w.branch)),
        tool_count: 0,
        used: 0,
        current: None,
        steps: Vec::new(),
    };
    let mut run = SubagentRun { reason: "error".into(), ..SubagentRun::default() };

    // Everything after the worktree exists runs inside this, and a panic (Marv: a provider that throws instead of
    // sending an error event) is caught, so the worktree is finished (its changes committed, its folder removed)
    // however the subagent ends: a reply, an error, ctrl+c, a "no", or a panic. The parent still gets what the
    // subagent had, and its branch.
    let body =
        AssertUnwindSafe(drive(host, input, ctx, agent_type, &who, worktree.as_ref(), &mut tally, &mut run)).catch_unwind().await;
    if let Err(panic) = body {
        run.reason = "error".into();
        run.error = Some(panic_text(panic.as_ref()));
    }
    if let Some(wt) = &worktree {
        let finished = AssertUnwindSafe(finish_worktree(wt, &input.description, run.reason != "end")).catch_unwind().await;
        run.branch_line = Some(from_marv(&match finished {
            Ok(line) => line,
            Err(panic) => format!("couldn't finish the worktree ({}); it's at {}.", panic_text(panic.as_ref()), wt.dir.display()),
        }));
    }
    run.tool_count = tally.tool_count;
    run.tokens_used = tally.used;
    Ok(subagent_report(&run))
}

/// The subagent's loop, folded into `tally` and `run`.
#[allow(clippy::too_many_arguments)]
async fn drive(
    host: &SessionAgentHost,
    input: &AgentInput,
    ctx: &ToolContext,
    agent_type: &AgentType,
    who: &str,
    worktree: Option<&Worktree>,
    tally: &mut Tally,
    run: &mut SubagentRun,
) {
    // Only subagent tools (never agent or memory, whatever the type lists), skill only when there are skills, and
    // github only where the session offers it (its main agent has the door; the subagent only reads, #47).
    let has_skills = !ctx.skills.is_empty();
    let available: Vec<ToolRef> = marv_tools::builtin_tools()
        .iter()
        .filter(|t| {
            let name = t.name();
            SUBAGENT_TOOLS.contains(&name)
                && agent_type.tools.iter().any(|n| n == name)
                && (name != "skill" || has_skills)
                && (name != "github" || ctx.doors.github)
        })
        .cloned()
        .collect();
    let names: Vec<String> = available.iter().map(|t| t.name().to_string()).collect();
    // Its own "always" scopes, without the parent's network, web, MCP or mesh ones (docs/SECURITY.md).
    let approver = host.approver.clone().map(|inner| {
        Arc::new(SessionApprover { inner, always: host.always.for_subagent(), cancel: ctx.cancel.clone() }) as Arc<dyn Approver>
    });
    let mut sub_ctx = subagent_context(ctx, who, worktree, approver);
    // marv-rs: a plug of its own (no wires), mounted in its sandbox, unplugged when it ends (docs/MESH.md). One that
    // can't be bound leaves it without the mesh, not without its task.
    let plug = host.mesh.as_ref().and_then(|mesh| mesh.plug_subagent().ok());
    if let Some(plug) = &plug {
        sub_ctx.sandbox = plug.sandbox(&sub_ctx.sandbox);
    }
    let system = subagent_prompt(&SubagentPromptInput {
        cwd: worktree.map_or_else(|| host.cwd.clone(), |w| shorten_home(&w.work_dir, &home_dir())),
        tools: names.clone(),
        body: agent_type.body.clone(),
        instructions: host.instructions.clone(),
        skills: if has_skills && names.iter().any(|n| n == "skill") {
            ctx.skills.iter().map(|s| Listed { name: s.name.clone(), description: s.description.clone() }).collect()
        } else {
            Vec::new()
        },
        worktree: worktree.map(|w| (w.branch.clone(), w.base.clone())),
        date: iso_date(now_ms()),
    });
    let mut available = available;
    available.extend(plug.iter().flat_map(|p| p.tools()));
    let specs = available.iter().map(|t| t.spec()).collect();
    let taint = sub_ctx.taint.clone();
    let child_taint = sub_ctx.taint.clone();
    let runner = Arc::new(SessionToolRunner::new(sub_ctx, Arc::new(available)));
    let history: History = Arc::new(Mutex::new(vec![ChatTurn::User { text: input.prompt.clone() }]));
    let mut options =
        AgentOptions::new(host.provider_for(agent_type.model.as_deref()), history, system, specs, runner, ctx.cancel.clone());
    options.max_steps = SUBAGENT_MAX_STEPS;
    options.after_step = plug.as_ref().map(|p| p.after_step(None, taint));

    tally.report();
    let events = run_agent(options);
    futures::pin_mut!(events);
    while let Some(event) = events.next().await {
        if let Some(id) = &tally.call_id {
            host.sink.event(id, &event);
        }
        match event {
            LoopEvent::Usage(usage) => {
                tally.used += usage.prompt_tokens + usage.completion_tokens;
                host.sink.usage(&usage);
            }
            LoopEvent::Assistant { text } => run.text = text,
            LoopEvent::ToolStart { call, label } => {
                tally.tool_count += 1;
                tally.current = Some(format!("{} {label}", call.name));
                tally.report();
            }
            LoopEvent::ToolEnd { call, run: result } => {
                let summary =
                    if result.result.is_error { "error".to_string() } else { result.result.summary.clone().unwrap_or_default() };
                tally.steps.push(format!("{} {} · {summary}", call.name, result.label));
                if result.declined && run.declined_step.is_none() {
                    run.declined_step = Some(format!("{} {}", call.name, result.label));
                }
                tally.current = None;
                tally.report();
            }
            LoopEvent::Error(message) => run.error = Some(message),
            LoopEvent::Done(reason) => {
                run.reason = reason_text(reason);
                // The loop's step-limit message is written for the main agent ("say continue"); stopped_text has
                // the right one.
                if reason == StopReason::MaxSteps {
                    run.error = None;
                }
            }
            _ => {}
        }
        // marv-rs: what tainted the subagent taints its parent too: its report can carry that text (or instructions
        // from it) back, and the parent's egress must ask from then on (docs/SECURITY.md). Each step, so a parent
        // that's stopped mid-run is marked too.
        ctx.taint.absorb(&child_taint);
    }
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

/// Ported from tests/agent-tool.test.ts: the agent tool through `marv_tools::run_tool`, with a scripted model.
#[cfg(test)]
mod run_tests {
    use std::collections::{HashMap, VecDeque};
    use std::path::{Path, PathBuf};

    use marv_core::{ProviderEvent, ToolCall, Usage};
    use marv_provider::{EventStream, Provider, ProviderFactory, ReasoningSupport, StreamOptions};
    use marv_sandbox::sandbox_available;
    use marv_tools::ToolOutcome;
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::agents::{AgentSource, general_purpose};
    use crate::runner::{AlwaysScopes, SubagentSink};
    use crate::worktree::tests::{Temp, init_repo, sh_git};

    type Requests = Arc<Mutex<Vec<(Vec<ChatTurn>, StreamOptions)>>>;

    fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
        m.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Replays one scripted reply per request and records every request (tests/fake-provider.ts).
    #[derive(Default)]
    struct Scripted {
        steps: Mutex<VecDeque<Vec<ProviderEvent>>>,
        requests: Requests,
    }

    impl Scripted {
        fn new(steps: Vec<Vec<ProviderEvent>>) -> Arc<Self> {
            Arc::new(Self { steps: Mutex::new(steps.into()), requests: Requests::default() })
        }
        fn requests(&self) -> Vec<(Vec<ChatTurn>, StreamOptions)> {
            lock(&self.requests).clone()
        }
        fn tool_names(&self, i: usize) -> Vec<String> {
            self.requests()[i].1.tools.iter().map(|t| t.name.clone()).collect()
        }
        fn system(&self, i: usize) -> String {
            self.requests()[i].1.system.clone().unwrap_or_default()
        }
        /// The tool results the model had read by request `i`.
        fn results(&self, i: usize) -> Vec<String> {
            self.requests()[i]
                .0
                .iter()
                .filter_map(|t| match t {
                    ChatTurn::Tool { text, .. } => Some(text.clone()),
                    _ => None,
                })
                .collect()
        }
    }

    impl Provider for Scripted {
        fn name(&self) -> &str {
            "scripted"
        }
        fn context_length(&self) -> Option<u64> {
            None
        }
        fn stream(&self, history: &[ChatTurn], options: StreamOptions) -> EventStream {
            lock(&self.requests).push((history.to_vec(), options));
            let step = lock(&self.steps).pop_front().unwrap_or_else(|| say("(script ended)"));
            Box::pin(futures::stream::iter(step))
        }
    }

    fn say(text: &str) -> Vec<ProviderEvent> {
        vec![ProviderEvent::TextDelta(text.into()), ProviderEvent::Done { reason: Some("stop".into()) }]
    }
    fn say_with_usage(text: &str, prompt_tokens: u64, completion_tokens: u64) -> Vec<ProviderEvent> {
        vec![
            ProviderEvent::TextDelta(text.into()),
            ProviderEvent::Usage(Usage { prompt_tokens, completion_tokens, ..Usage::default() }),
            ProviderEvent::Done { reason: Some("stop".into()) },
        ]
    }
    fn tool_call(id: &str, name: &str, args: serde_json::Value) -> ProviderEvent {
        ProviderEvent::ToolCall(ToolCall { id: id.into(), name: name.into(), arguments: args.to_string() })
    }
    fn use_tool(id: &str, name: &str, args: serde_json::Value) -> Vec<ProviderEvent> {
        vec![tool_call(id, name, args), ProviderEvent::Done { reason: Some("tool_calls".into()) }]
    }
    fn agent_call(args: serde_json::Value, id: &str) -> ToolCall {
        ToolCall { id: id.into(), name: "agent".into(), arguments: args.to_string() }
    }

    /// Hands out the same provider for every model, and records which models were asked for.
    struct Factory {
        provider: Arc<dyn Provider>,
        models: Arc<Mutex<Vec<Option<String>>>>,
    }
    impl ProviderFactory for Factory {
        fn id(&self) -> &str {
            "test"
        }
        fn model(&self) -> &str {
            "test-model"
        }
        fn make(&self, model: Option<&str>, _info: Option<ReasoningSupport>) -> Arc<dyn Provider> {
            lock(&self.models).push(model.map(str::to_string));
            self.provider.clone()
        }
    }

    #[derive(Default)]
    struct Seen {
        usage: Mutex<Vec<Usage>>,
        progress: Mutex<Vec<AgentProgress>>,
        /// (call id, event kind)
        events: Mutex<Vec<(String, String)>>,
        models: Arc<Mutex<Vec<Option<String>>>>,
    }
    struct Sink(Arc<Seen>);
    impl SubagentSink for Sink {
        fn usage(&self, usage: &Usage) {
            lock(&self.0.usage).push(*usage);
        }
        fn progress(&self, _call_id: &str, progress: AgentProgress) {
            lock(&self.0.progress).push(progress);
        }
        fn event(&self, call_id: &str, event: &LoopEvent) {
            let kind = match event {
                LoopEvent::TextDelta(_) => "text_delta",
                LoopEvent::Assistant { .. } => "assistant",
                LoopEvent::ToolStart { .. } => "tool_start",
                LoopEvent::ToolEnd { .. } => "tool_end",
                LoopEvent::Usage(_) => "usage",
                LoopEvent::Done(_) => "done",
                LoopEvent::Error(_) => "error",
                _ => "other",
            };
            lock(&self.0.events).push((call_id.into(), kind.into()));
        }
    }

    /// Records every request and answers with `decide`.
    struct Asker {
        asked: Mutex<Vec<ApprovalRequest>>,
        decide: Box<dyn Fn(&ApprovalRequest) -> Decision + Send + Sync>,
    }
    #[async_trait]
    impl Approver for Asker {
        async fn approve(&self, request: ApprovalRequest) -> Decision {
            let decision = (self.decide)(&request);
            lock(&self.asked).push(request);
            decision
        }
    }
    impl Asker {
        fn new(decide: impl Fn(&ApprovalRequest) -> Decision + Send + Sync + 'static) -> Arc<Self> {
            Arc::new(Self { asked: Mutex::default(), decide: Box::new(decide) })
        }
        fn yes() -> Arc<Self> {
            Self::new(|_| Decision::Yes)
        }
        fn asked(&self) -> Vec<ApprovalRequest> {
            lock(&self.asked).clone()
        }
    }

    struct Fixture {
        root: Temp,
        trees: Temp,
    }
    impl Fixture {
        fn new() -> Self {
            let root = Temp::new("agent-tool");
            std::fs::write(root.0.join("notes.txt"), "remember the milk\n").unwrap();
            Self { root, trees: Temp::new("agent-trees") }
        }
        /// A repository with notes.txt committed.
        fn git() -> Self {
            let f = Self::new();
            init_repo(&f.root.0);
            f
        }
        fn host(
            &self,
            provider: Arc<dyn Provider>,
            agents: Vec<AgentType>,
            worktrees: bool,
        ) -> (Arc<SessionAgentHost>, Arc<Seen>) {
            let seen = Arc::new(Seen::default());
            let host = SessionAgentHost {
                agents: Arc::new(agents),
                cwd: "~/proj".into(),
                instructions: None,
                worktrees_dir: worktrees.then(|| self.trees.0.clone()),
                factory: Arc::new(Factory { provider: provider.clone(), models: seen.models.clone() }),
                provider,
                approver: None,
                always: AlwaysScopes::default(),
                sink: Arc::new(Sink(seen.clone())),
                mesh: None,
            };
            (Arc::new(host), seen)
        }
    }

    /// The parent's context: the host, and `asker` both as the parent's approver and the host's client approver.
    fn ctx_with(host: Arc<SessionAgentHost>, asker: &Arc<Asker>, root: &Path) -> ToolContext {
        let mut host = Arc::try_unwrap(host).unwrap_or_else(|_| panic!("host is shared"));
        host.approver = Some(asker.clone());
        let mut ctx = ToolContext::new(root);
        ctx.agent_host = Some(Arc::new(host));
        ctx.approver = Some(asker.clone());
        ctx
    }

    fn sandbox_off(ctx: &mut ToolContext) {
        ctx.sandbox = Arc::new(marv_sandbox::Sandbox::from_env(false));
    }

    async fn run(call: ToolCall, ctx: &ToolContext) -> ToolOutcome {
        marv_tools::run_tool(&call, ctx, marv_tools::builtin_tools()).await
    }

    /// "marv/<slug>-<4 hex>" from `text`, after `prefix`.
    fn branch_in(text: &str, prefix: &str, slug: &str) -> Option<String> {
        let at = text.find(&format!("{prefix}marv/{slug}-"))? + prefix.len();
        let branch: String = text[at..].chars().take(format!("marv/{slug}-").len() + 4).collect();
        let id = &branch[branch.len() - 4..];
        id.chars().all(|c| c.is_ascii_hexdigit()).then_some(branch)
    }

    #[tokio::test]
    async fn the_subagent_starts_fresh_and_only_its_last_message_comes_back() {
        let f = Fixture::new();
        let provider = Scripted::new(vec![say_with_usage("Found 3 callers.", 1000, 200)]);
        let (host, seen) = f.host(provider.clone(), vec![general_purpose()], true);
        let asker = Asker::yes();
        let ctx = ctx_with(host, &asker, &f.root.0);
        let out =
            run(agent_call(serde_json::json!({"description": "find callers", "prompt": "Find callers of foo"}), "a1"), &ctx)
                .await;

        assert_eq!(provider.requests()[0].0, vec![ChatTurn::User { text: "Find callers of foo".into() }]);
        assert_eq!(out.result.output, "Found 3 callers.");
        assert_eq!(out.result.summary, "done · 0 tools · 1.2k tokens · \"Found 3 callers.\"");
        assert_eq!(out.label, "general-purpose · find callers");
        assert!(asker.asked().is_empty(), "a shared-folder dispatch doesn't ask");
        assert_eq!(*lock(&seen.usage), vec![Usage { prompt_tokens: 1000, completion_tokens: 200, ..Usage::default() }]);
    }

    #[tokio::test]
    async fn it_gets_subagent_tools_only_and_its_types_instructions() {
        let f = Fixture::new();
        let reviewer = AgentType {
            name: "reviewer".into(),
            body: "You review code.".into(),
            tools: vec!["read_file".into(), "grep".into()],
            source: AgentSource::Project,
            ..general_purpose()
        };
        let provider = Scripted::new(vec![say("ok"), say("ok")]);
        let (host, _) = f.host(provider.clone(), vec![general_purpose(), reviewer], true);
        let ctx = ctx_with(host, &Asker::yes(), &f.root.0);
        run(agent_call(serde_json::json!({"description": "x", "prompt": "p"}), "a1"), &ctx).await;
        run(agent_call(serde_json::json!({"type": "superpowers:reviewer", "description": "x", "prompt": "p"}), "a2"), &ctx).await;

        assert_eq!(provider.tool_names(0), ["read_file", "glob", "grep", "web_fetch", "edit_file", "write_file", "bash"]);
        assert_eq!(provider.tool_names(1), ["read_file", "grep"]);
        assert!(provider.system(1).starts_with("You review code."));
    }

    #[tokio::test]
    async fn a_tool_it_wasnt_given_is_refused_even_if_the_model_names_it() {
        let f = Fixture::new();
        let provider =
            Scripted::new(vec![use_tool("m1", "memory", serde_json::json!({"action": "add", "text": "x"})), say("ok")]);
        let (host, _) = f.host(provider.clone(), vec![general_purpose()], true);
        let ctx = ctx_with(host, &Asker::yes(), &f.root.0);
        run(agent_call(serde_json::json!({"description": "x", "prompt": "p"}), "a1"), &ctx).await;
        assert!(provider.results(1)[0].contains("Unknown tool \"memory\""));
    }

    #[tokio::test]
    async fn it_works_in_the_project_and_reports_progress() {
        let f = Fixture::new();
        let provider =
            Scripted::new(vec![use_tool("r1", "read_file", serde_json::json!({"path": "notes.txt"})), say("It says milk.")]);
        let (host, seen) = f.host(provider, vec![general_purpose()], true);
        let ctx = ctx_with(host, &Asker::yes(), &f.root.0);
        let out =
            run(agent_call(serde_json::json!({"description": "read notes", "prompt": "What's in notes.txt?"}), "a1"), &ctx).await;
        assert!(out.result.summary.starts_with("done · 1 tool ·"), "{}", out.result.summary);
        let progress = lock(&seen.progress).clone();
        assert!(progress.iter().any(|p| p.line == "shared folder · 1 tool · read_file notes.txt"), "{progress:?}");
        assert_eq!(progress.last().unwrap().steps, ["read_file notes.txt · 1 line"]);
    }

    #[tokio::test]
    async fn everything_its_loop_does_reaches_the_host_for_its_own_view() {
        let f = Fixture::new();
        let provider =
            Scripted::new(vec![use_tool("r1", "read_file", serde_json::json!({"path": "notes.txt"})), say("It says milk.")]);
        let (host, seen) = f.host(provider, vec![general_purpose()], true);
        let ctx = ctx_with(host, &Asker::yes(), &f.root.0);
        run(agent_call(serde_json::json!({"description": "read notes", "prompt": "p"}), "a7"), &ctx).await;
        let events = lock(&seen.events).clone();
        assert!(events.iter().all(|(id, _)| id == "a7"));
        let kinds: Vec<&str> = events.iter().map(|(_, k)| k.as_str()).filter(|k| *k != "other").collect();
        assert_eq!(kinds, ["tool_start", "tool_end", "text_delta", "assistant", "done"]);
    }

    #[tokio::test]
    async fn an_error_returns_what_it_had_marked_as_an_error() {
        let f = Fixture::new();
        let provider = Scripted::new(vec![vec![
            ProviderEvent::TextDelta("Partial findings.".into()),
            ProviderEvent::Error("Rate limited".into()),
        ]]);
        let (host, _) = f.host(provider, vec![general_purpose()], true);
        let ctx = ctx_with(host, &Asker::yes(), &f.root.0);
        let out = run(agent_call(serde_json::json!({"description": "x", "prompt": "p"}), "a1"), &ctx).await;
        assert!(out.result.is_error);
        assert!(out.result.output.starts_with("Partial findings."), "{}", out.result.output);
        assert!(out.result.output.contains("[Marv: It stopped on an error: Rate limited]"), "{}", out.result.output);
        assert!(out.result.summary.starts_with("stopped ·"));
    }

    #[tokio::test]
    async fn at_its_step_limit_it_says_to_start_a_new_one_not_to_say_continue() {
        let f = Fixture::new();
        let steps = (0..SUBAGENT_MAX_STEPS)
            .map(|i| use_tool(&format!("r{i}"), "read_file", serde_json::json!({"path": format!("notes-{i}.txt")})))
            .collect();
        let (host, _) = f.host(Scripted::new(steps), vec![general_purpose()], true);
        let ctx = ctx_with(host, &Asker::yes(), &f.root.0);
        let out = run(agent_call(serde_json::json!({"description": "endless", "prompt": "p"}), "a1"), &ctx).await;
        assert!(out.result.output.contains(&format!("It hit its {SUBAGENT_MAX_STEPS}-step limit before finishing")));
        assert!(out.result.output.contains("start a new subagent"));
        assert!(!out.result.output.contains("continue"), "{}", out.result.output);
    }

    #[tokio::test]
    async fn a_types_model_gets_its_own_provider() {
        let f = Fixture::new();
        let fast = AgentType {
            name: "fast".into(),
            model: Some("small-model".into()),
            source: AgentSource::Project,
            ..general_purpose()
        };
        let (host, seen) = f.host(Scripted::new(vec![say("ok")]), vec![general_purpose(), fast], true);
        let ctx = ctx_with(host, &Asker::yes(), &f.root.0);
        run(agent_call(serde_json::json!({"type": "fast", "description": "x", "prompt": "p"}), "a1"), &ctx).await;
        assert_eq!(*lock(&seen.models), [Some("small-model".to_string())]);
    }

    #[tokio::test]
    async fn unknown_types_and_nesting_are_errors_the_model_can_read() {
        let f = Fixture::new();
        let (host, _) = f.host(Scripted::new(vec![]), vec![general_purpose()], true);
        let ctx = ctx_with(host, &Asker::yes(), &f.root.0);
        let out = run(agent_call(serde_json::json!({"type": "nope", "description": "x", "prompt": "p"}), "a1"), &ctx).await;
        assert!(out.result.output.contains("no agent type \"nope\". Available: general-purpose"), "{}", out.result.output);
        // A subagent's context has no host.
        let sub = subagent_context(&ctx, "general-purpose · x", None, None);
        let out = run(agent_call(serde_json::json!({"description": "x", "prompt": "p"}), "a1"), &sub).await;
        assert!(out.result.output.contains("Subagents aren't available here."));
    }

    #[tokio::test]
    async fn a_shared_folder_subagents_changes_are_approved_by_the_user_labeled_with_who_asks() {
        let f = Fixture::new();
        let provider = Scripted::new(vec![
            use_tool("w1", "write_file", serde_json::json!({"path": "made.txt", "content": "hi\n"})),
            say("Made it."),
        ]);
        let (host, _) = f.host(provider, vec![general_purpose()], true);
        let asker = Asker::yes();
        let ctx = ctx_with(host, &asker, &f.root.0);
        run(agent_call(serde_json::json!({"description": "make file", "prompt": "p"}), "a1"), &ctx).await;
        let asked: Vec<_> = asker.asked().into_iter().map(|r| (r.tool, r.agent)).collect();
        assert_eq!(asked, [("write_file".to_string(), Some("general-purpose · make file".to_string()))]);
        assert!(f.root.0.join("made.txt").exists());
    }

    #[tokio::test]
    async fn in_yolo_mode_a_shared_folder_subagents_edits_run_without_asking() {
        let f = Fixture::new();
        let provider = Scripted::new(vec![
            use_tool("w1", "write_file", serde_json::json!({"path": "made.txt", "content": "hi\n"})),
            say("Made it."),
        ]);
        let (host, _) = f.host(provider, vec![general_purpose()], true);
        let asker = Asker::yes();
        let mut ctx = ctx_with(host, &asker, &f.root.0);
        ctx.yolo = true;
        run(agent_call(serde_json::json!({"description": "make file", "prompt": "p"}), "a1"), &ctx).await;
        assert!(asker.asked().is_empty());
        assert!(f.root.0.join("made.txt").exists());
    }

    #[tokio::test]
    async fn in_yolo_mode_what_isnt_safe_still_asks_labeled_with_who_asks() {
        let f = Fixture::new();
        let provider =
            Scripted::new(vec![use_tool("b1", "bash", serde_json::json!({"command": "curl x", "network": true})), say("never")]);
        let (host, _) = f.host(provider, vec![general_purpose()], true);
        let asker = Asker::new(|_| Decision::No);
        let mut ctx = ctx_with(host, &asker, &f.root.0);
        ctx.yolo = true;
        run(agent_call(serde_json::json!({"description": "fetch", "prompt": "p"}), "a1"), &ctx).await;
        let asked: Vec<_> = asker.asked().into_iter().map(|r| (r.tool, r.agent)).collect();
        assert_eq!(asked, [("bash".to_string(), Some("general-purpose · fetch".to_string()))]);
    }

    #[tokio::test]
    async fn a_no_inside_the_subagent_stops_it_and_the_parent() {
        let f = Fixture::new();
        let mut first = vec![ProviderEvent::TextDelta("I'll write made.txt now.".into())];
        first.extend(use_tool("w1", "write_file", serde_json::json!({"path": "made.txt", "content": "hi\n"})));
        let provider = Scripted::new(vec![first, say("never")]);
        let (host, _) = f.host(provider.clone(), vec![general_purpose()], true);
        let ctx = ctx_with(host, &Asker::new(|_| Decision::No), &f.root.0);
        let out = run(agent_call(serde_json::json!({"description": "x", "prompt": "p"}), "a1"), &ctx).await;
        assert!(out.result.declined);
        assert_eq!(provider.requests().len(), 1);
        // The parent must learn the write didn't happen, not just read "I'll write made.txt now."
        assert_eq!(
            out.result.output,
            "I'll write made.txt now.\n\n[Marv: The user declined its write_file made.txt, so it stopped there. Don't retry: wait for the user to say how to proceed.]"
        );
    }

    #[tokio::test]
    async fn stopped_by_the_user_its_reported_as_interrupted_not_declined() {
        let f = Fixture::new();
        let provider = Scripted::new(vec![
            use_tool("w1", "write_file", serde_json::json!({"path": "made.txt", "content": "hi\n"})),
            say("never"),
        ]);
        let (host, _) = f.host(provider, vec![general_purpose()], true);
        let stop = CancellationToken::new();
        let asker = Asker::new({
            let stop = stop.clone();
            move |_| {
                stop.cancel(); // Esc declines what's waiting and aborts
                Decision::No
            }
        });
        let mut ctx = ctx_with(host, &asker, &f.root.0);
        ctx.cancel = stop;
        let out = run(agent_call(serde_json::json!({"description": "x", "prompt": "p"}), "a1"), &ctx).await;
        assert!(!out.result.declined);
        assert!(out.result.is_error);
        assert!(out.result.summary.starts_with("stopped ·"), "{}", out.result.summary);
        assert!(out.result.output.contains("[Marv: Interrupted by the user before it finished.]"), "{}", out.result.output);
        assert!(!f.root.0.join("made.txt").exists());
    }

    #[tokio::test]
    async fn in_a_worktree_approve_once_to_start_it_and_its_edits_land_on_its_own_branch() {
        let f = Fixture::git();
        let provider = Scripted::new(vec![
            use_tool("w1", "write_file", serde_json::json!({"path": "new.txt", "content": "new\n"})),
            say("Added new.txt."),
        ]);
        let (host, _) = f.host(provider.clone(), vec![general_purpose()], true);
        let asker = Asker::yes();
        let ctx = ctx_with(host, &asker, &f.root.0);
        let out =
            run(agent_call(serde_json::json!({"description": "add file", "prompt": "p", "isolation": "worktree"}), "a1"), &ctx)
                .await;

        // The dispatch is approved; inside the worktree, edits run without asking (when the sandbox works).
        let asked = asker.asked();
        assert_eq!(asked[0].tool, "agent");
        assert_eq!(asked[0].scope.key, "agent:worktree");
        assert_eq!(asked.len(), if sandbox_available() { 1 } else { 2 });
        assert!(out.result.output.starts_with("Added new.txt."), "{}", out.result.output);
        let branch = branch_in(&out.result.output, "\n\nMarv: Branch ", "add-file").expect("a branch line");
        assert!(out.result.output.contains(&format!("{branch}: 1 commit")));
        assert_eq!(sh_git(&f.root.0, &["show", &format!("{branch}:new.txt")]), "new");
        assert!(!f.root.0.join("new.txt").exists());
        assert!(provider.system(0).contains(&format!("branch {branch}")));
    }

    #[tokio::test]
    async fn in_yolo_mode_starting_a_sandboxed_worktree_doesnt_ask_either() {
        if !sandbox_available() {
            return;
        }
        let f = Fixture::git();
        let provider = Scripted::new(vec![
            use_tool("w1", "write_file", serde_json::json!({"path": "new.txt", "content": "new\n"})),
            say("Added."),
        ]);
        let (host, _) = f.host(provider, vec![general_purpose()], true);
        let asker = Asker::yes();
        let mut ctx = ctx_with(host, &asker, &f.root.0);
        ctx.yolo = true;
        let out =
            run(agent_call(serde_json::json!({"description": "add file", "prompt": "p", "isolation": "worktree"}), "a1"), &ctx)
                .await;
        assert!(asker.asked().is_empty());
        assert!(out.result.output.contains("1 commit"), "{}", out.result.output);
    }

    #[tokio::test]
    async fn in_yolo_mode_without_the_sandbox_starting_it_still_asks() {
        let f = Fixture::git();
        let (host, _) = f.host(Scripted::new(vec![say("Nothing to do.")]), vec![general_purpose()], true);
        let asker = Asker::yes();
        let mut ctx = ctx_with(host, &asker, &f.root.0);
        ctx.yolo = true;
        sandbox_off(&mut ctx);
        run(agent_call(serde_json::json!({"description": "noop", "prompt": "p", "isolation": "worktree"}), "a1"), &ctx).await;
        let tools: Vec<String> = asker.asked().into_iter().map(|r| r.tool).collect();
        assert_eq!(tools, ["agent"]);
    }

    #[tokio::test]
    async fn started_from_a_subfolder_it_works_in_that_subfolder_of_its_worktree() {
        let f = Fixture::git();
        std::fs::create_dir(f.root.0.join("pkg")).unwrap();
        std::fs::write(f.root.0.join("pkg/p.txt"), "p\n").unwrap();
        std::fs::write(f.root.0.join("top.txt"), "top\n").unwrap();
        sh_git(&f.root.0, &["add", "-A"]);
        sh_git(&f.root.0, &["commit", "-q", "-m", "pkg"]);
        let mut first = vec![
            tool_call("r1", "read_file", serde_json::json!({"path": "../top.txt"})),
            tool_call("g1", "glob", serde_json::json!({"pattern": "*"})),
            tool_call("w1", "write_file", serde_json::json!({"path": "made.txt", "content": "m\n"})),
        ];
        if sandbox_available() {
            first.push(tool_call("b1", "bash", serde_json::json!({"command": "git status --porcelain"})));
        }
        first.push(ProviderEvent::Done { reason: Some("tool_calls".into()) });
        let provider = Scripted::new(vec![first, say("Made it.")]);
        let (host, _) = f.host(provider.clone(), vec![general_purpose()], true);
        let ctx = ctx_with(host, &Asker::yes(), &f.root.0.join("pkg"));
        let out =
            run(agent_call(serde_json::json!({"description": "sub", "prompt": "p", "isolation": "worktree"}), "a1"), &ctx).await;
        let results = provider.results(1);
        assert!(results[0].contains("outside the project"), "{}", results[0]);
        assert_eq!(results[1], "p.txt", "listed relative to pkg, nothing from the top");
        // In the sandbox git works, and the rest of the worktree is there (read-only), not listed as deleted.
        if sandbox_available() {
            assert_eq!(results[3], "?? pkg/made.txt\n\n[exit code 0]");
        }
        let trees_name = f.trees.0.file_name().unwrap().to_string_lossy().into_owned();
        let system = provider.system(0);
        assert!(system.contains(&format!("{trees_name}/sub-")) && system.contains("/pkg\n"), "{system}");
        let branch = branch_in(&out.result.output, "Marv: Branch ", "sub").expect("a branch line");
        assert!(out.result.output.contains(&format!("{branch}: 1 commit")));
        assert_eq!(sh_git(&f.root.0, &["show", &format!("{branch}:pkg/made.txt")]), "m");
        assert_eq!(sh_git(&f.root.0, &["show", &format!("{branch}:top.txt")]), "top");
    }

    #[tokio::test]
    async fn in_a_worktree_network_commands_still_ask() {
        let f = Fixture::git();
        let provider = Scripted::new(vec![
            use_tool("b1", "bash", serde_json::json!({"command": "bun install", "network": true})),
            say("never"),
        ]);
        let (host, _) = f.host(provider, vec![general_purpose()], true);
        let asker = Asker::new(|r| if r.tool == "agent" { Decision::Yes } else { Decision::No });
        let ctx = ctx_with(host, &asker, &f.root.0);
        let out =
            run(agent_call(serde_json::json!({"description": "install", "prompt": "p", "isolation": "worktree"}), "a1"), &ctx)
                .await;
        let asked: Vec<_> = asker.asked().into_iter().map(|r| (r.tool, r.network, r.agent)).collect();
        assert_eq!(
            asked,
            [("agent".to_string(), false, None), ("bash".to_string(), true, Some("general-purpose · install".to_string()))]
        );
        assert!(out.result.declined);
    }

    /// A provider that panics (Marv: one that throws instead of sending an error event).
    struct Panicking;
    impl Provider for Panicking {
        fn name(&self) -> &str {
            "panicking"
        }
        fn context_length(&self) -> Option<u64> {
            None
        }
        fn stream(&self, _history: &[ChatTurn], _options: StreamOptions) -> EventStream {
            panic!("connection reset")
        }
    }

    #[tokio::test]
    async fn a_provider_that_panics_still_finishes_the_worktree_and_reports_its_branch() {
        let f = Fixture::git();
        let (host, _) = f.host(Arc::new(Panicking), vec![general_purpose()], true);
        let ctx = ctx_with(host, &Asker::yes(), &f.root.0);
        let out =
            run(agent_call(serde_json::json!({"description": "boom", "prompt": "p", "isolation": "worktree"}), "a1"), &ctx).await;
        assert!(out.result.is_error);
        assert!(out.result.output.contains("connection reset"), "{}", out.result.output);
        assert!(branch_in(&out.result.output, "Marv: No changes (branch ", "boom").is_some(), "{}", out.result.output);
        assert_eq!(sh_git(&f.root.0, &["worktree", "list"]).lines().count(), 1);
    }

    #[tokio::test]
    async fn in_a_worktree_with_the_sandbox_off_its_edits_ask_the_user_labeled_with_who_asks() {
        let f = Fixture::git();
        let provider = Scripted::new(vec![
            use_tool("w1", "write_file", serde_json::json!({"path": "new.txt", "content": "new\n"})),
            say("Added."),
        ]);
        let (host, _) = f.host(provider, vec![general_purpose()], true);
        let asker = Asker::yes();
        let mut ctx = ctx_with(host, &asker, &f.root.0);
        sandbox_off(&mut ctx);
        run(agent_call(serde_json::json!({"description": "x", "prompt": "p", "isolation": "worktree"}), "a1"), &ctx).await;
        let asked: Vec<_> = asker.asked().into_iter().map(|r| (r.tool, r.agent)).collect();
        assert_eq!(asked, [("agent".to_string(), None), ("write_file".to_string(), Some("general-purpose · x".to_string()))]);
    }

    /// Writes a file on its first request; on its second says something, then the user stops the run.
    struct StoppedMidRun {
        stop: CancellationToken,
        step: Mutex<u32>,
    }
    impl Provider for StoppedMidRun {
        fn name(&self) -> &str {
            "interrupted"
        }
        fn context_length(&self) -> Option<u64> {
            None
        }
        fn stream(&self, _history: &[ChatTurn], _options: StreamOptions) -> EventStream {
            let step = {
                let mut step = lock(&self.step);
                *step += 1;
                *step
            };
            if step == 1 {
                return Box::pin(futures::stream::iter(use_tool(
                    "w1",
                    "write_file",
                    serde_json::json!({"path": "half.txt", "content": "half\n"}),
                )));
            }
            let stop = self.stop.clone();
            let text = futures::stream::iter([ProviderEvent::TextDelta("Halfway there.".into())]);
            let done = futures::stream::once(async move {
                stop.cancel();
                ProviderEvent::Done { reason: None }
            });
            Box::pin(text.chain(done))
        }
    }

    #[tokio::test]
    async fn an_interrupt_mid_run_commits_what_it_did_marked_interrupted() {
        let f = Fixture::git();
        let stop = CancellationToken::new();
        let provider = Arc::new(StoppedMidRun { stop: stop.clone(), step: Mutex::new(0) });
        let (host, _) = f.host(provider, vec![general_purpose()], true);
        let mut ctx = ctx_with(host, &Asker::yes(), &f.root.0);
        ctx.cancel = stop;
        let out =
            run(agent_call(serde_json::json!({"description": "halt", "prompt": "p", "isolation": "worktree"}), "a1"), &ctx).await;
        assert!(out.result.is_error);
        assert!(out.result.output.starts_with("Halfway there."), "{}", out.result.output);
        assert!(out.result.output.contains("[Marv: Interrupted by the user before it finished.]"));
        let branch = branch_in(&out.result.output, "\n\nMarv: Branch ", "halt").expect("a branch line");
        assert!(out.result.output.contains(&format!("{branch}: 1 commit")));
        assert_eq!(sh_git(&f.root.0, &["log", "-1", "--format=%s", &branch]), "marv: halt (interrupted)");
        assert_eq!(sh_git(&f.root.0, &["show", &format!("{branch}:half.txt")]), "half");
    }

    // Marv's "if finishing the worktree throws" test mocks finishWorktree; here finish_worktree turns every problem
    // into its line, and a panic in it is caught by run_subagent (`couldn't finish the worktree (…); it's at …`).

    #[tokio::test]
    async fn in_the_sandbox_git_status_works_but_the_shared_git_is_read_only() {
        if !sandbox_available() {
            return;
        }
        let f = Fixture::git();
        let git_file = f.root.0.join(".git/planted");
        let provider = Scripted::new(vec![
            vec![
                tool_call("b1", "bash", serde_json::json!({"command": "git status"})),
                tool_call("b2", "bash", serde_json::json!({"command": format!("touch {}", git_file.display())})),
                ProviderEvent::Done { reason: Some("tool_calls".into()) },
            ],
            say("Checked."),
        ]);
        let (host, _) = f.host(provider.clone(), vec![general_purpose()], true);
        let ctx = ctx_with(host, &Asker::yes(), &f.root.0);
        run(agent_call(serde_json::json!({"description": "probe", "prompt": "p", "isolation": "worktree"}), "a1"), &ctx).await;
        let results = provider.results(1);
        assert!(branch_in(&results[0], "On branch ", "probe").is_some(), "{}", results[0]);
        assert!(results[0].ends_with("[exit code 0]"), "{}", results[0]);
        assert!(results[1].contains("Read-only file system") && results[1].ends_with("[exit code 1]"), "{}", results[1]);
        assert!(!git_file.exists());
    }

    #[tokio::test]
    async fn without_a_worktrees_folder_a_worktree_dispatch_fails_before_asking() {
        let f = Fixture::git();
        let (host, _) = f.host(Scripted::new(vec![]), vec![general_purpose()], false);
        let asker = Asker::yes();
        let ctx = ctx_with(host, &asker, &f.root.0);
        let out =
            run(agent_call(serde_json::json!({"description": "x", "prompt": "p", "isolation": "worktree"}), "a1"), &ctx).await;
        assert!(out.result.output.contains("Worktrees aren't available here."), "{}", out.result.output);
        assert!(asker.asked().is_empty());
    }

    #[tokio::test]
    async fn the_approval_warns_about_uncommitted_changes() {
        let f = Fixture::git();
        std::fs::write(f.root.0.join("dirty.txt"), "x\n").unwrap();
        let (host, _) = f.host(Scripted::new(vec![say("ok")]), vec![general_purpose()], true);
        let asker = Asker::yes();
        let ctx = ctx_with(host, &asker, &f.root.0);
        run(agent_call(serde_json::json!({"description": "x", "prompt": "p", "isolation": "worktree"}), "a1"), &ctx).await;
        let warning = asker.asked()[0].preview.warning.clone().unwrap_or_default();
        assert!(warning.contains("1 uncommitted change in the project won't be in the worktree"), "{warning}");
    }

    #[tokio::test]
    async fn the_approval_says_what_will_run_without_asking_and_that_network_commands_still_ask() {
        let f = Fixture::git();
        let (host, _) = f.host(Scripted::new(vec![say("ok"), say("ok")]), vec![general_purpose()], true);
        let asker = Asker::yes();
        let ctx = ctx_with(host, &asker, &f.root.0);
        run(agent_call(serde_json::json!({"description": "x", "prompt": "p", "isolation": "worktree"}), "a1"), &ctx).await;
        let mut off = ctx.clone();
        sandbox_off(&mut off);
        run(agent_call(serde_json::json!({"description": "y", "prompt": "p", "isolation": "worktree"}), "a2"), &off).await;
        let notes: Vec<String> = asker.asked().into_iter().filter_map(|r| r.preview.note).collect();
        if sandbox_available() {
            assert!(
                notes[0]
                    .ends_with("edits and sandboxed commands run without asking inside its worktree; network commands still ask")
            );
        }
        assert!(notes[1].ends_with("asks before each change (no sandbox)"), "{}", notes[1]);
    }

    #[tokio::test]
    async fn from_a_subfolder_the_approval_says_where_it_works_and_that_the_count_is_the_whole_repositorys() {
        let f = Fixture::git();
        std::fs::create_dir(f.root.0.join("pkg")).unwrap();
        std::fs::write(f.root.0.join("dirty.txt"), "x\n").unwrap();
        let (host, _) = f.host(Scripted::new(vec![say("ok")]), vec![general_purpose()], true);
        let asker = Asker::yes();
        let ctx = ctx_with(host, &asker, &f.root.0.join("pkg"));
        run(agent_call(serde_json::json!({"description": "x", "prompt": "p", "isolation": "worktree"}), "a1"), &ctx).await;
        let preview = asker.asked()[0].preview.clone();
        assert!(preview.note.unwrap_or_default().contains(", working in pkg/"));
        assert!(preview.warning.unwrap_or_default().contains("1 uncommitted change in the repository won't be in the worktree"));
    }

    #[tokio::test]
    async fn a_description_is_short() {
        let f = Fixture::new();
        let (host, _) = f.host(Scripted::new(vec![]), vec![general_purpose()], true);
        let ctx = ctx_with(host, &Asker::yes(), &f.root.0);
        let out = run(agent_call(serde_json::json!({"description": "x".repeat(121), "prompt": "p"}), "a1"), &ctx).await;
        assert!(out.result.output.starts_with("Invalid input for agent"), "{}", out.result.output);
    }

    #[tokio::test]
    async fn outside_a_git_repo_a_worktree_dispatch_fails_before_asking() {
        let f = Fixture::new();
        let (host, _) = f.host(Scripted::new(vec![]), vec![general_purpose()], true);
        let asker = Asker::yes();
        let ctx = ctx_with(host, &asker, &f.root.0);
        let out =
            run(agent_call(serde_json::json!({"description": "x", "prompt": "p", "isolation": "worktree"}), "a1"), &ctx).await;
        assert_eq!(out.result.output, NOT_A_REPO);
        assert!(asker.asked().is_empty());
    }

    // A subagent's tool context.

    fn wt() -> Worktree {
        Worktree {
            dir: "/trees/x-1a2b".into(),
            branch: "marv/x-1a2b".into(),
            base: "abc1234".into(),
            repo: "/proj".into(),
            git_dir: "/proj/.git".into(),
            admin_dir: "/proj/.git/worktrees/x-1a2b".into(),
            prefix: "".into(),
            work_dir: "/trees/x-1a2b".into(),
        }
    }

    fn parent() -> ToolContext {
        let f = Fixture::new();
        let (host, _) = f.host(Scripted::new(vec![]), vec![general_purpose()], true);
        let mut ctx = ToolContext::new("/proj");
        ctx.agent_host = Some(host);
        ctx.memory = Some(marv_tools::MemoryPaths { personal: "/m/p.md".into(), project: "/m/q.md".into() });
        ctx.call_id = Some("a1".into());
        sandbox_off(&mut ctx);
        ctx
    }

    #[test]
    fn in_a_worktree_its_folder_the_shared_git_read_only_git_pinned_no_host_or_memory() {
        let sub = subagent_context(&parent(), "general-purpose · x", Some(&wt()), None);
        assert_eq!(sub.root, PathBuf::from("/trees/x-1a2b"));
        assert_eq!(sub.read_only, [PathBuf::from("/proj/.git")]);
        assert_eq!(sub.git_env, worktree_env(&wt()));
        assert!(sub.agent_host.is_none() && sub.memory.is_none() && sub.call_id.is_none());
        assert!(!sub.sandbox.active());
    }

    #[test]
    fn in_a_worktree_from_a_subfolder_that_subfolder_with_the_rest_of_the_worktree_read_only() {
        let mut parent = parent();
        parent.root = "/proj/pkg".into();
        let w = Worktree { repo: "/proj/pkg".into(), prefix: "pkg/".into(), work_dir: "/trees/x-1a2b/pkg".into(), ..wt() };
        let sub = subagent_context(&parent, "general-purpose · x", Some(&w), None);
        assert_eq!(sub.root, PathBuf::from("/trees/x-1a2b/pkg"));
        assert_eq!(sub.read_only, [PathBuf::from("/trees/x-1a2b"), PathBuf::from("/proj/.git")]);
        assert_eq!(sub.git_env, worktree_env(&wt()), "git still sees the whole worktree");
    }

    #[test]
    fn in_the_shared_folder_the_project_nothing_extra() {
        let sub = subagent_context(&parent(), "general-purpose · x", None, None);
        assert_eq!(sub.root, PathBuf::from("/proj"));
        assert!(sub.read_only.is_empty() && sub.git_env.is_empty());
        assert!(sub.agent_host.is_none() && sub.memory.is_none());
    }

    /// Marv's tests/doors.test.ts: the doors are the main agent's (#47, #52).
    #[test]
    fn a_subagents_context_has_no_doors_whatever_the_parent_has() {
        let mut p = parent();
        p.doors = marv_tools::types::Doors { host: true, process: true, github: true };
        let sub = subagent_context(&p, "general-purpose · x", None, None);
        assert_eq!(sub.doors, marv_tools::types::Doors::default());
    }

    /// The turn's one checkpoint covers a shared-folder subagent's edits; a worktree's branch is its own way back (#16).
    #[test]
    fn the_turns_checkpoint_goes_to_a_shared_folder_subagent_not_a_worktrees() {
        struct Nothing;
        #[async_trait]
        impl marv_tools::Checkpoint for Nothing {
            async fn checkpoint(&self) {}
        }
        let mut parent = parent();
        parent.checkpoint = Some(Arc::new(Nothing));
        assert!(subagent_context(&parent, "general-purpose · x", None, None).checkpoint.is_some());
        assert!(subagent_context(&parent, "general-purpose · x", Some(&wt()), None).checkpoint.is_none());
    }

    #[tokio::test]
    async fn with_the_sandbox_off_even_a_worktrees_requests_go_to_the_user() {
        let asker = Asker::new(|_| Decision::No);
        let sub = subagent_context(&parent(), "general-purpose · x", Some(&wt()), Some(asker.clone()));
        let request = ApprovalRequest {
            tool: "write_file".into(),
            label: "a.txt".into(),
            preview: Preview { title: "Write a.txt".into(), ..Preview::default() },
            scope: marv_tools::Scope { key: "write_file".into(), description: "".into() },
            agent: None,
            network: false,
            once: false,
            must_ask: false,
            protected: None,
            door: None,
        };
        assert_eq!(sub.approver.unwrap().approve(request).await, Decision::No);
        let asked = asker.asked();
        assert_eq!(asked.len(), 1);
        assert_eq!(asked[0].agent.as_deref(), Some("general-purpose · x"));
    }

    /// Answers by the first user message: the parent's script, or a subagent's.
    struct Routed(HashMap<String, Arc<dyn Provider>>);
    impl Provider for Routed {
        fn name(&self) -> &str {
            "routed"
        }
        fn context_length(&self) -> Option<u64> {
            None
        }
        fn stream(&self, history: &[ChatTurn], options: StreamOptions) -> EventStream {
            let first = history.iter().find_map(|t| match t {
                ChatTurn::User { text } => Some(text.clone()),
                _ => None,
            });
            self.0[&first.unwrap_or_default()].stream(history, options)
        }
    }

    /// Takes 40 ms to answer, counting how many answer at once.
    struct Slow {
        text: String,
        active: Arc<std::sync::atomic::AtomicUsize>,
        max: Arc<std::sync::atomic::AtomicUsize>,
    }
    impl Provider for Slow {
        fn name(&self) -> &str {
            "slow"
        }
        fn context_length(&self) -> Option<u64> {
            None
        }
        fn stream(&self, _history: &[ChatTurn], _options: StreamOptions) -> EventStream {
            use std::sync::atomic::Ordering::SeqCst;
            let (active, max, text) = (self.active.clone(), self.max.clone(), self.text.clone());
            Box::pin(
                futures::stream::once(async move {
                    let now = active.fetch_add(1, SeqCst) + 1;
                    max.fetch_max(now, SeqCst);
                    tokio::time::sleep(std::time::Duration::from_millis(40)).await;
                    active.fetch_sub(1, SeqCst);
                    futures::stream::iter([ProviderEvent::TextDelta(text), ProviderEvent::Done { reason: None }])
                })
                .flatten(),
            )
        }
    }

    #[tokio::test]
    async fn two_subagents_in_one_reply_run_at_the_same_time() {
        let f = Fixture::new();
        let (active, max) = (Arc::default(), Arc::new(std::sync::atomic::AtomicUsize::new(0)));
        let slow = |text: &str| -> Arc<dyn Provider> {
            Arc::new(Slow { text: text.into(), active: Arc::clone(&active), max: max.clone() })
        };
        let parent: Arc<dyn Provider> = Scripted::new(vec![
            vec![
                ProviderEvent::ToolCall(agent_call(serde_json::json!({"description": "a", "prompt": "alpha task"}), "a1")),
                ProviderEvent::ToolCall(agent_call(serde_json::json!({"description": "b", "prompt": "beta task"}), "a2")),
                ProviderEvent::Done { reason: None },
            ],
            say("Both reported."),
        ]);
        let provider: Arc<dyn Provider> = Arc::new(Routed(HashMap::from([
            ("start two".to_string(), parent),
            ("alpha task".to_string(), slow("A done")),
            ("beta task".to_string(), slow("B done")),
        ])));
        let (host, _) = f.host(provider.clone(), vec![general_purpose()], true);
        let mut ctx = ToolContext::new(&f.root.0);
        ctx.agent_host = Some(host);
        let runner = Arc::new(SessionToolRunner::new(ctx, Arc::new(marv_tools::builtin_tools().to_vec())));
        let history: History = Arc::new(Mutex::new(vec![ChatTurn::User { text: "start two".into() }]));
        let options = AgentOptions::new(provider, history.clone(), "S".into(), vec![], runner, CancellationToken::new());
        run_agent(options).collect::<Vec<_>>().await;
        assert_eq!(max.load(std::sync::atomic::Ordering::SeqCst), 2);
        let texts: Vec<String> = lock(&history)
            .iter()
            .filter_map(|t| match t {
                ChatTurn::Tool { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(texts, ["A done", "B done"]);
    }
}
