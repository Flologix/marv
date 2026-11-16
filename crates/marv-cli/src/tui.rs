//! Launching the TUI: the interactive half of Marv's `src/cli.tsx`, the engine half of `src/app.tsx` (commands,
//! config, setup, resume, startup notices), and the App's Herdr reporting (`src/herdr.ts` is marv-mesh's `herdr.rs`).
//!
//! Startup, in Marv's order: the project root is the current folder; read AGENTS.md once (#62: or CLAUDE.md, plus the
//! parents' and ~/.marv's, cut at 20k in all; the cut and the list of files go to the startup notice); load skills and agents (project + personal; problems shown at startup); load the MCP config and
//! start the manager in the background; memory; migrate `~/.ekko` (default location only); load the config file (a
//! broken one exits 1 with "Fix the file, or delete it to run setup again."); hidden PATH folders for the startup
//! notice; Herdr reporting in a Herdr pane; then the UI. On the way out: `session.flush()` (the App awaits it after
//! restoring the terminal), MCP close (≤ 2 s), Herdr release (≤ 1 s). SIGHUP (the terminal closed) and SIGTERM take the
//! same way out (#196, `crate::signals`), its terminal writes bounded.
//!
//! The TUI (`marv-tui`) doesn't depend on marv-session: it drives a [`SessionPort`] and renders [`UiEvent`]s. The
//! bridge is [`Bridge`]:
//! - [`SessionPort`] over [`MarvSession`]: `send` runs a turn on its own task and forwards its events;
//! - [`map_event`] turns each `SessionEvent` (and the `LoopEvent`s inside) into `UiEvent`s;
//! - `run_command` runs `commands::run_command` and applies the [`CommandAction`]: the engine side here (config,
//!   memory, compaction, resume, ratings), the UI side as events (`Message`, `Clear`, `OpenSetup`, `Sessions`, `Exit`);
//! - [`TuiApprover`] is the session's approver: each request becomes a `UiEvent::Approval` and waits for
//!   [`SessionPort::answer`]. The queue itself (head, "N more waiting", Always covers the scope) is the App's.
//!
//! Herdr (Marv `app.tsx`): `blocked` while an approval waits (message: the asking subagent and the request's label),
//! `working` while a turn or `/compact` runs, `idle` otherwise; the session (id and file) at startup and whenever the
//! id changes (`/clear`, a resume). The App has no hook for its own state, so the bridge tracks what it handed out:
//! approvals not yet answered, and work it started.
//!
//! `/undo` (Marv #16): the session's `undo_plan()`, its files and diffs asked through the same approval queue (`once`),
//! then `undo()` and `undo_summary`'s line; refused while a turn runs.
//!
//! MCP (Marv #6, #18, #46): `/mcp trust` trusts the project's waiting servers (for their exact config) and starts them;
//! `/mcp add` looks the source up (all fetching before the approval, Esc stops it), asks once (yes or no) with the
//! exact JSON, writes that JSON to `~/.marv/mcp.json` and starts the server without a restart; `/mcp search` asks the
//! MCP Registry; `/mcp remove` takes one of yours out and stops it (no approval: the user typed it, and it takes
//! capability away); `/mcp login` opens the browser where one can open, shows the link either way, and takes a pasted
//! callback address ([`SessionPort::login_callback`]) for a browser on another machine; `/mcp logout` revokes where
//! the server can and forgets the tokens. None of them is a turn. Once the servers have started, one notice says
//! which failed, which wait for trust and which need a login; a login that runs out later is told once.
//!
//! Port notes (marv-rs):
//! - `marv -c` resumes before the UI starts, so a message can't be typed into the session being swapped out (Marv held
//!   `send` with "Loading your last session…"); the App gets `Resumed` as its first event.
//! - The saved transcript is the session's default (the messages and the replies): the App owns the transcript the
//!   user sees and the boundary has no way to read it back. Tool entries and notices aren't restored on resume.
//! - A `must_ask` request (tainted egress, a protected path) carries its protected rule's reason into the preview's
//!   warning line, since the view's `ApprovalRequest` has no `protected` field.

use std::collections::HashMap;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use futures::future::BoxFuture;
use marv_agent::LoopEvent;
use marv_core::{StopReason, printable};
use marv_mcp::install::proposal::{ResolveDeps, added_notice, approval_for};
use marv_mcp::install::{Resolved, parse_add_args, resolve_add};
use marv_mcp::manager::{McpServerStatus, McpState};
use marv_mcp::oauth::{Authorize, CallbackCheck, Revoked};
use marv_mcp::personal_file::{add_personal_server, personal_server_names, remove_personal_server};
use marv_mesh::herdr::{HerdrReporter, HerdrState, herdr_target};
use marv_mesh::radio::{PresenceState, Radio, RosterState};
use marv_provider::ProviderKind;
use marv_provider::factory::{ConfigFactory, LoadModels, MakeProvider};
use marv_session::checks::{ProjectCheckResult, project_check_notice};
use marv_session::commands::{self, CommandAction, CommandContext, MemoryTarget};
use marv_session::config::{
    Config, ConfigStore, DiagnosticsSetting, Env, FileConfig, MaxStepsSetting, RadioSetting, default_config_dir, mask_key,
    migrate_legacy_config, needs_setup, preset, resolve_config,
};
use marv_session::diagnostics::notice as check_notice;
use marv_session::mcp_tools::McpHandle;
use marv_session::memory::{Memories, MemoryPaths, load_memory, memory_paths};
use marv_session::paths::{home_dir, project_key, shorten_home};
use marv_session::project_trust::{ProjectFiles, TrustStatus, load_project, untrusted_notice};
use marv_session::prompt::{InstructionSource, Instructions, instruction_sources_item, instructions_cut_item};
use marv_session::sessions::SessionStore;
use marv_session::skills::LoadedSkill;
use marv_session::trajectory::TrajectoryStore;
use marv_session::{
    CompactFailure, CompactResult, Configure, Feedback, MarvSession, ProviderChoice, RateOutcome, ResumeTarget, SendOptions,
    SessionEvent, SessionInit, TurnEndReason, TurnStatus,
};
use marv_tui::app::{App, AppOptions};
use marv_tui::view::{
    self, ApprovalId, CommandInfo, CommandKind, Notice, NoticeItem, NoticeTone, ProviderId, Role, SessionPort, SettingAction,
    SettingInfo, SetupAnswer, SetupContext, SetupStart, StatusInfo, UiEvent, WelcomeInfo,
};
use serde_json::Value;
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

/// How long the MCP servers get to shut down on the way out.
const MCP_CLOSE_TIMEOUT: Duration = Duration::from_secs(2);
/// Background processes get SIGTERM, then SIGKILL after 2 s (Marv #51).
const PROCESSES_STOP_TIMEOUT: Duration = Duration::from_millis(2500);
/// How long Herdr gets to hear that Marv left the pane.
const HERDR_RELEASE_TIMEOUT: Duration = Duration::from_secs(1);
const WORKING: &str = "Marv is working: stop the current turn (Esc) before resuming another session.";
const NEXT_TIME: &str = "Marv will use it from the next conversation (/clear starts one).";
const NOT_ADDED: &str = "Stopped: nothing was added.";

/// `/mcp add`'s lookup and `/mcp search`: the network, given the lookup's Esc (tests: the install fixtures).
pub type McpDeps = Arc<dyn Fn(CancellationToken) -> Arc<dyn ResolveDeps> + Send + Sync>;

// ---------------------------------------------------------------------------------------------------------------------
// Mapping: engine types → the TUI's view types
// ---------------------------------------------------------------------------------------------------------------------

fn diff_line(line: &marv_tools::DiffLine) -> view::DiffLine {
    let kind = match line.kind {
        marv_tools::DiffKind::Add => view::DiffKind::Add,
        marv_tools::DiffKind::Del => view::DiffKind::Del,
        marv_tools::DiffKind::Ctx => view::DiffKind::Ctx,
        marv_tools::DiffKind::Gap => view::DiffKind::Gap,
    };
    view::DiffLine { kind, text: line.text.clone(), old_line: line.old_line, new_line: line.new_line }
}

fn diff_shown(diff: &marv_tools::DiffShown) -> view::DiffShown {
    view::DiffShown { lines: diff.lines.iter().map(diff_line).collect(), more: diff.more }
}

/// The approval as the prompt shows it. A protected path's reason joins the warning line.
pub fn approval_request(request: &marv_tools::ApprovalRequest) -> view::ApprovalRequest {
    let p = &request.preview;
    let protected = request.protected.map(|rule| {
        let info = rule.info();
        format!("changes {}: {}", info.pattern, info.why)
    });
    let warning = match (p.warning.clone(), protected) {
        (Some(w), Some(why)) => Some(format!("{w} · {why}")),
        (w, why) => w.or(why),
    };
    view::ApprovalRequest {
        reason: request.reason,
        tool: request.tool.clone(),
        label: request.label.clone(),
        preview: view::Preview {
            title: p.title.clone(),
            diff: p.diff.as_ref().map(|d| d.iter().map(diff_line).collect()),
            command: p.command.clone(),
            text: p.text.clone(),
            note: p.note.clone(),
            warning,
            files: p.files.as_ref().map(|files| {
                files
                    .iter()
                    .map(|f| view::PreviewFile {
                        title: f.title.clone(),
                        diff: f.diff.as_ref().map(diff_shown),
                        note: f.note.clone(),
                    })
                    .collect()
            }),
        },
        scope: view::Scope { key: request.scope.key.clone(), description: request.scope.description.clone() },
        agent: request.agent.clone(),
        network: request.network,
        once: request.once,
        must_ask: request.must_ask,
        door: request.door,
    }
}

pub fn totals(t: &marv_session::Totals) -> view::Totals {
    view::Totals {
        requests: t.requests,
        prompt_tokens: t.prompt_tokens,
        cached_tokens: t.cached_tokens,
        completion_tokens: t.completion_tokens,
        cost: t.cost,
        estimated: t.estimated.unwrap_or(false),
        local: t.local.unwrap_or(false),
    }
}

/// A saved transcript entry: both sides keep Marv's JSON field names, so the session file's shape is the bridge.
pub fn transcript_message(m: &marv_session::Message) -> Option<view::Message> {
    serde_json::to_value(m).ok().and_then(|v| serde_json::from_value(v).ok())
}

/// A transcript entry (the App gives it its own id).
fn message(role: Role, text: impl Into<String>) -> view::Message {
    view::Message { id: 0, role, text: text.into(), is_error: false, markdown: false, notice: None, tool: None }
}

fn system(text: impl Into<String>, is_error: bool) -> UiEvent {
    UiEvent::Message(view::Message { is_error, ..message(Role::System, text) })
}

fn notice_message(notice: Notice) -> UiEvent {
    UiEvent::Message(view::Message { notice: Some(notice), ..message(Role::System, "") })
}

/// What a finished call's entry shows. A plain failure shows its message; a subagent that stopped shows its own
/// summary.
pub fn tool_status(run: &marv_agent::ToolRun) -> view::ToolStatus {
    let result = &run.result;
    let summary = if result.is_error && result.summary.as_deref() == Some("error") {
        result.output.split('\n').next().map(str::to_string)
    } else {
        result.summary.clone()
    };
    // A call declined with a reason (#185) didn't run either, though its turn goes on.
    let status = if run.declined || run.approval == marv_agent::ApprovalRecord::Declined {
        view::ToolState::Declined
    } else if result.is_error {
        view::ToolState::Error
    } else {
        view::ToolState::Done
    };
    let diff = run
        .display
        .as_ref()
        .and_then(|d| d.downcast_ref::<marv_session::ToolDisplay>())
        .and_then(|d| d.diff.as_ref())
        .map(diff_shown);
    view::ToolStatus { label: run.label.clone(), status, summary, steps: None, diff }
}

