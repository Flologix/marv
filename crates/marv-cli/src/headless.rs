//! Headless mode: `marv -p "<prompt>"`, one non-interactive turn in the current folder. Ports Marv's
//! `src/headless.ts`.
//!
//! Built on the SDK path (`marv_session::SessionBuilder`): it loads what the TUI loads (user + project sources), runs
//! one turn with no UI, and writes the final reply to stdout. Warnings go to stderr, so stdout stays the reply.
//!
//! Exit codes ([`ExitCode`]):
//! - 0: finished (the model answered normally);
//! - 1: an error (bad config, provider failure, bad arguments, unknown `--approve`);
//! - 2: stopped: step limit, output limit, or loop (no final answer);
//! - 3: an approval it couldn't get (refused, declined, interrupted). A refused call makes even a finished turn exit 3
//!   on purpose (Marv #38): whoever runs Marv should know it wanted something it couldn't do (`refused` says what).
//!
//! `--approve` answers "yes" (not "always", so each call is still checked) for its scopes and "unavailable" for
//! everything else: refused as an error the model can work around, not a decline that ends the turn. With
//! `--approve`, the step limit still stops: nobody can say "keep going". marv-rs (#59): before it stops, the model gets
//! one tools-off request to say what it finished, what's left and the next step; that is the reply, and the exit code
//! is still 2. marv-rs: `--approve mesh:<receptacle>`
//! pre-approves wires to that receptacle (docs/MESH.md); it never covers anything else, and yolo never wires.
//! marv-rs: a `must_ask` request (docs/SECURITY.md: egress once the session read untrusted content, a write to a
//! protected path) is one Marv asks a person about every time, even in yolo. `--approve` alone no longer answers it:
//! tainted egress also needs `--approve-tainted`, a protected-path write `--approve-protected`. Without them the call
//! is refused like any other (listed in `refused`, exit 3) with a stderr line naming the flag. Subagents reach the
//! same [`ScopeApprover`], so the same rule holds for them.
//! A mesh message never starts a turn here (`mesh.wake`, #78, is the TUI's): the one turn reads what's in the inbox
//! when it starts and between steps, and anything later waits for whoever runs the next one.
//!
//! **Bug fix (Marv 0.3.0):** Marv's text output wrote the model's reply to the terminal raw, so a reply could carry
//! escape sequences (OSC 52 clipboard writes, title changes, cursor moves) or bidi overrides. Text output and stderr
//! messages go through `marv_core::printable()`. JSON output is JSON-escaped.

use std::sync::Arc;

use async_trait::async_trait;
use futures::{Stream, StreamExt};
use marv_agent::LoopEvent;
use marv_core::printable;
use marv_session::ApprovalRecord;
use marv_session::Totals;
use marv_session::session::{SessionEvent, TurnEndReason};
use marv_tools::{ApprovalRequest, Approver, Decision};
use serde::Serialize;

use crate::cli::OutputFormat;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitCode {
    Finished = 0,
    Error = 1,
    Stopped = 2,
    NeedsApproval = 3,
}

/// The scope names and their matching live in `marv_session::approvals` (`APPROVE_PREFIXES` there) — the same
/// list the config's `"approve"` and the TUI's `--approve` use (#98).
/// `marv -p --approve`'s approver: yes for the listed scopes, unavailable for the rest. A request Marv always asks a
/// person about (`must_ask`) needs its opt-in flag as well: `tainted` (`--approve-tainted`) for egress once the
/// session read untrusted content, `protected` (`--approve-protected`) for a write to a protected path.
#[derive(Debug, Clone, Default)]
pub struct ScopeApprover {
    pub prefixes: Vec<String>,
    pub tainted: bool,
    pub protected: bool,
}

/// How much of a refused call's label its stderr line shows.
const LABEL_CHARS: usize = 120;

impl ScopeApprover {
    fn covers(&self, key: &str) -> bool {
        // The same matching the seeded "always" scopes use (#98): the listed prefixes cover their keys, and a
        // background process or a git command with the network needs network as well.
        marv_session::approvals::prefix_covers(&self.prefixes, key)
    }

    /// Why a request its scopes cover is still refused (a `must_ask` one without its opt-in flag), for stderr.
    pub fn must_ask_refusal(&self, request: &ApprovalRequest) -> Option<String> {
        // One line on stderr whatever the label holds (a multi-line command): its first line, cut short.
        let line = request.label.lines().next().unwrap_or_default();
        let first: String = line.chars().take(LABEL_CHARS).collect();
        let cut = first.len() < request.label.trim_end().len();
        let what = format!("{} ({first}{})", request.tool, if cut { " …" } else { "" });
        if request.protected.is_some() {
            return (!self.protected).then(|| {
                format!("refused {what}: it writes a protected path; add --approve-protected to approve that in marv -p")
            });
        }
        (request.must_ask && !self.tainted).then(|| {
            format!(
                "refused {what}: the session read untrusted content, so egress needs --approve-tainted as well as \
                 --approve"
            )
        })
    }
}

