//! The shapes every tool shares: the [`Tool`] trait, [`ToolContext`], [`ToolOutput`], approvals and previews.
//!
//! Ports Marv's `src/tools/types.ts`.
//!
//! - A tool is a name, a description the model reads to decide when to use it, an input schema (sent to the model as
//!   JSON schema; the typed `Input` validates what comes back, like zod), a `label` for the transcript, and `run`.
//! - `kind` Read runs freely; Write/Execute need approval unless `needs_approval` is defined, which then alone decides
//!   per call (`agent` asks only with `isolation: "worktree"`), or yolo runs it (`auto_safe`).
//! - `preview` runs before the approval: a [`ToolError`] there means the call can't succeed, so the user is never
//!   asked to approve it.
//! - `repair` (issue #10) fixes a malformed call whose meaning is beyond doubt, before validation; never fails.
//! - `repair_mode` (#65): how far `run_tool`'s schema-guided repair ([`crate::schema_repair`]) goes for this tool;
//!   built-in by default, MCP tools only coerce types.
//! - `ToolOutput::diff` and `ToolOutput::plan` are for the screen only: never sent to the model (the loop appends `output`) nor written to
//!   trajectories (they record named fields).
//! - `ToolContext::before_change` is started (not awaited) by the edit tools' preview and awaited in `run` before
//!   writing: the session takes the typecheck's baseline there, so it sees the project as it was.
//! - `ToolContext::formatter` (#67) is what the edit tools run after their write: the session builds it (detection
//!   plus the sandboxed run), the tools re-read the file and diff what's on disk.
//! - `ToolContext::checkpoint` (#16) is awaited by `run_tool` after the approval (or yolo) and before running any
//!   call whose tool takes one (`Tool::checkpoint`); `Preview::files` is /undo's list of files, each with its diff.
//!
//! marv-rs changes (docs/SECURITY.md):
//! - `ToolKind::Egress` and `uses_network`: once the session is tainted ([`marv_sandbox::TaintState`]) such calls ask
//!   even in yolo and even after an "always" (`ApprovalRequest::must_ask`).
//! - `protected_target`: a write to a protected path asks even in yolo.
//! - `provenance`: every result is tagged with its source; outside sources (web, MCP, mesh, GitHub, networked
//!   commands…) are fenced and taint the session. `kind` and `provenance` have no defaults: each tool declares both.
//! - The typed `Input` replaces zod. The trait is generic; [`DynTool`] is its object-safe, type-erased form that the
//!   registry holds (`Arc<dyn DynTool>`), and every `T: Tool` gets it for free.

use std::any::Any;
use std::path::PathBuf;
use std::sync::Arc;

use crate::formatter::Formatter;
use async_trait::async_trait;
pub use marv_core::Door;
use marv_core::{Provenance, ToolKind, ToolSpec};
use marv_sandbox::{ProtectedRule, Sandbox, TaintState};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::schema_repair::RepairMode;

/// An expected failure (bad path, missing file); its message goes to the model as is.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct ToolError(pub String);

impl ToolError {
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl From<marv_sandbox::SandboxError> for ToolError {
    fn from(err: marv_sandbox::SandboxError) -> Self {
        Self(err.to_string())
    }
}

/// One line of a diff, for showing a change before it's approved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DiffKind {
    Add,
    Del,
    Ctx,
    /// Skipped unchanged lines between hunks ("…").
    Gap,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffLine {
    pub kind: DiffKind,
    pub text: String,
    /// The line's number before the change (removed and unchanged lines).
    #[serde(rename = "oldLine", skip_serializing_if = "Option::is_none", default)]
    pub old_line: Option<usize>,
    /// The line's number after the change (added and unchanged lines).
    #[serde(rename = "newLine", skip_serializing_if = "Option::is_none", default)]
    pub new_line: Option<usize>,
}

impl DiffLine {
    pub fn gap() -> Self {
        Self { kind: DiffKind::Gap, text: "…".into(), old_line: None, new_line: None }
    }
}

/// A change as the transcript shows it: at most `MAX_SHOWN_DIFF` lines, and how many more there were.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffShown {
    pub lines: Vec<DiffLine>,
    pub more: usize,
}