/// A plan call's list, for the screen (`ToolDisplay::plan`); `None` for a call that failed.
fn plan_of(run: &marv_agent::ToolRun) -> Option<Vec<view::PlanItem>> {
    let display = run.display.as_ref()?.downcast_ref::<marv_session::ToolDisplay>()?;
    display.plan.as_deref().map(view_plan)
}

fn view_plan(items: &[marv_tools::PlanItem]) -> Vec<view::PlanItem> {
    items
        .iter()
        .map(|i| view::PlanItem {
            text: i.text.clone(),
            status: match i.status {
                marv_tools::PlanStatus::Pending => view::PlanStatus::Pending,
                marv_tools::PlanStatus::InProgress => view::PlanStatus::InProgress,
                marv_tools::PlanStatus::Done => view::PlanStatus::Done,
            },
        })
        .collect()
}

fn compact_outcome(result: CompactResult) -> view::CompactOutcome {
    match result {
        CompactResult::Compacted { summary, tokens_before, trigger } => {
            view::CompactOutcome::Compacted { summary, tokens_before, overflow: trigger.level() }
        }
        CompactResult::NotCompacted { reason, error, .. } => view::CompactOutcome::NotCompacted {
            reason: match reason {
                CompactFailure::Empty => "empty",
                CompactFailure::Stopped => "stopped",
                CompactFailure::Failed => "failed",
            }
            .into(),
            error,
        },
    }
}

/// One loop event as the UI sees it. `totals`: the session's, for a usage event.
pub fn map_loop(event: &LoopEvent, totals: &dyn Fn() -> view::Totals) -> Option<UiEvent> {
    use LoopEvent as L;
    Some(match event {
        // #96: the error right after it says what happened (or the loop's `Retry`, when it goes on past it).
        L::Discarded { .. } => return None,
        // Marv #63: the session's compaction notice that follows says what was done about it.
        L::Overflow { .. } => return None,
        // marv-rs: Marv's notes to the model are for the logs (stream-json, the trajectory); the TUI shows what follows.
        L::Note { .. } => return None,
        L::TextDelta(text) => UiEvent::TextDelta(text.clone()),
        L::ThinkingDelta(text) => UiEvent::ThinkingDelta(text.clone()),
        L::Usage(usage, _) => UiEvent::Usage { last: *usage, totals: totals() },
        L::Assistant { text } => UiEvent::Assistant { text: text.clone() },
        L::SentNow { text } => UiEvent::SentNow { text: text.clone() },
        L::ToolCallDelta { index, name, text } => {
            UiEvent::ToolCallDelta { index: *index, name: name.clone(), text: text.clone() }
        }
        L::ToolStart { call, label } => {
            UiEvent::ToolStart { call_id: call.id.clone(), name: call.name.clone(), label: label.clone() }
        }
        L::ToolEnd { call, run } => {
            UiEvent::ToolEnd { call_id: call.id.clone(), name: call.name.clone(), status: tool_status(run) }
        }
        L::ToolSkipped { call, output } => {
            UiEvent::ToolSkipped { call_id: call.id.clone(), name: call.name.clone(), output: output.clone() }
        }
        L::Error(message) => UiEvent::Error(message.clone()),
        L::StepLimit { steps, continued } => UiEvent::StepLimit { steps: *steps, continued: *continued },
        L::EmptyReply { next } => UiEvent::EmptyReply {
            next: match next {
                marv_agent::agent::EmptyNext::Retry => view::EmptyNext::Retry,
                marv_agent::agent::EmptyNext::Nudge => view::EmptyNext::Nudge,
                marv_agent::agent::EmptyNext::Stop => view::EmptyNext::Stop,
            },
        },
        L::CutOff { continued } => UiEvent::CutOff { continued: *continued },
        L::TextLoop { continued } => UiEvent::TextLoop { continued: *continued },
        L::AnnouncedStep { continued } => UiEvent::AnnouncedStep { continued: *continued },
        L::ToolLoop { continued } => UiEvent::ToolLoop { continued: *continued },
        L::Compacted => UiEvent::Compacted,
        L::Done(reason) => UiEvent::Done(*reason),
        L::Retry { attempt, max, message, continued, .. } => {
            UiEvent::Retry { attempt: *attempt, max: *max, message: message.clone(), continued: *continued }
        }
    })
}

/// What one turn's mapping remembers between events.
#[derive(Default)]
pub struct TurnMap {
    /// Each running subagent's title (its `agent` call's label), by call id: `Subagent` events carry only the id.
    titles: HashMap<String, String>,
    /// The project checks' last pass, for `/checks`.
    pub last_checks: Option<ProjectCheckResult>,
}

/// `/yolo on` in a Marv started with `MARV_YOLO=off` (#74).
pub const YOLO_HELD_OFF: &str = "Yolo stays off in this Marv: it was started with MARV_YOLO=off (a worker started by a Marv out of yolo). Start it without that variable to use yolo.";

/// The startup note of a Marv started with `MARV_TAINTED=1` (#74).
pub const TAINTED_AT_START: &str = "Started by another Marv whose conversation held untrusted content (MARV_TAINTED): its task counts as untrusted here too, so anything that can send data out asks first, even in yolo.";

/// The tainted-session line (marv-rs, docs/SECURITY.md): from now on egress asks, even in yolo.
pub fn tainted_text(source: marv_core::Provenance) -> String {
    let what = match source {
        marv_core::Provenance::Web => "a web page",
        marv_core::Provenance::Mcp => "an MCP server's output",
        marv_core::Provenance::Mesh => "a mesh message",
        marv_core::Provenance::Image => "an image",
        marv_core::Provenance::Hook => "a session-start hook's output",
        marv_core::Provenance::Herdr => "what another Herdr pane shows",
        marv_core::Provenance::Github => "text from GitHub",
        marv_core::Provenance::Network => "a networked command's output",
        _ => "untrusted content",
    };
    format!(
        "✻ This conversation now contains {what}. From here on, anything that can send data out (network commands, web, MCP, mesh) asks first, even in yolo."
    )
}

/// One session event as the UI's events (usually one; a subagent's usage also updates the session's totals).
pub fn map_event(
    event: SessionEvent,
    map: &mut TurnMap,
    totals: &dyn Fn() -> view::Totals,
    last: &dyn Fn() -> Option<marv_core::Usage>,
) -> Vec<UiEvent> {
    match event {
        SessionEvent::TurnStart { turn } => vec![UiEvent::TurnStart { turn }],
        SessionEvent::Status(status) => vec![UiEvent::Status(match status {
            TurnStatus::WaitingForMcp => view::TurnStatus::WaitingForMcp,
            TurnStatus::Compacting => view::TurnStatus::Compacting,
            TurnStatus::Checking => view::TurnStatus::Checking,
            TurnStatus::Checkpointing => view::TurnStatus::Checkpointing,
            TurnStatus::Running => view::TurnStatus::Running,
        })],
        SessionEvent::Loop(event) => {
            // Providers reuse call ids from step to step (Ollama's call_0…): a new agent call replaces the title.
            if let LoopEvent::ToolStart { call, label } = &event
                && call.name == "agent"
            {
                map.titles.insert(call.id.clone(), label.clone());
            }
            let mut out: Vec<UiEvent> = map_loop(&event, totals).into_iter().collect();
            // The main agent's plan call: its new list, pinned above the prompt (#15). Subagents have no plan tool.
            if let LoopEvent::ToolEnd { call, run } = &event
                && call.name == "plan"
                && let Some(items) = plan_of(run)
            {
                out.push(UiEvent::Plan(items));
            }
            out
        }
        SessionEvent::Subagent { call_id, event } => {
            let title = map.titles.get(&call_id).cloned().unwrap_or_default();
            let mut out = Vec::new();
            // Subagents' requests count toward the session's tokens and cost; the context bar stays the main agent's.
            if matches!(event, LoopEvent::Usage(..))
                && let Some(last) = last()
            {
                out.push(UiEvent::Usage { last, totals: totals() });
            }
            if let Some(event) = map_loop(&event, totals) {
                out.push(UiEvent::Subagent { call_id, title, event: Box::new(event) });
            }
            out
        }
        SessionEvent::SubagentProgress { call_id, progress } => vec![UiEvent::SubagentProgress {
            call_id,
            progress: view::AgentProgress { line: progress.line, steps: progress.steps },
        }],
        SessionEvent::Compaction(result) => vec![UiEvent::Compaction { automatic: true, outcome: compact_outcome(result) }],
        SessionEvent::Check { label, result } => {
            check_notice(label, &result).map(|text| system(text, false)).into_iter().collect()
        }
        SessionEvent::Checks(result) => {
            let notice = project_check_notice(&result);
            map.last_checks = Some(result);
            notice.map(|text| system(text, false)).into_iter().collect()
        }
        SessionEvent::Tainted { source } => vec![system(tainted_text(source), false)],
        // #124: Marv's line about a radio report it sent (checked names and fixed words; printable anyway).
        SessionEvent::RadioNote { text } => vec![system(printable(&format!("✻ {text}")), false)],
        SessionEvent::Notice { text } => vec![system(printable(&format!("✻ {text}")), false)],
        SessionEvent::CheckpointFailed { message } => {
            // Said once while the same failure lasts (`checkpoint_turn`), so it says what follows for later turns.
            vec![system(
                format!(
                    "This turn can't be undone: its checkpoint failed ({message}). Until that changes, later turns can't be undone either; this isn't repeated."
                ),
                true,
            )]
        }
        SessionEvent::NotSent { texts } => vec![UiEvent::NotSent { texts }],
        SessionEvent::TurnEnd { turn, reason } => vec![UiEvent::TurnEnd {
            turn,
            reason: match reason {
                TurnEndReason::Done(r) => view::TurnEndReason::Loop(r),
                TurnEndReason::Interrupted => view::TurnEndReason::Interrupted,
            },
        }],
    }
}

/// `custom`: a custom provider's name (#275), for [`ProviderKind::Custom`].
fn provider_id(kind: ProviderKind, custom: Option<&str>) -> ProviderId {
    match kind {
        ProviderKind::OpenRouter => ProviderId::OpenRouter,
        ProviderKind::Ollama => ProviderId::Ollama,
        ProviderKind::Zai => ProviderId::Zai,
        ProviderKind::ChatGpt => ProviderId::ChatGpt,
        ProviderKind::Kimi => ProviderId::Kimi,
        ProviderKind::DeepSeek => ProviderId::DeepSeek,
        ProviderKind::Qwen => ProviderId::Qwen,
        ProviderKind::LlamaCpp => ProviderId::LlamaCpp,
        ProviderKind::Custom => ProviderId::Custom(custom.unwrap_or_default().to_string()),
    }
}

fn provider_kind(id: &ProviderId) -> ProviderKind {
    match id {
        ProviderId::OpenRouter => ProviderKind::OpenRouter,
        ProviderId::Ollama => ProviderKind::Ollama,
        ProviderId::Zai => ProviderKind::Zai,
        ProviderId::ChatGpt => ProviderKind::ChatGpt,
        ProviderId::Kimi => ProviderKind::Kimi,
        ProviderId::DeepSeek => ProviderKind::DeepSeek,
        ProviderId::Qwen => ProviderKind::Qwen,
        ProviderId::LlamaCpp => ProviderKind::LlamaCpp,
        ProviderId::Custom(_) => ProviderKind::Custom,
    }
}

/// What a provider is called in "Saved to …" lines: its preset's label, a custom one's name (#275).
fn provider_label(id: &ProviderId) -> String {
    match id {
        ProviderId::Custom(name) => name.clone(),
        id => preset(provider_kind(id)).label.into(),
    }
}

fn model_info(m: marv_provider::ModelInfo) -> view::ModelInfo {
    view::ModelInfo {
        id: m.id,
        tools: m.tools,
        price_in: m.price_in,
        price_out: m.price_out,
        price_cache_read: m.price_cache_read,
        context: m.context,
        local: m.local,
        reasoning: m.reasoning.map(|r| match r {
            marv_provider::ReasoningSupport::Optional => view::ReasoningMode::Optional,
            marv_provider::ReasoningSupport::Mandatory => view::ReasoningMode::Mandatory,
        }),
        name: m.name,
    }
}