#[async_trait]
impl Approver for ScopeApprover {
    async fn approve(&self, request: ApprovalRequest) -> Decision {
        if !self.covers(&request.scope.key) {
            return Decision::Unavailable;
        }
        if let Some(why) = self.must_ask_refusal(&request) {
            eprintln!("marv: {}", printable(&why));
            return Decision::Unavailable;
        }
        Decision::Yes
    }
}

/// The approver (`None`: nobody approves) and the receptacles `--approve mesh:<r>` pre-approves.
pub type HeadlessApprovals = (Option<Arc<dyn Approver>>, Vec<marv_mesh::ReceptacleName>);

/// The opt-ins for what `--approve` alone never answers (`--approve-tainted`, `--approve-protected`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MustAsk {
    pub tainted: bool,
    pub protected: bool,
}

/// The `--approve` list as an approver (`None` for an empty list) plus the pre-approved mesh receptacles. An
/// unknown name is an error (exit 1), as Marv's `runHeadless` checked before anything ran. The scope names and
/// their matching live in `marv_session::approvals` — the same list the config's `"approve"` and the TUI's
/// `--approve` use (#98).
pub fn build_approver(names: &[String], must_ask: MustAsk) -> Result<HeadlessApprovals, String> {
    let (mesh, mut prefixes) = marv_session::approvals::parse_approvals(names)?;
    // A pre-approved receptacle's mesh tool calls are approved too (scope `mesh:<receptacle>:<mode>`).
    prefixes.extend(mesh.iter().map(|r| format!("mesh:{r}:")));
    let approver = (!prefixes.is_empty()).then(|| {
        Arc::new(ScopeApprover { prefixes, tainted: must_ask.tainted, protected: must_ask.protected }) as Arc<dyn Approver>
    });
    Ok((approver, mesh))
}