/// What the approval prompt shows before a tool changes something.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Preview {
    /// "Edit src/app.ts", "Run a command".
    pub title: String,
    pub diff: Option<Vec<DiffLine>>,
    /// A shell command, shown as "$ command".
    pub command: Option<String>,
    /// Plain text, e.g. a memory to save.
    pub text: Option<String>,
    /// A small detail line, e.g. "sandboxed · no network".
    pub note: Option<String>,
    /// Shown in the warning color, e.g. "runs WITHOUT a sandbox".
    pub warning: Option<String>,
    /// Several files, each with its own diff (/undo: what goes back where).
    pub files: Option<Vec<PreviewFile>>,
}

/// One file of a [`Preview::files`] list.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PreviewFile {
    /// "Restore src/app.ts".
    pub title: String,
    pub diff: Option<DiffShown>,
    /// "binary", "too large to show", "changed since".
    pub note: Option<String>,
}

/// "Don't ask again this session" applies to everything with the same key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scope {
    pub key: String,
    /// For the prompt: "Yes, don't ask again for <description>".
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalRequest {
    pub tool: String,
    pub label: String,
    pub preview: Preview,
    pub scope: Scope,
    /// Which subagent is asking ("implementer · Task 2"); None for the main agent. Set by the subagent's approver.
    pub agent: Option<String>,
    /// The action can reach the network (bash with network: true).
    pub network: bool,
    /// Yes or no only, no "don't ask again" (/mcp add).
    pub once: bool,
    /// marv-rs: a person must answer this one. The session's "always" wrapper must not answer it from remembered
    /// scopes (tainted egress, protected paths).
    pub must_ask: bool,
    /// marv-rs: the protected path rule this call touches, for the approval's warning.
    pub protected: Option<ProtectedRule>,
    /// The call leaves the sandbox through one of its doors: the prompt shows that door's warning line.
    pub door: Option<Door>,
    /// marv-rs #185: a model's tool call, so the person may decline it with a reason the model reads as the call's
    /// result (`Decision::Declined`). Only `run_tool`'s own request sets it: not the step-limit question, a mesh
    /// wire, /undo or /mcp add, where no model would read a reason.
    pub reason: bool,
    /// What it writes is loaded into every later conversation as the person's own (a memory) and the conversation is
    /// tainted (`Tool::persists`): only a person may answer it. An approver answering from a policy (`marv -p`'s)
    /// refuses it whatever it was told.
    pub persists: bool,
}

/// The doors this agent may use where a door is gated by who's running, not by the approver (an approver can be told
/// to say yes to an exact scope key: `marv -p --approve`). Set by the session for the main agent only; a subagent's
/// context is built without it. Marv's `Doors`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Doors {
    /// bash with sandbox: false: only where the user approves each command at a terminal (the TUI, or the SDK's
    /// hostCommands).
    pub host: bool,
    pub process: bool,
    /// The github tool's writes and sensitive reads (#47): the main agent's, where the session offers the tool; a
    /// subagent only reads.
    pub github: bool,
    /// The Herdr door's commands and spawns (#74): only where the user approves each one at a terminal (the TUI), as
    /// `host`. A subagent, `marv -p` and an SDK session without `host_commands` read panes at most.
    pub herdr: bool,
}

/// For a sandboxed bash command: a running process's forwarded port, and where its socket folder is (Marv #51).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessForward {
    pub port: u16,
    /// A short id, for the folder's mount point (`/run/marv/<id>`).
    pub id: String,
    /// The socket folder on the host.
    pub dir: PathBuf,
}

/// A background process's state (Marv's `ProcessInfo["state"]`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProcessState {
    Running,
    Exited,
}

/// A background process, as the process tool and `/processes` show it (Marv #51).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessInfo {
    pub name: String,
    pub command: String,
    /// Started with network: it shares the host's network, so its ports are the host's (nothing is forwarded).
    pub network: bool,
    pub state: ProcessState,
    /// When it exited: its code (128 + n when a signal ended it).
    pub exit_code: Option<i32>,
    /// Ports the user can open on their 127.0.0.1 (forwarded, or the host's own with network).
    pub ports: Vec<u16>,
    /// What wasn't forwarded, and why ("port 4789 is in use on the user's machine…").
    pub notes: Vec<String>,
    /// Milliseconds since the epoch.
    pub started_at: u64,
}

/// What a start hands back: the process, and the first lines of its output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartResult {
    pub info: ProcessInfo,
    pub output: String,
}