/// The providers setup offers: OpenRouter, the plans, and Ollama. The Z.ai Coding Plan and Sign in with ChatGPT (#159)
/// when their switch is on (`marv_provider::offered`: ChatGPT's on, Z.ai's off) or the config already uses them; Kimi Code,
/// DeepSeek and Qwen's Coding Plan always.
/// #278: llama.cpp after Ollama, when a llama-server is found (or the config already uses it).
fn offered_kinds(current: Option<ProviderKind>, llama_cpp: bool) -> Vec<ProviderKind> {
    let plans = [
        (ProviderKind::Zai, marv_provider::offered::ZAI_CODING_PLAN),
        (ProviderKind::ChatGpt, marv_provider::offered::CHATGPT_SIGN_IN),
        (ProviderKind::Kimi, true),
        (ProviderKind::Qwen, true),
        (ProviderKind::DeepSeek, true),
    ];
    let mut kinds = vec![ProviderKind::OpenRouter];
    kinds.extend(plans.into_iter().filter(|&(kind, on)| on || current == Some(kind)).map(|(kind, _)| kind));
    kinds.push(ProviderKind::Ollama);
    if llama_cpp || current == Some(ProviderKind::LlamaCpp) {
        kinds.push(ProviderKind::LlamaCpp);
    }
    kinds
}

/// #159: the ChatGPT sign-in's file: the config's (the provider reads that one).
fn auth_store(config: &Config, config_dir: &Path) -> marv_provider::chatgpt::AuthStore {
    let path = config.auth_file.clone().unwrap_or_else(|| config_dir.join(marv_provider::chatgpt::AUTH_FILE));
    marv_provider::chatgpt::AuthStore { path }
}

/// What the setup screen starts from. Keys go to the UI masked.
pub fn setup_context(start: SetupStart, file: Option<&FileConfig>, config: &Config, env: &Env) -> SetupContext {
    let llama_cpp = marv_session::local_model::find_server(file.and_then(|f| f.llama_cpp.as_ref()), env).is_ok();
    let kinds = offered_kinds(file.and_then(|f| f.provider), llama_cpp);
    let mut presets: Vec<view::ProviderPreset> = kinds
        .iter()
        .map(|&kind| {
            let p = preset(kind);
            view::ProviderPreset {
                id: provider_id(kind, None),
                label: p.label.into(),
                key_env: p.key_env.map(Into::into),
                key_url: p.key_url.map(Into::into),
                default_model: p.default_model.into(),
                sign_in: kind == ProviderKind::ChatGpt,
            }
        })
        .collect();
    // #275: the user's own endpoints, after the built-in ones. Their keys are in the config, so setup asks none.
    presets.extend(file.and_then(|f| f.providers.as_ref()).into_iter().flatten().map(|(name, entry)| view::ProviderPreset {
        id: ProviderId::Custom(name.clone()),
        label: printable(name),
        key_env: None,
        key_url: None,
        default_model: entry.models.first().cloned().unwrap_or_default(),
        sign_in: false,
    }));
    let env_keys = kinds
        .iter()
        .filter_map(|&kind| {
            let value = env.get(preset(kind).key_env?)?.trim();
            (!value.is_empty()).then(|| (provider_id(kind, None), mask_key(value)))
        })
        .collect();
    SetupContext {
        start,
        presets,
        current: file
            .and_then(|f| f.provider.map(|kind| (provider_id(kind, f.custom_provider.as_deref()), config.model.clone()))),
        saved_keys: kinds.iter().filter_map(|&kind| Some((provider_id(kind, None), mask_key(file?.key_for(kind)?)))).collect(),
        accounts: config
            .auth_file
            .clone()
            .filter(|_| kinds.contains(&ProviderKind::ChatGpt))
            .map(|path| marv_provider::chatgpt::AuthStore { path }.accounts())
            .unwrap_or_default()
            .into_iter()
            .enumerate()
            .filter_map(|(i, a)| {
                let info =
                    view::AccountInfo { id: a.client_id.clone()?, label: a.label(), signed_in: a.usable(), active: i == 0 };
                Some((ProviderId::ChatGpt, info))
            })
            .collect(),
        env_keys,
    }
}

/// Setup's answer as the file to save. A custom endpoint only applies to the provider it was saved with; `api_key:
/// None` keeps the saved key.
pub fn setup_file(file: Option<&FileConfig>, answer: &SetupAnswer) -> FileConfig {
    let kind = provider_kind(&answer.provider);
    let mut next = file.cloned().unwrap_or_default();
    if next.provider != Some(kind) {
        next.base_url = None;
    }
    next.provider = Some(kind);
    // #275: which endpoint, for a custom provider; none for a built-in one.
    next.custom_provider = match &answer.provider {
        ProviderId::Custom(name) => Some(name.clone()),
        _ => None,
    };
    next.model = Some(answer.model.clone());
    if let Some(key) = &answer.api_key {
        // #159: into the chosen provider's own field, never another's.
        next.set_key(kind, key.clone());
    }
    next
}

/// Collates the startup notices known at once into one block: a bullet per notice, titled with how many there are.
pub fn startup_notice(notes: Vec<(NoticeTone, NoticeItem)>) -> Option<Notice> {
    if notes.is_empty() {
        return None;
    }
    let n = notes.len();
    Some(Notice {
        title: format!("Startup · {n} note{}", if n == 1 { "" } else { "s" }),
        tone: notes.iter().map(|(t, _)| *t).max().unwrap_or(NoticeTone::Info),
        items: notes.into_iter().map(|(_, item)| item).collect(),
    })
}

/// What the session-start hooks did (#108), one line each: shown when a conversation starts, collapsed to that (the
/// output itself goes to the model, not the screen).
pub fn hooks_notice(runs: &[marv_session::hooks::HookRun]) -> Option<Notice> {
    if runs.is_empty() {
        return None;
    }
    let failed = runs.iter().any(|r| !r.ok());
    Some(Notice {
        title: format!("Session-start hooks · {} ran", runs.len()),
        tone: if failed { NoticeTone::Warning } else { NoticeTone::Info },
        items: runs.iter().map(|r| NoticeItem { text: printable(&r.line()), detail: None }).collect(),
    })
}

/// The startup notice's item for PATH folders the sandbox hides (Marv `hiddenPathFoldersItem`).
pub fn hidden_path_folders_item(folders: &[String]) -> NoticeItem {
    let one = folders.len() == 1;
    NoticeItem {
        text: format!(
            "{} PATH folder{} {} hidden from the sandbox: {}",
            folders.len(),
            if one { "" } else { "s" },
            if one { "is" } else { "are" },
            folders.join(", ")
        ),
        detail: Some(vec![
            if one {
                "Commands in Marv can't run tools there; tsc and node_modules scripts may fail.".into()
            } else {
                "Commands in Marv can't run tools in them; tsc and node_modules scripts may fail.".into()
            },
            "Move the tool to a folder the sandbox shows (e.g. ~/.local/bin), or open an issue.".into(),
        ]),
    }
}