/// The exit code for how a turn ended and how many calls were refused.
pub fn exit_code_for(reason: &str, refused: usize) -> ExitCode {
    if refused > 0 {
        return ExitCode::NeedsApproval;
    }
    match reason {
        "end" | "stop" => ExitCode::Finished,
        "max_steps" | "length" | "loop" => ExitCode::Stopped,
        "aborted" | "interrupted" | "declined" => ExitCode::NeedsApproval,
        _ => ExitCode::Error, // "error", or anything unknown
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnUsage {
    pub prompt_tokens: u64,
    pub cached_tokens: u64,
    pub completion_tokens: u64,
    pub cost: f64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Refused {
    pub tool: String,
    pub label: String,
}

/// `--output json`'s object (and stream-json's last line, `type: "result"`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HeadlessResult {
    /// The last assistant reply (not every text delta joined).
    pub reply: String,
    /// How the turn ended (`TurnEndReason::as_str`).
    pub reason: String,
    /// The model's replies, counted once each.
    pub steps: u32,
    /// This turn's tokens and cost (with -c, not the resumed session's).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<TurnUsage>,
    /// Calls refused because no one could approve them.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refused: Option<Vec<Refused>>,
}

/// The output a refused call's result contains (marv-tools' `run_tool`).
pub const NO_APPROVER: &str = "needs the user's approval, and there's no one to ask.";

/// Folds a turn's events into the result, writing stream-json lines and errors as they come.
#[derive(Default)]
pub struct Collector {
    pub reply: Option<String>,
    pub reason: String,
    pub errors: Vec<String>,
    pub refused: Vec<Refused>,
    steps: u32,
    counted: bool,
}

impl Collector {
    fn note_refused(&mut self, event: &SessionEvent) {
        // A subagent's calls are the turn's work too: one refused there may be missing from its report.
        let (SessionEvent::Loop(LoopEvent::ToolEnd { call, run })
        | SessionEvent::Subagent { event: LoopEvent::ToolEnd { call, run }, .. }) = event
        else {
            return;
        };
        // Or a command for the host, which headless never runs, whatever --approve says (#52).
        let refused_here = run.result.output.contains(NO_APPROVER)
            || (run.result.is_error && run.result.output.contains(marv_tools::bash::NO_HOST_DOOR));
        if !(refused_here || run.approval == ApprovalRecord::No || run.declined) {
            return;
        }
        let refused =
            Refused { tool: call.name.clone(), label: if run.label.is_empty() { call.name.clone() } else { run.label.clone() } };
        if !self.refused.contains(&refused) {
            self.refused.push(refused);
        }
    }

    /// One event: the result's state, plus what stream-json prints for it (a JSON line), if anything.
    pub fn event(&mut self, event: &SessionEvent) -> Option<serde_json::Value> {
        use marv_session::session::SessionEvent as E;
        use serde_json::json;
        self.note_refused(event);
        match event {
            E::Loop(e) => {
                use LoopEvent as L;
                // Steps: a reply ends in its text, its tool calls, or both: counted once at the first of those after
                // something new streamed in (not every provider reports usage per request).
                match e {
                    L::TextDelta(_) | L::ThinkingDelta(_) | L::ToolCallDelta { .. } | L::Usage(..) => self.counted = false,
                    L::Assistant { .. } | L::ToolStart { .. } | L::ToolSkipped { .. } if !self.counted => {
                        self.steps += 1;
                        self.counted = true;
                    }
                    _ => {}
                }
                match e {
                    L::TextDelta(text) => Some(json!({"type": "text_delta", "text": text})),
                    L::ThinkingDelta(text) => Some(json!({"type": "thinking_delta", "text": text})),
                    L::Assistant { text } => {
                        self.reply = Some(text.clone());
                        Some(json!({"type": "assistant", "text": text}))
                    }
                    L::ToolStart { call, label } => Some(json!({"type": "tool_start", "tool": call.name, "label": label})),
                    L::ToolEnd { call, run } => {
                        let mut line =
                            json!({"type": "tool_end", "tool": call.name, "label": run.label, "result": run.result.output});
                        // Marv's result has no isError when it isn't one and no approval when none was needed, and
                        // JSON.stringify leaves those out.
                        if run.result.is_error {
                            line["error"] = true.into();
                        }
                        if run.approval != ApprovalRecord::None {
                            line["approval"] = json!(run.approval);
                        }
                        Some(line)
                    }
                    L::ToolSkipped { call, output } => Some(json!({"type": "tool_skipped", "tool": call.name, "reason": output})),
                    L::Done(reason) => {
                        self.reason = TurnEndReason::Done(*reason).as_str().into();
                        Some(json!({"type": "done", "reason": self.reason}))
                    }
                    L::Error(message) => {
                        self.errors.push(message.clone());
                        Some(json!({"type": "error", "message": message}))
                    }
                    L::StepLimit { steps, continued } => {
                        Some(json!({"type": "step_limit", "steps": steps, "continued": continued}))
                    }
                    L::CutOff { continued } => Some(json!({"type": "cut_off", "continued": continued})),
                    L::TextLoop { continued } => Some(json!({"type": "text_loop", "continued": continued})),
                    L::AnnouncedStep { continued } => Some(json!({"type": "announced_step", "continued": continued})),
                    L::ToolLoop { continued } => Some(json!({"type": "tool_loop", "continued": continued})),
                    _ => None,
                }
            }
            E::Status(status) => {
                let status = match status {
                    marv_session::TurnStatus::WaitingForMcp => "waiting_for_mcp",
                    marv_session::TurnStatus::Compacting => "compacting",
                    marv_session::TurnStatus::Checking => "checking",
                    marv_session::TurnStatus::Checkpointing => "checkpointing",
                    marv_session::TurnStatus::Running => "running",
                };
                Some(json!({"type": "status", "status": status}))
            }
            E::Compaction(result) => Some(match result {
                marv_session::CompactResult::Compacted { .. } => json!({"type": "compaction", "compacted": true}),
                // #144: why it failed ("failed", "stopped", "empty") and the error, so a run's log says what happened.
                marv_session::CompactResult::NotCompacted { reason, error } => {
                    json!({"type": "compaction", "compacted": false, "reason": reason.as_str(), "error": error})
                }
            }),
            E::TurnEnd { reason, .. } => {
                self.reason = reason.as_str().into();
                None
            }
            _ => None,
        }
    }

    /// The result, given the session's totals before and after the turn.
    pub fn result(&self, before: &Totals, after: &Totals) -> HeadlessResult {
        let usage = (after.requests > before.requests).then(|| TurnUsage {
            prompt_tokens: after.prompt_tokens - before.prompt_tokens,
            cached_tokens: after.cached_tokens - before.cached_tokens,
            completion_tokens: after.completion_tokens - before.completion_tokens,
            cost: after.cost.unwrap_or(0.0) - before.cost.unwrap_or(0.0),
        });
        HeadlessResult {
            reply: self.reply.clone().unwrap_or_default(),
            reason: if self.reason.is_empty() { "error".into() } else { self.reason.clone() },
            steps: self.steps,
            usage,
            refused: (!self.refused.is_empty()).then(|| self.refused.clone()),
        }
    }
}

/// What `--output text` prints: the reply, made printable, ending in a newline.
pub fn text_output(reply: &str) -> String {
    let safe = printable(reply);
    if safe.is_empty() || safe.ends_with('\n') { safe } else { format!("{safe}\n") }
}

/// Reads the turn's events, prints them as `format` says, and returns the exit code.
pub async fn collect_and_output(
    mut events: impl Stream<Item = SessionEvent> + Unpin,
    format: OutputFormat,
    before: Totals,
    after: impl FnOnce() -> Totals,
) -> ExitCode {
    use std::io::Write;
    let mut collector = Collector::default();
    let mut out = std::io::stdout().lock();
    while let Some(event) = events.next().await {
        let line = collector.event(&event);
        if let SessionEvent::Loop(LoopEvent::Error(message)) = &event
            && format != OutputFormat::StreamJson
        {
            eprintln!("{}", printable(message));
        }
        if format == OutputFormat::StreamJson
            && let Some(line) = line
        {
            let _ = writeln!(out, "{line}");
        }
    }
    let result = collector.result(&before, &after());
    match format {
        OutputFormat::Json => {
            let _ = writeln!(out, "{}", serde_json::to_string_pretty(&result).unwrap_or_default());
        }
        OutputFormat::StreamJson => {
            let mut value = serde_json::to_value(&result).unwrap_or_default();
            if let serde_json::Value::Object(map) = &mut value {
                map.insert("type".into(), "result".into());
            }
            let _ = writeln!(out, "{value}");
        }
        OutputFormat::Text => {
            let _ = write!(out, "{}", text_output(&result.reply));
        }
    }
    let _ = out.flush();
    exit_code_for(&result.reason, collector.refused.len())
}

/// Options from the command line.
pub struct HeadlessOptions {
    pub prompt: String,
    pub output: OutputFormat,
    pub resume_latest: bool,
    pub model: Option<String>,
    pub max_steps: Option<u32>,
    pub approve: Vec<String>,
    /// `--approve-tainted` and `--approve-protected`.
    pub must_ask: MustAsk,
}

/// Runs one headless turn and writes the result to stdout. Returns the exit code.
pub async fn run_headless(options: HeadlessOptions) -> ExitCode {
    use marv_session::config::{ConfigStore, Env, default_config_dir, migrate_legacy_config, resolve_config};
    let (approve, mesh_grants) = match build_approver(&options.approve, options.must_ask) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::Error;
        }
    };
    // The real environment only (docs/SECURITY.md): never a project's .env.
    let mut env: Env = std::env::vars().collect();
    let home = marv_session::paths::home_dir();
    let config_dir = default_config_dir(&env, &home);
    if !env.contains_key("MARV_CONFIG_DIR") {
        let _ = migrate_legacy_config(&home);
    }
    let file = match ConfigStore::new(&config_dir).load() {
        Ok(f) => f,
        Err(e) => {
            eprintln!("{e}\n\nFix the file, or delete it to run setup again.");
            return ExitCode::Error;
        }
    };
    if let Some(model) = &options.model {
        env.insert("MARV_MODEL".into(), model.clone());
    }
    let config = resolve_config(file.as_ref(), &env);
    // The config's pre-approved scopes (#98) seed the session's "always" layer (never the headless approver, so a
    // subagent doesn't inherit them, as the guards require), and its writable folders (#98) are bound read-write
    // into every sandbox. Both go on stderr with the rest of the startup notice (stdout stays the reply).
    let mut seed_problems = Vec::new();
    let seeds = config
        .approve
        .iter()
        .filter(|name| match marv_session::approvals::parse_approvals(std::slice::from_ref(name)) {
            Ok(_) => true,
            Err(problem) => {
                seed_problems.push(problem);
                false
            }
        })
        .cloned()
        .collect::<Vec<_>>();
    for problem in &seed_problems {
        eprintln!("{problem}");
    }
    if !seeds.is_empty() {
        eprintln!("Pre-approved for this session (config): {}", seeds.join(", "));
    }
    // The SDK takes each provider's own options: Ollama's endpoint is `host` and it has a context length.
    let provider = match config.provider {
        marv_provider::ProviderKind::Ollama => marv_provider::ProviderOption::Ollama {
            model: config.model.clone(),
            host: Some(config.base_url.clone()),
            context_length: Some(config.context_length),
        },
        marv_provider::ProviderKind::OpenRouter => marv_provider::ProviderOption::OpenRouter {
            api_key: config.api_key.clone().unwrap_or_default(),
            model: Some(config.model.clone()),
            base_url: Some(config.base_url.clone()),
        },
        // #159: the saved sign-in; without one, the first request says to sign in with /setup.
        marv_provider::ProviderKind::ChatGpt => match config.auth_file.clone() {
            Some(auth_file) => marv_provider::ProviderOption::ChatGpt { model: config.model.clone(), auth_file },
            None => {
                eprintln!("ChatGPT: Marv doesn't know where the sign-in is kept: set HOME or MARV_CONFIG_DIR.");
                return ExitCode::Error;
            }
        },
        marv_provider::ProviderKind::Zai => marv_provider::ProviderOption::Zai {
            api_key: config.api_key.clone().unwrap_or_default(),
            model: Some(config.model.clone()),
            base_url: Some(config.base_url.clone()),
        },
    };
    let root = match std::env::current_dir() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Failed to create session: {e}");
            return ExitCode::Error;
        }
    };
    // The config's extra writable folders (#98), resolved once here (refused ones are problems on stderr, not
    // mounts), and bound read-write into every sandbox.
    let (writable, writable_problems) = marv_sandbox::resolve_writable(
        &config.writable,
        &home,
        &root,
        &marv_sandbox::runtime_paths(&marv_sandbox::runtime_env()),
        Some(&config_dir),
    );
    for problem in &writable_problems {
        eprintln!("{problem}");
    }
    if !writable.is_empty() {
        let shown: Vec<String> = writable.iter().map(|d| marv_session::paths::shorten_home(d, &home)).collect();
        eprintln!("Writable in the sandbox besides the project (config): {}", shown.join(", "));
    }
    // Where a door's program (gh, herdr) is never taken from: the project and the sandbox's writable folders.
    let door_outside: Vec<std::path::PathBuf> = std::iter::once(root.clone()).chain(writable.iter().cloned()).collect();
    let mut builder = marv_session::SessionBuilder::new(root.clone(), provider)
        .sources(&[marv_session::sources::Source::User, marv_session::sources::Source::Project])
        .max_steps(options.max_steps.unwrap_or(config.max_steps))
        .yolo(config.yolo)
        // Marv exits after the reply and stops its background processes (main.rs): the process tool says so (#86).
        .one_shot(true)
        .persist(marv_session::sdk::Store::Default)
        .trajectories(if config.trajectories { marv_session::sdk::Store::Default } else { marv_session::sdk::Store::Off })
        .pre_approve(&seeds)
        .writable(writable);
    if let Some(approve) = approve {
        builder = builder.approve(approve);
    }
    // gh with the user's login, when it's there (MARV_GITHUB=off hides it): reads run, writes need --approve github.
    // Asked here, before the mesh starts: another Marv lists this one once its main agent's plug is bound, and gh
    // auth status can take up to 2 s. No host_commands: nobody approves each command here (#52).
    let github = marv_tools::github::github_wanted(env.get("MARV_GITHUB").map(String::as_str)) && {
        let pairs: Vec<(String, String)> = env.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        marv_tools::github::github_available(&pairs, &door_outside, marv_tools::github::AUTH_CHECK).await
    };
    (builder.options.github, builder.options.github_known) = (github, Some(github));
    // In a Herdr pane, the herdr tool reads panes (#74); without host_commands its commands and spawns are refused
    // before anything is asked, whatever --approve says.
    builder.options.herdr = marv_session::herdr_door::setup_from_env(&env, config.herdr_peers, &door_outside);
    // A worker the Herdr door spawned from a tainted conversation starts tainted (MARV_TAINTED=1: only adds taint).
    builder.options.tainted_at_start = marv_session::config::tainted_at_start(&env);
    builder.options.compact_tokens = config.compact_tokens;
    // The mesh, when asked for (`--approve mesh:<r>` or `MARV_MESH_NAME`): otherwise a one-off `marv -p` isn't on it
    // and its model isn't offered the mesh tools. Wires are granted only for the listed receptacles, from this run's
    // own agents or from another Marv (its door).
    let wants_mesh = !mesh_grants.is_empty() || env.get("MARV_MESH_NAME").is_some_and(|n| !n.is_empty());
    let mut mesh = None;
    if wants_mesh {
        match crate::mesh_cmd::start(&env, &root, &door_outside) {
            Ok(Some(mut started)) => {
                let wires: Arc<dyn marv_session::mesh::WireApprover> = Arc::new(marv_session::mesh::HeadlessWires(mesh_grants));
                if let Err(e) = started.open_door(wires.clone()) {
                    eprintln!("marv: the mesh: {}", printable(&e));
                }
                builder.options.mesh = Some(started.handle.clone());
                builder.options.wire_approver = Some(wires);
                mesh = Some(started);
            }
            Ok(None) => {
                eprintln!("marv: the mesh needs {}; running without it.", marv_sandbox::RUNTIME_DIR_RULE);
                if let Some(why) = marv_sandbox::override_refusal_for(&env, Some(&root)) {
                    eprintln!("marv: {}", printable(&why));
                }
            }
            Err(e) => eprintln!("marv: the mesh: {}", printable(&e)),
        }
    }
    if options.resume_latest {
        builder = builder.resume(marv_session::ResumeTarget::Latest);
    }
    let user_config = ConfigStore::new(&config_dir).path;
    let o = &mut builder.options;
    (o.sandbox, o.thinking, o.diagnostics, o.checks, o.mcp_tools, o.config_dir) =
        (config.sandbox, config.thinking, config.diagnostics, config.checks, config.mcp_tools, Some(config_dir));
    o.reasoning_effort = config.reasoning_effort;
    o.formatter = config.formatter;
    o.checkpoints = config.checkpoints;
    // Session-start hooks (#108): from the user's own config file only; a project's naming some is told, never run.
    let refused;
    (o.hooks, refused) = marv_session::hooks::session_start_hooks(&root, &user_config, file.as_ref());
    if let Some(refused) = refused {
        eprintln!("marv: {refused}");
    }
    // To stderr, so stdout stays the reply: an MCP server that needs a login, a trajectory that can't be written.
    o.on_warning = Some(Arc::new(|text: String| eprintln!("marv: {}", printable(&text))));
    let session = match builder.build().await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Failed to create session: {}", printable(&e.to_string()));
            return ExitCode::Error;
        }
    };
    // The hooks (started by a resume already, else now) go with this prompt; one that failed is a line on stderr.
    session.run_start_hooks();
    for run in session.start_hooks_done().await.unwrap_or_default().iter().filter(|r| !r.ok()) {
        eprintln!("marv: session-start hook {}", printable(&run.line()));
    }
    // With -c the totals include the resumed conversation's: the turn's usage is what this turn added.
    let before = session.usage().totals;
    let code = match session.send(&options.prompt, marv_session::SendOptions::default()) {
        Ok(turn) => {
            let after_session = session.clone();
            collect_and_output(turn, options.output, before, move || after_session.usage().totals).await
        }
        Err(e) => {
            eprintln!("{}", printable(&e.to_string()));
            ExitCode::Error
        }
    };
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), session.close()).await;
    drop(mesh);
    code
}