/// What the process tool runs through: the session's process manager (marv-session's `ProcessManager`). Only the main
/// agent's context has one. Marv's `ProcessHost`.
#[async_trait]
pub trait ProcessHost: Send + Sync {
    async fn start(&self, command: &str, name: Option<&str>, network: bool) -> Result<StartResult, ToolError>;
    /// Its last `lines` lines of output (at most `MAX_LOG_LINES`).
    fn logs(&self, name: &str, lines: usize) -> Result<String, ToolError>;
    /// Stops it and forgets it; false when there's none of that name.
    async fn stop(&self, name: &str) -> bool;
    fn list(&self) -> Vec<ProcessInfo>;
    /// A one-shot session (`marv -p`): Marv stops every process when it exits after the reply (#86).
    fn one_shot(&self) -> bool {
        false
    }
}

/// The running processes' forwarded ports, as bash's relay needs them (Marv's `ToolContext.forwards`).
pub trait ProcessForwards: Send + Sync {
    fn forwards(&self) -> Vec<ProcessForward>;
}

/// An approver's answer. `Unavailable`: no one can approve this call (a headless run's `--approve` list doesn't
/// cover it), refused like a call with no approver at all, as an error the model can work around; `No` is a person
/// declining, which stops the run so they can say what to do instead.
///
/// `Declined(reason)` (#185) is a person declining with a short reason: the call doesn't run, its result tells the
/// model it was declined and why, and the turn goes on. It's a "no" everywhere an answer is read as approval (never
/// a yes, never remembered); the reason is the user's own words (`Provenance::User`), so it doesn't taint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Decision {
    Yes,
    Always,
    No,
    Declined(String),
    Unavailable,
}

/// How a call that needed approval got it (for trajectories). None on [`ToolOutput::approval`]: it needed none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Approval {
    /// Yolo ran it (`auto_safe`).
    Auto,
    /// The run was stopped before an answer counted.
    Interrupted,
    Decided(Decision),
}

/// Asks the user before a tool that changes something runs. The session wraps the client's approver to remember
/// "always" scopes (shared by the main agent and its subagents) and to queue parallel requests.
#[async_trait]
pub trait Approver: Send + Sync {
    async fn approve(&self, request: ApprovalRequest) -> Decision;
}

/// The session's hook before the first file change of a turn (the typecheck baseline). Never fails.
#[async_trait]
pub trait BeforeChange: Send + Sync {
    async fn before_change(&self);
}

/// The turn's checkpoint (marv-session's `checkpoints`): a snapshot of the project before the turn's first change, so
/// /undo can put it back. `run_tool` awaits it after the approval and before running any call whose tool takes one
/// ([`Tool::checkpoint`]); one snapshot per turn, shared by every call and by shared-folder subagents. Never fails.
#[async_trait]
pub trait Checkpoint: Send + Sync {
    async fn checkpoint(&self);
}

/// A loaded skill, as the `skill` tool needs it. Loading (`src/skills.ts`) lives in marv-session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    /// The skill's folder (`.marv/skills/<name>` or `~/.marv/skills/<name>`).
    pub dir: PathBuf,
    /// The instructions after the frontmatter.
    pub body: String,
    /// The other files in its folder, relative ("reference.md", "scripts/extract.py").
    pub files: Vec<String>,
    /// A trusted project skill's files as the person trusted them, by name: their hashes (`skill::content_hash`).
    /// A file read later that doesn't match (changed, or new) is quoted as the project's data. `None`: the person's
    /// own skill, whose files are theirs.
    pub trusted_files: Option<std::collections::BTreeMap<String, String>>,
}

/// Where the memory tool reads and writes (`~/.marv/memory/personal.md`, `~/.marv/memory/projects/<project>.md`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryPaths {
    pub personal: PathBuf,
    pub project: PathBuf,
}

/// A running subagent's state, for its transcript entry.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentProgress {
    /// "worktree marv/x-1a2b · 3 tools · read_file src/a.ts"
    pub line: String,
    /// Its finished tool calls, newest last ("read_file src/a.ts · 120 lines").
    pub steps: Vec<String>,
}

