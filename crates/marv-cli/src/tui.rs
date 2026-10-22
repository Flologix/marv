//! Launching the TUI: the interactive half of Marv's `src/cli.tsx`, the engine half of `src/app.tsx` (commands,
//! config, setup, resume, startup notices), and the App's Herdr reporting (`src/herdr.ts` is marv-mesh's `herdr.rs`).
//!
//! Startup, in Marv's order: the project root is the current folder; read AGENTS.md once (#62: or CLAUDE.md, plus the
//! parents' and ~/.marv's, cut at 20k in all; the cut and the list of files go to the startup notice); load skills and agents (project + personal; problems shown at startup); load the MCP config and
//! start the manager in the background; memory; migrate `~/.ekko` (default location only); load the config file (a
//! broken one exits 1 with "Fix the file, or delete it to run setup again."); hidden PATH folders for the startup
//! notice; Herdr reporting in a Herdr pane; then the UI. On the way out: `session.flush()` (the App awaits it after
//! restoring the terminal), MCP close (≤ 2 s), Herdr release (≤ 1 s).
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
    Config, ConfigStore, Env, FileConfig, MaxStepsSetting, RadioSetting, default_config_dir, mask_key, migrate_legacy_config,
    needs_setup, preset, resolve_config,
};
use marv_session::diagnostics::check_notice;
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
        CompactResult::Compacted { summary, tokens_before } => view::CompactOutcome::Compacted { summary, tokens_before },
        CompactResult::NotCompacted { reason, error } => view::CompactOutcome::NotCompacted {
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
        // #96: the error right after it says what happened.
        L::Discarded { .. } => return None,
        L::TextDelta(text) => UiEvent::TextDelta(text.clone()),
        L::ThinkingDelta(text) => UiEvent::ThinkingDelta(text.clone()),
        L::Usage(usage, _) => UiEvent::Usage { last: *usage, totals: totals() },
        L::Assistant { text } => UiEvent::Assistant { text: text.clone() },
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
        SessionEvent::Check(result) => check_notice(&result).map(|text| system(text, false)).into_iter().collect(),
        SessionEvent::Checks(result) => {
            let notice = project_check_notice(&result);
            map.last_checks = Some(result);
            notice.map(|text| system(text, false)).into_iter().collect()
        }
        SessionEvent::Tainted { source } => vec![system(tainted_text(source), false)],
        // #124: Marv's line about a radio report it sent (checked names and fixed words; printable anyway).
        SessionEvent::RadioNote { text } => vec![system(printable(&format!("✻ {text}")), false)],
        SessionEvent::CheckpointFailed { message } => {
            vec![system(format!("This turn can't be undone: its checkpoint failed ({message})."), true)]
        }
        SessionEvent::TurnEnd { turn, reason } => vec![UiEvent::TurnEnd {
            turn,
            reason: match reason {
                TurnEndReason::Done(r) => view::TurnEndReason::Loop(r),
                TurnEndReason::Interrupted => view::TurnEndReason::Interrupted,
            },
        }],
    }
}

fn provider_id(kind: ProviderKind) -> ProviderId {
    match kind {
        ProviderKind::OpenRouter => ProviderId::OpenRouter,
        ProviderKind::Ollama => ProviderId::Ollama,
        ProviderKind::Zai => ProviderId::Zai,
        ProviderKind::ChatGpt => ProviderId::ChatGpt,
    }
}