#[cfg(test)]
mod tests {
    use super::*;
    use marv_tools::{Preview, Scope};

    fn request(key: &str) -> ApprovalRequest {
        ApprovalRequest {
            tool: "bash".into(),
            label: "x".into(),
            preview: Preview::default(),
            scope: Scope { key: key.into(), description: key.into() },
            agent: None,
            network: false,
            once: false,
            must_ask: false,
            protected: None,
            door: None,
        }
    }

    // Marv's tests/github-tool.test.ts, "marv -p".
    #[tokio::test]
    async fn approve_github_approves_writes_and_sensitive_reads_nothing_else() {
        let (a, _) = build_approver(&["github".into()], MustAsk::default()).unwrap();
        let a = a.unwrap();
        assert_eq!(a.approve(request("github:pr merge")).await, Decision::Yes);
        assert_eq!(a.approve(request("github:api sensitive")).await, Decision::Yes);
        assert_eq!(a.approve(request("bash:echo")).await, Decision::Unavailable);
        assert_eq!(a.approve(request("bash:host:echo")).await, Decision::Unavailable);
        let (none, _) = build_approver(&["network".into()], MustAsk::default()).unwrap();
        assert_eq!(none.unwrap().approve(request("github:pr merge")).await, Decision::Unavailable);
    }