/// What the `agent` tool needs from the session to start subagents. Only the main agent's context has one, so
/// subagents can't start subagents. Implemented by marv-session (which owns the loop, providers and worktrees;
/// marv-tools can't depend on marv-agent, which depends on it).
#[async_trait]
pub trait AgentHost: Send + Sync {
    /// Agent types as (name, description), for errors and the preview.
    fn agent_types(&self) -> Vec<(String, String)>;
    /// Whether worktrees are available (`worktreesDir` set).
    fn worktrees(&self) -> bool;
    /// The worktree dispatch's preview: every check `run_subagent` makes before it starts.
    async fn preview(&self, input: &crate::agent::AgentInput, ctx: &ToolContext) -> Result<Preview, ToolError>;
    /// Runs the subagent (`runSubagent`) and returns its final report.
    async fn run(&self, input: &crate::agent::AgentInput, ctx: &ToolContext) -> Result<ToolOutput, ToolError>;
}

/// The status of one plan item; at most one is `InProgress` at a time (the current item).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanStatus {
    Pending,
    InProgress,
    Done,
}

/// One item of the plan tool's checklist (#15).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanItem {
    pub text: String,
    pub status: PlanStatus,
}

/// What the plan tool writes through: the session's state.
pub trait PlanHost: Send + Sync {
    /// The whole list, replacing the previous one; empty clears the plan.
    fn set(&self, items: Vec<PlanItem>);
}

/// Everything a tool call may use. Cheap to clone (paths and `Arc`s); `run_tool` clones it per call to set
/// `call_id` and `confined`.
#[derive(Clone)]
pub struct ToolContext {
    /// Absolute path of the project root; tools may not reach outside it.
    pub root: PathBuf,
    /// The home folder `~/` expands to (tests pass a fake one).
    pub home: PathBuf,
    /// Esc / ctrl+c (Marv's `AbortSignal`).
    pub cancel: CancellationToken,
    /// Without one, tools that need approval are refused ("there's no one to ask").
    pub approver: Option<Arc<dyn Approver>>,
    pub sandbox: Arc<Sandbox>,
    /// Yolo mode: calls whose tool says they're safe (`auto_safe`) run without asking.
    pub yolo: bool,
    /// Automatic approval is off: a turn the person at this Marv didn't start (a mesh wake, #78; the radio, #124).
    /// Yolo doesn't apply, a URL prefix approved earlier doesn't either, and every call that asks is asked `once`, so
    /// neither a remembered "always" nor a worktree subagent's automatic yes answers it (the Herdr door's light actions
    /// included, #74). Only a person at this Marv does. Subagents inherit it.
    pub no_auto_approval: bool,
    /// Set by `run_tool` for a call that runs without asking: bash then sees the project's `.git` read-only, and the
    /// edit tools re-check `.git` at apply time.
    pub confined: bool,
    /// Skills the `skill` tool can load.
    pub skills: Arc<Vec<Skill>>,
    pub memory: Option<MemoryPaths>,
    /// The id of the call being run (set by `run_tool`).
    pub call_id: Option<String>,
    /// Extra folders bash may read but not change: a worktree's shared `.git`.
    pub read_only: Vec<PathBuf>,
    pub agent_host: Option<Arc<dyn AgentHost>>,
    /// Where the plan tool keeps its list (#15). Only the main agent has one, so subagents have no plan.
    pub plan: Option<Arc<dyn PlanHost>>,
    /// Pins where git looks for this agent's file listing (a worktree subagent).
    pub git_env: Vec<(String, String)>,
    pub before_change: Option<Arc<dyn BeforeChange>>,
    /// The turn's checkpoint (see [`Checkpoint`]); separate from `before_change`, the checks' baseline.
    pub checkpoint: Option<Arc<dyn Checkpoint>>,
    /// The session's taint (marv-rs).
    pub taint: Arc<TaintState>,
    /// The doors out of the sandbox this agent has (see [`Doors`]). Default: none.
    pub doors: Doors,
    /// The session's background processes (Marv #51), for the process tool. Only the main agent has them.
    pub processes: Option<Arc<dyn ProcessHost>>,
    /// The running processes' forwarded ports: a sandboxed bash command (without network) gets a relay to each, so
    /// its `curl localhost:<port>` reaches the session's own servers. Subagents get this too (but not `processes`).
    pub forwards: Option<Arc<dyn ProcessForwards>>,
    /// The project's formatter (#67), built by the session: it runs on every file the edit tools write, inside the
    /// sandbox, before the tools' notes; without it (off, or no sandbox) the tools change nothing but their edit.
    pub formatter: Option<Arc<dyn Formatter>>,
    /// Commands that failed in the sandbox without network in a way that looked like it (#81): a retry without
    /// network is asked for with network instead (bash's `repair`). Fresh for each context (the session's turn, a
    /// subagent).
    pub network_failures: Arc<NetworkFailures>,
    /// This session's folder of saved outputs (#61, `~/.marv/tool-output/<session>`): a cut result is saved there
    /// whole, and `read_file` and `grep` may read it (only it, read-only). None: cuts aren't saved.
    pub tool_output: Option<PathBuf>,
    /// Where a running call says what it's waiting for (#201: "waiting for beta to accept the wire"), shown on its
    /// card. The main agent's only; None: nothing is shown.
    pub progress: Option<Arc<dyn ToolProgress>>,
}

