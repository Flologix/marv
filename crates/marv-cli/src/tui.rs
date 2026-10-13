//! Launching the TUI: the interactive half of Marv's `src/cli.tsx`, the engine half of `src/app.tsx` (commands,
//! config, setup, resume, startup notices), and the App's Herdr reporting (`src/herdr.ts` is marv-mesh's `herdr.rs`).
//!
//! Startup, in Marv's order: the project root is the current folder; read AGENTS.md once (cut at 20k, the cut notice
//! goes to the UI); load skills and agents (project + personal; problems shown at startup); load the MCP config and
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
//! Port notes (marv-rs):
//! - `marv -c` resumes before the UI starts, so a message can't be typed into the session being swapped out (Marv held
//!   `send` with "Loading your last session…"); the App gets `Resumed` as its first event.
//! - The saved transcript is the session's default (the messages and the replies): the App owns the transcript the
//!   user sees and the boundary has no way to read it back. Tool entries and notices aren't restored on resume.
//! - `/mcp add|search|remove|trust|login|logout` say they aren't available yet: `McpManager`'s methods are still being
//!   ported (#6). `/mcp` (the list) works.
//! - The MCP manager is only made when a config lists servers (Marv always made one, so `/mcp add` could start the
//!   first): its `start`/`close` aren't ported yet (#6), and with none to start there's nothing to wait for.
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
use marv_mesh::herdr::{HerdrReporter, HerdrState, herdr_target};
use marv_provider::ProviderKind;
use marv_provider::factory::{ConfigFactory, LoadModels, MakeProvider};
use marv_session::checks::{ProjectCheckResult, project_check_notice};
use marv_session::commands::{self, CommandAction, CommandContext, MemoryTarget};
use marv_session::config::{
    Config, ConfigStore, Env, FileConfig, MaxStepsSetting, default_config_dir, mask_key, migrate_legacy_config, needs_setup,
    preset, resolve_config,
};
use marv_session::diagnostics::check_notice;
use marv_session::mcp_tools::McpHandle;
use marv_session::memory::{Memories, MemoryPaths, load_memory, memory_paths};
use marv_session::paths::{home_dir, project_key, shorten_home};
use marv_session::prompt::{Instructions, instructions_cut_item, read_instructions};
use marv_session::sessions::SessionStore;
use marv_session::skills::LoadedSkill;
use marv_session::trajectory::TrajectoryStore;
use marv_session::{
    CompactFailure, CompactResult, Configure, Feedback, MarvSession, ProviderChoice, RateOutcome, ResumeTarget, SendOptions,
    SessionEvent, SessionInit, TurnEndReason, TurnStatus,
};
use marv_tui::app::{App, AppOptions};
use marv_tui::view::{
    self, ApprovalId, CommandInfo, Notice, NoticeItem, NoticeTone, ProviderId, Role, SessionPort, SetupAnswer, SetupContext,
    SetupStart, StatusInfo, UiEvent, WelcomeInfo,
};
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};
use tokio::sync::oneshot;

/// How long the MCP servers get to shut down on the way out.
const MCP_CLOSE_TIMEOUT: Duration = Duration::from_secs(2);
/// How long Herdr gets to hear that Marv left the pane.
const HERDR_RELEASE_TIMEOUT: Duration = Duration::from_secs(1);
const WORKING: &str = "Marv is working: stop the current turn (Esc) before resuming another session.";
const NEXT_TIME: &str = "Marv will use it from the next conversation (/clear starts one).";
const MCP_NOT_YET: &str = "That /mcp command isn't available in marv-rs yet (the MCP manager is still being ported).";

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
        tool: request.tool.clone(),
        label: request.label.clone(),
        preview: view::Preview {
            title: p.title.clone(),
            diff: p.diff.as_ref().map(|d| d.iter().map(diff_line).collect()),
            command: p.command.clone(),
            text: p.text.clone(),
            note: p.note.clone(),
            warning,
        },
        scope: view::Scope { key: request.scope.key.clone(), description: request.scope.description.clone() },
        agent: request.agent.clone(),
        network: request.network,
        once: request.once,
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
    let status = if run.declined {
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
        L::TextDelta(text) => UiEvent::TextDelta(text.clone()),
        L::ThinkingDelta(text) => UiEvent::ThinkingDelta(text.clone()),
        L::Usage(usage) => UiEvent::Usage { last: *usage, totals: totals() },
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
        L::ToolLoop { continued } => UiEvent::ToolLoop { continued: *continued },
        L::Compacted => UiEvent::Compacted,
        L::Done(reason) => UiEvent::Done(*reason),
        L::Retry { attempt, max, message, continued } => {
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

/// The tainted-session line (marv-rs, docs/SECURITY.md): from now on egress asks, even in yolo.
pub fn tainted_text(source: marv_core::Provenance) -> String {
    let what = match source {
        marv_core::Provenance::Web => "a web page",
        marv_core::Provenance::Mcp => "an MCP server's output",
        marv_core::Provenance::Mesh => "a mesh message",
        marv_core::Provenance::Image => "an image",
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
            TurnStatus::Running => view::TurnStatus::Running,
        })],
        SessionEvent::Loop(event) => {
            // Providers reuse call ids from step to step (Ollama's call_0…): a new agent call replaces the title.
            if let LoopEvent::ToolStart { call, label } = &event
                && call.name == "agent"
            {
                map.titles.insert(call.id.clone(), label.clone());
            }
            map_loop(&event, totals).into_iter().collect()
        }
        SessionEvent::Subagent { call_id, event } => {
            let title = map.titles.get(&call_id).cloned().unwrap_or_default();
            let mut out = Vec::new();
            // Subagents' requests count toward the session's tokens and cost; the context bar stays the main agent's.
            if matches!(event, LoopEvent::Usage(_))
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
    }
}