    // #144: a failed compaction says why in stream-json.
    #[test]
    fn a_failed_compaction_carries_its_reason_and_error() {
        let mut c = Collector::default();
        let failed = marv_session::CompactResult::NotCompacted {
            reason: marv_session::CompactFailure::Failed,
            error: marv_agent::compact::TOOL_CALL_ERROR.into(),
        };
        assert_eq!(
            c.event(&SessionEvent::Compaction(failed)),
            Some(
                serde_json::json!({"type": "compaction", "compacted": false, "reason": "failed", "error": marv_agent::compact::TOOL_CALL_ERROR})
            )
        );
        let done = marv_session::CompactResult::Compacted { summary: "S".into(), tokens_before: Some(9) };
        assert_eq!(c.event(&SessionEvent::Compaction(done)), Some(serde_json::json!({"type": "compaction", "compacted": true})));
    }

    // #52: a command for the host is refused in -p whatever --approve says, and listed as refused (exit 3).
    #[test]
    fn a_host_command_is_listed_as_refused() {
        let mut c = Collector::default();
        let call = marv_core::ToolCall { id: "1".into(), name: "bash".into(), arguments: "{}".into() };
        let result = marv_core::ToolResult {
            output: marv_tools::bash::NO_HOST_DOOR.into(),
            summary: Some("error".into()),
            is_error: true,
        };
        let run = marv_agent::ToolRun::new(result, "systemctl --user restart x");
        c.event(&SessionEvent::Loop(LoopEvent::ToolEnd { call, run }));
        assert_eq!(c.refused, vec![Refused { tool: "bash".into(), label: "systemctl --user restart x".into() }]);
    }