/// A running call's one-line status on its card (the session sends it the way it sends a subagent's progress).
pub trait ToolProgress: Send + Sync {
    /// `line` is Marv's own words, never model or mesh text.
    fn progress(&self, call_id: &str, line: String);
}

/// Sandboxed commands, by [`marv_core::command::command_key`], whose failure without network looked like the missing
/// network (the ones that got [`marv_sandbox::notes::NETWORK_NOTE`]), so a retry that only changes the tail
/// (`&& du -sh`, `| tail -2`) is recognized (#81).
#[derive(Debug, Default)]
pub struct NetworkFailures(std::sync::Mutex<std::collections::HashSet<String>>);

impl NetworkFailures {
    pub fn record(&self, command: &str) {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).insert(marv_core::command::command_key(command));
    }
    pub fn contains(&self, command: &str) -> bool {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).contains(&marv_core::command::command_key(command))
    }
}

impl ToolContext {
    /// A context for `root` with Marv's defaults: sandbox on (from the real environment), yolo off, no approver.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let sandbox = marv_sandbox::Sandbox::from_env(true);
        Self {
            root: root.into(),
            home: sandbox.home.clone(),
            cancel: CancellationToken::new(),
            approver: None,
            sandbox: Arc::new(sandbox),
            yolo: false,
            no_auto_approval: false,
            confined: false,
            skills: Arc::new(Vec::new()),
            memory: None,
            call_id: None,
            read_only: Vec::new(),
            agent_host: None,
            plan: None,
            git_env: Vec::new(),
            before_change: None,
            checkpoint: None,
            taint: Arc::new(TaintState::new()),
            doors: Doors::default(),
            processes: None,
            forwards: None,
            formatter: None,
            network_failures: Arc::default(),
            tool_output: None,
            progress: None,
        }
    }

    /// Shows `line` on this call's card while it runs (see [`ToolProgress`]); nothing without a sink or a call id.
    pub fn report_progress(&self, line: impl Into<String>) {
        if let (Some(progress), Some(call_id)) = (&self.progress, &self.call_id) {
            progress.progress(call_id, line.into());
        }
    }

    pub fn aborted(&self) -> bool {
        self.cancel.is_cancelled()
    }
}

/// What a tool hands back. Converted to [`marv_core::ToolResult`] for the loop with [`ToolOutput::into_core`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOutput {
    /// What the model sees.
    pub output: String,
    /// A few words for the transcript, e.g. "42 lines".
    pub summary: String,
    /// What a file change did, for the transcript only.
    pub diff: Option<DiffShown>,
    /// The plan tool's list after the call, for the screen only (like `diff`: never sent to the model or a
    /// trajectory; the call already holds it).
    pub plan: Option<Vec<PlanItem>>,
    /// The file a change wrote (with `diff`), project-relative: which project checks run after the step.
    pub changed: Option<String>,
    pub is_error: bool,
    /// The user said no (or the run was interrupted): the agent stops so they can say what to do instead.
    pub declined: bool,
    pub approval: Option<Approval>,
    /// Where `output` came from (marv-rs): text from an outside source is fenced as data and taints. [`ToolOutput::ok`]
    /// starts it at the project's; `run_tool` raises it to the tool's declared source ([`Tool::provenance_in`]).
    pub provenance: Provenance,
    /// The door the call went through (for trajectories); None for an ordinary call. Set by `run_tool`.
    pub door: Option<Door>,
}

impl ToolOutput {
    pub fn ok(output: impl Into<String>, summary: impl Into<String>) -> Self {
        Self {
            output: output.into(),
            summary: summary.into(),
            diff: None,
            plan: None,
            changed: None,
            is_error: false,
            declined: false,
            approval: None,
            provenance: Provenance::Project,
            door: None,
        }
    }