fn counted(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// An untrusted project server, for the trust notice: what it runs, what it reads from your environment, and which
/// project files.
fn describe_untrusted(s: &McpServerStatus) -> String {
    let mut extras = Vec::new();
    if !s.reads.is_empty() {
        let reads: Vec<String> = s.reads.iter().map(|v| format!("${v}")).collect();
        extras.push(format!("reads {} from your environment", reads.join(", ")));
    }
    if !s.runs_project_files.is_empty() {
        extras.push(format!("runs this project's {} (and whatever those load)", s.runs_project_files.join(", ")));
    }
    let which = if extras.is_empty() { String::new() } else { format!(", which {}", extras.join(" and ")) };
    format!("{} ({}){which}", s.name, s.target)
}

/// Once the servers have started: a note for any that failed, a project's servers waiting to be trusted (the trust
/// decision is the security gate: a cloned repository's .mcp.json could run anything, so it says what's at stake), and
/// those that need a login. None when there's nothing to say.
fn mcp_notice(status: &[McpServerStatus]) -> Option<Notice> {
    let of = |state: McpState| status.iter().filter(|s| s.state == state).collect::<Vec<_>>();
    let (failed, waiting, login) = (of(McpState::Failed), of(McpState::Untrusted), of(McpState::NeedsLogin));
    let mut items = Vec::new();
    if !failed.is_empty() {
        items.push(NoticeItem {
            text: format!("{} failed to start (see /mcp)", counted(failed.len(), "MCP server", "MCP servers")),
            detail: Some(failed.iter().map(|s| format!("{}: {}", s.name, s.error.as_deref().unwrap_or_default())).collect()),
        });
    }
    if !waiting.is_empty() {
        let mut detail: Vec<String> = waiting.iter().map(|s| describe_untrusted(s)).collect();
        detail.push("They'd run with your permissions, outside the sandbox. /mcp trust starts them.".into());
        items.push(NoticeItem {
            text: format!(
                "This project's .mcp.json lists {} you haven't trusted yet",
                counted(waiting.len(), "MCP server", "MCP servers")
            ),
            detail: Some(detail),
        });
    }
    if !login.is_empty() {
        items.push(NoticeItem {
            text: format!("{} a login", counted(login.len(), "MCP server needs", "MCP servers need")),
            detail: Some(login.iter().map(|s| format!("{}: /mcp login {}", s.name, s.name)).collect()),
        });
    }
    let count = failed.len() + waiting.len() + login.len();
    (count > 0).then(|| Notice {
        title: format!("MCP · {count} note{}", if count == 1 { "" } else { "s" }),
        tone: if failed.is_empty() { NoticeTone::Warning } else { NoticeTone::Error },
        items,
    })
}

/// "github: logged in · connected · 41 tools", or why not: the end of /mcp login.
fn login_result(s: &McpServerStatus) -> (String, bool) {
    match s.state {
        McpState::Connected => {
            let n = s.tools.len();
            let tools = format!("{n} tool{}", if n == 1 { "" } else { "s" });
            (format!("{}: logged in · connected · {tools}. Marv can use them from your next message.", s.name), false)
        }
        McpState::NeedsLogin => (format!("{}: logged in, but the server still refused the token (see /mcp).", s.name), true),
        state => {
            let why = s.error.clone().unwrap_or_else(|| format!("{state:?}").to_lowercase());
            (format!("{}: logged in, but it didn't connect: {why} (see /mcp).", s.name), true)
        }
    }
}

fn logout_text(name: &str, revoked: Revoked) -> String {
    let grant = match revoked {
        Revoked::Revoked => "The authorization server revoked the grant.",
        Revoked::Unsupported => {
            "The authorization server can't revoke it from here, so the grant still exists on its side: remove it in your account's settings there (usually under applications or integrations)."
        }
        Revoked::Failed => {
            "Revoking the grant failed, so it may still exist on the authorization server's side: remove it in your account's settings there."
        }
    };
    format!("Logged out of {name}: its tokens are forgotten. {grant}")
}

/// Where a login's URL points, as `URL.host` says it: the host, and the port when it isn't the scheme's.
fn url_host(url: &str) -> String {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    authority.rsplit_once('@').map_or(authority, |(_, h)| h).to_string()
}

/// The MCP manager, as startup makes it: always (so `/mcp add` can start the first server), starting in the
/// background. The config's problems are for the startup notice.
pub fn start_mcp(root: &Path, home: &Path, config_dir: &Path, env: &Env) -> (McpHandle, Vec<String>) {
    let loaded =
        marv_mcp::config::load_mcp_config(root, config_dir, env, marv_mcp::config::Include { personal: true, project: true });
    let mcp = marv_mcp::manager::McpManager::new(
        loaded.servers,
        marv_mcp::manager::ManagerOptions {
            root: root.to_path_buf(),
            project_key: project_key(root),
            home: home.to_path_buf(),
            version: env!("CARGO_PKG_VERSION").into(),
            trust: Some(marv_mcp::trust::McpTrust::new(config_dir.join("mcp-trust.json"))),
            // OAuth logins (/mcp login), shared with marv -p and SDK sessions using the same config folder.
            logins: Some(marv_mcp::oauth_store::McpTokenStore::new(config_dir.join("mcp-tokens.json"))),
            env: env.clone(),
            connect_timeout: marv_mcp::manager::CONNECT_TIMEOUT,
            login_timeout: marv_mcp::manager::LOGIN_TIMEOUT,
        },
    );
    let starting = mcp.clone();
    tokio::spawn(async move { starting.start().await });
    (mcp, loaded.problems)
}

/// The `/mcp login` waiting for its callback: a pasted callback address goes to it.
struct PendingLogin {
    id: u64,
    server: String,
    paste: Box<dyn FnMut(&str) -> CallbackCheck + Send>,
}

/// What `/mcp add`'s lookup and approval came to.
enum AddOutcome {
    Choices(Vec<String>),
    Declined(String),
    Added(String, Value),
    Failed(String),
}

// ---------------------------------------------------------------------------------------------------------------------
// Approvals and Herdr
// ---------------------------------------------------------------------------------------------------------------------

struct Ask {
    id: u64,
    /// Herdr's message: the asking subagent and the request's label.
    message: String,
    answer: oneshot::Sender<marv_tools::Decision>,
}

/// What the bridge has handed out and not yet seen finish: approvals and work. Herdr's state follows it.
pub struct Activity {
    tx: UnboundedSender<UiEvent>,
    next: AtomicU64,
    asks: Mutex<Vec<Ask>>,
    /// A turn or `/compact` is running.
    busy: AtomicBool,
    /// Something other than an empty prompt has the keyboard (`SessionPort::keyboard_held`): Herdr is told
    /// working, not idle, so a pane send (a prompt, not a command) can't type into it.
    held: AtomicBool,
    herdr: Option<HerdrReporter>,
    /// The mesh radio (#124): its presence's state follows Herdr's (blocked is `waiting`).
    radio: Mutex<Option<Arc<Radio>>>,
}

impl Activity {
    fn new(tx: UnboundedSender<UiEvent>, herdr: Option<HerdrReporter>) -> Arc<Self> {
        Arc::new(Self {
            tx,
            next: AtomicU64::new(1),
            asks: Mutex::default(),
            busy: AtomicBool::new(false),
            held: AtomicBool::new(false),
            herdr,
            radio: Mutex::default(),
        })
    }

    fn emit(&self, event: UiEvent) {
        let _ = self.tx.send(event);
    }

    /// Herdr's state: blocked at an approval, working while busy or while the keyboard is held (the palette, setup,
    /// a half-typed slash command…), idle otherwise.
    pub fn herdr_state(&self) -> (HerdrState, Option<String>) {
        match self.session_state() {
            (HerdrState::Idle, _) if self.held.load(Ordering::SeqCst) => (HerdrState::Working, None),
            state => state,
        }
    }

    /// The session's own state, whatever has the keyboard: what the radio shows.
    fn session_state(&self) -> (HerdrState, Option<String>) {
        let asks = self.asks.lock().unwrap_or_else(|e| e.into_inner());
        match asks.first() {
            Some(head) => (HerdrState::Blocked, Some(head.message.clone())),
            None if self.busy.load(Ordering::SeqCst) => (HerdrState::Working, None),
            None => (HerdrState::Idle, None),
        }
    }

    fn report(&self) {
        if let Some(herdr) = &self.herdr {
            let (state, message) = self.herdr_state();
            herdr.state(state, message.as_deref());
        }
        if let Some(radio) = self.radio.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            radio.set_state(match self.session_state().0 {
                HerdrState::Idle => PresenceState::Idle,
                HerdrState::Working => PresenceState::Working,
                HerdrState::Blocked => PresenceState::Waiting,
            });
        }
    }

    fn set_held(&self, held: bool) {
        if self.held.swap(held, Ordering::SeqCst) != held {
            self.report();
        }
    }

    fn set_busy(&self, busy: bool) {
        self.busy.store(busy, Ordering::SeqCst);
        self.report();
    }

    fn answer(&self, id: u64, decision: marv_tools::Decision) {
        let ask = {
            let mut asks = self.asks.lock().unwrap_or_else(|e| e.into_inner());
            asks.iter().position(|a| a.id == id).map(|i| asks.remove(i))
        };
        if let Some(ask) = ask {
            let _ = ask.answer.send(decision);
        }
        self.report();
    }

    /// The ask `id` is over. Still unanswered (its wait was dropped: a stopped turn, or #201 a door whose caller hung
    /// up), the App takes it off the screen too.
    fn forget(&self, id: u64) {
        let unanswered = {
            let mut asks = self.asks.lock().unwrap_or_else(|e| e.into_inner());
            let before = asks.len();
            asks.retain(|a| a.id != id);
            asks.len() != before
        };
        if unanswered {
            self.emit(UiEvent::ApprovalWithdrawn(ApprovalId(id)));
        }
        self.report();
    }

    /// An approval is waiting for the person (a turn's, or a wire asked for from another process while idle).
    fn waiting(&self) -> bool {
        !self.asks.lock().unwrap_or_else(|e| e.into_inner()).is_empty()
    }
}

/// Removes an ask the session stopped waiting for (the turn ended), so Herdr stops showing it.
struct Forget<'a>(&'a Activity, u64);
impl Drop for Forget<'_> {
    fn drop(&mut self) {
        self.0.forget(self.1);
    }
}

/// The session's approver: every request goes to the App's queue as a `UiEvent::Approval`.
pub struct TuiApprover(pub Arc<Activity>);

#[async_trait]
impl marv_tools::Approver for TuiApprover {
    async fn approve(&self, request: marv_tools::ApprovalRequest) -> marv_tools::Decision {
        let activity = &self.0;
        let id = activity.next.fetch_add(1, Ordering::SeqCst);
        let (send, receive) = oneshot::channel();
        let message = [request.agent.as_deref(), Some(request.label.as_str())]
            .into_iter()
            .flatten()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(": ");
        activity.asks.lock().unwrap_or_else(|e| e.into_inner()).push(Ask { id, message, answer: send });
        let _forget = Forget(activity, id);
        activity.report();
        activity.emit(UiEvent::Approval { id: ApprovalId(id), request: approval_request(&request) });
        // The App gone (its queue dropped) answers no, like a stopped run.
        receive.await.unwrap_or(marv_tools::Decision::No)
    }
}