fn provider_kind(id: ProviderId) -> ProviderKind {
    match id {
        ProviderId::OpenRouter => ProviderKind::OpenRouter,
        ProviderId::Ollama => ProviderKind::Ollama,
        ProviderId::Zai => ProviderKind::Zai,
        ProviderId::ChatGpt => ProviderKind::ChatGpt,
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

/// The providers setup offers: OpenRouter and Ollama, and (#159) the Z.ai Coding Plan and Sign in with ChatGPT when
/// their switch is on (`marv_provider::offered`) or the config already uses them.
fn offered_kinds(current: Option<ProviderKind>) -> Vec<ProviderKind> {
    let plans = [
        (ProviderKind::Zai, marv_provider::offered::ZAI_CODING_PLAN),
        (ProviderKind::ChatGpt, marv_provider::offered::CHATGPT_SIGN_IN),
    ];
    let mut kinds = vec![ProviderKind::OpenRouter];
    kinds.extend(plans.into_iter().filter(|&(kind, on)| on || current == Some(kind)).map(|(kind, _)| kind));
    kinds.push(ProviderKind::Ollama);
    kinds
}

/// #159: the ChatGPT sign-in's file: the config's (the provider reads that one).
fn auth_store(config: &Config, config_dir: &Path) -> marv_provider::chatgpt::AuthStore {
    let path = config.auth_file.clone().unwrap_or_else(|| config_dir.join(marv_provider::chatgpt::AUTH_FILE));
    marv_provider::chatgpt::AuthStore { path }
}

/// What the setup screen starts from. Keys go to the UI masked.
pub fn setup_context(start: SetupStart, file: Option<&FileConfig>, config: &Config, env: &Env) -> SetupContext {
    let kinds = offered_kinds(file.and_then(|f| f.provider));
    let presets = kinds
        .iter()
        .map(|&kind| {
            let p = preset(kind);
            view::ProviderPreset {
                id: provider_id(kind),
                label: p.label.into(),
                key_env: p.key_env.map(Into::into),
                key_url: p.key_url.map(Into::into),
                default_model: p.default_model.into(),
                sign_in: kind == ProviderKind::ChatGpt,
            }
        })
        .collect();
    let env_keys = kinds
        .iter()
        .filter_map(|&kind| {
            let value = env.get(preset(kind).key_env?)?.trim();
            (!value.is_empty()).then(|| (provider_id(kind), mask_key(value)))
        })
        .collect();
    SetupContext {
        start,
        presets,
        current: file.and_then(|f| f.provider).map(|kind| (provider_id(kind), config.model.clone())),
        saved_keys: kinds.iter().filter_map(|&kind| Some((provider_id(kind), mask_key(file?.key_for(kind)?)))).collect(),
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
    let kind = provider_kind(answer.provider);
    let mut next = file.cloned().unwrap_or_default();
    if next.provider != Some(kind) {
        next.base_url = None;
    }
    next.provider = Some(kind);
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

    fn forget(&self, id: u64) {
        self.asks.lock().unwrap_or_else(|e| e.into_inner()).retain(|a| a.id != id);
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
    /// Input submitted while a turn ran (the App hadn't seen it start, or had just seen it end): run in order next.
    queued: std::collections::VecDeque<(String, Option<String>)>,
    /// The App has left: no wake-ups, no queued input.
    closing: bool,
}

impl Drop for Bridge {
    fn drop(&mut self) {
        self.mesh_watch.cancel();
    }
}

fn factory(config: &Config, make: &MakeProvider, load: &LoadModels) -> ProviderChoice {
    ProviderChoice::Factory(Arc::new(ConfigFactory { config: config.provider_config(), make: make.clone(), load: load.clone() }))
}

impl Bridge {
    /// Makes the session from what startup found; events go to `tx`.
    pub fn start(s: Startup, tx: UnboundedSender<UiEvent>) -> anyhow::Result<Arc<Self>> {
        let activity = Activity::new(tx.clone(), s.herdr);
        let config = resolve_config(s.file.as_ref(), &s.env);
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
        init.formatter = config.formatter;
        init.compact_tokens = config.compact_tokens;
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
        resolve_config(self.file().as_ref(), &self.env)
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

    /// Runs one turn's events through the mapping, on its own task.
    fn run_turn(self: &Arc<Self>, text: String, for_model: Option<String>) {
        // The person typed: radio wakes paused by the overall limit may wake again (#124).
        self.radio_wake_limit.lock().unwrap_or_else(|e| e.into_inner()).person_typed();
        {
            let mut gate = self.gate.lock().unwrap_or_else(|e| e.into_inner());
            if gate.closing {
                return;
            }
            // A turn is running that the App didn't know of when this was typed (a wake-up whose start hadn't reached
            // it, or the end just shown while the next one is being handed over): this goes next, never lost.
            if gate.turn {
                gate.queued.push_back((text, for_model));
                if gate.woken {
                    self.emit(system("A mesh message woke Marv just now: yours goes next, when that turn ends.", false));
                }
                return;
            }
            gate.turn = true;
            // Working from before the turn's task starts, so Herdr hears it before anything the turn asks.
            self.activity.set_busy(true);
        }
        self.start_claimed(text, for_model);
    }

    /// Sends a message on a gate already taken for it.
    fn start_claimed(self: &Arc<Self>, text: String, for_model: Option<String>) {
        self.stop_suggesting();
        let turn = match self.session.send(&text, SendOptions { for_model, cancel: None }) {
            Ok(turn) => {
                self.radio_title();
                turn
            }
            Err(e) => {
                // The App started its turn on submit: end it (this one, the rejected turn: no other turn's end).
                self.emit(system(e.to_string(), true));
                self.emit(UiEvent::TurnEnd { turn: String::new(), reason: view::TurnEndReason::Loop(StopReason::Error) });
                self.finished(false);
                return;
            }
        };
        self.forward(turn);
    }

    /// The gate's turn is over: the next queued input starts on the same gate (nothing can slip in between), or the
    /// bridge is free. `done`: the turn's reply finished normally, so once idle the next message may be suggested.
    fn finished(self: &Arc<Self>, done: bool) {
        let next = {
            let mut gate = self.gate.lock().unwrap_or_else(|e| e.into_inner());
            gate.woken = false;
            match gate.queued.pop_front().filter(|_| !gate.closing) {
                Some(next) => Some(next),
                None => {
                    gate.turn = false;
                    self.activity.set_busy(false);
                    None
                }
            }
        };
        match next {
            Some((text, for_model)) => self.start_claimed(text, for_model),
            // Idle again: a message that rang the mail bell while busy, after the turn's last inbox read (its final
            // reply runs no `after_step`), would otherwise wait for some later message. `wake` checks everything
            // again (on, closing, approvals, the rate limit, which also bounds a wake that keeps failing).
            None => {
                // Only after a reply that finished: not after an error, a stop or the step limit.
                if done {
                    self.suggest_next();
                }
                let me = self.clone();
                tokio::spawn(async move { me.wake() });
            }
        }
    }

    /// A running turn's events through the mapping, on its own task; not busy once it ends.
    fn forward(self: &Arc<Self>, turn: marv_session::Turn) {
        let me = self.clone();
        tokio::spawn(async move {
            let mut turn = turn;
            let mut map = TurnMap::default();
            let usage_of = me.session.clone();
            let totals_fn = move || totals(&usage_of.usage().totals);
            let last_of = me.session.clone();
            let last_fn = move || last_of.usage().last;
            let mut finished = false;
            while let Some(event) = turn.next().await {
                let end = matches!(event, SessionEvent::TurnEnd { .. });
                finished |= matches!(event, SessionEvent::TurnEnd { reason: TurnEndReason::Done(StopReason::End), .. });
                for ui in map_event(event, &mut map, &totals_fn, &last_fn) {
                    me.emit(ui);
                }
                if end {
                    break;
                }
            }
            if let Some(result) = map.last_checks {
                *me.last_checks.lock().unwrap_or_else(|e| e.into_inner()) = Some(result);
            }
            // After the end went out: input the App sends on seeing it is queued and starts here, in order.
            me.finished(finished);
        });
    }

    /// Asks for the user's next message in the background and offers it to the prompt (`Session::suggest`), unless
    /// `suggestions` is off in the config. A new turn, a command or Esc stops it ([`Self::stop_suggesting`]).
    fn suggest_next(self: &Arc<Self>) {
        if !self.config().suggestions {
            return;
        }
        let cancel = CancellationToken::new();
        if let Some(old) = self.suggesting.lock().unwrap_or_else(|e| e.into_inner()).replace(cancel.clone()) {
            old.cancel();
        }
        let me = self.clone();
        tokio::spawn(async move {
            let suggestion = me.session.suggest(cancel.clone()).await;
            if cancel.is_cancelled() {
                return;
            }
            // Its tokens and cost count like any request's.
            let usage = me.session.usage();
            if let Some(last) = usage.last {
                me.emit(UiEvent::Usage { last, totals: totals(&usage.totals) });
            }
            if let Some(text) = suggestion {
                me.emit(UiEvent::Suggestion(text));
            }
        });
    }

    fn stop_suggesting(&self) {
        if let Some(cancel) = self.suggesting.lock().unwrap_or_else(|e| e.into_inner()).take() {
            cancel.cancel();
        }
    }

    /// `/compact [focus]`: shown as a turn (its waiting line, Esc stops it), reported like an automatic one.
    fn compact(self: &Arc<Self>, focus: Option<String>) {
        let me = self.clone();
        self.activity.set_busy(true);
        tokio::spawn(async move {
            me.emit(UiEvent::TurnStart { turn: String::new() });
            me.emit(UiEvent::Status(view::TurnStatus::Compacting));
            let outcome = match me.session.compact(focus.as_deref()).await {
                Ok(result) => compact_outcome(result),
                Err(e) => view::CompactOutcome::NotCompacted { reason: "failed".into(), error: e.to_string() },
            };
            let usage = me.session.usage();
            if let Some(last) = usage.last {
                me.emit(UiEvent::Usage { last, totals: totals(&usage.totals) });
            }
            me.emit(UiEvent::Compaction { automatic: false, outcome });
            me.activity.set_busy(false);
            me.emit(UiEvent::TurnEnd { turn: String::new(), reason: view::TurnEndReason::Loop(StopReason::End) });
        });
    }

    /// /undo: the last turn's changes, shown like an approval (each file's diff), and on a yes put back. Outside a
    /// turn only (a turn could be writing the same files); nothing is written before the yes.
    fn undo(self: &Arc<Self>) {
        if self.activity.busy.load(Ordering::SeqCst) || self.session.busy() {
            return self.emit(system("Marv is working: stop the current turn (Esc) before /undo.", true));
        }
        let me = self.clone();
        self.activity.set_busy(true);
        tokio::spawn(async move {
            me.undo_now().await;
            me.activity.set_busy(false);
        });
    }

    async fn undo_now(&self) {
        self.emit(UiEvent::Busy(Some("Reading the checkpoints…".into())));
        let plan = self.session.undo_plan().await;
        self.emit(UiEvent::Busy(None));
        let plan = match plan {
            Err(e) => return self.emit(system(format!("Couldn't read the checkpoints: {e}"), true)),
            Ok(Err(refusal)) if refusal.failed => return self.emit(system(format!("Couldn't undo: {}.", refusal.reason), true)),
            Ok(Err(refusal)) => {
                let why = refusal.reason.strip_prefix("nothing to undo: ").unwrap_or(&refusal.reason);
                return self.emit(system(format!("Nothing to undo: {why}."), false));
            }
            Ok(Ok(plan)) => plan,
        };
        if plan.files.is_empty() {
            let text = format!("Turn {} changed nothing in this project's folder, so there's nothing to undo.", plan.turn);
            return self.emit(system(text, false));
        }
        let request = marv_session::undo_request(&plan);
        let asked = marv_tools::Approver::approve(&TuiApprover(self.activity.clone()), request).await;
        if asked != marv_tools::Decision::Yes {
            return self.emit(system("Nothing was undone.", false));
        }
        self.emit(UiEvent::Busy(Some("Undoing…".into())));
        let undone = self.session.undo(&plan).await;
        self.emit(UiEvent::Busy(None));
        match undone {
            Ok(result) => self.emit(system(marv_session::undo_summary(&plan, &result), false)),
            Err(e) => self.emit(system(format!("Couldn't undo: {e}"), true)),
        }
    }

    /// Saves the config, has the session use it from its next turn, and says `notice`.
    fn save_config(&self, next: FileConfig, notice: String) {
        if let Err(e) = self.store.save(&next) {
            self.emit(system(format!("Could not save config: {e}"), true));
            return;
        }
        let config = resolve_config(Some(&next), &self.env);
        *self.file.lock().unwrap_or_else(|e| e.into_inner()) = Some(next);
        let configured = self.session.configure(Configure {
            provider: Some(factory(&config, &self.make_provider, &self.load_models)),
            thinking: Some(config.thinking),
            sandbox: Some(config.sandbox),
            yolo: Some(config.yolo && self.session_yolo_allowed()),
            trajectories: Some(config.trajectories),
            diagnostics: Some(config.diagnostics),
            checks: Some(config.checks),
            checkpoints: Some(config.checkpoints),
            max_steps: Some(config.max_steps),
            mcp_tools: Some(config.mcp_tools),
        });
        if let Err(e) = configured {
            self.emit(system(e.to_string(), true));
        }
        self.emit(UiEvent::StatusInfo(self.status_info()));
        self.emit(system(notice, false));
    }

    fn with_file(&self, edit: impl FnOnce(&mut FileConfig)) -> FileConfig {
        let config = self.config();
        let mut next = self.file().unwrap_or_else(|| FileConfig { provider: Some(config.provider), ..FileConfig::default() });
        edit(&mut next);
        next
    }

    async fn show_memory(&self) {
        let Some(paths) = &self.memory else {
            return self.emit(system("Memory isn't available.", false));
        };
        let memories = load_memory(paths).await.unwrap_or_default();
        let list = |entries: &[String]| {
            if entries.is_empty() {
                "*(nothing yet)*".to_string()
            } else {
                entries.iter().map(|e| format!("- {e}")).collect::<Vec<_>>().join("\n")
            }
        };
        let text = [
            format!("**Personal** (all projects): `{}`", shorten_home(&paths.personal, &self.home)),
            list(&memories.personal),
            format!("**This project**: `{}`", shorten_home(&paths.project, &self.home)),
            list(&memories.project),
            "Edit these files directly, use `/remember` and `/forget`, or ask Marv to remember or forget something.".into(),
        ]
        .join("\n\n");
        self.emit(UiEvent::Message(view::Message { markdown: true, ..message(Role::System, text) }));
    }

    async fn remember(&self, scope: MemoryTarget, text: &str) {
        let Some(paths) = &self.memory else { return };
        let (path, name) = match scope {
            MemoryTarget::Personal => (&paths.personal, "personal"),
            MemoryTarget::Project => (&paths.project, "project"),
        };
        match marv_tools::memory::add_memory(path, text).await {
            Ok(true) => self.emit(system(format!("Saved to {name} memory. {NEXT_TIME}"), false)),
            Ok(false) => self.emit(system(format!("That's already in {name} memory."), false)),
            Err(e) => self.emit(system(e.0, true)),
        }
    }

    async fn forget(&self, text: &str) {
        let Some(paths) = &self.memory else { return };
        // Look in both scopes; remove only when exactly one memory matches.
        let mut matches: Vec<(&Path, String)> = Vec::new();
        for path in [&paths.personal, &paths.project] {
            for m in marv_tools::memory::find_memory(path, text).await {
                matches.push((path, m));
            }
        }
        if matches.len() != 1 {
            let text = if matches.is_empty() {
                format!("No memory matches \"{text}\".")
            } else {
                let quoted: Vec<String> = matches.iter().map(|(_, m)| format!("\"{m}\"")).collect();
                format!("{} memories match; use more of the text: {}", matches.len(), quoted.join(", "))
            };
            return self.emit(system(text, true));
        }
        let (path, m) = &matches[0];
        match marv_tools::memory::remove_memory(path, m).await {
            Ok(removed) => self.emit(system(format!("Forgot: {removed}"), false)),
            Err(e) => self.emit(system(e.0, true)),
        }
    }

    /// `/good`, `/bad`, `/label`: feedback on the latest turn of this session, in its trajectory.
    fn rate(&self, score: i8, note: Option<String>, labels: Option<Vec<String>>) {
        let outcome = self.session.rate(Feedback { score, note: note.clone(), labels: labels.clone() });
        match outcome {
            RateOutcome::Off => {
                self.emit(system("Trajectory logging is off, so there's nothing to rate (/trajectories on).", true))
            }
            RateOutcome::Nothing => {
                self.emit(system("Nothing to rate yet: ratings apply to the last turn of this conversation.", true))
            }
            RateOutcome::Rated => {
                let what = match &labels {
                    Some(labels) => format!("Labeled the last turn: {}", labels.join(", ")),
                    None => format!("Rated the last turn: {}", if score > 0 { "good" } else { "bad" }),
                };
                let note = note.map(|n| format!(" ({n})")).unwrap_or_default();
                self.emit(system(format!("{what}{note}."), false));
            }
        }
    }

    async fn clear(self: &Arc<Self>) {
        // A new conversation (and session file), with the memories saved during the last one.
        if let Err(e) = self.session.clear().await {
            return self.emit(system(format!("Couldn't start a new conversation: {e}"), true));
        }
        self.emit(UiEvent::Clear);
        self.emit(UiEvent::Welcome(self.welcome()));
        self.radio_title();
        self.show_hooks();
        self.report_session().await;
    }

    /// Applies one command's action (Marv's `handleSubmit` switch).
    async fn apply(self: &Arc<Self>, line: &str, action: CommandAction) {
        let config = self.config();
        match action {
            CommandAction::Print { text, is_error, markdown } => {
                self.emit(UiEvent::Message(view::Message { is_error, markdown, ..message(Role::System, text) }))
            }
            CommandAction::Clear => self.clear().await,
            CommandAction::Setup => {
                let file = self.file();
                self.emit(UiEvent::OpenSetup(setup_context(SetupStart::Reconfigure, file.as_ref(), &config, &self.env)));
            }
            CommandAction::Model { id: None } => {
                let file = self.file();
                self.emit(UiEvent::OpenSetup(setup_context(SetupStart::Model, file.as_ref(), &config, &self.env)));
            }
            CommandAction::Model { id: Some(id) } => {
                let next = self.with_file(|f| f.model = Some(id.clone()));
                let label = preset(next.provider.unwrap_or(ProviderKind::OpenRouter)).label;
                self.save_config(next, format!("Saved to {} · {label} · {id}", shorten_home(&self.store.path, &self.home)));
            }
            CommandAction::Thinking { on } => {
                let mandatory = matches!(
                    self.session.model_info().and_then(|i| i.reasoning),
                    Some(marv_provider::ReasoningSupport::Mandatory)
                );
                let notice = if on {
                    "Thinking on: models may reason before answering (slower, often better).".to_string()
                } else {
                    let except =
                        if mandatory { format!(" (except {}, which always reasons)", config.model) } else { String::new() };
                    format!("Thinking off: models answer directly{except}.")
                };
                self.save_config(self.with_file(|f| f.thinking = Some(on)), notice);
            }
            CommandAction::Sandbox { on } => {
                let notice = if on {
                    "Sandbox on: bash commands run in bubblewrap."
                } else {
                    "Sandbox off: bash commands run directly on your system (each still needs your approval)."
                };
                self.save_config(self.with_file(|f| f.sandbox = Some(on)), notice.into());
            }
            // MARV_YOLO=off (#74: a worker the Herdr door spawned from a session out of yolo) holds yolo off for this
            // process: /yolo on would claim otherwise, and write yolo into the user's config for every Marv.
            CommandAction::Yolo { on: true }
                if marv_session::config::yolo_held_off(self.env.get("MARV_YOLO").map(String::as_str)) =>
            {
                self.emit(system(YOLO_HELD_OFF, true));
            }
            CommandAction::Yolo { on } => {
                let notice = commands::yolo_status(&Config { yolo: on, ..config.clone() }, self.sandbox_available);
                self.save_config(self.with_file(|f| f.yolo = Some(on)), notice);
            }
            CommandAction::Trajectories { on } => {
                let path = self.trajectories_dir.as_ref().map(|d| shorten_home(d, &self.home));
                let status = commands::trajectories_status(&Config { trajectories: on, ..config.clone() }, path.as_deref());
                self.save_config(self.with_file(|f| f.trajectories = Some(on)), format!("Trajectories {status}"));
            }
            CommandAction::Diagnostics { on } => {
                let status = commands::diagnostics_status(&Config { diagnostics: on, ..config.clone() });
                self.save_config(self.with_file(|f| f.diagnostics = Some(on)), format!("Diagnostics {status}"));
            }
            CommandAction::Checks { on } => {
                let status = commands::checks_status(&Config { checks: on, ..config.clone() });
                self.save_config(self.with_file(|f| f.checks = Some(on)), format!("Checks {status}"));
            }
            CommandAction::Checkpoints { on } => {
                let status = commands::checkpoints_status(&Config { checkpoints: on, ..config.clone() });
                self.save_config(self.with_file(|f| f.checkpoints = Some(on)), format!("Checkpoints {status}"));
            }
            CommandAction::MeshWake { on } => {
                let status = commands::mesh_wake_status(&Config { mesh_wake: on, ..config.clone() });
                self.save_config(self.with_file(|f| f.mesh_wake = Some(on)), format!("Mesh wake {status}."));
                if let Some(radio) = self.radio() {
                    radio.set_wake(on);
                }
            }
            CommandAction::Radio { on } => self.turn_radio(on).await,
            CommandAction::Tx { to, text } => self.tx(to, &text),
            CommandAction::Ask { to, text } => self.ask(&to, &text),
            CommandAction::RadioView => match self.radio() {
                Some(radio) if radio.is_on() => self.emit(UiEvent::OpenRadio),
                Some(_) => self.emit(system(commands::RADIO_OFF, false)),
                None => self.emit(system(
                    "This Marv isn't on the mesh (no runtime folder, or its door didn't open), so it isn't on the radio.",
                    false,
                )),
            },
            CommandAction::Undo => self.undo(),
            CommandAction::Steps { every } => {
                let saved = self.with_file(|f| f.max_steps = Some(MaxStepsSetting::Every(every)));
                // MARV_MAX_STEPS still wins over the file: say what's saved, and what this session actually uses.
                let effective = resolve_config(Some(&saved), &self.env).max_steps;
                let notice = if effective == every {
                    commands::steps_status(&Config { max_steps: every, ..config.clone() })
                } else {
                    format!(
                        "Saved, but MARV_MAX_STEPS={} overrides it while it's set. Now: {}",
                        self.env.get("MARV_MAX_STEPS").map(String::as_str).unwrap_or_default(),
                        commands::steps_status(&Config { max_steps: effective, ..config.clone() })
                    )
                };
                self.save_config(saved, notice);
            }
            CommandAction::Feedback { score, note, labels } => self.rate(score, note, labels),
            CommandAction::Resume => {
                if self.activity.busy.load(Ordering::SeqCst) {
                    return self.emit(system(WORKING, true));
                }
                self.open_picker().await;
            }
            CommandAction::Memory => self.show_memory().await,
            CommandAction::Remember { scope, text } => self.remember(scope, &text).await,
            CommandAction::Forget { text } => self.forget(&text).await,
            CommandAction::Compact { focus } => self.compact(focus),
            CommandAction::Processes { stop } => {
                let me = self.clone();
                tokio::spawn(async move {
                    let stopped = me.session.stop_process(&stop).await;
                    me.emit(if stopped {
                        system(format!("Stopped {}.", printable(&stop)), false)
                    } else {
                        system(format!("No background process named \"{}\". /processes lists them.", printable(&stop)), true)
                    });
                });
            }
            CommandAction::Peers { read, stop } => {
                if !self.session.herdr_offered() {
                    return self.emit(system("No Herdr peers here: Marv isn't running in a Herdr pane.", true));
                }
                let me = self.clone();
                tokio::spawn(async move { me.peers(read, stop).await });
            }
            CommandAction::Skill { name, args } => {
                let Some(skill) = self.skills.iter().find(|s| s.skill.name == name) else { return };
                let for_model = marv_session::skills::skill_message(&skill.skill, &args);
                // As typed: the user's line, then the turn (the App starts it on its first event).
                self.emit(UiEvent::Message(message(Role::User, line)));
                self.run_turn(line.to_string(), Some(for_model));
            }
            CommandAction::McpTrust => self.trust_mcp().await,
            CommandAction::TrustProject => self.trust_project().await,
            CommandAction::McpAdd { args } => self.add_mcp(&args).await,
            CommandAction::McpSearch { words } => self.find_mcp(&words).await,
            CommandAction::McpRemove { name } => self.remove_mcp(&name).await,
            CommandAction::McpLogin { server } => self.login_mcp(&server).await,
            CommandAction::McpLogout { server } => self.logout_mcp(&server).await,
            CommandAction::Exit => self.emit(UiEvent::Exit),
        }
    }

    // ---- Mesh wake (marv-rs #78) ----

    /// This Marv's name on the machine's mesh (its door opened), for `/mesh`.
    pub fn set_mesh_name(&self, name: &str) {
        *self.mesh_name.lock().unwrap_or_else(|e| e.into_inner()) = Some(name.to_string());
    }

    /// Waits for messages landing in an inbox and, with `mesh.wake` on, lets one start a turn ([`Self::wake`]).
    /// Only the TUI does this: `marv -p` runs its one turn and never wakes.
    pub fn watch_mesh(self: &Arc<Self>) {
        let Some(mesh) = self.session.mesh() else { return };
        let (me, stop) = (Arc::downgrade(self), self.mesh_watch.clone());
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = mesh.mail() => {}
                    _ = stop.cancelled() => return,
                }
                let Some(bridge) = me.upgrade() else { return };
                bridge.wake();
            }
        });
    }

    /// A message came in. It starts a turn only when wake is on, nothing is running (a turn, `/compact`, a lookup)
    /// and no approval waits; otherwise it waits in the inbox for the next turn, as it always has. The woken turn
    /// reads it fenced and tainting (`MarvSession::wake`), with who woke it shown first. A wire over
    /// `WAKES_PER_MINUTE` is told once, and its messages wait too.
    fn wake(self: &Arc<Self>) {
        if !self.config().mesh_wake {
            return;
        }
        let Some(mesh) = self.session.mesh() else { return };
        // Decided and claimed under the gate: a submission on its way has the gate first, and one after waits.
        let mut gate = self.gate.lock().unwrap_or_else(|e| e.into_inner());
        if gate.closing
            || gate.turn
            || self.activity.busy.load(Ordering::SeqCst)
            || self.activity.waiting()
            || self.session.busy()
        {
            return;
        }
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64);
        let check = marv_session::mesh::wake_check(&mesh, &mut self.wake_limit.lock().unwrap_or_else(|e| e.into_inner()), now);
        for wire in &check.limited {
            self.emit(system(
                format!(
                    "{} woke Marv {} times a minute already: its messages on {} wait for your next turn.",
                    printable(wire.sender.as_str()),
                    marv_session::mesh::WAKES_PER_MINUTE,
                    printable(wire.receptacle.as_str())
                ),
                false,
            ));
        }
        if let Some(by) = check.wake {
            (gate.turn, gate.woken) = (true, true);
            self.activity.set_busy(true);
            drop(gate);
            self.stop_suggesting();
            match self.session.wake(&by) {
                Ok(Some(turn)) => {
                    self.emit(system(by.line(), false));
                    self.forward(turn);
                }
                Ok(None) | Err(_) => self.finished(false),
            }
            return;
        }
        // #124: an addressed radio message (never a broadcast, never at hop 2 or more), within the radio's limits. The
        // turn it starts asks for everything that would ask (`MarvSession::wake_radio`).
        if mesh.unread_radio().is_empty() {
            return;
        }
        let check = marv_session::radio::radio_wake_check(
            &mesh,
            &mut self.radio_wake_limit.lock().unwrap_or_else(|e| e.into_inner()),
            now,
        );
        for sender in &check.limited {
            self.emit(system(
                format!(
                    "✻ Radio: {} woke Marv {} times a minute (or within {} s) already: its messages wait for your next turn.",
                    printable(sender),
                    marv_session::radio::RADIO_WAKES_PER_MINUTE,
                    marv_session::radio::RADIO_WAKE_GAP_MS / 1000
                ),
                false,
            ));
        }
        if check.paused {
            self.emit(system(
                format!(
                    "✻ Radio wakes paused: {} in {} minutes. Messages wait for your next turn.",
                    marv_session::radio::RADIO_WAKES_OVERALL,
                    marv_session::radio::RADIO_WAKES_WINDOW_MS / 60_000
                ),
                false,
            ));
        }
        let Some(by) = check.wake else { return };
        (gate.turn, gate.woken) = (true, true);
        self.activity.set_busy(true);
        drop(gate);
        self.stop_suggesting();
        match self.session.wake_radio(&by) {
            Ok(Some(turn)) => {
                self.emit(system(format!("✻ {}", by.line()), false));
                self.forward(turn);
            }
            Ok(None) | Err(_) => self.finished(false),
        }
    }

    // ---- The mesh radio (marv-rs #124, phase 1: presence) ----

    fn radio(&self) -> Option<Arc<Radio>> {
        self.radio.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// This Marv's radio, its door open: presence follows the session (state, title, wake), the strip follows the
    /// roster, and it joins now if the user's own config has it on. Only the TUI has one: `marv -p` is never on it.
    pub fn set_radio(self: &Arc<Self>, radio: Arc<Radio>) {
        let config = self.config();
        radio.set_titles(config.radio_titles);
        radio.set_wake(config.mesh_wake);
        *self.radio.lock().unwrap_or_else(|e| e.into_inner()) = Some(radio.clone());
        *self.activity.radio.lock().unwrap_or_else(|e| e.into_inner()) = Some(radio.clone());
        self.radio_title();
        self.activity.report();
        let (me, stop) = (Arc::downgrade(self), self.mesh_watch.clone());
        let mut changes = radio.subscribe();
        let watched = radio.clone();
        tokio::spawn(async move {
            // Reports already in the traffic list when the TUI started aren't new.
            let mut shown = std::collections::HashSet::new();
            for t in watched.traffic_list() {
                shown.insert(format!("{}>{}", t.id, t.reply_to.unwrap_or_default()));
            }
            loop {
                if let Some(bridge) = me.upgrade() {
                    bridge.emit(UiEvent::Radio(bridge.radio_view()));
                    bridge.show_reports(&watched, &mut shown);
                }
                tokio::select! {
                    _ = stop.cancelled() => return,
                    changed = changes.changed() => if changed.is_err() { return },
                }
            }
        });
        if config.mesh_radio {
            tokio::spawn(async move { radio.turn_on().await });
        }
    }

    /// The radio as the strip and `/radio` show it.
    fn radio_view(&self) -> view::RadioView {
        let Some(radio) = self.radio().filter(|r| r.is_on()) else { return view::RadioView::default() };
        let peers = radio
            .roster()
            .into_iter()
            .map(|e| view::RadioPeer {
                name: e.name,
                state: match e.state {
                    RosterState::Idle => view::RadioState::Idle,
                    RosterState::Working => view::RadioState::Working,
                    RosterState::Waiting => view::RadioState::Waiting,
                    RosterState::Silent => view::RadioState::Silent,
                    RosterState::Left => view::RadioState::Left,
                    RosterState::Lost => view::RadioState::Lost,
                },
                title: e.title,
                project: e.project,
                pane: e.pane,
                since_ms: e.since_ms,
            })
            .collect();
        let traffic = radio
            .traffic_list()
            .into_iter()
            .map(|t| {
                let request = if t.kind == marv_mesh::radio::Kind::Request { Some(t.id.as_str()) } else { t.reply_to.as_deref() }
                    .map(marv_mesh::radio::short_id);
                view::RadioTraffic {
                    at_ms: t.at_ms,
                    id: t.id,
                    outgoing: t.outgoing,
                    peers: t.peers,
                    by: t.by.as_str().to_string(),
                    kind: t.kind.as_str().to_string(),
                    request,
                    text: t.text,
                    refused: t.refused,
                }
            })
            .collect();
        view::RadioView { on: true, me: radio.name.clone(), my_title: radio.presence().title, peers, traffic }
    }

    /// `/tx`: the person's message on the radio (`by: user`, hop 0). One line in Marv's voice in this transcript;
    /// nothing of it reaches this Marv's own model.
    fn tx(&self, to: Vec<String>, text: &str) {
        let Some(radio) = self.radio().filter(|r| r.is_on()) else {
            return self.emit(system(commands::RADIO_OFF, false));
        };
        let addressed = if to.is_empty() { marv_mesh::radio::To::All } else { marv_mesh::radio::To::Names(to) };
        match radio.send_msg(&addressed, marv_mesh::radio::By::User, 0, text) {
            Err(err) => self.emit(system(format!("Radio: not sent: {}", printable(&err)), true)),
            Ok(report) => {
                if !report.sent.is_empty() {
                    let view = self.radio_view();
                    let short = |name: &str| marv_tui::ui::radio_view::short_name(&view, name);
                    let who = match &addressed {
                        marv_mesh::radio::To::All => "all".to_string(),
                        marv_mesh::radio::To::Names(_) => {
                            report.sent.iter().map(|n| format!("@{}", short(n))).collect::<Vec<_>>().join(" ")
                        }
                    };
                    self.emit(system(printable(&format!("✻ Radio → {who}: {text}")), false));
                }
                for (peer, why) in &report.refused {
                    self.emit(system(printable(&format!("Radio: not sent to {peer}: {why}.")), true));
                }
            }
        }
    }

    /// `/ask @name text`: the person's request on the radio (#124, phase 3; `by: user`, hop 0): one Marv, which
    /// acknowledges and reports back. One line in Marv's voice here; nothing of it reaches this Marv's own model (the
    /// ack and the report do, fenced, at its next turn).
    fn ask(&self, to: &str, text: &str) {
        let Some(radio) = self.radio().filter(|r| r.is_on()) else {
            return self.emit(system(commands::RADIO_OFF, false));
        };
        match radio.send_request(to, marv_mesh::radio::By::User, 0, text) {
            Err(err) => self.emit(system(format!("Radio: not sent: {}", printable(&err)), true)),
            Ok(id) => {
                let view = self.radio_view();
                let peer = radio.resolve(to).unwrap_or_else(|_| to.to_string());
                let short = marv_tui::ui::radio_view::short_name(&view, &peer);
                let request = marv_mesh::radio::short_id(&id);
                self.emit(system(printable(&format!("✻ Radio → @{short} request {request}: {text}")), false));
            }
        }
    }

    /// A report on one of this Marv's requests came in (#124): a line in the transcript for the person, in Marv's
    /// voice with the report's text (printable). The model reads it fenced at its next turn, as any radio traffic.
    fn show_reports(&self, radio: &Radio, shown: &mut std::collections::HashSet<String>) {
        for t in radio.traffic_list() {
            let report = matches!(t.kind, marv_mesh::radio::Kind::Done | marv_mesh::radio::Kind::Failed);
            if t.outgoing
                || !report
                || t.refused.is_some()
                || !shown.insert(format!("{}>{}", t.id, t.reply_to.clone().unwrap_or_default()))
            {
                continue;
            }
            let from = t.peers.first().cloned().unwrap_or_default();
            let short = t.reply_to.as_deref().map(|r| format!(" {}", marv_mesh::radio::short_id(r))).unwrap_or_default();
            self.emit(system(printable(&format!("✻ Radio ← {from} {}{short}: {}", t.kind.as_str(), t.text)), false));
        }
    }

    /// Presence's title: the conversation's first user message (empty for a new one), never anything else.
    fn radio_title(&self) {
        if let Some(radio) = self.radio() {
            radio.set_title(&self.session.first_message());
        }
    }

    /// `/radio on|off`: saved in the user's own config, and joined or left at once.
    async fn turn_radio(self: &Arc<Self>, on: bool) {
        let titles = self.file().and_then(|f| f.mesh_radio).is_none_or(|r| r.titles);
        let notice = match (on, self.radio()) {
            (true, Some(_)) => format!(
                "Radio on: your other Marvs here with the radio on show in the strip above the prompt (/radio lists \
                 them and what was said). They see this Marv's state and conversation title; /tx sends them a \
                 message, which their models read as data.{}",
                if self.session.radio_offered() {
                    ""
                } else {
                    " This Marv's model gets radio_tx from the next conversation (/clear): its tools stay as they are \
                     for this one."
                }
            ),
            (true, None) => "Radio on, saved. This Marv isn't on the mesh (no runtime folder, or its door didn't open), \
                 so it joins from the next start."
                .to_string(),
            (false, _) => "Radio off: this Marv left it.".to_string(),
        };
        self.save_config(self.with_file(|f| f.mesh_radio = Some(RadioSetting { on, titles })), notice);
        let Some(radio) = self.radio() else { return };
        if on {
            radio.set_titles(titles);
            radio.turn_on().await;
        } else {
            radio.turn_off().await;
        }
        self.emit(UiEvent::Radio(self.radio_view()));
    }

    /// Leaves the radio on the way out: a `left` to every link (bounded), so the others don't wait for a timeout.
    pub async fn leave_radio(&self) {
        if let Some(radio) = self.radio().filter(|r| r.is_on()) {
            radio.turn_off().await;
        }
    }

    /// The App is leaving (#78): no wake-up starts from now on, queued input is dropped, and the watcher stops. Before
    /// the App's flush and the shutdown's interrupt, so nothing starts a turn after the terminal has closed.
    pub fn close_mesh(&self) {
        let mut gate = self.gate.lock().unwrap_or_else(|e| e.into_inner());
        gate.closing = true;
        gate.queued.clear();
        self.mesh_watch.cancel();
    }

    // ---- MCP (Marv #6, #18, #46) ----

    /// Once the servers have started: one notice for those that failed, wait for trust or need a login; from then on
    /// a server whose login runs out mid-session (a refresh refused) is told once.
    pub fn watch_mcp(self: &Arc<Self>) {
        let Some(mcp) = self.mcp.clone() else { return };
        let me = Arc::downgrade(self);
        tokio::spawn(async move {
            mcp.ready().await;
            let Some(bridge) = me.upgrade() else { return };
            let status = mcp.status();
            {
                let mut told = bridge.told_needs_login.lock().unwrap_or_else(|e| e.into_inner());
                told.extend(status.iter().filter(|s| s.state == McpState::NeedsLogin).map(|s| s.name.clone()));
            }
            let watched = me.clone();
            let manager = Arc::downgrade(&mcp);
            mcp.set_on_change(move || {
                let (Some(bridge), Some(mcp)) = (watched.upgrade(), manager.upgrade()) else { return };
                for s in mcp.status() {
                    let mut told = bridge.told_needs_login.lock().unwrap_or_else(|e| e.into_inner());
                    if s.state == McpState::NeedsLogin && told.insert(s.name.clone()) {
                        drop(told);
                        bridge.emit(notice_message(Notice {
                            title: "MCP".into(),
                            tone: NoticeTone::Warning,
                            items: vec![NoticeItem {
                                text: format!("{} needs a new login: /mcp login {}", s.name, s.name),
                                detail: Some(vec!["Its tools fail until then.".into()]),
                            }],
                        }));
                    } else if s.state == McpState::Connected {
                        told.remove(&s.name);
                    }
                }
            });
            if let Some(notice) = mcp_notice(&status) {
                bridge.emit(notice_message(notice));
            }
        });
    }

    /// Busy work outside a turn: Esc stops it (`abort` cancels its token), `label` shows where "Thinking…" would.
    async fn busy_with<T>(&self, label: String, work: impl AsyncFnOnce(CancellationToken) -> T) -> T {
        let cancel = CancellationToken::new();
        *self.busy_work.lock().unwrap_or_else(|e| e.into_inner()) = Some(cancel.clone());
        self.activity.set_busy(true);
        self.emit(UiEvent::Busy(Some(label)));
        let result = work(cancel).await;
        self.emit(UiEvent::Busy(None));
        self.activity.set_busy(false);
        *self.busy_work.lock().unwrap_or_else(|e| e.into_inner()) = None;
        result
    }

    /// /trust: the project's instruction files, skills and agent types as startup loaded them, and the subfolder
    /// instruction files this session quoted, trusted (for exactly those contents) from the next start: the system
    /// prompt and the tool list stay as they are in this session.
    async fn trust_project(&self) {
        let quoted = self.session.nested_quoted();
        let result = marv_session::project_trust::trust_project(
            &self.config_dir,
            &self.root,
            &self.project_files,
            &self.project_trust,
            &quoted,
            "the next start of Marv here",
        )
        .await;
        match result {
            Ok(text) => self.emit(system(text, false)),
            Err(text) => self.emit(system(text, true)),
        }
    }

    /// /mcp trust: the project's servers the user hasn't trusted yet, trusted (for this exact config) and started.
    async fn trust_mcp(&self) {
        let waiting = self.mcp.as_ref().map(|m| m.untrusted()).unwrap_or_default();
        let Some(mcp) = self.mcp.as_ref().filter(|_| !waiting.is_empty()) else {
            return self.emit(system("No MCP servers are waiting to be trusted (/mcp lists them).", false));
        };
        let names: Vec<&str> = waiting.iter().map(|s| s.name.as_str()).collect();
        self.emit(system(format!("Starting {}…", names.join(", ")), false));
        match mcp.trust_all().await {
            Ok(started) => {
                let lines: Vec<String> = started
                    .iter()
                    .map(|s| match s.state {
                        McpState::Connected => format!("{}: connected · {} tools", s.name, s.tools.len()),
                        _ => format!("{}: failed: {}", s.name, s.error.as_deref().unwrap_or_default()),
                    })
                    .collect();
                let failed = started.iter().any(|s| s.state != McpState::Connected);
                self.emit(system(lines.join("\n"), failed));
            }
            Err(e) => self.emit(system(format!("Couldn't trust the servers: {e}"), true)),
        }
    }

    /// /mcp add: look the source up (all fetching happens here, before the approval), ask once with the exact entry,
    /// write that entry to ~/.marv/mcp.json, and start it. Nothing fetched after the yes changes what runs: the file
    /// gets the JSON the approval showed, parsed back.
    async fn add_mcp(&self, args: &str) {
        let options = match parse_add_args(args) {
            Ok(options) => options,
            Err(e) => return self.emit(system(e, true)),
        };
        let label = format!("Looking up {}…", printable(&options.source));
        let outcome = self
            .busy_with(label, async |cancel: CancellationToken| {
                let looked_up = self.look_up_and_ask(&options, &cancel).await;
                looked_up.unwrap_or_else(|e| {
                    // Esc: the lookup's error is the stop, not the source's fault.
                    AddOutcome::Failed(if cancel.is_cancelled() {
                        NOT_ADDED.into()
                    } else {
                        format!("Couldn't add the server: {e}")
                    })
                })
            })
            .await;
        let (name, entry) = match outcome {
            AddOutcome::Failed(text) => return self.emit(system(text, true)),
            AddOutcome::Choices(choices) => {
                let list: Vec<String> = choices.iter().map(|c| format!("- `{}`", printable(c))).collect();
                let text = format!("That README names several servers. Add one with:\n\n{}", list.join("\n"));
                return self.emit(UiEvent::Message(view::Message { markdown: true, ..message(Role::System, text) }));
            }
            AddOutcome::Declined(name) => return self.emit(system(format!("Not added: {name}."), false)),
            AddOutcome::Added(name, entry) => (name, entry),
        };
        // Loaded exactly as at startup, so ${VAR}s are filled in the same way.
        let mut env = self.env.clone();
        env.insert("MARV_PROJECT_DIR".into(), self.root.to_string_lossy().into_owned());
        let loaded = marv_mcp::config::parse_mcp_servers(&[(name.clone(), entry)], &env);
        let Some(server) = loaded.servers.into_iter().next() else {
            let prefix = format!("\"{name}\" in mcpServers: ");
            let why = loaded.problems.first().map_or("it couldn't be loaded", |p| p.strip_prefix(&prefix).unwrap_or(p));
            let text =
                format!("Added {name} to ~/.marv/mcp.json, but didn't start it: {why}. Set it in your shell and restart Marv.");
            return self.emit(system(text, true));
        };
        let Some(mcp) = &self.mcp else {
            return self.emit(system(format!("Added {name} to ~/.marv/mcp.json. Restart Marv to start it."), false));
        };
        self.emit(system(format!("Starting {name}…"), false));
        match mcp.add(server).await {
            Ok(status) => {
                let (text, is_error) = added_notice(&status);
                self.emit(system(text, is_error));
            }
            Err(e) => self.emit(system(
                format!("Added {name} to ~/.marv/mcp.json, but didn't start it now: {e} It starts when you restart Marv."),
                true,
            )),
        }
    }

    /// /mcp add's lookup, its checks and its question; on a yes, the entry written.
    async fn look_up_and_ask(
        &self,
        options: &marv_mcp::install::AddOptions,
        cancel: &CancellationToken,
    ) -> Result<AddOutcome, String> {
        let deps = (self.mcp_deps)(cancel.clone());
        let proposal = match resolve_add(options, &*deps).await.map_err(|e| e.to_string())? {
            Resolved::Choices(c) => return Ok(AddOutcome::Choices(c.choices)),
            Resolved::Proposal(p) => p,
        };
        let name = proposal.name.clone();
        let status = self.mcp.as_ref().map(|m| m.status()).unwrap_or_default();
        // Removed in this conversation: its tools stay listed until /clear, so it can't come back yet (McpManager::add).
        if status.iter().any(|s| s.name == name && s.state == McpState::Removed) {
            return Err(format!(
                "\"{name}\" was removed in this conversation and its tools stay listed until the next one: /clear first, or pick another --name."
            ));
        }
        if personal_server_names(&self.config_dir).contains(&name) {
            return Err(format!(
                "You already have a server named \"{name}\": pick another with --name, or /mcp remove {name} first."
            ));
        }
        // A project server with that name is running (or waiting for trust) under it: McpManager::add can't start a
        // second one, and replacing it mid-conversation would swap the tools under the model. Another name is simpler.
        if status.iter().any(|s| s.name == name && s.source == marv_mcp::config::ServerSource::Project) {
            return Err(format!("This project has a server named \"{name}\": pick another with --name."));
        }
        if cancel.is_cancelled() {
            return Ok(AddOutcome::Failed(NOT_ADDED.into()));
        }
        let (approval, json) = approval_for(&proposal, &|v| self.env.contains_key(v))?;
        let request = marv_session::mcp_install::add_request(&approval);
        let asked = marv_tools::Approver::approve(&TuiApprover(self.activity.clone()), request).await;
        if asked != marv_tools::Decision::Yes {
            return Ok(AddOutcome::Declined(name));
        }
        let entry: Value = serde_json::from_str(&json).map_err(|e| e.to_string())?;
        add_personal_server(&self.config_dir, &name, entry.clone()).await.map_err(|e| e.to_string())?;
        Ok(AddOutcome::Added(name, entry))
    }

    /// /mcp search: the MCP Registry's latest, active entries.
    async fn find_mcp(&self, words: &str) {
        let found = self
            .busy_with("Searching the MCP Registry…".into(), async |cancel: CancellationToken| {
                let deps = (self.mcp_deps)(cancel);
                marv_mcp::install::registry::search_registry(words, &*deps).await
            })
            .await;
        match found {
            Ok(hits) => {
                let text = marv_mcp::install::registry::search_text(words, &hits);
                self.emit(UiEvent::Message(view::Message { markdown: true, ..message(Role::System, text) }));
            }
            Err(e) => self.emit(system(format!("Couldn't search the MCP Registry: {e}"), true)),
        }
    }

    /// /mcp remove: one of your servers out of ~/.marv/mcp.json, and stopped. No approval: the user typed it, and it
    /// takes capability away.
    async fn remove_mcp(&self, name: &str) {
        let status = self.mcp.as_ref().map(|m| m.status()).unwrap_or_default();
        if !personal_server_names(&self.config_dir).iter().any(|n| n == name) {
            let project = status.iter().any(|s| s.name == name && s.source == marv_mcp::config::ServerSource::Project);
            let text = if project {
                format!("\"{name}\" is this project's server: remove it from .mcp.json.")
            } else {
                format!("You have no server named \"{name}\" in ~/.marv/mcp.json (/mcp lists them).")
            };
            return self.emit(system(text, true));
        }
        if let Err(e) = remove_personal_server(&self.config_dir, name).await {
            return self.emit(system(e.to_string(), true));
        }
        let running = status
            .iter()
            .any(|s| s.name == name && s.source == marv_mcp::config::ServerSource::Personal && s.state != McpState::Removed);
        if running && let Some(mcp) = &self.mcp {
            let _ = mcp.remove(name).await;
        }
        let stopped = if running { " and stopped it. Its tools stay listed (failing) until /clear or a restart." } else { "." };
        self.emit(system(format!("Removed {name} from ~/.marv/mcp.json{stopped}"), false));
    }

    /// /mcp login: not a turn (the prompt stays free, and a running turn keeps its tools). The browser opens where it
    /// can; the link is shown either way, with the paste fallback for a browser on another machine. Tokens are never
    /// shown.
    async fn login_mcp(&self, name: &str) {
        let Some(mcp) = &self.mcp else { return };
        let (tx, open, server) = (self.activity.tx.clone(), self.open_url.clone(), name.to_string());
        let authorize: Authorize = Arc::new(move |url| {
            let text = format!(
                "Log in to {server} at {}: opening your browser… If it doesn't open, open this link and, if your browser then shows an error page, paste its address here:\n{url}",
                url_host(url.as_str())
            );
            let _ = tx.send(system(text, false));
            open(url);
        });
        let login = match mcp.login(name, authorize) {
            Ok(login) => login,
            Err(e) => return self.emit(system(e.to_string(), true)),
        };
        let id = self.next_login.fetch_add(1, Ordering::SeqCst);
        *self.pending_login.lock().unwrap_or_else(|e| e.into_inner()) =
            Some(PendingLogin { id, server: name.to_string(), paste: login.paste });
        self.told_needs_login.lock().unwrap_or_else(|e| e.into_inner()).insert(name.to_string());
        match login.done.await {
            Ok(Ok(status)) => {
                if status.state == McpState::Connected {
                    self.told_needs_login.lock().unwrap_or_else(|e| e.into_inner()).remove(name);
                }
                let (text, is_error) = login_result(&status);
                self.emit(system(text, is_error));
            }
            Ok(Err(e)) => self.emit(system(format!("{name}: not logged in: {e}"), true)),
            Err(_) => self.emit(system(format!("{name}: not logged in: the login stopped"), true)),
        }
        let mut pending = self.pending_login.lock().unwrap_or_else(|e| e.into_inner());
        if pending.as_ref().is_some_and(|p| p.id == id) {
            *pending = None;
        }
    }

    /// A pasted callback address: shown as "(login callback)", handed to the waiting login.
    fn paste_callback(&self, text: &str) {
        self.emit(UiEvent::Message(message(Role::User, "(login callback)")));
        let mut pending = self.pending_login.lock().unwrap_or_else(|e| e.into_inner());
        let Some(login) = pending.as_mut() else {
            drop(pending);
            let text = "No login is waiting for that address (/mcp login <server> starts one). It wasn't sent to the model.";
            return self.emit(system(text, true));
        };
        let check = (login.paste)(text);
        let server = login.server.clone();
        drop(pending);
        match check {
            CallbackCheck::Ok { .. } => self.emit(system(format!("Got it: finishing the login to {server}…"), false)),
            CallbackCheck::Refused { why } if why == "state" || why == "path" => self.emit(system(
                "That address isn't from this login: paste the address the browser ended up on after the latest /mcp login.",
                true,
            )),
            CallbackCheck::Refused { why } if why == "used" => self.emit(system("This login already has its callback.", true)),
            // An error= callback: the login's `done` fails with the reason, and login_mcp says so.
            CallbackCheck::Refused { .. } => {}
        }
    }

    /// /mcp logout: revoke where the server can, forget the login.
    async fn logout_mcp(&self, name: &str) {
        let Some(mcp) = &self.mcp else { return };
        self.told_needs_login.lock().unwrap_or_else(|e| e.into_inner()).insert(name.to_string());
        match mcp.logout(name).await {
            Ok(revoked) => self.emit(system(logout_text(name, revoked), false)),
            Err(e) => self.emit(system(e.to_string(), true)),
        }
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
        };
        commands::run_command(line, &ctx)
    }
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

    fn login_callback(&self, text: &str) -> bool {
        if !marv_mcp::oauth::is_callback(text) {
            return false;
        }
        self.0.paste_callback(text.trim());
        true
    }

    fn load_models(&self, provider: ProviderId) {
        let b = &self.0;
        let kind = provider_kind(provider);
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
        let label = preset(provider_kind(answer.provider)).label;
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

pub async fn run(resume: Option<bool>, approve: Vec<String>) -> anyhow::Result<i32> {
    // The real environment only (docs/SECURITY.md): never a project's .env.
    let env: Env = std::env::vars().collect();
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
    let config = resolve_config(file.as_ref(), &env);
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
    let result = App::new(Arc::new(Port(bridge.clone())), rx, options).run().await;
    if tty {
        let _ = write!(std::io::stdout(), "\x1b[23;0t");
        let _ = std::io::stdout().flush();
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

#[cfg(test)]
mod tests {
    use super::*;
    use marv_core::{ChatTurn, ProviderEvent, ToolCall, ToolResult};
    use marv_mesh::herdr::{HerdrSend, HerdrTarget};
    use marv_provider::{EventStream, Provider, StreamOptions};
    use serde_json::Value;
    use std::collections::VecDeque;
    use std::sync::atomic::AtomicUsize;

    // #98: the TUI's --approve merges with the config's "approve" list; an unknown name is a problem, and the
    // valid ones still stand.
    #[test]
    fn the_tui_merges_config_and_cli_approve_scopes() {
        let (scopes, problems) = merge_pre_approved(vec!["web".into()], &["network".to_string(), "web".to_string()]);
        assert_eq!(scopes, ["web", "network"], "deduped, cli after config");
        assert!(problems.is_empty());
        let (scopes, problems) = merge_pre_approved(vec!["web".into()], &["netwrk".to_string()]);
        assert_eq!(scopes, ["web"], "the typo doesn't reach the session");
        assert!(problems[0].contains("Unknown --approve scope"), "{:?}", problems); // #82 review: the config's own names are checked the same way.
        let (scopes, problems) = merge_pre_approved(vec!["web".into(), "netwrk".into()], &[]);
        assert_eq!(scopes, ["web"]);
        assert!(problems[0].starts_with("\"approve\" in your config: Unknown --approve scope \"netwrk\""), "{problems:?}");
    }
    use tokio::sync::mpsc::UnboundedReceiver;

    /// Replays one scripted reply per request (Marv's `tests/fake-provider.ts`).
    struct Scripted(Mutex<VecDeque<Vec<ProviderEvent>>>);
    impl Provider for Scripted {
        fn name(&self) -> &str {
            "scripted"
        }
        fn context_length(&self) -> Option<u64> {
            None
        }
        fn stream(&self, _history: &[ChatTurn], _options: StreamOptions) -> EventStream {
            let reply = self.0.lock().unwrap().pop_front().unwrap_or_else(|| vec![ProviderEvent::Done { reason: None }]);
            Box::pin(futures::stream::iter(reply))
        }
    }

    fn say(text: &str) -> Vec<ProviderEvent> {
        vec![ProviderEvent::TextDelta(text.into()), ProviderEvent::Done { reason: Some("stop".into()) }]
    }
    fn write(id: &str, path: &str, content: &str) -> Vec<ProviderEvent> {
        let arguments = serde_json::json!({ "path": path, "content": content }).to_string();
        vec![
            ProviderEvent::ToolCall(ToolCall { id: id.into(), name: "write_file".into(), arguments }),
            ProviderEvent::Done { reason: Some("tool_calls".into()) },
        ]
    }

    /// Records every Herdr request and answers at once.
    #[derive(Default)]
    struct Recorder(Mutex<Vec<Value>>);
    #[async_trait]
    impl HerdrSend for Recorder {
        async fn send(&self, request: &Value, _timeout: Duration) -> bool {
            self.0.lock().unwrap().push(request.clone());
            true
        }
    }
    impl Recorder {
        fn states(&self) -> Vec<String> {
            self.0
                .lock()
                .unwrap()
                .iter()
                .filter(|r| r["method"] == "pane.report_agent")
                .map(|r| r["params"]["state"].as_str().unwrap_or_default().to_string())
                .collect()
        }
        fn find(&self, f: impl Fn(&Value) -> bool) -> Option<Value> {
            self.0.lock().unwrap().iter().find(|r| f(r)).cloned()
        }
    }

    /// A temp project with notes.txt and a config folder; removed when the test ends.
    struct Dirs {
        base: PathBuf,
        project: PathBuf,
        config: PathBuf,
    }
    impl Dirs {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let n = NEXT.fetch_add(1, Ordering::SeqCst);
            let base = std::env::temp_dir().join(format!("marv-cli-tui-{}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&base);
            let (project, config) = (base.join("home/project"), base.join("config"));
            std::fs::create_dir_all(&project).unwrap();
            std::fs::create_dir_all(&config).unwrap();
            std::fs::write(project.join("notes.txt"), "hi\n").unwrap();
            let project = std::fs::canonicalize(project).unwrap();
            Self { base, project, config }
        }
    }
    impl Drop for Dirs {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.base);
            // And again when the test's thread ends: the session's background work (the checkpoint store's prune and
            // its `git gc`) can still be running when the test's body ends, and recreated the folder after it was
            // removed (#89). The thread ends after its runtime has shut down and killed that work.
            AT_TEST_END.with(|later| later.borrow_mut().push(RemoveOnDrop(self.base.clone())));
        }
    }

    struct RemoveOnDrop(PathBuf);
    impl Drop for RemoveOnDrop {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    thread_local! {
        // Dropped when the test's thread ends (libtest gives each test its own thread), passed or panicked.
        static AT_TEST_END: std::cell::RefCell<Vec<RemoveOnDrop>> = const { std::cell::RefCell::new(Vec::new()) };
    }

    /// Ollama with yolo off: every change asks.
    fn asks() -> FileConfig {
        FileConfig {
            provider: Some(ProviderKind::Ollama),
            model: Some("qwen3.5:9b".into()),
            yolo: Some(false),
            diagnostics: Some(false),
            checks: Some(false),
            // A suggestion is one more request after each turn: off, so each test's scripted replies stay its turns'.
            suggestions: Some(false),
            ..FileConfig::default()
        }
    }

    fn startup(dirs: &Dirs, file: Option<FileConfig>, replies: Vec<Vec<ProviderEvent>>, herdr: Option<HerdrReporter>) -> Startup {
        let provider: Arc<dyn Provider> = Arc::new(Scripted(Mutex::new(replies.into())));
        Startup {
            root: dirs.project.clone(),
            home: dirs.base.join("home"),
            config_dir: dirs.config.clone(),
            store: ConfigStore::new(&dirs.config),
            file,
            env: Env::new(),
            version: "9.9.9".into(),
            instructions: None,
            instruction_problems: Vec::new(),
            skills: Vec::new(),
            skill_problems: Vec::new(),
            agents: vec![marv_session::agents::general_purpose()],
            agent_problems: Vec::new(),
            project_files: ProjectFiles::default(),
            project_trust: TrustStatus::Nothing,
            nested_trust: Arc::default(),
            memory: Some((memory_paths(&dirs.config, &dirs.project), Memories::default())),
            mcp: None,
            mcp_problems: Vec::new(),
            hidden_path_folders: Vec::new(),
            sandbox_available: false,
            pre_approved: Vec::new(),
            writable: Vec::new(),
            approval_problems: Vec::new(),
            github: false,
            sessions: Some(Arc::new(SessionStore::new(dirs.config.join("sessions")))),
            trajectories: None,
            herdr,
            mesh: None,
            make_provider: Arc::new(move |_, _| provider.clone()),
            load_models: Arc::new(|_, _| {
                Box::pin(async {
                    Ok(vec![marv_provider::ModelInfo { id: "qwen3.5:9b".into(), local: true, ..Default::default() }])
                })
            }),
            mcp_deps: Arc::new(|_| panic!("no network in tests")),
            open_url: Arc::new(|_| panic!("no browser in tests")),
        }
    }

    /// The next event, or a panic after a while.
    async fn next(rx: &mut UnboundedReceiver<UiEvent>) -> UiEvent {
        tokio::time::timeout(Duration::from_secs(10), rx.recv()).await.expect("an event in time").expect("the channel open")
    }

    /// Events until (and including) the first one `stop` matches.
    async fn until(rx: &mut UnboundedReceiver<UiEvent>, stop: impl Fn(&UiEvent) -> bool) -> Vec<UiEvent> {
        let mut seen = Vec::new();
        loop {
            let event = next(rx).await;
            let done = stop(&event);
            seen.push(event);
            if done {
                return seen;
            }
        }
    }

    fn texts(events: &[UiEvent]) -> Vec<String> {
        events.iter().filter_map(|e| if let UiEvent::Message(m) = e { Some(m.text.clone()) } else { None }).collect()
    }

    // Marv tests/app.test.tsx "/undo" (#16): the project is a temp git repository; turns write notes.txt.
    mod undo {
        use super::*;

        fn repo(dirs: &Dirs) {
            for args in [
                &["init", "-q", "-b", "main"][..],
                &["add", "-A"],
                &["-c", "user.email=t@t", "-c", "user.name=t", "-c", "commit.gpgSign=false", "commit", "-q", "-m", "first"],
            ] {
                std::process::Command::new("git").args(args).current_dir(&dirs.project).output().unwrap();
            }
        }

        fn note(dirs: &Dirs) -> String {
            std::fs::read_to_string(dirs.project.join("notes.txt")).unwrap()
        }

        fn is_message(e: &UiEvent, part: &str) -> bool {
            matches!(e, UiEvent::Message(m) if m.text.contains(part))
        }

        /// A bridge whose first turn ("change the note") wrote notes.txt, approved.
        async fn after_a_turn(dirs: &Dirs) -> (Port, UnboundedReceiver<UiEvent>) {
            repo(dirs);
            let replies = vec![write("w", "notes.txt", "buy bread\n"), say("Changed the note.")];
            let (tx, mut rx) = unbounded_channel();
            let port = Port(Bridge::start(startup(dirs, Some(asks()), replies, None), tx).unwrap());
            port.send("change the note".into());
            let seen = until(&mut rx, |e| matches!(e, UiEvent::Approval { .. })).await;
            let Some(UiEvent::Approval { id, .. }) = seen.last() else { unreachable!() };
            port.answer(*id, view::Decision::Yes);
            until(&mut rx, |e| matches!(e, UiEvent::TurnEnd { .. })).await;
            assert_eq!(note(dirs), "buy bread\n");
            (port, rx)
        }

        #[tokio::test]
        async fn shows_the_turns_files_and_diffs_yes_puts_them_back() {
            let dirs = Dirs::new();
            let (port, mut rx) = after_a_turn(&dirs).await;
            port.run_command("/undo".into());
            let seen = until(&mut rx, |e| matches!(e, UiEvent::Approval { .. })).await;
            let Some(UiEvent::Approval { id, request }) = seen.last() else { unreachable!() };
            assert_eq!(request.preview.title, "Undo turn 1 (\"change the note\")? 1 file");
            assert!(request.once, "no \"don't ask again\"");
            let files = request.preview.files.as_ref().unwrap();
            assert_eq!(files[0].title, "Restore notes.txt");
            let lines: Vec<(view::DiffKind, &str)> =
                files[0].diff.as_ref().unwrap().lines.iter().map(|l| (l.kind, l.text.as_str())).collect();
            assert_eq!(lines, [(view::DiffKind::Del, "buy bread"), (view::DiffKind::Add, "hi")]);
            port.answer(*id, view::Decision::Yes);
            until(&mut rx, |e| is_message(e, "↶ Undid turn 1: 1 file restored")).await;
            assert_eq!(note(&dirs), "hi\n");
            // Nothing more to undo.
            port.run_command("/undo".into());
            until(&mut rx, |e| is_message(e, "Nothing to undo")).await;
        }

        #[tokio::test]
        async fn no_changes_nothing() {
            let dirs = Dirs::new();
            let (port, mut rx) = after_a_turn(&dirs).await;
            port.run_command("/undo".into());
            let seen = until(&mut rx, |e| matches!(e, UiEvent::Approval { .. })).await;
            let Some(UiEvent::Approval { id, .. }) = seen.last() else { unreachable!() };
            port.answer(*id, view::Decision::No);
            until(&mut rx, |e| is_message(e, "Nothing was undone.")).await;
            assert_eq!(note(&dirs), "buy bread\n");
        }

        #[tokio::test]
        async fn with_nothing_to_undo_it_says_why() {
            let dirs = Dirs::new();
            repo(&dirs);
            let (tx, mut rx) = unbounded_channel();
            let port = Port(Bridge::start(startup(&dirs, Some(asks()), vec![], None), tx).unwrap());
            port.run_command("/undo".into());
            let seen = until(&mut rx, |e| is_message(e, "Nothing to undo")).await;
            assert!(texts(&seen).last().unwrap().contains("no turn in this session has changed files yet"));
        }

        #[tokio::test]
        async fn refused_during_a_turn() {
            let dirs = Dirs::new();
            repo(&dirs);
            let replies = vec![write("w", "notes.txt", "x\n"), say("Done.")];
            let (tx, mut rx) = unbounded_channel();
            let port = Port(Bridge::start(startup(&dirs, Some(asks()), replies, None), tx).unwrap());
            port.send("go".into());
            let seen = until(&mut rx, |e| matches!(e, UiEvent::Approval { .. })).await;
            let Some(UiEvent::Approval { id, .. }) = seen.last() else { unreachable!() };
            // The turn waits at its approval: /undo now is refused, and asks nothing.
            port.run_command("/undo".into());
            let seen = until(&mut rx, |e| is_message(e, "before /undo")).await;
            assert!(!seen.iter().any(|e| matches!(e, UiEvent::Approval { .. })));
            port.answer(*id, view::Decision::Yes);
            until(&mut rx, |e| matches!(e, UiEvent::TurnEnd { .. })).await;
            assert_eq!(note(&dirs), "x\n");
        }
    }

    // marv-rs #78: an idle Marv starts a turn when a wired inbox gets a message (opt-in `mesh.wake`).
    mod mesh_wake {
        use super::*;
        use marv_mesh::approval::WireRequest;
        use marv_mesh::switchboard::Switchboard;
        use marv_mesh::{ReceptacleName, WireMode};
        use marv_session::mesh::{WAKES_PER_MINUTE, parent_id};

        const WOKEN: &str = "Woken by a message from worker-1 on agent.parent.inbox";

        /// A served switchboard and a worker plug; stopped and removed on drop.
        struct Mesh {
            sb: Arc<Switchboard>,
            worker: marv_mesh::plug::Plug,
            cancel: CancellationToken,
            dir: PathBuf,
        }
        impl Drop for Mesh {
            fn drop(&mut self) {
                self.cancel.cancel();
                let _ = std::fs::remove_dir_all(&self.dir);
            }
        }
        fn mesh() -> Mesh {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let dir = std::env::temp_dir().join(format!("mt-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::SeqCst)));
            let sb = Arc::new(Switchboard::new("s", dir.clone(), Arc::new(marv_mesh::switchboard::NoTrajectory)));
            let cancel = CancellationToken::new();
            tokio::spawn(Arc::clone(&sb).serve(cancel.clone()));
            let worker = sb.plug_next("worker").unwrap();
            Mesh { sb, worker, cancel, dir }
        }
        fn inbox() -> ReceptacleName {
            ReceptacleName::new("agent.parent.inbox").unwrap()
        }
        impl Mesh {
            /// The user grants worker-1's wire into Marv's inbox.
            async fn wire(&self) {
                let id = self.worker.id.clone();
                let request = WireRequest { from: id.clone(), to: inbox(), mode: WireMode::Send, asked_by: id };
                self.sb.grant(request, marv_mesh::wire::Approver::Tui).await.unwrap();
            }
            async fn send(&self, text: &str) {
                let body = serde_json::json!({ "text": text });
                self.sb.deliver(&self.worker.id, inbox().as_str(), WireMode::Send, body).await.unwrap();
            }
            fn unread(&self) -> usize {
                self.sb.unread(&parent_id()).len()
            }
        }

        fn wakes() -> FileConfig {
            FileConfig { mesh_wake: Some(true), ..asks() }
        }

        fn bridge(
            dirs: &Dirs,
            file: FileConfig,
            replies: Vec<Vec<ProviderEvent>>,
            mesh: &Mesh,
        ) -> (Arc<Bridge>, UnboundedReceiver<UiEvent>) {
            let (tx, rx) = unbounded_channel();
            let mut s = startup(dirs, Some(file), replies, None);
            s.mesh = Some(mesh.sb.clone());
            let bridge = Bridge::start(s, tx).unwrap();
            bridge.watch_mesh();
            (bridge, rx)
        }

        /// What arrives within a short while (nothing more expected).
        async fn quiet(rx: &mut UnboundedReceiver<UiEvent>) -> Vec<UiEvent> {
            let mut seen = Vec::new();
            while let Ok(Some(e)) = tokio::time::timeout(Duration::from_millis(300), rx.recv()).await {
                seen.push(e);
            }
            seen
        }
        fn started(events: &[UiEvent]) -> bool {
            events.iter().any(|e| matches!(e, UiEvent::TurnStart { .. }))
        }

        #[tokio::test]
        async fn off_by_default_a_message_waits_for_the_next_turn() {
            let (dirs, mesh) = (Dirs::new(), mesh());
            let (bridge, mut rx) = bridge(&dirs, asks(), vec![say("Read it.")], &mesh);
            assert!(!bridge.config().mesh_wake);
            mesh.wire().await;
            mesh.send("hi").await;
            assert!(!started(&quiet(&mut rx).await), "no turn while off");
            assert_eq!(mesh.unread(), 1);
            // The user's next turn reads it, as before.
            Port(bridge.clone()).send("anything new?".into());
            until(&mut rx, |e| matches!(e, UiEvent::TurnEnd { .. })).await;
            assert_eq!(mesh.unread(), 0);
        }

        #[tokio::test]
        async fn on_a_message_starts_a_turn_that_says_who_woke_it_and_taints() {
            let (dirs, mesh) = (Dirs::new(), mesh());
            let (bridge, mut rx) = bridge(&dirs, wakes(), vec![say("worker-1 says hi.")], &mesh);
            mesh.wire().await;
            mesh.send("hi").await;
            let seen = until(&mut rx, |e| matches!(e, UiEvent::TurnEnd { .. })).await;
            let shown = texts(&seen);
            assert_eq!(shown.first().map(String::as_str), Some(WOKEN), "{shown:?}");
            assert!(shown.iter().any(|t| t.contains("now contains a mesh message")), "{shown:?}");
            assert!(seen.iter().any(|e| matches!(e, UiEvent::Assistant { text } if text == "worker-1 says hi.")));
            assert_eq!(mesh.unread(), 0);
            assert!(bridge.session().tainted().contains(&marv_core::Provenance::Mesh));
        }

        #[tokio::test]
        async fn never_while_an_approval_waits() {
            let (dirs, mesh) = (Dirs::new(), mesh());
            let (bridge, mut rx) = bridge(&dirs, wakes(), vec![say("x")], &mesh);
            mesh.wire().await;
            // A wire asked for from another process waits for the person, the session idle.
            let ask = WireRequest {
                from: marv_mesh::PlugId::new("peer-alpha").unwrap(),
                to: inbox(),
                mode: WireMode::Send,
                asked_by: marv_mesh::PlugId::new("peer-alpha").unwrap(),
            };
            let approver = TuiApprover(bridge.activity.clone());
            let waiting =
                tokio::spawn(
                    async move { marv_tools::Approver::approve(&approver, marv_session::mesh::wire_approval(&ask)).await },
                );
            let seen = until(&mut rx, |e| matches!(e, UiEvent::Approval { .. })).await;
            let Some(UiEvent::Approval { id, .. }) = seen.last() else { unreachable!() };
            mesh.send("hi").await;
            assert!(!started(&quiet(&mut rx).await), "no turn under an open approval");
            assert_eq!(mesh.unread(), 1, "it waits for the next turn");
            Port(bridge.clone()).answer(*id, view::Decision::No);
            waiting.await.unwrap();
        }

        /// #124: a radio message straight into the radio queue, as a link delivers one.
        fn radio_message(mesh: &Mesh, seq: u64, to_all: bool, text: &str) {
            let message =
                marv_mesh::radio::RadioMessage::msg(&format!("beta:{seq}"), "beta", to_all, marv_mesh::radio::By::User, 0, text);
            assert!(mesh.sb.deliver_radio(message, 50));
        }

        /// #124: with wake on, an addressed radio message starts a turn (Marv's line says who, `(user)` shown), and a
        /// broadcast doesn't; the woken turn reads both.
        #[tokio::test]
        async fn an_addressed_radio_message_wakes_and_a_broadcast_doesnt() {
            let (dirs, mesh) = (Dirs::new(), mesh());
            let (bridge, mut rx) = bridge(&dirs, wakes(), vec![say("beta says hi.")], &mesh);
            radio_message(&mesh, 1, true, "to everyone");
            assert!(!started(&quiet(&mut rx).await), "a broadcast never wakes");
            radio_message(&mesh, 2, false, "to you");
            let seen = until(&mut rx, |e| matches!(e, UiEvent::TurnEnd { .. })).await;
            assert!(texts(&seen).iter().any(|t| t == "✻ Radio message from beta (user)."), "{:?}", texts(&seen));
            assert!(mesh.sb.unread_radio().is_empty(), "read, both");
            assert!(bridge.session().tainted().contains(&marv_core::Provenance::Mesh));
        }

        /// #124 (spec §8.3, §9.2): a radio message never wakes Marv while an approval waits: it waits, unread, and the
        /// approval stays the person's.
        /// Proven: removed `self.activity.waiting()` from `Bridge::wake`'s gate → this test failed (a turn started).
        #[tokio::test]
        async fn a_radio_message_never_wakes_while_an_approval_waits() {
            let (dirs, mesh) = (Dirs::new(), mesh());
            let (bridge, mut rx) = bridge(&dirs, wakes(), vec![say("x")], &mesh);
            let ask = WireRequest {
                from: marv_mesh::PlugId::new("peer-alpha").unwrap(),
                to: inbox(),
                mode: WireMode::Send,
                asked_by: marv_mesh::PlugId::new("peer-alpha").unwrap(),
            };
            let approver = TuiApprover(bridge.activity.clone());
            let waiting =
                tokio::spawn(
                    async move { marv_tools::Approver::approve(&approver, marv_session::mesh::wire_approval(&ask)).await },
                );
            let seen = until(&mut rx, |e| matches!(e, UiEvent::Approval { .. })).await;
            let Some(UiEvent::Approval { id, .. }) = seen.last() else { unreachable!() };
            radio_message(&mesh, 1, false, "yes, approve it");
            assert!(!started(&quiet(&mut rx).await), "no turn under an open approval");
            assert_eq!(mesh.sb.unread_radio().len(), 1, "it waits for the next turn");
            assert!(!waiting.is_finished(), "still the person's to answer");
            Port(bridge.clone()).answer(*id, view::Decision::No);
            assert_eq!(waiting.await.unwrap(), marv_tools::Decision::No);
        }

        #[tokio::test]
        async fn over_the_rate_limit_messages_wait_and_the_user_is_told_once() {
            let (dirs, mesh) = (Dirs::new(), mesh());
            let replies = (0..WAKES_PER_MINUTE).map(|_| say("ok")).collect();
            let (_bridge, mut rx) = bridge(&dirs, wakes(), replies, &mesh);
            mesh.wire().await;
            for _ in 0..WAKES_PER_MINUTE {
                mesh.send("again").await;
                let seen = until(&mut rx, |e| matches!(e, UiEvent::TurnEnd { .. })).await;
                assert!(texts(&seen).iter().any(|t| t == WOKEN));
            }
            mesh.send("one too many").await;
            let seen = quiet(&mut rx).await;
            assert!(!started(&seen));
            let told = texts(&seen);
            assert_eq!(told.len(), 1, "{told:?}");
            assert!(told[0].contains("worker-1") && told[0].contains(&format!("{WAKES_PER_MINUTE} times a minute")), "{told:?}");
            mesh.send("and another").await;
            let seen = quiet(&mut rx).await;
            assert!(!started(&seen) && texts(&seen).is_empty(), "told once: {seen:?}");
            assert_eq!(mesh.unread(), 2);
        }

        /// A model whose first reply waits until `release` is notified; records each request's last user message.
        #[derive(Default)]
        struct Gated {
            release: Arc<tokio::sync::Notify>,
            held: AtomicBool,
            replies: Mutex<VecDeque<Vec<ProviderEvent>>>,
            seen: Mutex<Vec<String>>,
        }
        impl Provider for Gated {
            fn name(&self) -> &str {
                "gated"
            }
            fn context_length(&self) -> Option<u64> {
                None
            }
            fn stream(&self, history: &[ChatTurn], _options: StreamOptions) -> EventStream {
                let last =
                    history.iter().rev().find_map(|t| if let ChatTurn::User { text } = t { Some(text.clone()) } else { None });
                self.seen.lock().unwrap().push(last.unwrap_or_default());
                let reply = self.replies.lock().unwrap().pop_front().unwrap_or_else(|| say("(script ended)"));
                let (hold, release) = (!self.held.swap(true, Ordering::SeqCst), self.release.clone());
                Box::pin(
                    futures::stream::once(async move {
                        if hold {
                            release.notified().await;
                        }
                        futures::stream::iter(reply)
                    })
                    .flatten(),
                )
            }
        }

        fn gated_bridge(dirs: &Dirs, mesh: &Mesh, model: Arc<Gated>) -> (Arc<Bridge>, UnboundedReceiver<UiEvent>) {
            let (tx, rx) = unbounded_channel();
            let mut s = startup(dirs, Some(wakes()), vec![], None);
            s.mesh = Some(mesh.sb.clone());
            s.make_provider = Arc::new(move |_, _| model.clone() as Arc<dyn Provider>);
            let bridge = Bridge::start(s, tx).unwrap();
            bridge.watch_mesh();
            (bridge, rx)
        }
        fn ends(events: &[UiEvent]) -> Vec<String> {
            events.iter().filter_map(|e| if let UiEvent::TurnEnd { turn, .. } = e { Some(turn.clone()) } else { None }).collect()
        }

        /// Greptile on #110: the user presses Enter after a wake took the session but before the App saw its
        /// TurnStart. The input isn't lost and the woken turn isn't ended for the App: it runs when the wake ends.
        #[tokio::test]
        async fn input_typed_while_a_wake_starts_runs_after_it() {
            let (dirs, mesh) = (Dirs::new(), mesh());
            let model = Arc::new(Gated::default());
            model.replies.lock().unwrap().extend([say("worker-1 says hi."), say("Here's your answer.")]);
            let (bridge, mut rx) = gated_bridge(&dirs, &mesh, model.clone());
            mesh.wire().await;
            mesh.send("hi").await;
            until(&mut rx, |e| matches!(e, UiEvent::TurnStart { .. })).await;
            // The woken turn is waiting on its model: the App submits as if idle.
            Port(bridge.clone()).send("what's new?".into());
            let seen = quiet(&mut rx).await;
            assert!(ends(&seen).is_empty(), "nothing ends the woken turn: {seen:?}");
            assert!(texts(&seen).iter().any(|t| t.contains("yours goes next")), "{seen:?}");
            model.release.notify_one();
            let seen = until(&mut rx, |e| matches!(e, UiEvent::Assistant { text } if text == "Here's your answer.")).await;
            let seen = [seen, until(&mut rx, |e| matches!(e, UiEvent::TurnEnd { .. })).await].concat();
            let ended = ends(&seen);
            assert_eq!(ended.len(), 2, "{seen:?}");
            assert!(ended.iter().all(|t| !t.is_empty()), "only real turns end: {ended:?}");
            assert!(seen.iter().any(|e| matches!(e, UiEvent::Assistant { text } if text == "worker-1 says hi.")));
            let asked = model.seen.lock().unwrap().clone();
            assert_eq!(asked.len(), 2, "{asked:?}");
            assert!(asked[1].ends_with("what's new?"), "{asked:?}");
        }

        /// The other way round: a submission on its way first, a message then can't start a turn alongside it.
        #[tokio::test]
        async fn a_wake_never_starts_while_a_submission_runs() {
            let (dirs, mesh) = (Dirs::new(), mesh());
            let model = Arc::new(Gated::default());
            model.replies.lock().unwrap().extend([say("done")]);
            let (bridge, mut rx) = gated_bridge(&dirs, &mesh, model.clone());
            mesh.wire().await;
            Port(bridge.clone()).send("go".into());
            until(&mut rx, |e| matches!(e, UiEvent::TurnStart { .. })).await;
            mesh.send("hi").await;
            let seen = quiet(&mut rx).await;
            assert!(!texts(&seen).iter().any(|t| t == WOKEN), "{seen:?}");
            model.release.notify_one();
            let seen = until(&mut rx, |e| matches!(e, UiEvent::TurnEnd { .. })).await;
            assert!(!texts(&seen).iter().any(|t| t == WOKEN));
            assert!(model.seen.lock().unwrap().len() <= 2);
        }

        /// A message that lands after a turn's last inbox read (here, during its final reply: no step follows, so it
        /// isn't drained) rang the mail bell while busy. Once that turn ends and Marv is idle, it wakes a turn: it
        /// doesn't sit unread until some other message arrives.
        #[tokio::test]
        async fn a_message_that_lands_during_a_turns_last_reply_wakes_once_it_ends() {
            let (dirs, mesh) = (Dirs::new(), mesh());
            let model = Arc::new(Gated::default());
            model.replies.lock().unwrap().extend([say("done"), say("worker-1 says hi.")]);
            let (bridge, mut rx) = gated_bridge(&dirs, &mesh, model.clone());
            mesh.wire().await;
            Port(bridge.clone()).send("go".into());
            until(&mut rx, |e| matches!(e, UiEvent::TurnStart { .. })).await;
            mesh.send("hi").await;
            quiet(&mut rx).await;
            model.release.notify_one();
            until(&mut rx, |e| matches!(e, UiEvent::TurnEnd { .. })).await;
            let seen = until(&mut rx, |e| matches!(e, UiEvent::TurnEnd { .. })).await;
            assert!(texts(&seen).iter().any(|t| t == WOKEN), "{seen:?}");
            assert!(seen.iter().any(|e| matches!(e, UiEvent::Assistant { text } if text == "worker-1 says hi.")));
            assert_eq!(mesh.unread(), 0);
        }

        /// Greptile on #110: once the App is leaving (its flush), a message starts no turn.
        #[tokio::test]
        async fn no_wake_after_the_app_exits() {
            let (dirs, mesh) = (Dirs::new(), mesh());
            let (bridge, mut rx) = bridge(&dirs, wakes(), vec![say("x")], &mesh);
            mesh.wire().await;
            Port(bridge.clone()).flush().await;
            mesh.send("hi").await;
            assert!(!started(&quiet(&mut rx).await), "no turn after exit");
            assert_eq!(mesh.unread(), 1);
            // And a wake asked for directly is refused too.
            bridge.wake();
            assert!(!started(&quiet(&mut rx).await));
        }

        #[tokio::test]
        async fn mesh_wake_on_saves_and_mesh_shows_the_wires() {
            let (dirs, mesh) = (Dirs::new(), mesh());
            let (bridge, mut rx) = bridge(&dirs, asks(), vec![], &mesh);
            mesh.wire().await;
            let port = Port(bridge.clone());
            port.run_command("/mesh wake on".into());
            let seen = until(&mut rx, |e| matches!(e, UiEvent::Message(_))).await;
            assert!(texts(&seen)[0].starts_with("Mesh wake on: a message on a wired inbox starts a turn"), "{seen:?}");
            assert_eq!(ConfigStore::new(&dirs.config).load().unwrap().unwrap().mesh_wake, Some(true));
            assert!(bridge.config().mesh_wake);
            port.run_command("/mesh".into());
            let shown = texts(&[next(&mut rx).await]);
            assert!(shown[0].contains("- `worker-1 -> agent.parent.inbox`") && shown[0].contains("**Wake**: on"), "{shown:?}");
            port.run_command("/mesh wake off".into());
            until(&mut rx, |e| matches!(e, UiEvent::Message(_))).await;
            assert!(!bridge.config().mesh_wake);
        }
    }

    // Marv tests/herdr.test.tsx: "App in a Herdr pane".
    #[tokio::test]
    async fn reports_idle_working_blocked_at_an_approval_and_idle_when_done() {
        let dirs = Dirs::new();
        let recorder = Arc::new(Recorder::default());
        let target = HerdrTarget { socket_path: "/unused".into(), pane_id: "w1:p7".into() };
        let herdr = HerdrReporter::new(target, Some(recorder.clone() as Arc<dyn HerdrSend>));
        let replies = vec![write("c1", "made.txt", "hello\n"), say("Created it.")];
        let (tx, mut rx) = unbounded_channel();
        let bridge = Bridge::start(startup(&dirs, Some(asks()), replies, Some(herdr.clone())), tx).unwrap();
        bridge.report_state();
        bridge.report_session().await;
        herdr.settled().await;
        assert_eq!(recorder.states(), ["idle"]);
        let session = recorder.find(|r| r["method"] == "pane.report_agent_session").unwrap();
        let id = session["params"]["agent_session_id"].as_str().unwrap();
        let path = bridge.sessions.as_ref().unwrap().file(&dirs.project, id);
        assert_eq!(session["params"]["agent_session_path"], path.to_string_lossy().as_ref());

        let port = Port(bridge.clone());
        port.send("make a file".into());
        let seen = until(&mut rx, |e| matches!(e, UiEvent::Approval { .. })).await;
        let Some(UiEvent::Approval { id, request }) = seen.last() else { unreachable!() };
        assert_eq!((request.tool.as_str(), request.label.as_str()), ("write_file", "made.txt"));
        assert!(request.preview.diff.is_some(), "the approval shows the diff");
        herdr.settled().await;
        let blocked = recorder.find(|r| r["params"]["state"] == "blocked").unwrap();
        assert_eq!(blocked["params"]["message"], "made.txt");

        port.answer(*id, view::Decision::Yes);
        let seen = until(&mut rx, |e| matches!(e, UiEvent::TurnEnd { .. })).await;
        let end = seen.iter().find_map(|e| if let UiEvent::ToolEnd { status, .. } = e { Some(status.clone()) } else { None });
        let end = end.expect("the write's tool end");
        assert_eq!(end.status, view::ToolState::Done);
        let diff = end.diff.expect("the diff view");
        assert!(diff.lines.iter().any(|l| l.kind == view::DiffKind::Add && l.text == "hello"));
        assert!(seen.iter().any(|e| matches!(e, UiEvent::Assistant { text } if text == "Created it.")));
        assert_eq!(std::fs::read_to_string(dirs.project.join("made.txt")).unwrap(), "hello\n");

        herdr.settled().await;
        let states = recorder.states();
        let at = |s: &str| states.iter().position(|x| x == s).unwrap();
        assert!(at("working") < at("blocked"));
        assert_eq!(states.last().unwrap(), "idle");
    }

    /// A pane send is a prompt, not a command: while the palette, setup or a half-typed slash command has the
    /// keyboard, Herdr hears working, not idle, so nothing is typed into it; idle again once the prompt is free.
    #[tokio::test]
    async fn a_held_keyboard_is_working_to_herdr_and_free_again_is_idle() {
        let dirs = Dirs::new();
        let recorder = Arc::new(Recorder::default());
        let target = HerdrTarget { socket_path: "/unused".into(), pane_id: "w1:p7".into() };
        let herdr = HerdrReporter::new(target, Some(recorder.clone() as Arc<dyn HerdrSend>));
        let (tx, _rx) = unbounded_channel();
        let bridge = Bridge::start(startup(&dirs, Some(asks()), vec![], Some(herdr.clone())), tx).unwrap();
        bridge.report_state();
        herdr.settled().await;
        let port = Port(bridge.clone());
        port.keyboard_held(true);
        assert_eq!(bridge.activity.herdr_state(), (HerdrState::Working, None));
        assert_eq!(bridge.activity.session_state(), (HerdrState::Idle, None), "the radio still shows idle");
        herdr.settled().await;
        assert_eq!(recorder.states().last().unwrap(), "working");
        port.keyboard_held(false);
        assert_eq!(bridge.activity.herdr_state(), (HerdrState::Idle, None));
        herdr.settled().await;
        assert_eq!(recorder.states().last().unwrap(), "idle");
    }

    fn calls(id: &str, name: &str, args: serde_json::Value) -> Vec<ProviderEvent> {
        vec![
            ProviderEvent::ToolCall(ToolCall { id: id.into(), name: name.into(), arguments: args.to_string() }),
            ProviderEvent::Done { reason: Some("tool_calls".into()) },
        ]
    }

    // Marv #47, #52: in the TUI a host command and a github write each ask, with their door (the approval shows the
    // door's warning line, marv_tui::ui::approval::door_warning). Yolo on: neither runs unasked.
    #[tokio::test]
    async fn a_host_command_and_a_github_write_ask_with_their_door() {
        if !marv_sandbox::sandbox_available() {
            eprintln!("skipped: bubblewrap isn't available here");
            return;
        }
        let dirs = Dirs::new();
        let outside = dirs.base.join("outside-marker");
        let replies = vec![
            // A "no" ends the turn there: no reply follows either call.
            calls("h1", "bash", serde_json::json!({"command": format!("touch {}", outside.display()), "sandbox": false})),
            calls("g1", "github", serde_json::json!({"args": ["pr", "comment", "1", "--body", "hi"]})),
        ];
        let mut start = startup(&dirs, Some(FileConfig { yolo: Some(true), ..asks() }), replies, None);
        start.github = true;
        let (tx, mut rx) = unbounded_channel();
        let bridge = Bridge::start(start, tx).unwrap();
        let port = Port(bridge.clone());
        for (door, tool, once) in [(marv_core::Door::Host, "bash", true), (marv_core::Door::Github, "github", false)] {
            port.send("go".into());
            let seen = until(&mut rx, |e| matches!(e, UiEvent::Approval { .. })).await;
            let Some(UiEvent::Approval { id, request }) = seen.last() else { unreachable!() };
            assert_eq!((request.tool.as_str(), request.door, request.once), (tool, Some(door), once));
            port.answer(*id, view::Decision::No);
            until(&mut rx, |e| matches!(e, UiEvent::TurnEnd { .. })).await;
        }
        assert!(!outside.exists());
    }

    #[tokio::test]
    async fn the_status_bar_counts_running_processes_and_processes_stop_takes_one_off() {
        // Marv's app.test.tsx (#51): the status bar counts it; /processes stops it.
        if !marv_sandbox::sandbox_available() || std::process::Command::new("python3").arg("--version").output().is_err() {
            eprintln!("skipped: needs bubblewrap and python3");
            return;
        }
        let dirs = Dirs::new();
        // network: true runs bash directly in the sandbox (no relay binary needed in a unit test); a server that listens
        // ends the start's wait for a port at once.
        let command = "python3 -m http.server 0 --bind 127.0.0.1";
        let start = serde_json::json!({"action": "start", "command": command, "name": "web", "network": true});
        let replies = vec![
            vec![
                ProviderEvent::ToolCall(ToolCall { id: "p1".into(), name: "process".into(), arguments: start.to_string() }),
                ProviderEvent::Done { reason: Some("tool_calls".into()) },
            ],
            say("It's up."),
        ];
        let (tx, mut rx) = unbounded_channel();
        let bridge = Bridge::start(startup(&dirs, Some(asks()), replies, None), tx).unwrap();
        assert_eq!(bridge.status_info().processes, 0);
        let port = Port(bridge.clone());
        port.send("start the server".into());
        let seen = until(&mut rx, |e| matches!(e, UiEvent::Approval { .. })).await;
        let Some(UiEvent::Approval { id, request }) = seen.last() else { unreachable!() };
        assert_eq!(request.preview.title, "Start a background process (web)");
        port.answer(*id, view::Decision::Yes);
        until(&mut rx, |e| matches!(e, UiEvent::StatusInfo(info) if info.processes == 1)).await;
        until(&mut rx, |e| matches!(e, UiEvent::TurnEnd { .. })).await;
        port.run_command("/processes stop web".into());
        let seen = until(&mut rx, |e| matches!(e, UiEvent::StatusInfo(info) if info.processes == 0)).await;
        let seen =
            [seen, until(&mut rx, |e| texts(std::slice::from_ref(e)).iter().any(|t| t.contains("Stopped web."))).await].concat();
        assert!(texts(&seen).iter().any(|t| t == "Stopped web."), "{:?}", texts(&seen));
        bridge.session().stop_processes().await;
    }

    #[tokio::test]
    async fn no_declines_and_stops_the_turn_and_the_queue_forgets_it() {
        let dirs = Dirs::new();
        let (tx, mut rx) = unbounded_channel();
        let bridge = Bridge::start(startup(&dirs, Some(asks()), vec![write("c1", "made.txt", "x")], None), tx).unwrap();
        let port = Port(bridge.clone());
        port.send("make a file".into());
        let seen = until(&mut rx, |e| matches!(e, UiEvent::Approval { .. })).await;
        let Some(UiEvent::Approval { id, .. }) = seen.last() else { unreachable!() };
        assert_eq!(bridge.activity.herdr_state(), (HerdrState::Blocked, Some("made.txt".into())));
        port.answer(*id, view::Decision::No);
        let seen = until(&mut rx, |e| matches!(e, UiEvent::TurnEnd { .. })).await;
        assert!(seen.iter().any(|e| matches!(e, UiEvent::ToolEnd { status, .. } if status.status == view::ToolState::Declined)));
        assert!(seen.iter().any(|e| matches!(e, UiEvent::Done(StopReason::Declined))));
        assert!(!dirs.project.join("made.txt").exists());
        assert_eq!(bridge.activity.herdr_state(), (HerdrState::Idle, None));
    }

    #[tokio::test]
    async fn esc_at_an_approval_interrupts_and_the_session_is_free_again() {
        let dirs = Dirs::new();
        let (tx, mut rx) = unbounded_channel();
        let replies = vec![write("c1", "made.txt", "x"), say("again")];
        let bridge = Bridge::start(startup(&dirs, Some(asks()), replies, None), tx).unwrap();
        let port = Port(bridge.clone());
        port.send("make a file".into());
        until(&mut rx, |e| matches!(e, UiEvent::Approval { .. })).await;
        // What the App does on Esc: abort (its queue is declined by the session's stop, or by its own answers).
        port.abort();
        let seen = until(&mut rx, |e| matches!(e, UiEvent::TurnEnd { .. })).await;
        assert!(seen.iter().any(|e| matches!(e, UiEvent::Done(StopReason::Aborted))));
        assert_eq!(bridge.activity.herdr_state(), (HerdrState::Idle, None));
        assert!(bridge.activity.asks.lock().unwrap().is_empty(), "a stopped run's ask is forgotten");
        port.send("hello".into());
        let seen = until(&mut rx, |e| matches!(e, UiEvent::TurnEnd { .. })).await;
        assert!(seen.iter().any(|e| matches!(e, UiEvent::Assistant { text } if text == "again")));
    }

    #[tokio::test]
    async fn after_a_finished_turn_the_next_message_is_suggested_unless_thats_off() {
        let dirs = Dirs::new();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let on = FileConfig { suggestions: Some(true), ..asks() };
        let port = Port(Bridge::start(startup(&dirs, Some(on), vec![say("Fixed it."), say("run the tests")], None), tx).unwrap());
        port.send("fix it".into());
        let events = until(&mut rx, |e| matches!(e, UiEvent::Suggestion(_))).await;
        assert!(matches!(events.last(), Some(UiEvent::Suggestion(t)) if t == "run the tests"), "{events:?}");
        assert!(events.iter().any(|e| matches!(e, UiEvent::TurnEnd { .. })), "after the turn");

        let dirs = Dirs::new();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let port =
            Port(Bridge::start(startup(&dirs, Some(asks()), vec![say("Fixed it."), say("run the tests")], None), tx).unwrap());
        port.send("fix it".into());
        until(&mut rx, |e| matches!(e, UiEvent::TurnEnd { .. })).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        while let Ok(event) = rx.try_recv() {
            assert!(!matches!(event, UiEvent::Suggestion(_)), "suggestions are off");
        }
    }

    #[tokio::test]
    async fn a_session_comes_back_with_its_transcript() {
        let dirs = Dirs::new();
        {
            let (tx, mut rx) = unbounded_channel();
            let bridge = Bridge::start(startup(&dirs, Some(asks()), vec![say("Hi there.")], None), tx).unwrap();
            Port(bridge.clone()).send("hello".into());
            until(&mut rx, |e| matches!(e, UiEvent::TurnEnd { .. })).await;
            Port(bridge).flush().await;
        }
        let (tx, mut rx) = unbounded_channel();
        let bridge = Bridge::start(startup(&dirs, Some(asks()), vec![], None), tx).unwrap();
        bridge.resume_latest().await;
        let UiEvent::Resumed(resumed) = next(&mut rx).await else { panic!("not resumed") };
        let shown: Vec<(Role, &str)> = resumed.transcript.iter().map(|m| (m.role, m.text.as_str())).collect();
        assert_eq!(shown, [(Role::User, "hello"), (Role::Assistant, "Hi there.")]);
        assert!(matches!(next(&mut rx).await, UiEvent::StatusInfo(_)));

        // /resume lists the others, not this one.
        Port(bridge.clone()).run_command("/resume".into());
        let UiEvent::Sessions(list) = next(&mut rx).await else { panic!("not the list") };
        assert!(list.iter().all(|s| s.id != bridge.session.id()));
    }

    /// #74: in a Marv started with MARV_YOLO=off, /yolo on says it stays off, names the variable, and writes nothing.
    #[tokio::test]
    async fn yolo_on_is_refused_under_marv_yolo_off() {
        let dirs = Dirs::new();
        let (tx, mut rx) = unbounded_channel();
        let mut start = startup(&dirs, Some(FileConfig { yolo: Some(false), ..asks() }), vec![], None);
        start.env.insert("MARV_YOLO".into(), "off".into());
        let bridge = Bridge::start(start, tx).unwrap();
        Port(bridge.clone()).run_command("/yolo on".into());
        let seen = until(&mut rx, |e| matches!(e, UiEvent::Message(_))).await;
        assert!(texts(&seen).iter().any(|t| t.contains("MARV_YOLO=off")), "{:?}", texts(&seen));
        assert!(ConfigStore::new(&dirs.config).load().unwrap().is_none_or(|f| f.yolo != Some(true)), "nothing written");
        assert!(!bridge.status_info().yolo);
    }

    #[tokio::test]
    async fn commands_change_the_config_and_say_so() {
        let dirs = Dirs::new();
        let (tx, mut rx) = unbounded_channel();
        let bridge = Bridge::start(startup(&dirs, Some(asks()), vec![], None), tx).unwrap();
        let port = Port(bridge.clone());

        port.run_command("/yolo on".into());
        let seen = until(&mut rx, |e| matches!(e, UiEvent::Message(_))).await;
        assert!(seen.iter().any(|e| matches!(e, UiEvent::StatusInfo(s) if s.yolo)));
        assert!(texts(&seen)[0].starts_with("Yolo on: file edits run without asking"));
        assert_eq!(ConfigStore::new(&dirs.config).load().unwrap().unwrap().yolo, Some(true));

        port.run_command("/model other:7b".into());
        let seen = until(&mut rx, |e| matches!(e, UiEvent::Message(_))).await;
        assert!(texts(&seen)[0].ends_with("· Ollama · other:7b"));
        assert_eq!(bridge.config().model, "other:7b");

        port.run_command("/model".into());
        let UiEvent::OpenSetup(ctx) = next(&mut rx).await else { panic!("not setup") };
        assert_eq!((ctx.start, ctx.current), (SetupStart::Model, Some((ProviderId::Ollama, "other:7b".into()))));

        port.run_command("/remember use tabs".into());
        assert_eq!(texts(&[next(&mut rx).await]), [format!("Saved to personal memory. {NEXT_TIME}")]);
        port.run_command("/forget tabs".into());
        assert_eq!(texts(&[next(&mut rx).await]), ["Forgot: use tabs"]);

        port.run_command("/mcp trust".into());
        assert_eq!(texts(&[next(&mut rx).await]), ["No MCP servers are waiting to be trusted (/mcp lists them)."]);
        port.run_command("/nope".into());
        assert!(matches!(next(&mut rx).await, UiEvent::Message(m) if m.is_error && m.text.starts_with("Unknown command: /nope")));
        port.run_command("/good".into());
        assert!(matches!(next(&mut rx).await, UiEvent::Message(m) if m.is_error));
        port.run_command("/exit".into());
        assert_eq!(next(&mut rx).await, UiEvent::Exit);
    }

    // #44: the palette lists the settings with their values, and a toggle goes the way its slash command goes.
    #[tokio::test]
    async fn the_palettes_settings_show_their_values_and_a_toggle_is_saved_and_announced_like_its_command() {
        let dirs = Dirs::new();
        let (tx, mut rx) = unbounded_channel();
        let bridge = Bridge::start(startup(&dirs, Some(asks()), vec![], None), tx).unwrap();
        let port = Port(bridge.clone());
        let setting = |name: &str| port.settings().into_iter().find(|s| s.name == name).unwrap();
        let yolo = setting("Yolo");
        assert_eq!((yolo.value.as_str(), &yolo.action, yolo.while_busy), ("off", &SettingAction::Run("/yolo on".into()), true));
        assert_eq!(setting("Model").value, "qwen3.5:9b");
        assert_eq!(setting("Model").action, SettingAction::Run("/model".into()));
        assert_eq!(setting("Step limit").action, SettingAction::Prefill("/steps ".into()));

        let SettingAction::Run(line) = yolo.action else { unreachable!() };
        port.run_command(line);
        let seen = until(&mut rx, |e| matches!(e, UiEvent::Message(_))).await;
        assert!(seen.iter().any(|e| matches!(e, UiEvent::StatusInfo(s) if s.yolo)));
        assert!(texts(&seen)[0].starts_with("Yolo on: file edits run without asking"));
        assert_eq!(ConfigStore::new(&dirs.config).load().unwrap().unwrap().yolo, Some(true));
        // Asked again when the palette next opens: the new value, and the flip back.
        assert_eq!(setting("Yolo").value, "on");
        assert_eq!(setting("Yolo").action, SettingAction::Run("/yolo off".into()));

        // The menu's entries carry what the palette needs: built-ins with their busy rule.
        let menu = port.commands();
        let entry = |name: &str| menu.iter().find(|c| c.name == name).unwrap().clone();
        assert!(entry("cost").while_busy && !entry("undo").while_busy);
        assert_eq!(entry("cost").kind, CommandKind::Command);
    }

    #[tokio::test]
    async fn compact_runs_like_a_turn_and_clear_starts_a_new_conversation() {
        let dirs = Dirs::new();
        let (tx, mut rx) = unbounded_channel();
        let bridge = Bridge::start(startup(&dirs, Some(asks()), vec![], None), tx).unwrap();
        let port = Port(bridge.clone());
        port.run_command("/compact".into());
        let seen = until(&mut rx, |e| matches!(e, UiEvent::TurnEnd { .. })).await;
        assert!(matches!(seen[0], UiEvent::TurnStart { .. }));
        assert!(seen.iter().any(|e| matches!(
            e,
            UiEvent::Compaction { automatic: false, outcome: view::CompactOutcome::NotCompacted { reason, .. } } if reason == "empty"
        )));
        let before = bridge.session.id();
        port.run_command("/clear".into());
        // (The session may say something changed first: the status bar is sent again.)
        let seen = until(&mut rx, |e| matches!(e, UiEvent::Welcome(_))).await;
        assert!(seen.contains(&UiEvent::Clear));
        assert_ne!(bridge.session.id(), before);
    }

    fn notices(events: &[UiEvent]) -> Vec<Notice> {
        events.iter().filter_map(|e| if let UiEvent::Message(m) = e { m.notice.clone() } else { None }).collect()
    }
    fn is_hooks_notice(e: &UiEvent) -> bool {
        matches!(e, UiEvent::Message(m) if m.notice.as_ref().is_some_and(|n| n.title.starts_with("Session-start hooks")))
    }

    // #108: the user's hooks run at start and again after /clear, each time shown as one line; a project's hooks are
    // told about and never run.
    #[tokio::test]
    async fn session_start_hooks_show_at_start_and_after_clear_and_a_projects_are_refused() {
        let dirs = Dirs::new();
        let count = dirs.base.join("runs.txt");
        let pwned = dirs.project.join("PWNED");
        std::fs::create_dir_all(dirs.project.join(".marv")).unwrap();
        let planted = serde_json::json!({
            "hooks": {"sessionStart": [format!("touch '{}'", pwned.display())]},
            "approve": ["network"],
        });
        std::fs::write(dirs.project.join(".marv/config.json"), planted.to_string()).unwrap();
        let command = format!("echo x >> '{0}'; echo \"queue run $(wc -l < '{0}')\"", count.display());
        let file = FileConfig {
            hooks: Some(marv_session::hooks::Hooks { session_start: vec![marv_session::hooks::HookCommand::new(command)] }),
            ..asks()
        };
        let (tx, mut rx) = unbounded_channel();
        let bridge = Bridge::start(startup(&dirs, Some(file), vec![], None), tx).unwrap();
        bridge.announce(None);
        let startup = notices(&until(&mut rx, |e| matches!(e, UiEvent::Message(m) if m.notice.is_some())).await);
        assert!(startup[0].items.iter().any(|i| i.text.contains(".marv/config.json has \"hooks\": not run")), "{startup:?}");
        // #98: a project's "approve" is told about and never read (nothing pre-approved).
        assert!(startup[0].items.iter().any(|i| i.text.contains("has \"approve\": ignored")), "{startup:?}");
        assert!(!startup[0].items.iter().any(|i| i.text.starts_with("Pre-approved")), "{startup:?}");
        bridge.show_hooks();
        let shown = notices(&until(&mut rx, is_hooks_notice).await).pop().unwrap();
        assert_eq!(shown.items.len(), 1);
        assert!(shown.items[0].text.ends_with("ms") && shown.items[0].text.contains(": 1 line, "), "{shown:?}");
        Port(bridge.clone()).run_command("/clear".into());
        until(&mut rx, is_hooks_notice).await;
        assert_eq!(std::fs::read_to_string(&count).unwrap().lines().count(), 2);
        assert!(!pwned.exists(), "the project's hook ran");
    }

    #[tokio::test]
    async fn first_run_opens_setup_and_setup_saves_the_file() {
        let dirs = Dirs::new();
        let (tx, mut rx) = unbounded_channel();
        let bridge = Bridge::start(startup(&dirs, None, vec![], None), tx).unwrap();
        bridge.announce(None);
        let UiEvent::OpenSetup(ctx) = next(&mut rx).await else { panic!("not setup") };
        assert_eq!((ctx.start, ctx.current.clone()), (SetupStart::FirstRun, None));
        let port = Port(bridge.clone());
        port.load_models(ProviderId::Ollama);
        let UiEvent::Models(Ok(models)) = next(&mut rx).await else { panic!("no models") };
        assert_eq!(models[0].id, "qwen3.5:9b");
        port.complete_setup(SetupAnswer { provider: ProviderId::Ollama, model: "qwen3.5:9b".into(), api_key: None });
        let seen = until(&mut rx, |e| matches!(e, UiEvent::Message(_))).await;
        assert!(texts(&seen)[0].starts_with("Saved to ") && texts(&seen)[0].ends_with("· Ollama · qwen3.5:9b"));
        let saved = ConfigStore::new(&dirs.config).load().unwrap().unwrap();
        assert_eq!((saved.provider, saved.model.as_deref()), (Some(ProviderKind::Ollama), Some("qwen3.5:9b")));
    }

    // #82: /permissions shows the pre-approved scopes and an "always" answer, and never reaches the model.
    #[tokio::test]
    async fn permissions_lists_the_sessions_grants_and_stays_off_the_conversation() {
        let dirs = Dirs::new();
        let replies = vec![write("c1", "made.txt", "hello\n"), say("Created it.")];
        let mut s = startup(&dirs, Some(asks()), replies, None);
        s.pre_approved = vec!["web".into()];
        let (tx, mut rx) = unbounded_channel();
        let bridge = Bridge::start(s, tx).unwrap();
        let port = Port(bridge.clone());
        port.send("make a file".into());
        let seen = until(&mut rx, |e| matches!(e, UiEvent::Approval { .. })).await;
        let Some(UiEvent::Approval { id, .. }) = seen.last() else { unreachable!() };
        port.answer(*id, view::Decision::Always);
        until(&mut rx, |e| matches!(e, UiEvent::TurnEnd { .. })).await;
        let (conversation, system) = (bridge.session.conversation(), bridge.session.system_prompt());
        port.run_command("/permissions".into());
        let shown = texts(&[until(&mut rx, |e| matches!(e, UiEvent::Message(_))).await.pop().unwrap()]);
        assert!(shown[0].contains("**Permissions**"), "{shown:?}");
        assert!(shown[0].contains("- `web` (from --approve): web_fetch from any site"), "{shown:?}");
        assert!(shown[0].contains("- File changes: (all)"), "{shown:?}");
        assert!(shown[0].contains("**Yolo**: off"), "{shown:?}");
        assert_eq!(bridge.session.conversation(), conversation, "nothing of it joins the history");
        assert_eq!(bridge.session.system_prompt(), system, "nor the system prompt");
    }

    // #82 review: config ["web", "netwrk"]: web is seeded and listed, netwrk is warned about and listed nowhere.
    #[tokio::test]
    async fn an_unknown_config_scope_is_warned_about_and_not_listed() {
        let dirs = Dirs::new();
        // No config file: the notice ends with setup opening, as in the test below.
        let mut s = startup(&dirs, None, vec![], None);
        (s.pre_approved, s.approval_problems) = merge_pre_approved(vec!["web".into(), "netwrk".into()], &[]);
        let (tx, mut rx) = unbounded_channel();
        let bridge = Bridge::start(s, tx).unwrap();
        bridge.announce(None);
        let seen = until(&mut rx, |e| matches!(e, UiEvent::OpenSetup(_))).await;
        let notice: Vec<String> = seen
            .iter()
            .filter_map(|e| if let UiEvent::Message(m) = e { m.notice.as_ref() } else { None })
            .flat_map(|n| {
                n.items.iter().flat_map(|i| std::iter::once(i.text.clone()).chain(i.detail.clone().unwrap_or_default()))
            })
            .collect();
        let notice = notice.join("\n");
        assert!(notice.contains("Pre-approved for this session: web\n"), "{notice}");
        assert!(notice.contains("Unknown --approve scope \"netwrk\""), "warned: {notice}");
        assert!(bridge.session.permissions().pre_approved == ["web"]);
        Port(bridge.clone()).run_command("/permissions".into());
        let shown = texts(&[until(&mut rx, |e| matches!(e, UiEvent::Message(_))).await.pop().unwrap()]);
        assert!(shown[0].contains("- `web`"), "{shown:?}");
        assert!(!shown[0].contains("netwrk"), "{shown:?}");
        assert!(!bridge.session.system_prompt().contains("netwrk"));
    }

    // #98: the startup notice names what's pre-approved, which extra folders are writable, and any problem.
    #[tokio::test]
    async fn the_startup_notice_names_pre_approved_scopes_and_writable_folders() {
        let dirs = Dirs::new();
        let mut s = startup(&dirs, None, vec![], None);
        s.pre_approved = vec!["web".into(), "network".into()];
        s.writable = vec![dirs.config.join("beads")];
        s.approval_problems = vec!["Refusing writable sandbox folder \"~/.ssh\": it is, or holds, a protected path.".into()];
        let (tx, mut rx) = unbounded_channel();
        let bridge = Bridge::start(s, tx).unwrap();
        bridge.announce(None);
        let seen = until(&mut rx, |e| matches!(e, UiEvent::OpenSetup(_))).await;
        let (texts, details): (Vec<String>, Vec<String>) = seen
            .iter()
            .filter_map(|e| if let UiEvent::Message(m) = e { m.notice.as_ref() } else { None })
            .map(|n| {
                (
                    n.items.iter().map(|i| i.text.clone()).collect::<Vec<_>>().join("\n"),
                    n.items.iter().flat_map(|i| i.detail.clone().unwrap_or_default()).collect::<Vec<_>>().join("\n"),
                )
            })
            .fold((Vec::new(), Vec::new()), |(mut a, mut b), (t, d)| {
                a.push(t);
                b.push(d);
                (a, b)
            });
        let text = texts.join("\n");
        let detail = details.join("\n");
        assert!(text.contains("Pre-approved for this session: web, network"), "{text}");
        assert!(text.contains("Writable in the sandbox besides the project"), "{text}");
        assert!(text.contains("A problem with your pre-approved scopes or writable folders"), "{text}");
        assert!(detail.contains("Refusing writable sandbox folder"), "{detail}");
        assert!(
            detail.contains("Protected paths, package.json lifecycle scripts, mesh wires and tainted egress still ask"),
            "{detail}"
        );
    }

    #[test]
    fn setup_keeps_the_saved_key_and_drops_another_providers_endpoint() {
        let file = FileConfig {
            provider: Some(ProviderKind::Ollama),
            model: Some("m".into()),
            base_url: Some("http://gpu:11434".into()),
            api_key: Some("sk-or-v1-0123456789abcdef".into()),
            ..FileConfig::default()
        };
        let answer = SetupAnswer { provider: ProviderId::OpenRouter, model: "z-ai/glm-5.3".into(), api_key: None };
        let next = setup_file(Some(&file), &answer);
        assert_eq!(next.base_url, None);
        assert_eq!(next.api_key, file.api_key);
        let same = setup_file(Some(&file), &SetupAnswer { provider: ProviderId::Ollama, model: "n".into(), api_key: None });
        assert_eq!(same.base_url.as_deref(), Some("http://gpu:11434"));

        let env = Env::from([("OPENROUTER_API_KEY".to_string(), "sk-or-v1-fromtheenvironment".to_string())]);
        let ctx = setup_context(SetupStart::Reconfigure, Some(&file), &resolve_config(Some(&file), &env), &env);
        assert_eq!(ctx.saved_keys, vec![(ProviderId::OpenRouter, "sk-or-v1-0…cdef".to_string())]);
        assert_eq!(ctx.env_keys, [(ProviderId::OpenRouter, "sk-or-v1-f…ment".to_string())]);
        assert!(!format!("{ctx:?}").contains("fromtheenvironment"), "keys reach the UI masked");
    }

    /// #159: the plans are offered only when their switch is on (both off until the vendors clear Marv), or when the
    /// config already uses one; a plan's key goes to its own field.
    #[test]
    fn setup_offers_the_plans_behind_their_switches_and_keeps_keys_apart() {
        let ids = |file: Option<&FileConfig>| {
            let env = Env::new();
            let ctx = setup_context(SetupStart::Reconfigure, file, &resolve_config(file, &env), &env);
            ctx.presets.iter().map(|p| p.id).collect::<Vec<_>>()
        };
        let mut expected = vec![ProviderId::OpenRouter];
        if marv_provider::offered::ZAI_CODING_PLAN {
            expected.push(ProviderId::Zai);
        }
        if marv_provider::offered::CHATGPT_SIGN_IN {
            expected.push(ProviderId::ChatGpt);
        }
        expected.push(ProviderId::Ollama);
        assert_eq!(ids(None), expected);
        let zai = FileConfig { provider: Some(ProviderKind::Zai), ..FileConfig::default() };
        assert!(ids(Some(&zai)).contains(&ProviderId::Zai), "a config that uses it keeps it in setup");

        let file = FileConfig { api_key: Some("sk-or-keep".into()), ..FileConfig::default() };
        let answer = SetupAnswer { provider: ProviderId::Zai, model: "glm-5.3".into(), api_key: Some("zai-new".into()) };
        let next = setup_file(Some(&file), &answer);
        assert_eq!((next.api_key.as_deref(), next.zai_api_key.as_deref()), (Some("sk-or-keep"), Some("zai-new")));
    }

    #[test]
    fn approvals_carry_the_protected_paths_reason() {
        let request = marv_tools::ApprovalRequest {
            reason: false,
            persists: false,
            tool: "write_file".into(),
            label: ".envrc".into(),
            preview: marv_tools::Preview { title: "Create .envrc".into(), ..Default::default() },
            scope: marv_tools::Scope { key: "files".into(), description: "file changes".into() },
            agent: Some("implementer · Task 2".into()),
            network: false,
            once: true,
            must_ask: true,
            protected: marv_sandbox::protected::rule_for_relative(Path::new(".envrc")),
            door: None,
        };
        let shown = approval_request(&request);
        assert!(shown.preview.warning.unwrap().starts_with("changes .envrc"));
        assert_eq!((shown.agent.as_deref(), shown.once), (Some("implementer · Task 2"), true));
        // The TUI's queue needs `must_ask` too: an "always" on another request never answers this one.
        assert!(shown.must_ask);
    }

    /// #77: the session's one prompt for a call and its new wire reaches the TUI as "Yes (wire it)" or "No".
    #[test]
    fn a_call_needing_a_new_wire_shows_yes_wire_it_or_no() {
        use marv_mesh::{PlugId, ReceptacleName, WireMode, approval::WireRequest};
        let request = WireRequest {
            from: PlugId::new("parent").unwrap(),
            to: ReceptacleName::new("peer.web.agent.parent.inbox").unwrap(),
            mode: WireMode::Send,
            asked_by: PlugId::new("parent").unwrap(),
        };
        let call = marv_tools::Preview { title: "Send a message".into(), text: Some("hi".into()), ..Default::default() };
        let shown = approval_request(&marv_session::mesh::call_wire_approval(&request, call));
        assert_eq!(marv_session::mesh::MESH_WIRE_CALL, marv_tui::ui::approval::MESH_WIRE_CALL);
        let view = marv_tui::ui::approval::ApprovalView::new(shown, 0, true);
        let labels: Vec<&str> = view.select.items.iter().map(|i| i.label.as_str()).collect();
        assert_eq!(labels, ["Yes (wire it)", "No"]);
    }

    /// #185: a model's call (run_tool's request sets `reason`) reaches the TUI still offering "No, and say why";
    /// the step-limit question, /undo and a call needing a new wire don't.
    #[test]
    fn only_a_models_call_offers_a_reason_in_the_tui() {
        use marv_mesh::{PlugId, ReceptacleName, WireMode, approval::WireRequest};
        use marv_session::checkpoints::store::{Checkpoint, CheckpointKind};
        let model_call = marv_tools::ApprovalRequest {
            reason: true,
            persists: false,
            tool: "bash".into(),
            label: "bun test".into(),
            preview: marv_tools::Preview {
                title: "Run a command".into(),
                command: Some("bun test".into()),
                ..Default::default()
            },
            scope: marv_tools::Scope { key: "bash::bun test".into(), description: "this exact command".into() },
            agent: None,
            network: false,
            once: false,
            must_ask: false,
            protected: None,
            door: None,
        };
        let shown = approval_request(&model_call);
        assert!(shown.reason);
        let view = marv_tui::ui::approval::ApprovalView::new(shown, 0, true);
        assert!(view.select.items.iter().any(|i| i.label.starts_with("No, and say why")));

        let checkpoint = |kind| Checkpoint { sha: "a".into(), tree: "b".into(), turn: 1, kind, label: "x".into() };
        let undo = marv_session::UndoPlan {
            turn: 1,
            label: "add the parser".into(),
            before: checkpoint(CheckpointKind::Before),
            after: checkpoint(CheckpointKind::After),
            files: Vec::new(),
            not_restored: Vec::new(),
            head: "a".into(),
            targets: Default::default(),
        };
        let wire = WireRequest {
            from: PlugId::new("parent").unwrap(),
            to: ReceptacleName::new("peer.web.agent.parent.inbox").unwrap(),
            mode: WireMode::Send,
            asked_by: PlugId::new("parent").unwrap(),
        };
        let call = marv_tools::Preview { title: "Send a message".into(), ..Default::default() };
        for request in [
            marv_session::runner::step_limit_request(25, 25),
            marv_session::undo_request(&undo),
            marv_session::mesh::call_wire_approval(&wire, call),
        ] {
            let tool = request.tool.clone();
            let shown = approval_request(&request);
            assert!(!shown.reason, "{tool}");
            let view = marv_tui::ui::approval::ApprovalView::new(shown, 0, true);
            assert!(!view.select.items.iter().any(|i| i.label.contains("say why")), "{tool}");
        }
    }

    /// #185: a call declined with a reason shows declined (it didn't run), not done, though the turn goes on.
    #[test]
    fn a_call_declined_with_a_reason_shows_declined() {
        let result =
            || ToolResult { output: marv_tools::declined_because("not now"), summary: Some("declined".into()), is_error: false };
        let mut run = marv_agent::ToolRun::new(result(), "bun test");
        run.approval = marv_agent::ApprovalRecord::Declined;
        assert!(!run.declined);
        assert_eq!(tool_status(&run).status, view::ToolState::Declined);
        let ran =
            marv_agent::ToolRun { approval: marv_agent::ApprovalRecord::Yes, ..marv_agent::ToolRun::new(result(), "bun test") };
        assert_eq!(tool_status(&ran).status, view::ToolState::Done);
    }

    #[test]
    fn tool_entries_show_a_failures_message_and_subagents_keep_their_title() {
        let call = ToolCall { id: "a1".into(), name: "agent".into(), arguments: "{}".into() };
        let failed = marv_agent::ToolRun::new(
            ToolResult { output: "No such file\nmore".into(), summary: Some("error".into()), is_error: true },
            "x.ts",
        );
        let status = tool_status(&failed);
        assert_eq!((status.status, status.summary.as_deref()), (view::ToolState::Error, Some("No such file")));

        let mut map = TurnMap::default();
        let t = || view::Totals { requests: 3, ..Default::default() };
        let last = || Some(marv_core::Usage { prompt_tokens: 10, completion_tokens: 1, cached_tokens: None, cost: None });
        map_event(SessionEvent::Loop(LoopEvent::ToolStart { call, label: "general-purpose · fix".into() }), &mut map, &t, &last);
        let out = map_event(
            SessionEvent::Subagent { call_id: "a1".into(), event: LoopEvent::Usage(Default::default(), Default::default()) },
            &mut map,
            &t,
            &last,
        );
        assert!(matches!(&out[0], UiEvent::Usage { totals, last } if totals.requests == 3 && last.prompt_tokens == 10));
        assert!(matches!(&out[1], UiEvent::Subagent { title, .. } if title == "general-purpose · fix"));
        let tainted = map_event(SessionEvent::Tainted { source: marv_core::Provenance::Web }, &mut map, &t, &last);
        assert!(texts(&tainted)[0].contains("a web page"));
    }

    #[test]
    fn the_main_agents_plan_call_pins_its_list_and_a_subagents_doesnt() {
        let call = ToolCall { id: "p1".into(), name: "plan".into(), arguments: "{}".into() };
        let mut run = marv_agent::ToolRun::new(
            ToolResult { output: "Plan saved: 0 of 1 done; now: A.".into(), summary: Some("0/1 done".into()), is_error: false },
            "Plan",
        );
        let items = vec![marv_tools::PlanItem { text: "A".into(), status: marv_tools::PlanStatus::InProgress }];
        run.display = Some(Arc::new(marv_session::ToolDisplay { diff: None, plan: Some(items) }));
        let mut map = TurnMap::default();
        let (t, last) = (view::Totals::default, || None);
        let ended = LoopEvent::ToolEnd { call: call.clone(), run: run.clone() };
        let out = map_event(SessionEvent::Loop(ended.clone()), &mut map, &t, &last);
        let expected = vec![view::PlanItem { text: "A".into(), status: view::PlanStatus::InProgress }];
        assert!(matches!(&out[..], [UiEvent::ToolEnd { .. }, UiEvent::Plan(items)] if *items == expected));
        let sub = map_event(SessionEvent::Subagent { call_id: "a1".into(), event: ended }, &mut map, &t, &last);
        assert!(!sub.iter().any(|e| matches!(e, UiEvent::Plan(_))));
        // A failed call (no list) changes nothing.
        let failed = marv_agent::ToolRun::new(ToolResult { output: "x".into(), summary: None, is_error: true }, "Plan");
        let out = map_event(SessionEvent::Loop(LoopEvent::ToolEnd { call, run: failed }), &mut map, &t, &last);
        assert!(!out.iter().any(|e| matches!(e, UiEvent::Plan(_))));
    }

    // Marv tests/app.test.tsx "MCP servers", "MCP logins (#18)" and "/mcp add, search and remove" (#27). Nothing
    // leaves the machine: npm, PyPI, the registry and GitHub are the install fixtures, `npx` is a shell script that
    // logs how it was run and speaks MCP over stdio, the OAuth server is a local fake, and the browser is a test's.
    mod mcp {
        use super::*;
        use marv_mcp::install::proposal::{Fetched, ResolveDeps};
        use std::collections::HashMap as Map;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio_util::sync::CancellationToken;

        const NPM_FILESYSTEM: &str = include_str!("../../marv-mcp/src/install/fixtures/npm-server-filesystem.json");
        const REGISTRY_SEARCH: &str = include_str!("../../marv-mcp/src/install/fixtures/registry-search.json");

        /// The resolvers' network, from the fixtures.
        #[derive(Default)]
        struct Net {
            asked: Mutex<Vec<String>>,
            readme: Option<String>,
        }
        #[async_trait]
        impl ResolveDeps for Net {
            async fn get(&self, url: &str, _a: &str, _m: usize, _c: &CancellationToken) -> marv_mcp::Result<Fetched> {
                self.asked.lock().unwrap().push(url.to_string());
                let body = if url.starts_with("http://fake/npm/@modelcontextprotocol%2Fserver-filesystem")
                    || url.starts_with("http://fake/npm/@modelcontextprotocol/server-filesystem")
                {
                    NPM_FILESYSTEM
                } else if url.starts_with("http://fake/registry/v0/servers?") {
                    REGISTRY_SEARCH
                } else {
                    return Ok(Fetched { status: 404, body: "{\"error\":\"Not found\"}".into() });
                };
                Ok(Fetched { status: 200, body: body.into() })
            }
            async fn readme(&self, _o: &str, _r: &str, _c: &CancellationToken) -> marv_mcp::Result<Option<String>> {
                Ok(self.readme.clone())
            }
            fn npm_registry(&self) -> &str {
                "http://fake/npm"
            }
            fn pypi(&self) -> &str {
                "http://fake/pypi"
            }
            fn registry(&self) -> &str {
                "http://fake/registry"
            }
            fn now_ms(&self) -> u64 {
                1_791_806_400_000 // 2026-10-12T12:00:00Z, as the fixtures expect
            }
            fn cancel(&self) -> CancellationToken {
                CancellationToken::new()
            }
        }

        /// A stdio MCP server in sh with one tool, `read`; it logs its name and arguments to `log`. The handshake is
        /// told by its `protocolVersion` (tests/guards' `server_instructions_never_leave_the_parser` keeps the
        /// method's name out of this crate).
        fn fake_server(path: &Path, log: &Path) {
            let script = format!(
                r#"#!/bin/sh
echo "$(basename "$0") $*" >> '{}'
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  case "$line" in
    *'"protocolVersion"'*) printf '{{"jsonrpc":"2.0","id":%s,"result":{{"protocolVersion":"2025-06-18","capabilities":{{"tools":{{}}}},"serverInfo":{{"name":"fake","version":"1.0.0"}}}}}}\n' "$id" ;;
    *'"method":"tools/list"'*) printf '{{"jsonrpc":"2.0","id":%s,"result":{{"tools":[{{"name":"read","description":"Reads.","inputSchema":{{"type":"object","properties":{{}}}}}}]}}}}\n' "$id" ;;
    *) [ -n "$id" ] && printf '{{"jsonrpc":"2.0","id":%s,"result":{{}}}}\n' "$id" ;;
  esac
done
"#,
                log.display()
            );
            marv_tools::github::fake::write_executable(path, &script);
        }

        struct Mcp {
            port: Port,
            rx: UnboundedReceiver<UiEvent>,
            mcp: McpHandle,
            opened: Arc<Mutex<Vec<String>>>,
            net: Arc<Net>,
            log: PathBuf,
            bin: PathBuf,
        }

        impl Mcp {
            fn bridge(&self) -> &Arc<Bridge> {
                &self.port.0
            }
            /// Events until a message containing `part`; their texts.
            async fn says(&mut self, part: &str) -> Vec<String> {
                let seen = until(&mut self.rx, |e| matches!(e, UiEvent::Message(m) if m.text.contains(part))).await;
                texts(&seen)
            }
            /// Events until a notice whose items mention `part`; that notice.
            async fn notice(&mut self, part: &str) -> Notice {
                let has = |e: &UiEvent| {
                    matches!(e, UiEvent::Message(view::Message { notice: Some(n), .. })
                    if n.items.iter().any(|i| i.text.contains(part)))
                };
                let seen = until(&mut self.rx, has).await;
                let Some(UiEvent::Message(view::Message { notice: Some(n), .. })) = seen.last() else { unreachable!() };
                n.clone()
            }
            async fn approval(&mut self) -> (ApprovalId, view::ApprovalRequest) {
                let seen = until(&mut self.rx, |e| matches!(e, UiEvent::Approval { .. })).await;
                let Some(UiEvent::Approval { id, request }) = seen.last() else { unreachable!() };
                (*id, request.clone())
            }
            fn mcp_file(&self, dirs: &Dirs) -> Value {
                serde_json::from_str(&std::fs::read_to_string(dirs.config.join("mcp.json")).unwrap()).unwrap()
            }
            fn ran(&self) -> String {
                std::fs::read_to_string(&self.log).unwrap_or_default()
            }
        }

        /// A bridge as `run` makes it: an MCP manager even with no servers, so `/mcp add` can start the first.
        fn start(dirs: &Dirs, project_servers: Option<Value>, net: Net) -> Mcp {
            let bin = dirs.base.join("bin");
            std::fs::create_dir_all(&bin).unwrap();
            let log = dirs.base.join("ran.log");
            fake_server(&bin.join("npx"), &log);
            fake_server(&bin.join("fake-mcp"), &log);
            if let Some(servers) = project_servers {
                std::fs::write(dirs.project.join(".mcp.json"), json!({ "mcpServers": servers }).to_string()).unwrap();
            }
            let mut s = startup(dirs, Some(asks()), vec![], None);
            s.env = [
                ("PATH".to_string(), format!("{}:/usr/bin:/bin", bin.display())),
                ("OPENROUTER_API_KEY".to_string(), "sk-or-test".to_string()),
            ]
            .into();
            let (mcp, problems) = start_mcp(&s.root, &s.home, &s.config_dir, &s.env);
            assert_eq!(problems, Vec::<String>::new());
            s.mcp = Some(mcp.clone());
            let net = Arc::new(net);
            let deps = net.clone();
            s.mcp_deps = Arc::new(move |_cancel| deps.clone() as Arc<dyn ResolveDeps>);
            let opened = Arc::new(Mutex::new(Vec::new()));
            let o = opened.clone();
            s.open_url = Arc::new(move |url| o.lock().unwrap().push(url.to_string()));
            let (tx, rx) = unbounded_channel();
            let bridge = Bridge::start(s, tx).unwrap();
            bridge.watch_mcp();
            Mcp { port: Port(bridge), rx, mcp, opened, net, log, bin }
        }

        use serde_json::json;

        #[tokio::test]
        async fn a_projects_server_waits_for_mcp_trust_and_says_so() {
            let dirs = Dirs::new();
            let fake = dirs.base.join("bin/fake-mcp");
            let mut t = start(
                &dirs,
                Some(json!({ "test": { "command": fake, "args": ["--key=${OPENROUTER_API_KEY}"] } })),
                Net::default(),
            );
            let notice = t.notice("you haven't trusted yet").await;
            assert_eq!((notice.title.as_str(), notice.tone), ("MCP · 1 note", NoticeTone::Warning));
            assert_eq!(notice.items[0].text, "This project's .mcp.json lists 1 MCP server you haven't trusted yet");
            let detail = notice.items[0].detail.clone().unwrap();
            assert!(detail[0].starts_with("test ("), "{detail:?}");
            assert!(detail[0].ends_with(", which reads $OPENROUTER_API_KEY from your environment"), "{detail:?}");
            assert_eq!(detail[1], "They'd run with your permissions, outside the sandbox. /mcp trust starts them.");
            assert!(t.ran().is_empty(), "nothing ran before the trust");

            t.port.run_command("/mcp trust".into());
            let said = t.says("test: connected").await;
            assert!(said.contains(&"Starting test…".to_string()), "{said:?}");
            assert_eq!(said.last().unwrap(), "test: connected · 1 tools");
            assert!(t.ran().starts_with("fake-mcp --key="));
            let trust = marv_mcp::trust::McpTrust::new(dirs.config.join("mcp-trust.json"));
            let include = marv_mcp::config::Include { personal: true, project: true };
            let env = t.bridge().env.clone();
            let config = marv_mcp::config::load_mcp_config(&dirs.project, &dirs.config, &env, include).servers.remove(0);
            assert!(trust.is_trusted(&project_key(&dirs.project), &config));
            t.port.run_command("/mcp trust".into());
            t.says("No MCP servers are waiting to be trusted (/mcp lists them).").await;
            t.mcp.close().await;
        }

        #[tokio::test]
        async fn mcp_add_shows_one_approval_with_the_command_and_the_json_and_no_writes_nothing() {
            let dirs = Dirs::new();
            let mut t = start(&dirs, None, Net::default());
            t.port.run_command("/mcp add @modelcontextprotocol/server-filesystem".into());
            let (id, request) = t.approval().await;
            assert!(request.once, "no \"don't ask again\"");
            assert_eq!(request.preview.title, "Add MCP server \"filesystem\" to ~/.marv/mcp.json (yours, every project)");
            assert_eq!(request.preview.command.as_deref(), Some("npx -y @modelcontextprotocol/server-filesystem@2026.1.14"));
            let text = request.preview.text.clone().unwrap();
            assert!(text.contains("2026.1.14, published 2026-01-14"), "{text}");
            assert!(text.contains("writes   \"filesystem\": {\"command\":\"npx\""), "{text}");
            assert!(
                request
                    .preview
                    .note
                    .as_deref()
                    .unwrap()
                    .starts_with("runs outside the sandbox, with your permissions and network")
            );
            t.port.answer(id, view::Decision::No);
            t.says("Not added: filesystem.").await;
            assert!(!dirs.config.join("mcp.json").exists());
            assert!(t.mcp.status().is_empty());
            assert!(t.ran().is_empty());
        }

        #[tokio::test]
        async fn yes_writes_exactly_the_entry_shown_starts_it_and_mcp_remove_takes_it_out() {
            let dirs = Dirs::new();
            let mut t = start(&dirs, None, Net::default());
            t.port.run_command("/mcp add @modelcontextprotocol/server-filesystem".into());
            let seen = until(&mut t.rx, |e| matches!(e, UiEvent::Approval { .. })).await;
            assert!(seen.contains(&UiEvent::Busy(Some("Looking up @modelcontextprotocol/server-filesystem…".into()))));
            let Some(UiEvent::Approval { id, request }) = seen.last() else { unreachable!() };
            let shown = request.preview.text.clone().unwrap();
            t.port.answer(*id, view::Decision::Yes);
            let said = t.says("connected · 1 tool").await;
            assert!(said.contains(&"Starting filesystem…".to_string()));
            assert_eq!(
                said.last().unwrap(),
                "Added filesystem to ~/.marv/mcp.json: connected · 1 tool. Marv can use it from your next message."
            );
            let written = t.mcp_file(&dirs)["mcpServers"]["filesystem"].clone();
            let shown_json = shown.rsplit_once("writes   \"filesystem\": ").unwrap().1;
            assert_eq!(serde_json::from_str::<Value>(shown_json).unwrap(), written, "exactly the entry shown");
            let home = dirs.base.join("home");
            assert_eq!(t.ran().trim(), "npx -y @modelcontextprotocol/server-filesystem@2026.1.14");
            assert!(home.is_dir());
            let status = t.mcp.status();
            assert_eq!(
                (status[0].name.as_str(), status[0].source, status[0].state),
                ("filesystem", marv_mcp::config::ServerSource::Personal, marv_mcp::manager::McpState::Connected)
            );
            assert!(t.mcp.specs().iter().any(|s| s.name == "mcp__filesystem__read"));

            // A name you already have is refused before asking.
            t.port.run_command("/mcp add @modelcontextprotocol/server-filesystem".into());
            let said = t.says("Couldn't add the server").await;
            assert!(said.last().unwrap().contains("You already have a server named \"filesystem\""));
            assert!(said.last().unwrap().contains("--name"));

            t.port.run_command("/mcp remove filesystem".into());
            let said = t.says("Removed filesystem").await;
            assert_eq!(
                said.last().unwrap(),
                "Removed filesystem from ~/.marv/mcp.json and stopped it. Its tools stay listed (failing) until /clear or a restart."
            );
            assert_eq!(t.mcp_file(&dirs)["mcpServers"], json!({}));
            assert_eq!(t.mcp.status()[0].state, marv_mcp::manager::McpState::Removed);
            // Removed in this conversation: it can't come back until the next one.
            t.port.run_command("/mcp add @modelcontextprotocol/server-filesystem".into());
            let said = t.says("Couldn't add the server").await;
            assert!(said.last().unwrap().contains("was removed in this conversation"), "{said:?}");
            t.mcp.close().await;
        }

        #[tokio::test]
        async fn lookups_that_fail_bad_options_and_unset_variables() {
            let dirs = Dirs::new();
            let mut t = start(&dirs, None, Net::default());
            t.port.run_command("/mcp add nope".into());
            let said = t.says("Couldn't add the server").await;
            assert!(said.last().unwrap().contains("no npm package named nope"), "{said:?}");
            assert!(!dirs.config.join("mcp.json").exists());

            let asked = t.net.asked.lock().unwrap().len();
            t.port.run_command("/mcp add weather --env API_KEY=sk-123".into());
            t.says("--env takes a variable's name").await;
            assert_eq!(t.net.asked.lock().unwrap().len(), asked, "nothing was looked up");

            // An unset ${VAR}: written, not started, and Marv says to set it and restart.
            t.port.run_command("/mcp add @modelcontextprotocol/server-filesystem --env NOT_SET_HERE".into());
            let (id, request) = t.approval().await;
            assert!(request.preview.text.unwrap().contains("NOT_SET_HERE (not set"));
            t.port.answer(id, view::Decision::Yes);
            let said = t.says("restart Marv").await;
            assert_eq!(
                said.last().unwrap(),
                "Added filesystem to ~/.marv/mcp.json, but didn't start it: ${NOT_SET_HERE} isn't set. Set it in your shell and restart Marv."
            );
            assert_eq!(t.mcp_file(&dirs)["mcpServers"]["filesystem"]["env"], json!({"NOT_SET_HERE": "${NOT_SET_HERE}"}));
            assert!(t.mcp.status().is_empty());
        }

        #[tokio::test]
        async fn a_name_this_projects_mcp_json_uses_is_refused_before_asking() {
            let dirs = Dirs::new();
            let fake = dirs.base.join("bin/fake-mcp");
            let mut t = start(&dirs, Some(json!({ "filesystem": { "command": fake } })), Net::default());
            t.notice("you haven't trusted yet").await;
            t.port.run_command("/mcp add @modelcontextprotocol/server-filesystem".into());
            let said = t.says("Couldn't add the server").await;
            assert!(said.last().unwrap().contains("This project has a server named \"filesystem\": pick another with --name."));
            assert!(!dirs.config.join("mcp.json").exists());
            // And /mcp remove points at the project's file.
            t.port.run_command("/mcp remove filesystem".into());
            t.says("\"filesystem\" is this project's server: remove it from .mcp.json.").await;
            t.port.run_command("/mcp remove nope".into());
            t.says("You have no server named \"nope\" in ~/.marv/mcp.json (/mcp lists them).").await;
        }

        #[tokio::test]
        async fn a_readme_naming_several_servers_lists_them_and_search_prints_the_registrys_matches() {
            let dirs = Dirs::new();
            let servers = json!({"mcpServers": {"a": {"command": "npx", "args": ["-y", "weather-mcp"]}, "b": {"command": "uvx", "args": ["mcp-server-fetch"]}}});
            let readme = format!("# Weather\n\n```json\n{servers}\n```\n");
            let mut t = start(&dirs, None, Net { readme: Some(readme), ..Net::default() });
            t.port.run_command("/mcp add https://github.com/acme/weather".into());
            let seen = until(&mut t.rx, |e| matches!(e, UiEvent::Message(m) if m.text.contains("names several servers"))).await;
            let Some(UiEvent::Message(m)) = seen.last() else { unreachable!() };
            assert!(m.markdown);
            assert!(m.text.contains("- `/mcp add weather-mcp`"), "{}", m.text);
            assert!(m.text.contains("- `/mcp add pypi:mcp-server-fetch`"), "{}", m.text);

            t.port.run_command("/mcp search filesystem".into());
            let seen = until(&mut t.rx, |e| matches!(e, UiEvent::Message(m) if m.text.contains("MCP Registry"))).await;
            assert!(seen.contains(&UiEvent::Busy(Some("Searching the MCP Registry…".into()))));
            let said = texts(&seen);
            assert!(said.last().unwrap().contains("isn't curated"));
            assert!(said.last().unwrap().contains("`/mcp add io.modelcontextprotocol/filesystem`"));
        }

        #[tokio::test]
        async fn esc_stops_a_lookup_and_nothing_is_added() {
            let dirs = Dirs::new();
            let mut t = start(&dirs, None, Net::default());
            t.port.run_command("/mcp add @modelcontextprotocol/server-filesystem".into());
            let (id, _) = t.approval().await;
            // Esc at the approval: the App declines it and aborts.
            t.port.answer(id, view::Decision::No);
            t.port.abort();
            let said = t.says("filesystem").await;
            assert!(said.last().unwrap() == "Not added: filesystem." || said.last().unwrap() == "Stopped: nothing was added.");
            assert!(!dirs.config.join("mcp.json").exists());
        }

        // ---- logins (Marv #18), against a local OAuth server ----

        /// An MCP server that is its own OAuth authorization server: metadata, registration, an `/authorize` that
        /// redirects straight back, `/token`, `/revoke`. Tokens are random; `/mcp` wants one.
        struct OAuth {
            base: String,
            revoked: Arc<Mutex<Vec<String>>>,
            /// Every token stops working and refreshes are refused (a login that ran out).
            expired: Arc<AtomicBool>,
        }

        async fn oauth_server() -> OAuth {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
            let (tokens, revoked) = (Arc::new(Mutex::new(Vec::new())), Arc::new(Mutex::new(Vec::new())));
            let expired = Arc::new(AtomicBool::new(false));
            let (b, tk, rv, ex) = (base.clone(), tokens.clone(), revoked.clone(), expired.clone());
            tokio::spawn(async move {
                while let Ok((stream, _)) = listener.accept().await {
                    let (b, tk, rv, ex) = (b.clone(), tk.clone(), rv.clone(), ex.clone());
                    tokio::spawn(async move {
                        let _ = oauth_one(stream, &b, &tk, &rv, ex.load(Ordering::SeqCst)).await;
                    });
                }
            });
            OAuth { base, revoked, expired }
        }

        fn form(body: &str) -> Map<String, String> {
            url_pairs(body)
        }

        fn url_pairs(query: &str) -> Map<String, String> {
            query.split('&').filter_map(|kv| kv.split_once('=')).map(|(k, v)| (k.to_string(), percent_decode(v))).collect()
        }

        fn percent_decode(v: &str) -> String {
            let b = v.replace('+', " ").into_bytes();
            let mut out = Vec::new();
            let mut i = 0;
            while i < b.len() {
                if b[i] == b'%'
                    && let Some(n) =
                        std::str::from_utf8(&b[i + 1..(i + 3).min(b.len())]).ok().and_then(|h| u8::from_str_radix(h, 16).ok())
                {
                    out.push(n);
                    i += 3;
                    continue;
                }
                out.push(b[i]);
                i += 1;
            }
            String::from_utf8_lossy(&out).into_owned()
        }

        fn percent_encode(v: &str) -> String {
            v.bytes()
                .map(|c| {
                    if c.is_ascii_alphanumeric() || b"-._~".contains(&c) { (c as char).to_string() } else { format!("%{c:02X}") }
                })
                .collect()
        }

        async fn oauth_one(
            mut stream: tokio::net::TcpStream,
            base: &str,
            tokens: &Mutex<Vec<String>>,
            revoked: &Mutex<Vec<String>>,
            expired: bool,
        ) -> std::io::Result<()> {
            let (method, target, headers, body) = read_request(&mut stream).await?;
            let (path, query) = target.split_once('?').unwrap_or((&target, ""));
            let json = |status: u16, v: Value| {
                (status, vec![("content-type".to_string(), "application/json".to_string())], v.to_string())
            };
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let random = || format!("{:08x}", NEXT.fetch_add(1, Ordering::SeqCst) * 2_654_435_761 % 4_294_967_291);
            let (status, extra, body) = match (method.as_str(), path) {
                ("POST", "/mcp") => {
                    let auth = headers.get("authorization").cloned().unwrap_or_default();
                    let ok =
                        !expired && auth.strip_prefix("Bearer ").is_some_and(|t| tokens.lock().unwrap().iter().any(|k| k == t));
                    if !ok {
                        let www = format!("Bearer resource_metadata=\"{base}/.well-known/oauth-protected-resource/mcp\"");
                        (401, vec![("www-authenticate".to_string(), www)], "unauthorized".to_string())
                    } else {
                        let message: Value = serde_json::from_str(&body).unwrap_or_default();
                        let result = match message["method"].as_str().unwrap_or_default() {
                            // The handshake (see `fake_server`).
                            _ if message["params"]["protocolVersion"].is_string() => {
                                json!({"protocolVersion": message["params"]["protocolVersion"], "capabilities": {"tools": {}}, "serverInfo": {"name": "remote", "version": "1"}})
                            }
                            "tools/list" => {
                                json!({"tools": [{"name": "echo", "description": "Echo.", "inputSchema": {"type": "object", "properties": {}}}]})
                            }
                            _ => json!({}),
                        };
                        match message.get("id") {
                            Some(id) => json(200, json!({"jsonrpc": "2.0", "id": id, "result": result})),
                            None => (202, vec![], String::new()),
                        }
                    }
                }
                (_, "/.well-known/oauth-protected-resource/mcp" | "/.well-known/oauth-protected-resource") => {
                    json(200, json!({"resource": format!("{base}/mcp"), "authorization_servers": [base]}))
                }
                (_, "/.well-known/oauth-authorization-server") => json(
                    200,
                    json!({
                        "issuer": base,
                        "authorization_endpoint": format!("{base}/authorize"),
                        "token_endpoint": format!("{base}/token"),
                        "registration_endpoint": format!("{base}/register"),
                        "revocation_endpoint": format!("{base}/revoke"),
                        "response_types_supported": ["code"],
                        "code_challenge_methods_supported": ["S256"],
                    }),
                ),
                ("POST", "/register") => {
                    let mut v: Value = serde_json::from_str(&body).unwrap_or_default();
                    v["client_id"] = format!("client-{}", random()).into();
                    json(201, v)
                }
                ("GET", "/authorize") => {
                    let q = url_pairs(query);
                    let back = format!(
                        "{}?code=code-{}&state={}",
                        q.get("redirect_uri").cloned().unwrap_or_default(),
                        random(),
                        percent_encode(q.get("state").map_or("", String::as_str))
                    );
                    (302, vec![("location".to_string(), back)], String::new())
                }
                ("POST", "/token") if expired => json(400, json!({"error": "invalid_grant"})),
                ("POST", "/token") => {
                    let (a, r) = (format!("at-{}", random()), format!("rt-{}", random()));
                    tokens.lock().unwrap().push(a.clone());
                    json(200, json!({"access_token": a, "refresh_token": r, "token_type": "Bearer", "expires_in": 3600}))
                }
                ("POST", "/revoke") => {
                    revoked.lock().unwrap().push(form(&body).get("token").cloned().unwrap_or_default());
                    (200, vec![], String::new())
                }
                _ => (404, vec![], "not found".to_string()),
            };
            let mut out = format!("HTTP/1.1 {status} X\r\ncontent-length: {}\r\nconnection: close\r\n", body.len());
            for (k, v) in extra {
                out.push_str(&format!("{k}: {v}\r\n"));
            }
            out.push_str("\r\n");
            out.push_str(&body);
            stream.write_all(out.as_bytes()).await?;
            stream.shutdown().await
        }

        async fn read_request(
            stream: &mut tokio::net::TcpStream,
        ) -> std::io::Result<(String, String, Map<String, String>, String)> {
            let mut data = Vec::new();
            let mut buf = [0u8; 4096];
            let end = loop {
                if let Some(i) = data.windows(4).position(|w| w == b"\r\n\r\n") {
                    break i;
                }
                let n = stream.read(&mut buf).await?;
                if n == 0 {
                    return Err(std::io::ErrorKind::UnexpectedEof.into());
                }
                data.extend_from_slice(&buf[..n]);
            };
            let head = String::from_utf8_lossy(&data[..end]).to_string();
            let mut lines = head.split("\r\n");
            let mut first = lines.next().unwrap_or_default().split(' ');
            let (method, target) = (first.next().unwrap_or_default().to_string(), first.next().unwrap_or("/").to_string());
            let headers: Map<String, String> =
                lines.filter_map(|l| l.split_once(':')).map(|(k, v)| (k.trim().to_lowercase(), v.trim().to_string())).collect();
            let length: usize = headers.get("content-length").and_then(|l| l.parse().ok()).unwrap_or(0);
            let mut body = data[end + 4..].to_vec();
            while body.len() < length {
                let n = stream.read(&mut buf).await?;
                if n == 0 {
                    break;
                }
                body.extend_from_slice(&buf[..n]);
            }
            Ok((method, target, headers, String::from_utf8_lossy(&body).into_owned()))
        }

        /// A GET without following redirects: the `location` it answers with.
        async fn location(url: &str) -> String {
            let rest = url.strip_prefix("http://").unwrap();
            let (host, path) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
            let mut stream = tokio::net::TcpStream::connect(host).await.unwrap();
            let request = format!("GET {path} HTTP/1.1\r\nhost: {host}\r\nconnection: close\r\n\r\n");
            stream.write_all(request.as_bytes()).await.unwrap();
            let mut answer = String::new();
            stream.read_to_string(&mut answer).await.unwrap();
            answer.split("\r\n").find_map(|l| l.strip_prefix("location: ")).map(str::to_string).unwrap_or_default()
        }

        async fn opened(t: &Mcp) -> String {
            for _ in 0..1000 {
                if let Some(url) = t.opened.lock().unwrap().first().cloned() {
                    return url;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            panic!("the login never opened anything");
        }

        async fn remote(dirs: &Dirs) -> (Mcp, OAuth) {
            let server = oauth_server().await;
            std::fs::write(
                dirs.config.join("mcp.json"),
                json!({"mcpServers": {"remote": {"type": "http", "url": format!("{}/mcp", server.base)}}}).to_string(),
            )
            .unwrap();
            (start(dirs, None, Net::default()), server)
        }

        #[tokio::test]
        async fn startup_lists_the_server_that_needs_a_login_and_the_menu_offers_login_and_logout() {
            let dirs = Dirs::new();
            let (mut t, _server) = remote(&dirs).await;
            let notice = t.notice("needs a login").await;
            assert_eq!(notice.title, "MCP · 1 note");
            assert_eq!(notice.items[0].text, "1 MCP server needs a login");
            assert_eq!(notice.items[0].detail.as_deref(), Some(&["remote: /mcp login remote".to_string()][..]));
            let menu: Vec<String> = t.port.commands().into_iter().map(|c| c.name).collect();
            assert!(menu.contains(&"mcp login remote".to_string()) && menu.contains(&"mcp logout remote".to_string()));
        }

        #[tokio::test]
        async fn mcp_login_shows_the_link_opens_it_takes_the_pasted_callback_and_the_code_stays_out() {
            let dirs = Dirs::new();
            let (mut t, server) = remote(&dirs).await;
            t.notice("needs a login").await;
            t.port.run_command("/mcp login remote".into());
            let said = t.says("Log in to remote at 127.0.0.1:").await;
            assert!(said.last().unwrap().contains("paste its address here:\nhttp://127.0.0.1:"), "{said:?}");
            let url = opened(&t).await;
            assert!(url.starts_with(&format!("{}/authorize?", server.base)));
            let pasted = location(&url).await;
            let code = pasted.split("code=").nth(1).unwrap().split('&').next().unwrap().to_string();
            assert!(code.starts_with("code-"));
            assert!(t.port.login_callback(&pasted));
            let seen = until(&mut t.rx, |e| matches!(e, UiEvent::Message(m) if m.text.contains("logged in · connected"))).await;
            assert!(
                seen.iter().any(|e| matches!(e, UiEvent::Message(m) if m.role == Role::User && m.text == "(login callback)"))
            );
            assert!(texts(&seen).contains(&"Got it: finishing the login to remote…".to_string()));
            assert_eq!(
                texts(&seen).last().unwrap(),
                "remote: logged in · connected · 1 tool. Marv can use them from your next message."
            );
            assert!(!format!("{seen:?}").contains(&code), "the code is never shown");
            assert!(t.mcp.specs().iter().any(|s| s.name == "mcp__remote__echo"));

            // The same address again: this login has had its callback.
            assert!(t.port.login_callback(&pasted));
            t.says("No login is waiting for that address (/mcp login <server> starts one). It wasn't sent to the model.").await;

            t.port.run_command("/mcp logout remote".into());
            let said = t.says("Logged out of remote").await;
            assert_eq!(
                said.last().unwrap(),
                "Logged out of remote: its tokens are forgotten. The authorization server revoked the grant."
            );
            assert_eq!(server.revoked.lock().unwrap().len(), 1);
            assert_eq!(t.mcp.status()[0].state, marv_mcp::manager::McpState::NeedsLogin);
            t.port.run_command("/mcp logout remote".into());
            t.says("remote isn't logged in.").await;
            t.mcp.close().await;
        }

        async fn logged_in(t: &mut Mcp) {
            t.notice("needs a login").await;
            t.port.run_command("/mcp login remote".into());
            let url = opened(t).await;
            assert!(t.port.login_callback(&location(&url).await));
            t.says("logged in · connected").await;
        }

        #[tokio::test]
        async fn a_login_that_runs_out_mid_session_is_told_once() {
            let dirs = Dirs::new();
            let (mut t, server) = remote(&dirs).await;
            logged_in(&mut t).await;
            server.expired.store(true, Ordering::SeqCst);
            let echo = t.mcp.tools().iter().find(|tool| tool.name == "mcp__remote__echo").cloned().unwrap();
            for _ in 0..2 {
                let failed = echo.run(Default::default(), CancellationToken::new()).await;
                assert!(failed.unwrap_err().to_string().contains("remote needs a new login: the user can run /mcp login remote"));
            }
            let notice = t.notice("needs a new login").await;
            assert_eq!(notice.items[0].text, "remote needs a new login: /mcp login remote");
            assert_eq!(notice.items[0].detail.as_deref(), Some(&["Its tools fail until then.".to_string()][..]));
            t.port.run_command("/exit".into());
            let rest = until(&mut t.rx, |e| *e == UiEvent::Exit).await;
            assert!(!format!("{rest:?}").contains("needs a new login"), "told once: {rest:?}");
            t.mcp.close().await;
        }

        #[tokio::test]
        async fn a_callback_with_no_login_waiting_is_refused_and_ordinary_text_isnt_taken() {
            let dirs = Dirs::new();
            let mut t = start(&dirs, None, Net::default());
            assert!(!t.port.login_callback("hello"));
            assert!(t.port.login_callback("http://127.0.0.1:5555/callback?code=code-secret&state=abc"));
            let seen = until(&mut t.rx, |e| matches!(e, UiEvent::Message(m) if m.text.starts_with("No login is waiting"))).await;
            assert!(!format!("{seen:?}").contains("code-secret"));
        }

        #[tokio::test]
        async fn a_login_to_an_unknown_server_says_so() {
            let dirs = Dirs::new();
            let mut t = start(&dirs, None, Net::default());
            t.port.run_command("/mcp login nope".into());
            let seen = until(&mut t.rx, |e| matches!(e, UiEvent::Message(_))).await;
            assert!(matches!(seen.last(), Some(UiEvent::Message(m)) if m.is_error), "{seen:?}");
            let _ = (&t.bin, &t.bridge());
        }
    }

    #[test]
    fn startup_problems_are_one_notice_in_the_worst_tone() {
        assert_eq!(startup_notice(vec![]), None);
        let notice = startup_notice(vec![
            (NoticeTone::Warning, hidden_path_folders_item(&["~/.local/share/mise/bin".into()])),
            (NoticeTone::Error, NoticeItem { text: "1 skill couldn't be loaded (see /skills)".into(), detail: None }),
        ])
        .unwrap();
        assert_eq!((notice.title.as_str(), notice.tone), ("Startup · 2 notes", NoticeTone::Error));
        assert_eq!(notice.items[0].text, "1 PATH folder is hidden from the sandbox: ~/.local/share/mise/bin");
    }
}