    pub fn error(output: impl Into<String>) -> Self {
        Self { is_error: true, ..Self::ok(output, "error") }
    }

    pub fn into_core(self) -> marv_core::ToolResult {
        marv_core::ToolResult { output: self.output, summary: Some(self.summary), is_error: self.is_error }
    }
}

/// A tool's own repair of a malformed call: the input to use instead, and what to tell the model (a `Marv:` note).
#[derive(Debug, Clone, PartialEq)]
pub struct Repaired {
    pub input: Value,
    pub note: Option<String>,
}

/// A tool. Implement this; the registry holds it as `Arc<dyn DynTool>`.
#[async_trait]
pub trait Tool: Send + Sync + 'static {
    /// The call's arguments, deserialized from the model's JSON (unknown fields are refused, like zod's strict object).
    type Input: DeserializeOwned + Send + Sync + 'static;

    fn name(&self) -> &str;
    /// What the model reads to decide when to use it.
    fn description(&self) -> &str;
    /// The input's JSON schema, as sent to the model. Must be the same bytes every time (prompt cache).
    fn schema(&self) -> Value;
    /// Read runs freely; the rest need approval unless `needs_approval` decides. No default: every tool says what it
    /// is, so a new one can't run without asking by omission.
    fn kind(&self) -> ToolKind;
    /// Where this tool's output comes from (marv-rs): no result of it is trusted more than this. No default: every
    /// tool says, so a new one can't hand the model outside text as the project's by omission. `run_tool` raises every
    /// result to at least this source ([`Provenance::least_trusted`]), whatever the tool set on it. A tool whose
    /// results are sometimes less trusted says so per call ([`Tool::provenance_in`]) or on the result itself (a mesh
    /// answer, a saved MCP output read back): a result is never made more trusted than the tool set it.
    fn provenance(&self) -> Provenance;
    /// `provenance` for one call in `ctx` (bash with the network or on the host). Default: `provenance`.
    fn provenance_in(&self, _input: &Self::Input, _ctx: &ToolContext) -> Provenance {
        self.provenance()
    }
    /// Calls of this tool that come together in one reply run at the same time (subagents).
    fn parallel(&self) -> bool {
        false
    }
    /// Whether `run_tool` takes the turn's checkpoint before a call runs. Default: a kind that changes files (Write,
    /// Execute; marv-rs's Egress is Marv's "read" web_fetch or an MCP or mesh tool). False for tools that change
    /// nothing in the project (MCP servers act outside it, memory writes ~/.marv, a subagent's own calls take it).
    fn checkpoint(&self) -> bool {
        matches!(self.kind(), ToolKind::Write | ToolKind::Execute)
    }
    /// Checks serde can't express (zod's `.min()`, `.max()`, `.int()` ranges). The message follows "Invalid input for <tool>:".
    fn check(&self, _input: &Self::Input) -> Result<(), String> {
        Ok(())
    }
    /// Short label for the transcript, e.g. the file path.
    fn label(&self, input: &Self::Input) -> String;
    /// Overrides `kind` for one call: whether it needs approval.
    fn needs_approval(&self, _input: &Self::Input) -> Option<bool> {
        None
    }
    /// Whether this call can reach the network (worktree approvals still ask; tainted sessions always ask).
    fn uses_network(&self, _input: &Self::Input) -> bool {
        false
    }
    /// `uses_network` for a call in `ctx` (as `scope_in`). Default: `uses_network`.
    fn uses_network_in(&self, input: &Self::Input, _ctx: &ToolContext) -> bool {
        self.uses_network(input)
    }
    /// What the approval prompt shows. A `ToolError` here means the call can't succeed: nobody is asked.
    async fn preview(&self, _input: &Self::Input, _ctx: &ToolContext) -> Result<Preview, ToolError> {
        Ok(Preview { title: self.name().to_string(), ..Preview::default() })
    }
    /// What "don't ask again" covers. Default: the tool's name.
    fn scope(&self, _input: &Self::Input) -> Option<Scope> {
        None
    }
    /// `scope` for a call in `ctx` (Marv passes the context to `scope` since Sprint 4: bash's host door depends on
    /// the session's sandbox). Default: `scope`. marv-rs keeps both so the tools that don't need the context don't
    /// change.
    fn scope_in(&self, input: &Self::Input, _ctx: &ToolContext) -> Option<Scope> {
        self.scope(input)
    }
    /// Whether yolo may run this call without asking: what it can change is confined to the project (never its
    /// `.git`) and it can't reach the network. Default: always ask.
    fn auto_safe(&self, _input: &Self::Input, _ctx: &ToolContext) -> bool {
        false
    }
    /// The protected path this call would change, if any (marv-rs): it then asks even in yolo.
    fn protected_target(&self, _input: &Self::Input, _ctx: &ToolContext) -> Option<ProtectedRule> {
        None
    }
    /// A malformed call whose meaning is beyond doubt, fixed before validation (issue #10). None: leave it.
    fn repair(&self, _raw: &Value, _ctx: &ToolContext) -> Option<Repaired> {
        None
    }
    /// How far `run_tool` repairs the arguments against `schema` (#65). Default: a built-in tool, whose schema is
    /// exactly what it accepts.
    fn repair_mode(&self) -> RepairMode {
        RepairMode::Builtin
    }
    /// marv-rs: the URL this call reaches (web_fetch), as `TaintState`'s prefixes are matched against. In a tainted
    /// session a URL under a prefix the user approved runs without asking, and "always" approves the URL's prefix.
    fn egress_url(&self, _input: &Self::Input) -> Option<String> {
        None
    }
    /// The door this call leaves the sandbox through, if any: `run_tool` copies it onto the call's
    /// [`ApprovalRequest`] and its result (Marv Sprint 4).
    fn door(&self, _input: &Self::Input, _ctx: &ToolContext) -> Option<Door> {
        None
    }
    /// Whether this call must be asked every time ([`ApprovalRequest::once`]): no "don't ask again", ever.
    fn once(&self, _input: &Self::Input, _ctx: &ToolContext) -> bool {
        false
    }
    /// Whether this call writes something every later conversation loads as the person's own words (a memory). In a
    /// tainted conversation such a call is asked of a person every time (`must_ask` and `once`): a line planted by
    /// outside text would outlive the conversation. Default: no.
    fn persists(&self, _input: &Self::Input, _ctx: &ToolContext) -> bool {
        false
    }
    /// marv-rs (#77): this call's approval, asked by the tool itself in one prompt that holds the registry's
    /// `preview` and more (a mesh call's new wire, which only a person grants). `None` (the default): the registry
    /// asks as usual. A tool that answers here must ask a person, yes or no (`once` and `must_ask`, never yolo or a
    /// remembered "always"): its prompt stands in for the registry's, tainted session or not. `Err` fails the call.
    async fn approve_itself(
        &self,
        _input: &Self::Input,
        _ctx: &ToolContext,
        _preview: &Preview,
    ) -> Option<Result<Decision, ToolError>> {
        None
    }
    async fn run(&self, input: &Self::Input, ctx: &ToolContext) -> Result<ToolOutput, ToolError>;
}