    #[tokio::test]
    async fn approve_scopes_match_prefixes() {
        let (none, _) = build_approver(&[], MustAsk::default()).unwrap();
        assert!(none.is_none());
        let (a, _) = build_approver(&["git_write".into()], MustAsk::default()).unwrap();
        let a = a.unwrap();
        assert_eq!(a.approve(request("bash:git:commit")).await, Decision::Yes);
        // git_write alone never covers a command with the network: a push needs network too.
        assert_eq!(a.approve(request("bash:net+git:git push")).await, Decision::Unavailable);
        assert_eq!(a.approve(request("bash:net:git:;curl x")).await, Decision::Unavailable);
        assert_eq!(a.approve(request("bash:net:curl")).await, Decision::Unavailable);
        let (both, _) = build_approver(&["git_write".into(), "network".into()], MustAsk::default()).unwrap();
        assert_eq!(both.unwrap().approve(request("bash:net+git:git push")).await, Decision::Yes);
        let (a, _) = build_approver(&["files".into(), "web".into()], MustAsk::default()).unwrap();
        let a = a.unwrap();
        assert_eq!(a.approve(request("files")).await, Decision::Yes);
        assert_eq!(a.approve(request("web:docs.rs")).await, Decision::Yes);
        assert_eq!(a.approve(request("memory")).await, Decision::Unavailable);
    }