fn decision(d: view::Decision) -> marv_tools::Decision {
    match d {
        view::Decision::Yes => marv_tools::Decision::Yes,
        view::Decision::Always => marv_tools::Decision::Always,
        view::Decision::No => marv_tools::Decision::No,
        view::Decision::Declined(reason) => marv_tools::Decision::Declined(reason),
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// The bridge
// ---------------------------------------------------------------------------------------------------------------------

/// Everything startup found, for [`Bridge::start`].
pub struct Startup {
    pub root: PathBuf,
    pub home: PathBuf,
    pub config_dir: PathBuf,
    pub store: ConfigStore,
    pub file: Option<FileConfig>,
    /// The real environment (never a project's .env).
    pub env: Env,
    pub version: String,
    pub instructions: Option<Instructions>,
    /// Instruction files found but not read, and why (#62: a symlink out of the project, not a regular file).
    pub instruction_problems: Vec<String>,
    pub skills: Vec<LoadedSkill>,
    pub skill_problems: Vec<String>,
    pub agents: Vec<marv_session::agents::AgentType>,
    pub agent_problems: Vec<String>,
    /// The project's instruction files, skills and agent types as startup found them, and whether the person trusted
    /// them (`marv_session::project_trust`): `instructions`, `skills` and `agents` were loaded to match.
    pub project_files: ProjectFiles,
    pub project_trust: TrustStatus,
    /// Which subfolder instruction files count as the person's.
    pub nested_trust: Arc<marv_session::project_trust::NestedTrust>,
    pub memory: Option<(MemoryPaths, Memories)>,
    pub mcp: Option<McpHandle>,
    pub mcp_problems: Vec<String>,
    /// Shortened for display.
    pub hidden_path_folders: Vec<String>,
    pub sandbox_available: bool,
    /// Pre-approved scopes (#98): the config's "approve" and the run's --approve, seeded as "always".
    pub pre_approved: Vec<String>,
    /// Extra writable sandbox folders (#98), resolved (canonical, refused ones left out).
    pub writable: Vec<PathBuf>,
    /// Problems with those two (an unknown scope name, a refused writable folder), for the startup notice.
    pub approval_problems: Vec<String>,
    /// gh is there and logged in (checked once at startup): the session offers the github tool (#47).
    pub github: bool,
    pub sessions: Option<Arc<SessionStore>>,
    pub trajectories: Option<Arc<TrajectoryStore>>,
    pub herdr: Option<HerdrReporter>,
    /// The process's switchboard (`mesh_cmd::start`), if it has one.
    pub mesh: Option<marv_session::mesh::MeshHandle>,
    /// How a provider is made from the config (`create_provider`; tests: a scripted one).
    pub make_provider: MakeProvider,
    pub load_models: LoadModels,
    /// `marv -m <model>`: the model for this run, never saved (the config's stays the default).
    pub model: Option<String>,
    /// `/mcp add`'s lookup and `/mcp search` (live: [`marv_session::mcp_install::LiveDeps`]).
    pub mcp_deps: McpDeps,
    /// `/mcp login`: opens the authorization URL where a browser can open (tests: their own, so none ever opens).
    pub open_url: Authorize,
}

/// The TUI's `SessionPort` over a [`MarvSession`].
pub struct Bridge {
    session: MarvSession,
    activity: Arc<Activity>,
    root: PathBuf,
    home: PathBuf,
    store: ConfigStore,
    file: Mutex<Option<FileConfig>>,
    env: Env,
    version: String,
    /// The instruction files in the prompt (#62), for the welcome banner and the startup notice.
    instruction_sources: Vec<InstructionSource>,
    instruction_problems: Vec<String>,
    skills: Vec<LoadedSkill>,
    skill_problems: Vec<String>,
    agents: Vec<marv_session::agents::AgentType>,
    agent_problems: Vec<String>,
    /// What `/trust` trusts: the files as startup showed them (a file changed since asks again at the next start).
    project_files: ProjectFiles,
    project_trust: TrustStatus,
    memory: Option<MemoryPaths>,
    mcp: Option<McpHandle>,
    mcp_problems: Vec<String>,
    hidden_path_folders: Vec<String>,
    sandbox_available: bool,
    pre_approved: Vec<String>,
    writable: Vec<PathBuf>,
    approval_problems: Vec<String>,
    sessions: Option<Arc<SessionStore>>,
    trajectories_dir: Option<PathBuf>,
    last_checks: Mutex<Option<ProjectCheckResult>>,
    make_provider: MakeProvider,
    load_models: LoadModels,
    /// The model for this session only (`-m`, `/model <id>`), over the config's; `None`: the config's.
    model: Mutex<Option<String>>,
    /// ~/.marv (or MARV_CONFIG_DIR): where config.json is, mcp.json is too.
    config_dir: PathBuf,
    mcp_deps: McpDeps,
    open_url: Authorize,
    /// Busy work outside a turn (/mcp add's lookup, /mcp search): Esc stops it.
    busy_work: Mutex<Option<CancellationToken>>,
    pending_login: Mutex<Option<PendingLogin>>,
    /// #159: the ChatGPT sign-in setup is waiting for, until it ends or setup closes.
    sign_in: Mutex<Option<CancellationToken>>,
    next_login: AtomicU64,
    /// Servers already said to need a login (at startup or since), so a mid-session expiry is told once.
    told_needs_login: Mutex<std::collections::HashSet<String>>,
    /// This Marv's name on the machine's mesh, once its door is open (for `/mesh`).
    mesh_name: Mutex<Option<String>>,
    /// Wake-ups per wire this minute (#78, `mesh.wake`).
    wake_limit: Mutex<marv_session::mesh::WakeLimit>,
    /// Radio wakes per sender and overall (#124); paused ones wake again once the person types.
    radio_wake_limit: Mutex<marv_session::radio::RadioWakeLimit>,
    /// Stops [`Bridge::watch_mesh`] when the App leaves ([`Bridge::close_mesh`]) or the bridge goes.
    mesh_watch: CancellationToken,
    /// The mesh radio (#124), once the door is open ([`Bridge::set_radio`]).
    radio: Mutex<Option<Arc<Radio>>>,
    /// Who may start a turn (#78): user submissions and wake-ups go through this one lock, so a wake can't start
    /// once a submission is on its way, and a submission that meets a running turn waits for it instead of failing.
    gate: Mutex<Gate>,
    /// A project's `.marv/config.json` named `hooks` (#108): the startup note saying they were not run.
    hooks_refused: Option<String>,
    /// The next-step suggestion being asked for after a turn: a new turn, a command or Esc stops it.
    suggesting: Mutex<Option<CancellationToken>>,
}

/// [`Bridge::gate`]'s state.
#[derive(Default)]
struct Gate {
    /// A turn started through the gate is running (until its events are forwarded, and the next queued one starts).
    turn: bool,
    /// That turn is a wake-up: the App may not have seen it start yet.
    woken: bool,
    /// Input waiting for the running turn (#184), run in order next: the person's queued messages (Enter while a turn
    /// runs, and "send now" text the turn ended before reading), and input the App sent without having seen the turn
    /// start (a wake-up whose start hadn't reached it) or after seeing it end. One queue for all of it, so a mesh-woken
    /// turn and the person's turn never race.
    queued: std::collections::VecDeque<Queued>,
    /// The last turn was stopped (or failed) with messages queued: they wait for the person, never start on their
    /// own. Enter on an empty prompt sends the next; anything the person sends or queues releases the hold. Input the
    /// App already showed is never held.
    held: bool,
    /// The App has left: no wake-ups, no queued input.
    closing: bool,
}

/// One input waiting in [`Gate::queued`].
struct Queued {
    text: String,
    for_model: Option<String>,
    /// Already in the App's transcript (it sent it as if idle): starts whatever happens, never listed nor edited.
    /// Otherwise the person queued it: listed above the prompt ([`UiEvent::Queued`]), and its line is shown when it
    /// starts.
    shown: bool,
}

/// How a gate's turn ended, for [`Bridge::finished`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ended {
    /// The reply finished: the queue goes on, and once idle the next message may be suggested.
    Done,
    /// Nothing ran (a wake with nothing to do): the queue goes on.
    Quiet,
    /// The person stopped it: the queue waits for them.
    Stopped,
    /// An error, the step limit, a refused send: the queue waits too.
    Failed,
}

impl Gate {
    /// What the App lists above the prompt: the person's queued messages, oldest first.
    fn listed(&self) -> Vec<String> {
        self.queued.iter().filter(|q| !q.shown).map(|q| q.text.clone()).collect()
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        self.mesh_watch.cancel();
    }
}

/// `config` with the session's model (`-m`, `/model <id>`) in place of the saved one, when there is one.
fn with_model(mut config: Config, model: Option<&str>) -> Config {
    if let Some(model) = model {
        config.model = model.to_string();
    }
    config
}

fn factory(config: &Config, make: &MakeProvider, load: &LoadModels) -> ProviderChoice {
    ProviderChoice::Factory(Arc::new(ConfigFactory { config: config.provider_config(), make: make.clone(), load: load.clone() }))
}

impl Bridge {
    /// Makes the session from what startup found; events go to `tx`.
    pub fn start(s: Startup, tx: UnboundedSender<UiEvent>) -> anyhow::Result<Arc<Self>> {
        let activity = Activity::new(tx.clone(), s.herdr);
        let config = with_model(resolve_config(s.file.as_ref(), &s.env), s.model.as_deref());
        let mut init = SessionInit::new(s.root.clone(), factory(&config, &s.make_provider, &s.load_models));
        init.cwd = Some(shorten_home(&s.root, &s.home));
        init.version = s.version.clone();
        init.instructions = s.instructions.as_ref().map(|i| i.text.clone());
        init.nested_instructions = true;
        init.nested_trust = s.nested_trust.clone();
        init.skills = s.skills.clone();
        init.agents = s.agents.clone();
        init.memory = s.memory.clone();
        init.mcp = s.mcp.clone();
        // Marv's own keys, redacted from every tool result (docs/SECURITY.md; the MCP servers' tokens are added per turn).
        init.secrets = marv_session::config::configured_secrets(s.file.as_ref(), &s.env, Some(&config));
        // The CLI started them, so the CLI closes them (on the way out).
        init.owns_mcp = false;
        init.approve = Some(Arc::new(TuiApprover(activity.clone())));
        init.mesh = s.mesh.clone();
        // #124: the TUI's session can be on the radio; the first conversation offers radio_tx if the user's own
        // config has the radio on (the radio itself joins once the door is open: `set_radio`).
        init.radio = Some(config.mesh_radio);
        init.max_steps = Some(config.max_steps);
        (init.sandbox, init.yolo, init.diagnostics, init.checks, init.mcp_tools) =
            (config.sandbox, config.yolo, config.diagnostics, config.checks, config.mcp_tools);
        init.approve_sandboxed = config.approve_sandboxed;
        init.formatter = config.formatter;
        init.compact_tokens = config.compact_tokens;
        (init.compaction, init.compact_tail_tokens) = (config.compaction, config.compact_tail_tokens);
        // #98: the config's "approve" and this run's --approve seed the session's "always" scopes; the writable
        // folders are resolved and checked already (refused ones are in approval_problems, not here).
        init.pre_approved = s.pre_approved.clone();
        init.writable = s.writable.clone();
        // The person at this terminal approves each command on the host (#52); gh where it's logged in (#47).
        init.host_commands = true;
        init.github = s.github;
        // In a Herdr pane, the herdr tool (#74): reads, light actions, and commands and workers asked every time.
        let outside: Vec<PathBuf> = std::iter::once(s.root.clone()).chain(s.writable.iter().cloned()).collect();
        init.herdr = marv_session::herdr_door::setup_from_env(&s.env, config.herdr_peers, &outside);
        // Spawned by a Marv whose conversation held untrusted text (MARV_TAINTED=1, #74): start tainted.
        init.tainted_at_start = marv_session::config::tainted_at_start(&s.env);
        init.no_yolo = marv_session::sdk::no_yolo_reason(&s.root, &s.home);
        if init.no_yolo.is_some() {
            init.yolo = false;
        }
        init.sessions = s.sessions.clone();
        init.trajectories = s.trajectories.clone();
        init.log_trajectories = config.trajectories;
        init.worktrees_dir = Some(s.config_dir.join("worktrees").join(project_key(&s.root)));
        init.tool_output_dir = Some(s.config_dir.join(marv_tools::output::TOOL_OUTPUT_DIR));
        init.file_locks_dir = Some(s.config_dir.join(marv_tools::file_lock::LOCK_FOLDER));
        init.checkpoints = config.checkpoints;
        init.checkpoints_dir = Some(s.config_dir.join("checkpoints").join(project_key(&s.root)));
        // Session-start hooks (#108): from the user's own config file only. A project's naming some is only told.
        let hooks_refused;
        (init.hooks, hooks_refused) = marv_session::hooks::session_start_hooks(&s.root, &s.store.path, s.file.as_ref());
        let (on_change, on_warning) = (tx.clone(), tx.clone());
        // The model's info arrived (its context window): the status bar is asked again via `status_info`, below.
        let changed = Arc::new(Mutex::new(None::<std::sync::Weak<Bridge>>));
        let changed_in = changed.clone();
        init.on_change = Some(Arc::new(move || {
            if let Some(bridge) = changed_in.lock().unwrap_or_else(|e| e.into_inner()).as_ref().and_then(|w| w.upgrade()) {
                let _ = on_change.send(UiEvent::StatusInfo(bridge.status_info()));
            }
        }));
        init.on_warning = Some(Arc::new(move |text: String| {
            let _ = on_warning.send(system(text, true));
        }));
        let session = MarvSession::new(init)?;
        let trajectories_dir = s.trajectories.as_ref().map(|t| t.dir.join(project_key(&s.root)));
        let bridge = Arc::new(Self {
            session,
            activity,
            root: s.root,
            home: s.home,
            store: s.store,
            file: Mutex::new(s.file),
            env: s.env,
            version: s.version,
            instruction_sources: s.instructions.map(|i| i.sources).unwrap_or_default(),
            instruction_problems: s.instruction_problems,
            skills: s.skills,
            skill_problems: s.skill_problems,
            agents: s.agents,
            agent_problems: s.agent_problems,
            project_files: s.project_files,
            project_trust: s.project_trust,
            memory: s.memory.map(|(paths, _)| paths),
            mcp: s.mcp,
            mcp_problems: s.mcp_problems,
            hidden_path_folders: s.hidden_path_folders,
            sandbox_available: s.sandbox_available,
            pre_approved: s.pre_approved,
            writable: s.writable,
            approval_problems: s.approval_problems,
            sessions: s.sessions,
            trajectories_dir,
            last_checks: Mutex::default(),
            make_provider: s.make_provider,
            load_models: s.load_models,
            model: Mutex::new(s.model.clone()),
            config_dir: s.config_dir,
            mcp_deps: s.mcp_deps,
            open_url: s.open_url,
            busy_work: Mutex::default(),
            pending_login: Mutex::default(),
            sign_in: Mutex::default(),
            next_login: AtomicU64::new(0),
            told_needs_login: Mutex::default(),
            mesh_name: Mutex::default(),
            wake_limit: Mutex::default(),
            radio_wake_limit: Mutex::default(),
            mesh_watch: CancellationToken::new(),
            radio: Mutex::default(),
            gate: Mutex::default(),
            hooks_refused,
            suggesting: Mutex::default(),
        });
        *changed.lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::downgrade(&bridge));
        Ok(bridge)
    }

    pub fn session(&self) -> &MarvSession {
        &self.session
    }

    fn emit(&self, event: UiEvent) {
        self.activity.emit(event);
    }

    fn file(&self) -> Option<FileConfig> {
        self.file.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn config(&self) -> Config {
        with_model(resolve_config(self.file().as_ref(), &self.env), self.session_model().as_deref())
    }

    fn session_model(&self) -> Option<String> {
        self.model.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn cwd(&self) -> String {
        shorten_home(&self.root, &self.home)
    }

    pub fn status_info(&self) -> StatusInfo {
        let config = self.config();
        StatusInfo {
            model: self.session.provider_name(),
            cwd: self.cwd(),
            yolo: config.yolo && self.session_yolo_allowed(),
            context_length: self.session.context_length(),
            // The process manager calls the session's on_change when one starts or ends: the bar is asked again.
            processes: self.session.processes().iter().filter(|p| p.state == marv_tools::ProcessState::Running).count(),
        }
    }

    /// `/peers` (#74): the Herdr panes this session opened, one read, or one stopped. Names, tasks and pane text come
    /// from the model or another program: all of it goes through `printable()`.
    async fn peers(&self, read: Option<String>, stop: Option<String>) {
        if let Some(name) = stop {
            return self.emit(match self.session.stop_peer(&name).await {
                Ok((peer, stopped)) => system(
                    printable(&format!(
                        "{} {}.{}",
                        peer.name,
                        match stopped {
                            marv_mesh::herdr::Stopped::Closed => format!("stopped, and pane {} closed", peer.pane),
                            marv_mesh::herdr::Stopped::Gone => format!("forgotten: pane {} was already gone", peer.pane),
                        },
                        peer.branch
                            .map(|b| format!(" Its worktree (branch {b}) stays: remove it once it's merged."))
                            .unwrap_or_default()
                    )),
                    false,
                ),
                Err(e) => system(printable(&e), true),
            });
        }
        if let Some(name) = read {
            return self.emit(match self.session.read_peer(&name, 40).await {
                Ok(text) => system(printable(&format!("{name}'s last lines:\n{text}")), false),
                Err(e) => system(printable(&e), true),
            });
        }
        let (peers, states): (Vec<_>, Vec<_>) = self.session.peer_states().await.into_iter().unzip();
        self.emit(system(marv_session::herdr_door::peers_text(&peers, &states), false));
    }

    fn session_yolo_allowed(&self) -> bool {
        marv_session::sdk::no_yolo_reason(&self.root, &self.home).is_none()
    }

    pub fn welcome(&self) -> WelcomeInfo {
        let memories = self.session.memory().map_or(0, |m| m.personal.len() + m.project.len());
        WelcomeInfo {
            version: self.version.clone(),
            cwd: self.cwd(),
            instructions: self.instruction_sources.iter().filter(|s| !s.left_out()).map(|s| s.shown.clone()).collect(),
            skills: self.skills.len(),
            memories,
            mcp_servers: self.mcp.as_ref().map_or(0, |m| m.status().len()),
        }
    }

    /// The App's options: the welcome banner, the status bar, the splash.
    pub fn app_options(&self, splash_ms: u64) -> AppOptions {
        AppOptions { welcome: self.welcome(), status: self.status_info(), splash_ms, render: Default::default() }
    }

    /// Startup problems known at once (skills, PATH, AGENTS.md cut, MCP config, agents) as ONE notice, and setup on
    /// first run.
    pub fn announce(&self, cut: Option<&marv_session::prompt::InstructionsCut>) {
        let mut notes = Vec::new();
        if !self.skill_problems.is_empty() {
            let n = self.skill_problems.len();
            notes.push((
                NoticeTone::Error,
                NoticeItem {
                    text: format!("{} couldn't be loaded (see /skills)", counted(n, "skill", "skills")),
                    detail: Some(self.skill_problems.clone()),
                },
            ));
        }
        if !self.hidden_path_folders.is_empty() {
            notes.push((NoticeTone::Warning, hidden_path_folders_item(&self.hidden_path_folders)));
        }
        // #98: what's pre-approved and which extra folders are writable — so the user sees what Marv won't ask
        // about (and what a problem with those settings means).
        if !self.approval_problems.is_empty() {
            notes.push((
                NoticeTone::Warning,
                NoticeItem {
                    text: "A problem with your pre-approved scopes or writable folders".into(),
                    detail: Some(self.approval_problems.clone()),
                },
            ));
        }
        if !self.pre_approved.is_empty() {
            notes.push((
                NoticeTone::Info,
                NoticeItem {
                    text: format!("Pre-approved for this session: {}", self.pre_approved.join(", ")),
                    detail: Some(vec![
                        "They run without asking, like \"always\" at the start of the session.".into(),
                        "Protected paths, package.json lifecycle scripts, mesh wires and tainted egress still ask.".into(),
                    ]),
                },
            ));
        }
        if !self.writable.is_empty() {
            let shown: Vec<String> = self.writable.iter().map(|d| shorten_home(d, &self.home)).collect();
            notes.push((
                NoticeTone::Info,
                NoticeItem {
                    text: format!(
                        "Writable in the sandbox besides the project: {}",
                        shown.iter().map(|d| format!("`{d}`")).collect::<Vec<_>>().join(", ")
                    ),
                    detail: Some(vec!["Commands can write there without asking; nothing else outside the project.".into()]),
                },
            ));
        }
        if let Some(cut) = cut {
            let item = instructions_cut_item(cut);
            notes.push((NoticeTone::Info, NoticeItem { text: item.text, detail: item.detail }));
        }
        // Which instruction files the model has (#62), when it's more than the project's AGENTS.md.
        if let Some(item) = instruction_sources_item(&self.instruction_sources, &self.instruction_problems) {
            let tone = if self.instruction_problems.is_empty() { NoticeTone::Info } else { NoticeTone::Warning };
            notes.push((tone, NoticeItem { text: item.text, detail: item.detail }));
        }
        if !self.mcp_problems.is_empty() {
            let n = self.mcp_problems.len();
            notes.push((
                NoticeTone::Error,
                NoticeItem {
                    text: format!("{} (see /mcp)", counted(n, "MCP config problem", "MCP config problems")),
                    detail: Some(self.mcp_problems.clone()),
                },
            ));
        }
        if !self.agent_problems.is_empty() {
            let n = self.agent_problems.len();
            notes.push((
                NoticeTone::Error,
                NoticeItem {
                    text: format!("{} couldn't be loaded (see /agents)", counted(n, "agent file", "agent files")),
                    detail: Some(self.agent_problems.clone()),
                },
            ));
        }
        if let Some(refused) = &self.hooks_refused {
            notes.push((NoticeTone::Warning, NoticeItem { text: refused.clone(), detail: None }));
        }
        // A project's instructions the person hasn't trusted (a cloned repository's): quoted to the model as data.
        if let Some(text) = untrusted_notice(&self.project_trust, &self.project_files, "/trust") {
            let detail = self.project_files.files.keys().map(|f| printable(f)).collect();
            notes.push((NoticeTone::Warning, NoticeItem { text, detail: Some(detail) }));
        }
        if marv_session::config::tainted_at_start(&self.env) {
            notes.push((NoticeTone::Warning, NoticeItem { text: TAINTED_AT_START.into(), detail: None }));
        }
        if let Some(notice) = startup_notice(notes) {
            self.emit(notice_message(notice));
        }
        let file = self.file();
        let config = self.config();
        if needs_setup(file.as_ref(), &config) {
            self.emit(UiEvent::OpenSetup(setup_context(SetupStart::FirstRun, file.as_ref(), &config, &self.env)));
        }
    }

    /// The pane's session for Herdr (again after `/clear` or a resume, which change it).
    pub async fn report_session(&self) {
        if let Some(herdr) = &self.activity.herdr {
            let id = self.session.id();
            let path = self.sessions.as_ref().map(|s| s.file(&self.root, &id).to_string_lossy().into_owned());
            herdr.session(&id, path.as_deref()).await;
        }
    }

    /// Starts this conversation's session-start hooks if they haven't started (#108), and shows them once they've
    /// run. Called at startup and after `/clear` or a resume (which start a round of their own).
    pub fn show_hooks(self: &Arc<Self>) {
        self.session.run_start_hooks();
        let bridge = self.clone();
        tokio::spawn(async move {
            if let Some(notice) = bridge.session.start_hooks_done().await.as_deref().and_then(hooks_notice) {
                bridge.emit(notice_message(notice));
            }
        });
    }

    /// Checks for a newer Marv in the background (at most once a day, `crate::update`) and says so in one line. Never
    /// holds startup up; nothing is shown when it's off, up to date, or offline.
    pub fn show_update(self: &Arc<Self>) {
        let on = self.config().update_check;
        let env = self.env.clone();
        let bridge = self.clone();
        tokio::spawn(async move {
            if let Some(text) = crate::update::startup_notice(on, &env, &marv_session::paths::home_dir()).await {
                bridge.emit(system(text, false));
            }
        });
    }

    /// Herdr's first state: idle.
    pub fn report_state(&self) {
        self.activity.report();
    }

    /// `marv -c`: the latest session here, before the UI starts.
    pub async fn resume_latest(&self) {
        match self.session.resume(ResumeTarget::Latest).await {
            Ok(Some(resumed)) => self.show_resumed(resumed),
            Ok(None) => self.emit(system("No saved session to continue in this project.", false)),
            Err(e) => self.emit(system(format!("Couldn't load the last session: {e}"), true)),
        }
    }

    fn show_resumed(&self, resumed: marv_session::Resumed) {
        self.radio_title();
        self.emit(UiEvent::Resumed(view::Resumed {
            id: resumed.id,
            updated_at: resumed.updated_at,
            model: resumed.model,
            transcript: resumed.transcript.iter().filter_map(transcript_message).collect(),
            totals: totals(&resumed.totals),
            plan: resumed.plan.as_deref().map(view_plan),
        }));
        self.emit(UiEvent::StatusInfo(self.status_info()));
    }

    /// `/resume` and `marv -r`: the saved sessions of this project, newest first, without this one.
    pub async fn open_picker(&self) {
        let Some(sessions) = &self.sessions else { return };
        let current = self.session.id();
        let list = sessions
            .list(&self.root)
            .await
            .into_iter()
            .filter(|s| s.id != current)
            .map(|s| view::SessionSummary {
                id: s.id,
                title: s.title,
                updated_at: s.updated_at,
                messages: s.messages,
                model: s.model,
            })
            .collect();
        self.emit(UiEvent::Sessions(list));
    }

    /// The facts a command may read, then its action.
    fn command_action(&self, line: &str) -> CommandAction {
        let config = self.config();
        let usage = self.session.usage();
        let mcp_status = self.mcp.as_ref().map(|m| m.status()).unwrap_or_default();
        // The project's checks for /checks, read when it runs (a check can be added at any time); other commands skip
        // the disk.
        let wants_checks = line.trim().strip_prefix("/checks").is_some_and(|r| r.is_empty() || r.starts_with(' '));
        let (checks, check_problems) =
            if wants_checks { marv_session::checks::load_checks(&self.root) } else { (Vec::new(), Vec::new()) };
        let last_checks = self.last_checks.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let trajectories_path = self.trajectories_dir.as_ref().map(|d| shorten_home(d, &self.home));
        let processes = self.session.processes();
        let checkpoint_problem = self.session.checkpoint_problem();
        let permissions = self.session.permissions();
        let wants_diagnostics = line.trim().strip_prefix("/diagnostics").is_some_and(|r| r.trim().is_empty());
        let diagnostics = wants_diagnostics.then(|| self.session.diagnostics_info());
        let mesh = self.session.mesh().map(|mesh| {
            let inbox = marv_mesh::ReceptacleName::inbox(&marv_session::mesh::parent_id());
            commands::MeshStatus {
                name: self.mesh_name.lock().unwrap_or_else(|e| e.into_inner()).clone(),
                wired: mesh
                    .table()
                    .all()
                    .iter()
                    .filter(|w| w.to == inbox && w.mode == marv_mesh::WireMode::Send)
                    .map(|w| format!("{} -> {}", w.from, w.to))
                    .collect(),
            }
        });
        let ctx = CommandContext {
            config: &config,
            config_path: shorten_home(&self.store.path, &self.home),
            skills: &self.skills,
            agents: &self.agents,
            agent_problems: &self.agent_problems,
            skill_problems: &self.skill_problems,
            mcp: Some((&mcp_status, &self.mcp_problems)),
            trajectories_path,
            usage: Some((&usage.totals, usage.last.as_ref(), usage.context_length)),
            checks: Some((&checks, &check_problems, last_checks.as_ref())),
            checkpoints: Some(&checkpoint_problem),
            hidden_path_folders: &self.hidden_path_folders,
            sandbox_available: self.sandbox_available,
            processes: self.session.processes_offered().then_some(&processes[..]),
            mesh: mesh.as_ref(),
            permissions: Some(&permissions),
            diagnostics: diagnostics.as_ref(),
        };
        commands::run_command(line, &ctx)
    }
}

/// What `/diagnostics clear` (#187) says when it's done.
fn cleared_notice(cleared: Result<Option<u64>, String>) -> UiEvent {
    let (text, is_error) = match cleared {
        Ok(Some(bytes)) => {
            (format!("Deleted this project's check cache ({}).", marv_session::diagnostics::cache::human_size(bytes)), false)
        }
        Ok(None) => ("This project has no check cache.".to_string(), false),
        Err(e) => (format!("Couldn't delete this project's check cache: {}", printable(&e)), true),
    };
    system(text, is_error)
}

/// The palette's settings (#44) from the settings table, with their values in `config`.
pub fn setting_infos(config: &Config) -> Vec<SettingInfo> {
    commands::settings()
        .iter()
        .map(|s| SettingInfo {
            name: s.name.into(),
            value: s.value(config),
            description: s.description().into(),
            action: match s.choice(config) {
                commands::SettingChoice::Run(line) => SettingAction::Run(line),
                commands::SettingChoice::Prefill(text) => SettingAction::Prefill(text),
            },
            while_busy: s.while_busy(),
        })
        .collect()
}

/// The handle the App holds: every method returns at once; results come back as events.
pub struct Port(pub Arc<Bridge>);

impl SessionPort for Port {
    fn commands(&self) -> Vec<CommandInfo> {
        let b = &self.0;
        // The `/` menu and the ctrl+k palette are both built from this list (#44).
        let mut menu: Vec<CommandInfo> = commands::commands()
            .iter()
            .map(|c| CommandInfo { while_busy: c.while_busy, ..CommandInfo::new(c.name, c.description) })
            .chain(b.skills.iter().map(|s| CommandInfo {
                kind: CommandKind::Skill,
                ..CommandInfo::new(s.skill.name.clone(), s.skill.description.clone())
            }))
            .collect();
        // Server names after "/mcp login " and "/mcp logout ".
        for name in b.mcp.as_ref().map(|m| m.loginable()).unwrap_or_default() {
            menu.push(CommandInfo::new(format!("mcp login {name}"), format!("Log in to {name} (OAuth, in your browser)")));
            menu.push(CommandInfo::new(format!("mcp logout {name}"), format!("Forget {name}'s login")));
        }
        menu
    }

    fn settings(&self) -> Vec<SettingInfo> {
        setting_infos(&self.0.config())
    }

    fn send(&self, text: String) {
        self.0.run_turn(text, None);
    }

    /// "Send now" (Alt+Enter): into the running turn, at its next step. No turn to take it (it just ended): queued,
    /// as Enter would have (#184).
    fn send_now(&self, text: String) {
        self.0.radio_wake_limit.lock().unwrap_or_else(|e| e.into_inner()).person_typed();
        if self.0.session.send_now(&text).is_err() {
            self.0.queue(text);
        }
    }

    fn queue(&self, text: String) {
        self.0.queue(text);
    }

    fn unqueue_last(&self) -> Option<String> {
        self.0.unqueue(true)
    }

    fn take_queued(&self) -> Option<String> {
        self.0.unqueue(false)
    }

    fn run_command(&self, line: String) {
        self.0.stop_suggesting();
        let bridge = self.0.clone();
        let action = bridge.command_action(&line);
        tokio::spawn(async move { bridge.apply(&line, action).await });
    }

    fn answer(&self, id: ApprovalId, d: view::Decision) {
        self.0.activity.answer(id.0, decision(d));
    }

    fn stop_suggesting(&self) {
        self.0.stop_suggesting();
    }

    fn abort(&self) {
        self.0.stop_suggesting();
        self.0.session.interrupt();
        if let Some(work) = self.0.busy_work.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            work.cancel();
        }
    }

    fn is_login_callback(&self, text: &str) -> bool {
        marv_mcp::oauth::is_callback(text)
    }

    fn keyboard_held(&self, held: bool) {
        self.0.activity.set_held(held);
    }

    /// The processes view (ctrl+p): every process, and the selected one's last lines of output.
    fn load_processes(&self, output_of: Option<String>) {
        let b = &self.0;
        let now = marv_tools::process::now_ms();
        let list = b.session.processes();
        let rows = list
            .iter()
            .map(|p| view::ProcessRow {
                name: p.name.clone(),
                command: p.command.clone(),
                status: marv_tools::process::process_status(p, now),
                running: p.state == marv_tools::ProcessState::Running,
            })
            .collect::<Vec<_>>();
        let name = output_of.or_else(|| rows.first().map(|r| r.name.clone()));
        let output = name.and_then(|n| {
            let text = b.session.process_logs(&n, marv_tui::ui::process_panel::OUTPUT_ROWS).ok()?;
            Some((n, text))
        });
        b.emit(UiEvent::Processes(view::ProcessesView { rows, output }));
        // The count on the status bar follows a process that exited on its own.
        b.emit(UiEvent::StatusInfo(b.status_info()));
    }

    /// The processes view's stop: what `/processes stop <name>` does (its message, the status bar's count).
    fn stop_process(&self, name: String) {
        self.run_command(format!("/processes stop {name}"));
    }

    fn login_callback(&self, text: &str) -> bool {
        if !marv_mcp::oauth::is_callback(text) {
            return false;
        }
        self.0.paste_callback(text.trim());
        true
    }

    fn load_models(&self, provider: ProviderId) {
        let b = &self.0;
        let kind = provider_kind(&provider);
        // #278: the GGUF files in the usual places; any other path can be typed.
        if provider == ProviderId::LlamaCpp {
            let files = marv_session::local_model::gguf_files(&b.env);
            let models = files
                .into_iter()
                .map(|id| marv_provider::ModelInfo { id, local: true, tools: Some(true), ..Default::default() })
                .map(model_info)
                .collect();
            let _ = b.activity.tx.send(UiEvent::Models(Ok(models)));
            return;
        }
        // #275: a custom provider's list, from its endpoint with its key and headers (or the config's list).
        if let ProviderId::Custom(name) = &provider {
            let entry = b.file().and_then(|f| f.providers?.get(name).cloned());
            let endpoint = entry.map(|entry| marv_session::providers::endpoint(name, &entry, &b.env));
            let tx = b.activity.tx.clone();
            tokio::spawn(async move {
                let result = match endpoint {
                    Some(endpoint) => marv_provider::endpoint::list_models(&reqwest::Client::new(), &endpoint).await,
                    None => Err("It isn't in ~/.marv/config.json's \"providers\" any more.".into()),
                };
                let _ = tx.send(UiEvent::Models(result.map(|list| list.into_iter().map(model_info).collect())));
            });
            return;
        }
        // A custom endpoint (baseUrl) only applies to the provider it was saved with.
        let file = b.file();
        let base_url = file.as_ref().filter(|f| f.provider == Some(kind)).and_then(|f| f.base_url.clone());
        let config = resolve_config(Some(&FileConfig { provider: Some(kind), base_url, ..FileConfig::default() }), &b.env);
        let models = if kind == ProviderKind::ChatGpt {
            // #159: the signed-in account's models, with its token.
            let store = auth_store(&config, &b.config_dir);
            Box::pin(async move {
                marv_provider::chatgpt::list_models(&marv_provider::chatgpt::ChatGptCredentials {
                    store,
                    endpoints: Default::default(),
                })
                .await
            })
        } else {
            (b.load_models)(kind, config.base_url)
        };
        let tx = b.activity.tx.clone();
        tokio::spawn(async move {
            let result = models.await.map(|list| list.into_iter().map(model_info).collect());
            let _ = tx.send(UiEvent::Models(result));
        });
    }

    fn complete_setup(&self, answer: SetupAnswer) {
        let b = &self.0;
        let next = setup_file(b.file().as_ref(), &answer);
        // The picker saves the default: a session's own model (`-m`, `/model <id>`) gives way to it.
        *b.model.lock().unwrap_or_else(|e| e.into_inner()) = None;
        let label = provider_label(&answer.provider);
        b.save_config(next, format!("Saved to {} · {label} · {}", shorten_home(&b.store.path, &b.home), answer.model));
    }

    fn sign_in(&self, _provider: ProviderId, target: view::SignInTarget) {
        let b = self.0.clone();
        let cancel = CancellationToken::new();
        if let Some(old) = b.sign_in.lock().unwrap_or_else(|e| e.into_inner()).replace(cancel.clone()) {
            old.cancel();
        }
        let store = auth_store(&b.config(), &b.config_dir);
        tokio::spawn(async move {
            let target = match target {
                view::SignInTarget::Current => marv_provider::chatgpt::SignInTarget::Current,
                view::SignInTarget::Account(id) => marv_provider::chatgpt::SignInTarget::Account(id),
                view::SignInTarget::New => marv_provider::chatgpt::SignInTarget::New,
            };
            let done = match marv_provider::chatgpt::start_sign_in(store, Default::default(), target).await {
                Ok(sign_in) => {
                    // Shown without the id_token_hint (a token); the browser gets the whole address.
                    b.emit(UiEvent::SignIn(view::SignInProgress::Started { url: sign_in.shown_url().to_string() }));
                    if let Ok(url) = sign_in.url().parse() {
                        (b.open_url)(url);
                    }
                    sign_in.finish(cancel).await.map(|a| a.email.unwrap_or_else(|| "your ChatGPT account".into()))
                }
                Err(why) => Err(why),
            };
            b.emit(UiEvent::SignIn(view::SignInProgress::Done(done)));
        });
    }

    fn use_account(&self, _provider: ProviderId, id: String) {
        let b = self.0.clone();
        let store = auth_store(&b.config(), &b.config_dir);
        tokio::spawn(async move {
            let done = store.activate(&id).await.map(|a| a.label());
            b.emit(UiEvent::SignIn(view::SignInProgress::Done(done)));
        });
    }

    fn cancel_sign_in(&self) {
        if let Some(cancel) = self.0.sign_in.lock().unwrap_or_else(|e| e.into_inner()).take() {
            cancel.cancel();
        }
    }

    fn sign_out(&self, _provider: ProviderId) {
        let b = self.0.clone();
        let store = auth_store(&b.config(), &b.config_dir);
        tokio::spawn(async move {
            let text = match marv_provider::chatgpt::sign_out(&store, &Default::default()).await {
                Ok(true) => "Signed out of ChatGPT.".to_string(),
                Ok(false) => {
                    "Signed out of ChatGPT here; OpenAI didn't confirm it, so you can also disconnect Marv in ChatGPT's settings."
                        .into()
                }
                Err(why) => format!("Couldn't sign out of ChatGPT: {why}"),
            };
            b.emit(system(text, false));
        });
    }

    fn resume(&self, id: Option<String>) {
        let bridge = self.0.clone();
        tokio::spawn(async move {
            // Never into a running turn: its reply would land in the other conversation, and be saved there.
            if bridge.activity.busy.load(Ordering::SeqCst) || bridge.session.busy() {
                return bridge.emit(system(WORKING, true));
            }
            let target = id.map_or(ResumeTarget::Latest, ResumeTarget::Id);
            match bridge.session.resume(target).await {
                Ok(Some(resumed)) => {
                    bridge.show_resumed(resumed);
                    bridge.show_hooks();
                    bridge.report_session().await;
                }
                Ok(None) => bridge.emit(system("That session couldn't be loaded.", true)),
                Err(e) if bridge.session.busy() => bridge.emit(system(format!("{WORKING} ({e})"), true)),
                Err(e) => bridge.emit(system(format!("That session couldn't be loaded: {e}."), true)),
            }
        });
    }

    fn flush(&self) -> BoxFuture<'static, ()> {
        // The App is on its way out: nothing new may start (a mesh wake, queued input).
        self.0.close_mesh();
        let session = self.0.session.clone();
        Box::pin(async move { session.flush().await })
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// Startup
// ---------------------------------------------------------------------------------------------------------------------

/// Runs the interactive session. `resume`: `Some(true)` for -c (latest), `Some(false)` for -r (pick).
/// Pre-approved scopes (#98): the config's "approve" and this run's --approve, merged and deduped, seeded as
/// "always" for the session through the session's own path (so must_ask and taint still win; a subagent inherits
/// neither). An unknown name is a problem for the startup notice, never a silent approval.
/// The config's names are checked like --approve's (#82): an unknown one is dropped with a problem, the rest kept.
pub fn merge_pre_approved(from_config: Vec<String>, from_cli: &[String]) -> (Vec<String>, Vec<String>) {
    let (mut names, mut problems) = marv_session::approvals::known_names(&from_config, "\"approve\" in your config");
    let (cli, cli_problems) = marv_session::approvals::known_names(from_cli, "--approve");
    problems.extend(cli_problems);
    for name in cli {
        if !names.contains(&name) {
            names.push(name);
        }
    }
    (names, problems)
}

/// Extra writable sandbox folders (#98): resolved once here (refused entries are problems, never mounts).
pub fn resolve_writable(config: &[String], home: &Path, root: &Path, config_dir: &Path) -> (Vec<PathBuf>, Vec<String>) {
    let runtime = marv_sandbox::runtime_paths(&marv_sandbox::runtime_env());
    marv_sandbox::resolve_writable(config, home, root, &runtime, Some(config_dir))
}

pub async fn run(
    resume: Option<bool>,
    approve: Vec<String>,
    model: Option<String>,
    local_model: Option<String>,
    quit: crate::signals::Quit,
) -> anyhow::Result<i32> {
    // The real environment only (docs/SECURITY.md): never a project's .env.
    let mut env: Env = std::env::vars().collect();
    // #278: `--local-model`: llama.cpp with that file for this run (the config file isn't changed).
    // It picks the model too, so `-m` gives way to it.
    let model = if local_model.is_some() { None } else { model };
    if let Some(model) = local_model {
        env.insert("MARV_LOCAL_MODEL".into(), model);
    }
    let home = home_dir();
    // The project is wherever Marv was started; the tools can't reach outside it.
    let cwd = std::env::current_dir()?;
    let root = std::fs::canonicalize(&cwd).unwrap_or(cwd);
    let config_dir = default_config_dir(&env, &home);
    // The project's instructions, skills and agent types are the person's only once they trusted exactly these files
    // here (docs/SECURITY.md): a cloned repository's are quoted as its data, and its skills and agents aren't loaded.
    // Each file is read once and hashed as read (`load_project`).
    let read = load_project(&root, &home, &config_dir, true, true, None);
    let (project_files, project_trust, nested_trust) = (read.files, read.status, read.nested);
    let instructions = read.instructions;
    let instruction_problems = read.instruction_problems;
    let (skills, skill_problems) = (read.skills, read.skill_problems);
    let (agents, agent_problems) = (read.agents, read.agent_problems);
    // MCP servers start now, in the background; the first request waits for them.
    let (mcp, mcp_problems) = start_mcp(&root, &home, &config_dir, &env);
    let paths = memory_paths(&config_dir, &root);
    let memories = load_memory(&paths).await.unwrap_or_default();

    // Marv was called Ekko: carry its config over (only for the default location).
    if !env.contains_key("MARV_CONFIG_DIR") {
        let _ = migrate_legacy_config(&home);
    }
    let store = ConfigStore::new(&config_dir);
    let file = match store.load() {
        Ok(file) => file,
        Err(e) => {
            eprintln!("{}\n\nFix the file, or delete it to run setup again.", printable(&e.to_string()));
            return Ok(1);
        }
    };
    let config = with_model(resolve_config(file.as_ref(), &env), model.as_deref());
    // Pre-approved scopes (#98) and the writable folders, resolved once (refused ones are problems, not mounts).
    let (pre_approved, mut approval_problems) = merge_pre_approved(config.approve.clone(), &approve);
    let (writable, writable_problems) = resolve_writable(&config.writable, &home, &root, &config_dir);
    approval_problems.extend(writable_problems);
    // Where a door's program (gh, herdr) is never taken from: the project and the sandbox's writable folders.
    let door_outside: Vec<PathBuf> = std::iter::once(root.clone()).chain(writable.iter().cloned()).collect();
    // Whether to offer the github tool (#47): asked of gh once, alongside the rest of startup, since the tool list is
    // fixed for the session. At most 2 s (gh auth status asks GitHub; offline it falls back to gh's hosts.yml). gh is
    // never taken from the project or a sandbox-writable folder (only an absolute PATH entry outside them).
    let github_check = {
        let wanted = marv_tools::github::github_wanted(env.get("MARV_GITHUB").map(String::as_str));
        let pairs: Vec<(String, String)> = env.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        let outside = door_outside.clone();
        tokio::spawn(async move {
            wanted && marv_tools::github::github_available(&pairs, &outside, marv_tools::github::AUTH_CHECK).await
        })
    };
    let hidden = marv_sandbox::home::hidden_path_folders(
        env.get("PATH").map(String::as_str).unwrap_or_default(),
        &home,
        Some(&root),
        &|p| p.is_dir(),
    )
    .iter()
    .map(|dir| shorten_home(dir, &home))
    .collect();
    // In a Herdr pane, Herdr shows what Marv is doing.
    let herdr = herdr_target(&env).map(|target| HerdrReporter::new(target, None));
    // The mesh (v2): this Marv's switchboard, put on the mesh once the TUI can ask for wires (below).
    let (mut mesh, mut mesh_note) = match crate::mesh_cmd::start(&env, &root, &door_outside) {
        // A MARV_RUNTIME_DIR that was refused says why (a symlink on the way, say), rather than the mesh just being off.
        Ok(None) => {
            (None, marv_sandbox::override_refusal_for(&env, Some(&root)).map(|why| (format!("Not on the mesh: {why}"), true)))
        }
        Ok(mesh) => (mesh, None),
        Err(e) => (None, Some((format!("Not on the mesh: {e}"), true))),
    };

    let cut = instructions.as_ref().and_then(|i| i.cut.clone());
    let github = github_check.await.unwrap_or(false);
    let mcp_deps: McpDeps = {
        let env = env.clone();
        Arc::new(move |cancel| Arc::new(marv_session::mcp_install::LiveDeps::new(env.clone(), cancel)) as Arc<dyn ResolveDeps>)
    };
    let open_url: Authorize = {
        let env = env.clone();
        Arc::new(move |url| {
            if marv_mcp::oauth::can_open_browser(&env, std::env::consts::OS) {
                marv_mcp::oauth::open_browser(&url, std::env::consts::OS);
            }
        })
    };
    let (tx, rx) = unbounded_channel();
    let startup = Startup {
        root: root.clone(),
        home: home.clone(),
        config_dir: config_dir.clone(),
        store,
        file,
        env,
        version: env!("CARGO_PKG_VERSION").into(),
        instructions,
        instruction_problems,
        skills,
        skill_problems,
        agents,
        agent_problems,
        project_files,
        project_trust,
        nested_trust,
        memory: Some((paths, memories)),
        mcp: Some(mcp.clone()),
        mcp_problems,
        hidden_path_folders: hidden,
        sandbox_available: marv_sandbox::backend().available(),
        pre_approved,
        writable,
        approval_problems,
        github,
        sessions: Some(Arc::new(SessionStore::new(config_dir.join("sessions")))),
        trajectories: Some(Arc::new(TrajectoryStore::new(config_dir.join("trajectories")))),
        herdr: herdr.clone(),
        mesh: mesh.as_ref().map(|m| m.handle.clone()),
        make_provider: Arc::new(|config, info| marv_provider::create_provider(config, info, None)),
        load_models: Arc::new(|kind, base_url| {
            Box::pin(async move { marv_provider::models::list_models(kind, &base_url, None).await })
        }),
        model,
        mcp_deps,
        open_url,
    };
    let notes = tx.clone();
    let bridge = Bridge::start(startup, tx)?;
    bridge.announce(cut.as_ref());
    bridge.watch_mcp();
    // With `mesh.wake` on (#78), a message on a wired inbox starts a turn while Marv is idle.
    bridge.watch_mesh();
    // Wires other processes ask for (`marv mesh wire`, another Marv) wait in the same approval queue, for a person.
    if let Some(mesh) = &mut mesh {
        let person = marv_session::mesh::PersonWires {
            inner: Arc::new(TuiApprover(bridge.activity.clone())),
            by: marv_mesh::wire::Approver::Tui,
        };
        mesh_note = Some(match mesh.open_door(Arc::new(person)) {
            Ok(()) => {
                bridge.set_mesh_name(&mesh.name);
                // The radio (#124): there whenever the door is, on only if the user's own config (or /radio on) says.
                bridge.set_radio(mesh.start_radio(bridge.config().mesh_wake));
                (
                    format!(
                        "On the mesh as {} (marv mesh ls; others reach you at peer.{}.agent.parent.inbox).",
                        mesh.name, mesh.name
                    ),
                    false,
                )
            }
            Err(e) => (format!("Not on the mesh: {e}"), true),
        });
    }
    if let Some((text, is_error)) = mesh_note {
        let _ = notes.send(system(printable(&text), is_error));
    }
    match resume {
        // Before the UI, so nothing typed lands in the conversation being swapped out.
        Some(true) => bridge.resume_latest().await,
        Some(false) => bridge.open_picker().await,
        None => {}
    }
    // After `-c`'s resume, which starts its own round: the hooks run once here, not twice.
    bridge.show_hooks();
    bridge.show_update();
    bridge.report_state();
    bridge.report_session().await;

    // The tab would otherwise keep the shell's title: push it, set ours (OSC 0), and pop it back on the way out.
    let tty = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    if tty {
        let _ = write!(std::io::stdout(), "\x1b[22;0t\x1b]0;marv\x07");
        let _ = std::io::stdout().flush();
    }
    let options = bridge.app_options(1200);
    // The App restores the terminal, then awaits `flush()` (the last turn and pending trajectory records).
    // SIGHUP (the terminal closed) or SIGTERM: out as on /exit (#196), and on through the rest of this way out.
    let result = if quit.signal().is_some() {
        // It came during startup: no UI just to leave it (the terminal may be gone), but the session is saved.
        let _ = tokio::time::timeout(marv_tui::app::QUIT_FLUSH_TIMEOUT, Port(bridge.clone()).flush()).await;
        Ok(())
    } else {
        let on_quit = quit.clone();
        let app =
            App::new(Arc::new(Port(bridge.clone())), rx, options).with_quit(Box::pin(async move { on_quit.arrived().await }));
        app.run().await
    };
    if tty {
        let pop = || {
            let _ = write!(std::io::stdout(), "\x1b[23;0t");
            let _ = std::io::stdout().flush();
        };
        // A terminal that's gone fails the write (ignored); one that stopped reading would block it (left behind).
        if quit.signal().is_some() {
            marv_tui::render_options::within(marv_tui::app::QUIT_RESTORE_TIMEOUT, pop);
        } else {
            pop();
        }
    }
    // No wake-up from here on (the App's flush did this already, unless it failed first), then stop a turn still
    // running: its tools mustn't outlive the UI.
    bridge.close_mesh();
    bridge.session().interrupt();
    // Background processes stop with Marv (#51): SIGTERM, then SIGKILL after 2 s; their sandboxes also die with Marv
    // if it's killed harder (bwrap's --die-with-parent).
    let _ = tokio::time::timeout(PROCESSES_STOP_TIMEOUT, bridge.session().stop_processes()).await;
    // Let servers shut down cleanly, but don't hang on one that won't.
    {
        let _ = tokio::time::timeout(MCP_CLOSE_TIMEOUT, tokio::spawn(async move { mcp.close().await })).await;
    }
    // Herdr stops showing an agent in this pane; no waiting on a Herdr that doesn't answer.
    if let Some(herdr) = herdr {
        let _ = tokio::time::timeout(HERDR_RELEASE_TIMEOUT, herdr.release()).await;
    }
    // Off the radio with a goodbye (bounded), then off the mesh: the door's socket goes, and the plugs' with the
    // switchboard.
    bridge.leave_radio().await;
    drop(mesh);
    result?;
    Ok(0)
}

mod mcp_servers;
mod radio_mesh;
mod settings;
mod turns;

#[cfg(test)]
mod tests;