/// A tool's input after `parse`, type-erased.
pub type ParsedInput = Box<dyn Any + Send + Sync>;
type In<'a> = &'a (dyn Any + Send + Sync);

/// The object-safe form of [`Tool`]: what the registry and `run_tool` use. Every `Tool` implements it; MCP tools
/// (loose input, the server validates) can implement `Tool` with `Input = serde_json::Value`.
#[async_trait]
pub trait DynTool: Send + Sync {
    fn name(&self) -> &str;
    fn spec(&self) -> ToolSpec;
    fn kind(&self) -> ToolKind;
    fn provenance(&self) -> Provenance;
    fn provenance_in(&self, input: In<'_>, ctx: &ToolContext) -> Provenance;
    fn parallel(&self) -> bool;
    fn checkpoint(&self) -> bool;
    /// Deserialize and check; the error is the text after "Invalid input for <tool>:".
    fn parse(&self, raw: Value) -> Result<ParsedInput, String>;
    fn label(&self, input: In<'_>) -> String;
    fn needs_approval(&self, input: In<'_>) -> Option<bool>;
    fn uses_network(&self, input: In<'_>, ctx: &ToolContext) -> bool;
    async fn preview(&self, input: In<'_>, ctx: &ToolContext) -> Result<Preview, ToolError>;
    fn scope(&self, input: In<'_>, ctx: &ToolContext) -> Option<Scope>;
    fn auto_safe(&self, input: In<'_>, ctx: &ToolContext) -> bool;
    fn protected_target(&self, input: In<'_>, ctx: &ToolContext) -> Option<ProtectedRule>;
    fn repair(&self, raw: &Value, ctx: &ToolContext) -> Option<Repaired>;
    fn repair_mode(&self) -> RepairMode;
    fn egress_url(&self, input: In<'_>) -> Option<String>;
    fn door(&self, input: In<'_>, ctx: &ToolContext) -> Option<Door>;
    fn once(&self, input: In<'_>, ctx: &ToolContext) -> bool;
    fn persists(&self, input: In<'_>, ctx: &ToolContext) -> bool;
    async fn approve_itself(&self, input: In<'_>, ctx: &ToolContext, preview: &Preview) -> Option<Result<Decision, ToolError>>;
    async fn run(&self, input: In<'_>, ctx: &ToolContext) -> Result<ToolOutput, ToolError>;
}

fn typed<T: Tool>(input: In<'_>) -> &T::Input {
    input.downcast_ref::<T::Input>().expect("input parsed by this tool")
}

#[async_trait]
impl<T: Tool> DynTool for T {
    fn name(&self) -> &str {
        Tool::name(self)
    }
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: Tool::name(self).to_string(),
            description: Tool::description(self).to_string(),
            parameters: Tool::schema(self),
        }
    }
    fn kind(&self) -> ToolKind {
        Tool::kind(self)
    }
    fn provenance(&self) -> Provenance {
        Tool::provenance(self)
    }
    fn provenance_in(&self, input: In<'_>, ctx: &ToolContext) -> Provenance {
        Tool::provenance_in(self, typed::<T>(input), ctx)
    }
    fn parallel(&self) -> bool {
        Tool::parallel(self)
    }
    fn checkpoint(&self) -> bool {
        Tool::checkpoint(self)
    }
    fn parse(&self, raw: Value) -> Result<ParsedInput, String> {
        let input: T::Input = serde_json::from_value(raw).map_err(|e| e.to_string())?;
        Tool::check(self, &input)?;
        Ok(Box::new(input))
    }
    fn label(&self, input: In<'_>) -> String {
        Tool::label(self, typed::<T>(input))
    }
    fn needs_approval(&self, input: In<'_>) -> Option<bool> {
        Tool::needs_approval(self, typed::<T>(input))
    }
    fn uses_network(&self, input: In<'_>, ctx: &ToolContext) -> bool {
        Tool::uses_network_in(self, typed::<T>(input), ctx)
    }
    async fn preview(&self, input: In<'_>, ctx: &ToolContext) -> Result<Preview, ToolError> {
        Tool::preview(self, typed::<T>(input), ctx).await
    }
    fn scope(&self, input: In<'_>, ctx: &ToolContext) -> Option<Scope> {
        Tool::scope_in(self, typed::<T>(input), ctx)
    }
    fn auto_safe(&self, input: In<'_>, ctx: &ToolContext) -> bool {
        Tool::auto_safe(self, typed::<T>(input), ctx)
    }
    fn protected_target(&self, input: In<'_>, ctx: &ToolContext) -> Option<ProtectedRule> {
        Tool::protected_target(self, typed::<T>(input), ctx)
    }
    fn repair(&self, raw: &Value, ctx: &ToolContext) -> Option<Repaired> {
        Tool::repair(self, raw, ctx)
    }
    fn repair_mode(&self) -> RepairMode {
        Tool::repair_mode(self)
    }
    fn egress_url(&self, input: In<'_>) -> Option<String> {
        Tool::egress_url(self, typed::<T>(input))
    }
    fn door(&self, input: In<'_>, ctx: &ToolContext) -> Option<Door> {
        Tool::door(self, typed::<T>(input), ctx)
    }
    fn once(&self, input: In<'_>, ctx: &ToolContext) -> bool {
        Tool::once(self, typed::<T>(input), ctx)
    }
    fn persists(&self, input: In<'_>, ctx: &ToolContext) -> bool {
        Tool::persists(self, typed::<T>(input), ctx)
    }
    async fn approve_itself(&self, input: In<'_>, ctx: &ToolContext, preview: &Preview) -> Option<Result<Decision, ToolError>> {
        Tool::approve_itself(self, typed::<T>(input), ctx, preview).await
    }
    async fn run(&self, input: In<'_>, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        Tool::run(self, typed::<T>(input), ctx).await
    }
}