    #[tokio::test]
    async fn approve_process_approves_starting_a_process_not_one_on_the_users_network_without_network_too() {
        let approver = |names: &[&str]| {
            build_approver(&names.iter().map(|n| n.to_string()).collect::<Vec<_>>(), MustAsk::default()).unwrap().0.unwrap()
        };
        let only = approver(&["process"]);
        assert_eq!(only.approve(request("process:bun run dev")).await, Decision::Yes);
        assert_eq!(only.approve(request("process:net:bun run dev")).await, Decision::Unavailable);
        let both = approver(&["process", "network"]);
        assert_eq!(both.approve(request("process:net:bun run dev")).await, Decision::Yes);
        let none = approver(&["network"]);
        assert_eq!(none.approve(request("process:bun run dev")).await, Decision::Unavailable);
        assert_eq!(none.approve(request("process:net:bun run dev")).await, Decision::Unavailable);
    }

    #[tokio::test]
    async fn unknown_scopes_are_errors_and_mesh_flags_are_grants() {
        assert!(
            build_approver(&["custom_scope".into()], MustAsk::default())
                .err()
                .unwrap()
                .contains("Unknown --approve scope \"custom_scope\"")
        );
        let (approver, mesh) = build_approver(&["mesh:agent.parent.inbox".into()], MustAsk::default()).unwrap();
        assert_eq!(mesh.len(), 1);
        // The receptacle's own mesh calls are approved, nothing else.
        let approver = approver.unwrap();
        assert_eq!(approver.approve(request("mesh:agent.parent.inbox:send")).await, Decision::Yes);
        assert_eq!(approver.approve(request("mesh:agent.parent.inbox2:send")).await, Decision::Unavailable);
        assert_eq!(approver.approve(request("bash:net:curl")).await, Decision::Unavailable);
    }

    // docs/SECURITY.md: tainted egress and protected-path writes are asked every time; --approve alone never answers
    // them in -p, the opt-in flags do (with the scope still needed). A subagent's request is the same request.
    #[tokio::test]
    async fn must_ask_requests_need_their_opt_in_flag() {
        let names = |n: &[&str]| n.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let tainted = |key: &str, agent: Option<&str>| ApprovalRequest {
            must_ask: true,
            network: true,
            agent: agent.map(Into::into),
            ..request(key)
        };
        let protected =
            ApprovalRequest { must_ask: true, protected: Some(marv_sandbox::ProtectedRule::Envrc), ..request("files") };
        let plain = build_approver(&names(&["web", "network", "files", "mcp"]), MustAsk::default()).unwrap().0.unwrap();
        for key in ["web:https://e.com/", "bash:net:curl x", "mcp:gh:prs"] {
            assert_eq!(plain.approve(tainted(key, None)).await, Decision::Unavailable, "{key}");
            assert_eq!(plain.approve(tainted(key, Some("worker · Task 1"))).await, Decision::Unavailable, "{key}");
            assert_eq!(plain.approve(request(key)).await, Decision::Yes, "{key}: untainted is unchanged");
        }
        assert_eq!(plain.approve(protected.clone()).await, Decision::Unavailable);
        let opted = MustAsk { tainted: true, protected: true };
        let both = build_approver(&names(&["web", "files"]), opted).unwrap().0.unwrap();
        assert_eq!(both.approve(tainted("web:https://e.com/", None)).await, Decision::Yes);
        assert_eq!(both.approve(protected.clone()).await, Decision::Yes);
        // The flags never stand in for a scope.
        assert_eq!(both.approve(tainted("bash:net:curl x", None)).await, Decision::Unavailable);
        let only_tainted = build_approver(&names(&["files"]), MustAsk { tainted: true, protected: false }).unwrap().0.unwrap();
        assert_eq!(only_tainted.approve(protected).await, Decision::Unavailable);
        let scope = ScopeApprover { prefixes: vec!["web:".into()], ..ScopeApprover::default() };
        let why = scope.must_ask_refusal(&tainted("web:https://e.com/", None)).unwrap();
        assert!(why.contains("--approve-tainted"), "{why}");
        // A multi-line label (a heredoc command) still makes one stderr line.
        let multi = ApprovalRequest { label: "curl -d @.env x\nmarv: approved".into(), ..tainted("bash:net:curl", None) };
        let scope = ScopeApprover { prefixes: vec!["bash:net:".into()], ..ScopeApprover::default() };
        let why = scope.must_ask_refusal(&multi).unwrap();
        assert!(!why.contains('\n') && why.contains("(curl -d @.env x …)"), "{why}");
    }