fn provider_kind(id: ProviderId) -> ProviderKind {
    match id {
        ProviderId::OpenRouter => ProviderKind::OpenRouter,
        ProviderId::Ollama => ProviderKind::Ollama,
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
    }
}

/// What the setup screen starts from. Keys go to the UI masked.
pub fn setup_context(start: SetupStart, file: Option<&FileConfig>, config: &Config, env: &Env) -> SetupContext {
    let kinds = [ProviderKind::OpenRouter, ProviderKind::Ollama];
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
        saved_key: file.and_then(|f| f.api_key.as_deref()).map(mask_key),
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
        next.api_key = Some(key.clone());
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
    herdr: Option<HerdrReporter>,
}

impl Activity {
    fn new(tx: UnboundedSender<UiEvent>, herdr: Option<HerdrReporter>) -> Arc<Self> {
        Arc::new(Self { tx, next: AtomicU64::new(1), asks: Mutex::default(), busy: AtomicBool::new(false), herdr })
    }

    fn emit(&self, event: UiEvent) {
        let _ = self.tx.send(event);
    }

    /// Herdr's state: blocked at an approval, working while busy, idle otherwise.
    pub fn herdr_state(&self) -> (HerdrState, Option<String>) {
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
    pub skills: Vec<LoadedSkill>,
    pub skill_problems: Vec<String>,
    pub agents: Vec<marv_session::agents::AgentType>,
    pub agent_problems: Vec<String>,
    pub memory: Option<(MemoryPaths, Memories)>,
    pub mcp: Option<McpHandle>,
    pub mcp_problems: Vec<String>,
    /// Shortened for display.
    pub hidden_path_folders: Vec<String>,
    pub sandbox_available: bool,
    pub sessions: Option<Arc<SessionStore>>,
    pub trajectories: Option<Arc<TrajectoryStore>>,
    pub herdr: Option<HerdrReporter>,
    /// How a provider is made from the config (`create_provider`; tests: a scripted one).
    pub make_provider: MakeProvider,
    pub load_models: LoadModels,
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
    instructions: bool,
    skills: Vec<LoadedSkill>,
    skill_problems: Vec<String>,
    agents: Vec<marv_session::agents::AgentType>,
    agent_problems: Vec<String>,
    memory: Option<MemoryPaths>,
    mcp: Option<McpHandle>,
    mcp_problems: Vec<String>,
    hidden_path_folders: Vec<String>,
    sandbox_available: bool,
    sessions: Option<Arc<SessionStore>>,
    trajectories_dir: Option<PathBuf>,
    last_checks: Mutex<Option<ProjectCheckResult>>,
    make_provider: MakeProvider,
    load_models: LoadModels,
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
        init.skills = s.skills.clone();
        init.agents = s.agents.clone();
        init.memory = s.memory.clone();
        init.mcp = s.mcp.clone();
        // The CLI started them, so the CLI closes them (on the way out).
        init.owns_mcp = false;
        init.approve = Some(Arc::new(TuiApprover(activity.clone())));
        init.max_steps = Some(config.max_steps);
        (init.sandbox, init.yolo, init.diagnostics, init.checks, init.mcp_tools) =
            (config.sandbox, config.yolo, config.diagnostics, config.checks, config.mcp_tools);
        init.no_yolo = marv_session::sdk::no_yolo_reason(&s.root, &s.home);
        if init.no_yolo.is_some() {
            init.yolo = false;
        }
        init.sessions = s.sessions.clone();
        init.trajectories = s.trajectories.clone();
        init.log_trajectories = config.trajectories;
        init.worktrees_dir = Some(s.config_dir.join("worktrees").join(project_key(&s.root)));
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
            instructions: s.instructions.is_some(),
            skills: s.skills,
            skill_problems: s.skill_problems,
            agents: s.agents,
            agent_problems: s.agent_problems,
            memory: s.memory.map(|(paths, _)| paths),
            mcp: s.mcp,
            mcp_problems: s.mcp_problems,
            hidden_path_folders: s.hidden_path_folders,
            sandbox_available: s.sandbox_available,
            sessions: s.sessions,
            trajectories_dir,
            last_checks: Mutex::default(),
            make_provider: s.make_provider,
            load_models: s.load_models,
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
        }
    }

    fn session_yolo_allowed(&self) -> bool {
        marv_session::sdk::no_yolo_reason(&self.root, &self.home).is_none()
    }

    pub fn welcome(&self) -> WelcomeInfo {
        let memories = self.session.memory().map_or(0, |m| m.personal.len() + m.project.len());
        WelcomeInfo {
            version: self.version.clone(),
            cwd: self.cwd(),
            instructions: self.instructions,
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
        if let Some(cut) = cut {
            let item = instructions_cut_item(cut);
            notes.push((NoticeTone::Info, NoticeItem { text: item.text, detail: item.detail }));
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
        self.emit(UiEvent::Resumed(view::Resumed {
            id: resumed.id,
            updated_at: resumed.updated_at,
            model: resumed.model,
            transcript: resumed.transcript.iter().filter_map(transcript_message).collect(),
            totals: totals(&resumed.totals),
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
        // Working from before the turn's task starts, so Herdr hears it before anything the turn asks.
        self.activity.set_busy(true);
        let turn = match self.session.send(&text, SendOptions { for_model, cancel: None }) {
            Ok(turn) => turn,
            Err(e) => {
                self.activity.set_busy(false);
                // The App started its turn on submit: end it.
                self.emit(system(e.to_string(), true));
                self.emit(UiEvent::TurnEnd { turn: String::new(), reason: view::TurnEndReason::Loop(StopReason::Error) });
                return;
            }
        };
        let me = self.clone();
        tokio::spawn(async move {
            let mut turn = turn;
            let mut map = TurnMap::default();
            let usage_of = me.session.clone();
            let totals_fn = move || totals(&usage_of.usage().totals);
            let last_of = me.session.clone();
            let last_fn = move || last_of.usage().last;
            while let Some(event) = turn.next().await {
                let end = matches!(event, SessionEvent::TurnEnd { .. });
                if end {
                    me.activity.set_busy(false);
                }
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
            me.activity.set_busy(false);
        });
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
            CommandAction::Skill { name, args } => {
                let Some(skill) = self.skills.iter().find(|s| s.skill.name == name) else { return };
                let for_model = marv_session::skills::skill_message(&skill.skill, &args);
                // As typed: the user's line, then the turn (the App starts it on its first event).
                self.emit(UiEvent::Message(message(Role::User, line)));
                self.run_turn(line.to_string(), Some(for_model));
            }
            CommandAction::McpTrust
            | CommandAction::McpAdd { .. }
            | CommandAction::McpSearch { .. }
            | CommandAction::McpRemove { .. }
            | CommandAction::McpLogin { .. }
            | CommandAction::McpLogout { .. } => self.emit(system(MCP_NOT_YET, true)),
            CommandAction::Exit => self.emit(UiEvent::Exit),
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
            hidden_path_folders: &self.hidden_path_folders,
            sandbox_available: self.sandbox_available,
        };
        commands::run_command(line, &ctx)
    }
}

/// The handle the App holds: every method returns at once; results come back as events.
pub struct Port(pub Arc<Bridge>);

impl SessionPort for Port {
    fn commands(&self) -> Vec<CommandInfo> {
        let b = &self.0;
        let mut menu: Vec<CommandInfo> = commands::commands()
            .iter()
            .map(|c| CommandInfo { name: c.name.into(), description: c.description.into() })
            .chain(b.skills.iter().map(|s| CommandInfo { name: s.skill.name.clone(), description: s.skill.description.clone() }))
            .collect();
        // Server names after "/mcp login " and "/mcp logout ".
        for name in b.mcp.as_ref().map(|m| m.loginable()).unwrap_or_default() {
            menu.push(CommandInfo {
                name: format!("mcp login {name}"),
                description: format!("Log in to {name} (OAuth, in your browser)"),
            });
            menu.push(CommandInfo { name: format!("mcp logout {name}"), description: format!("Forget {name}'s login") });
        }
        menu
    }

    fn send(&self, text: String) {
        self.0.run_turn(text, None);
    }

    fn run_command(&self, line: String) {
        let bridge = self.0.clone();
        let action = bridge.command_action(&line);
        tokio::spawn(async move { bridge.apply(&line, action).await });
    }

    fn answer(&self, id: ApprovalId, d: view::Decision) {
        self.0.activity.answer(id.0, decision(d));
    }

    fn abort(&self) {
        self.0.session.interrupt();
    }

    fn load_models(&self, provider: ProviderId) {
        let b = &self.0;
        let kind = provider_kind(provider);
        // A custom endpoint (baseUrl) only applies to the provider it was saved with.
        let file = b.file();
        let base_url = file.as_ref().filter(|f| f.provider == Some(kind)).and_then(|f| f.base_url.clone());
        let config = resolve_config(Some(&FileConfig { provider: Some(kind), base_url, ..FileConfig::default() }), &b.env);
        let models = (b.load_models)(kind, config.base_url);
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
                    bridge.report_session().await;
                }
                Ok(None) => bridge.emit(system("That session couldn't be loaded.", true)),
                Err(e) if bridge.session.busy() => bridge.emit(system(format!("{WORKING} ({e})"), true)),
                Err(e) => bridge.emit(system(format!("That session couldn't be loaded: {e}."), true)),
            }
        });
    }

    fn flush(&self) -> BoxFuture<'static, ()> {
        let session = self.0.session.clone();
        Box::pin(async move { session.flush().await })
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// Startup
// ---------------------------------------------------------------------------------------------------------------------

/// Runs the interactive session. `resume`: `Some(true)` for -c (latest), `Some(false)` for -r (pick).
pub async fn run(resume: Option<bool>) -> anyhow::Result<i32> {
    // The real environment only (docs/SECURITY.md): never a project's .env.
    let env: Env = std::env::vars().collect();
    let home = home_dir();
    // The project is wherever Marv was started; the tools can't reach outside it.
    let cwd = std::env::current_dir()?;
    let root = std::fs::canonicalize(&cwd).unwrap_or(cwd);
    let config_dir = default_config_dir(&env, &home);
    let instructions = read_instructions(&root);
    let (skills, skill_problems) = marv_session::skills::load_skills(Some(&root), Some(&home));
    let (agents, agent_problems) = marv_session::agents::load_agents(Some(&root), Some(&home));
    // MCP servers start now, in the background; the first request waits for them.
    let loaded =
        marv_mcp::config::load_mcp_config(&root, &config_dir, &env, marv_mcp::config::Include { personal: true, project: true });
    let mcp: Option<McpHandle> = (!loaded.servers.is_empty()).then(|| {
        marv_mcp::manager::McpManager::new(
            loaded.servers,
            marv_mcp::manager::ManagerOptions {
                root: root.clone(),
                project_key: project_key(&root),
                home: home.clone(),
                version: env!("CARGO_PKG_VERSION").into(),
                trust: Some(marv_mcp::trust::McpTrust::new(config_dir.join("mcp-trust.json"))),
                // OAuth logins (/mcp login), shared with marv -p and SDK sessions using the same config folder.
                logins: Some(marv_mcp::oauth_store::McpTokenStore::new(config_dir.join("mcp-tokens.json"))),
                env: env.clone(),
                connect_timeout: marv_mcp::manager::CONNECT_TIMEOUT,
                login_timeout: marv_mcp::manager::LOGIN_TIMEOUT,
            },
        )
    });
    if let Some(mcp) = &mcp {
        let mcp = mcp.clone();
        tokio::spawn(async move { mcp.start().await });
    }
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
    // PATH folders under the home folder the sandbox hides are found once here and reported at startup (Marv #34:
    // with node from mise, the post-edit typecheck and other node_modules scripts silently never ran).
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

    let cut = instructions.as_ref().and_then(|i| i.cut.clone());
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
        skills,
        skill_problems,
        agents,
        agent_problems,
        memory: Some((paths, memories)),
        mcp: mcp.clone(),
        mcp_problems: loaded.problems,
        hidden_path_folders: hidden,
        sandbox_available: marv_sandbox::bwrap::sandbox_available(),
        sessions: Some(Arc::new(SessionStore::new(config_dir.join("sessions")))),
        trajectories: Some(Arc::new(TrajectoryStore::new(config_dir.join("trajectories")))),
        herdr: herdr.clone(),
        make_provider: Arc::new(|config, info| marv_provider::create_provider(config, info, None)),
        load_models: Arc::new(|kind, base_url| {
            Box::pin(async move { marv_provider::models::list_models(kind, &base_url, None).await })
        }),
    };
    let bridge = Bridge::start(startup, tx)?;
    bridge.announce(cut.as_ref());
    match resume {
        // Before the UI, so nothing typed lands in the conversation being swapped out.
        Some(true) => bridge.resume_latest().await,
        Some(false) => bridge.open_picker().await,
        None => {}
    }
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
    // Stop a turn still running: its tools mustn't outlive the UI.
    bridge.session().interrupt();
    // Let servers shut down cleanly, but don't hang on one that won't.
    if let Some(mcp) = mcp {
        let _ = tokio::time::timeout(MCP_CLOSE_TIMEOUT, tokio::spawn(async move { mcp.close().await })).await;
    }
    // Herdr stops showing an agent in this pane; no waiting on a Herdr that doesn't answer.
    if let Some(herdr) = herdr {
        let _ = tokio::time::timeout(HERDR_RELEASE_TIMEOUT, herdr.release()).await;
    }
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

    /// A temp project with notes.txt and a config folder; removed on drop.
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
        }
    }

    /// Ollama with yolo off: every change asks.
    fn asks() -> FileConfig {
        FileConfig {
            provider: Some(ProviderKind::Ollama),
            model: Some("qwen3.5:9b".into()),
            yolo: Some(false),
            diagnostics: Some(false),
            checks: Some(false),
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
            skills: Vec::new(),
            skill_problems: Vec::new(),
            agents: vec![marv_session::agents::general_purpose()],
            agent_problems: Vec::new(),
            memory: Some((memory_paths(&dirs.config, &dirs.project), Memories::default())),
            mcp: None,
            mcp_problems: Vec::new(),
            hidden_path_folders: Vec::new(),
            sandbox_available: false,
            sessions: Some(Arc::new(SessionStore::new(dirs.config.join("sessions")))),
            trajectories: None,
            herdr,
            make_provider: Arc::new(move |_, _| provider.clone()),
            load_models: Arc::new(|_, _| {
                Box::pin(async {
                    Ok(vec![marv_provider::ModelInfo { id: "qwen3.5:9b".into(), local: true, ..Default::default() }])
                })
            }),
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
        assert!(matches!(next(&mut rx).await, UiEvent::Message(m) if m.is_error && m.text == MCP_NOT_YET));
        port.run_command("/nope".into());
        assert!(matches!(next(&mut rx).await, UiEvent::Message(m) if m.is_error && m.text.starts_with("Unknown command: /nope")));
        port.run_command("/good".into());
        assert!(matches!(next(&mut rx).await, UiEvent::Message(m) if m.is_error));
        port.run_command("/exit".into());
        assert_eq!(next(&mut rx).await, UiEvent::Exit);
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
        assert_eq!(ctx.saved_key.as_deref(), Some("sk-or-v1-0…cdef"));
        assert_eq!(ctx.env_keys, [(ProviderId::OpenRouter, "sk-or-v1-f…ment".to_string())]);
        assert!(!format!("{ctx:?}").contains("fromtheenvironment"), "keys reach the UI masked");
    }

    #[test]
    fn approvals_carry_the_protected_paths_reason() {
        let request = marv_tools::ApprovalRequest {
            tool: "write_file".into(),
            label: ".envrc".into(),
            preview: marv_tools::Preview { title: "Create .envrc".into(), ..Default::default() },
            scope: marv_tools::Scope { key: "files".into(), description: "file changes".into() },
            agent: Some("implementer · Task 2".into()),
            network: false,
            once: true,
            must_ask: true,
            protected: marv_sandbox::protected::rule_for_relative(Path::new(".envrc")),
        };
        let shown = approval_request(&request);
        assert!(shown.preview.warning.unwrap().starts_with("changes .envrc"));
        assert_eq!((shown.agent.as_deref(), shown.once), (Some("implementer · Task 2"), true));
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
            SessionEvent::Subagent { call_id: "a1".into(), event: LoopEvent::Usage(Default::default()) },
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