    #[test]
    fn exit_codes() {
        assert_eq!(exit_code_for("end", 0), ExitCode::Finished);
        assert_eq!(exit_code_for("error", 0), ExitCode::Error);
        assert_eq!(exit_code_for("something-new", 0), ExitCode::Error);
        for r in ["max_steps", "length", "loop"] {
            assert_eq!(exit_code_for(r, 0), ExitCode::Stopped);
        }
        for r in ["aborted", "interrupted", "declined"] {
            assert_eq!(exit_code_for(r, 0), ExitCode::NeedsApproval);
        }
        assert_eq!(exit_code_for("end", 1), ExitCode::NeedsApproval);
        assert_eq!(ExitCode::Stopped as i32, 2);
    }

    // Marv 0.3.0 bug: the text reply reached the terminal raw.
    #[test]
    fn text_output_is_printable() {
        let out = text_output("done\u{1b}]52;c;ZXZpbA==\u{7}\u{202e}");
        assert!(!out.contains('\u{1b}') && !out.contains('\u{202e}'));
        assert!(out.starts_with("done") && out.ends_with('\n'));
        assert_eq!(text_output(""), "");
    }

    #[test]
    fn steps_reply_and_refusals_are_collected() {
        let mut c = Collector::default();
        let call = marv_core::ToolCall { id: "1".into(), name: "bash".into(), arguments: "{}".into() };
        let refused = LoopEvent::ToolEnd {
            call: call.clone(),
            run: marv_agent::ToolRun::new(
                marv_core::ToolResult { output: format!("bash {NO_APPROVER}"), summary: None, is_error: true },
                "rm",
            ),
        };
        for e in [
            LoopEvent::TextDelta("a".into()),
            LoopEvent::ToolStart { call: call.clone(), label: "rm".into() },
            refused,
            LoopEvent::TextDelta("b".into()),
            LoopEvent::Assistant { text: "done".into() },
            LoopEvent::Done(marv_core::StopReason::End),
        ] {
            c.event(&SessionEvent::Loop(e));
        }
        c.event(&SessionEvent::TurnEnd { turn: "t".into(), reason: TurnEndReason::Done(marv_core::StopReason::End) });
        let r = c.result(&Totals::default(), &Totals::default());
        assert_eq!((r.reply.as_str(), r.reason.as_str(), r.steps), ("done", "end", 2));
        assert_eq!(r.refused.unwrap(), vec![Refused { tool: "bash".into(), label: "rm".into() }]);
        assert_eq!(exit_code_for(&r.reason, 1), ExitCode::NeedsApproval);
    }

    // stream-json's tool_end as Marv writes it: no `error: false`, no `approval: "none"`.
    #[test]
    fn tool_end_leaves_out_what_marv_leaves_out() {
        let mut c = Collector::default();
        let call = marv_core::ToolCall { id: "1".into(), name: "read_file".into(), arguments: "{}".into() };
        let ok = marv_agent::ToolRun::new(marv_core::ToolResult { output: "x".into(), summary: None, is_error: false }, "a.js");
        let line = c.event(&SessionEvent::Loop(LoopEvent::ToolEnd { call: call.clone(), run: ok })).unwrap();
        assert_eq!(line, serde_json::json!({"type": "tool_end", "tool": "read_file", "label": "a.js", "result": "x"}));
        let mut failed =
            marv_agent::ToolRun::new(marv_core::ToolResult { output: "no".into(), summary: None, is_error: true }, "a.js");
        failed.approval = ApprovalRecord::Auto;
        let line = c.event(&SessionEvent::Loop(LoopEvent::ToolEnd { call, run: failed })).unwrap();
        assert_eq!((&line["error"], &line["approval"]), (&serde_json::json!(true), &serde_json::json!("auto")));
    }
}
