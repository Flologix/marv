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
//! `--approve`, the step limit still stops: nobody can say "keep going". marv-rs: `--approve mesh:<receptacle>`
//! pre-approves wires to that receptacle (docs/MESH.md); it never covers anything else, and yolo never wires.
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

/// Known `--approve` names and the scope-key prefixes they match.
pub const APPROVE_PREFIXES: [(&str, &[&str]); 9] = [
    ("git_write", &["bash:git:", "bash:net:git:"]),
    ("network", &["bash:net:"]),
    ("web", &["web:"]),
    ("mcp", &["mcp:"]),
    ("files", &["files"]),
    ("memory", &["memory"]),
    ("agent_worktree", &["agent:worktree"]),
    ("process", &["process:"]),
    // The github tool's writes and sensitive reads (#47); its reads need nothing.
    ("github", &["github:"]),
];

/// `marv -p --approve`'s approver: yes for the listed scopes, unavailable for the rest.
#[derive(Debug, Clone)]
pub struct ScopeApprover {
    pub prefixes: Vec<String>,
}

#[async_trait]
impl Approver for ScopeApprover {
    async fn approve(&self, request: ApprovalRequest) -> Decision {
        let key = &request.scope.key;
        // A background process that shares the user's network needs network too, as a bash command with it does.
        if key.starts_with("process:net:") {
            let has = |prefix: &str| self.prefixes.iter().any(|p| p == prefix);
            return if has("process:") && has("bash:net:") { Decision::Yes } else { Decision::Unavailable };
        }
        if self.prefixes.iter().any(|p| key == p || key.starts_with(p.as_str())) { Decision::Yes } else { Decision::Unavailable }
    }
}

/// The approver (`None`: nobody approves) and the receptacles `--approve mesh:<r>` pre-approves.
pub type HeadlessApprovals = (Option<Arc<dyn Approver>>, Vec<marv_mesh::ReceptacleName>);

/// The `--approve` list as an approver (`None` for an empty list) plus the pre-approved mesh receptacles. An
/// unknown name is an error (exit 1), as Marv's `runHeadless` checked before anything ran.
pub fn build_approver(names: &[String]) -> Result<HeadlessApprovals, String> {
    let (mesh, names) = marv_session::mesh::headless_grants(names)?;
    let mut prefixes = Vec::new();
    for name in &names {
        match APPROVE_PREFIXES.iter().find(|(n, _)| n == name) {
            Some((_, p)) => prefixes.extend(p.iter().map(|s| s.to_string())),
            None => {
                let known: Vec<&str> = APPROVE_PREFIXES.iter().map(|(n, _)| *n).collect();
                return Err(format!("Unknown --approve scope \"{name}\". Known: {}, mesh:<receptacle>", known.join(", ")));
            }
        }
    }
    // A pre-approved receptacle's mesh tool calls are approved too (scope `mesh:<receptacle>:<mode>`).
    prefixes.extend(mesh.iter().map(|r| format!("mesh:{r}:")));
    let approver = (!prefixes.is_empty()).then(|| Arc::new(ScopeApprover { prefixes }) as Arc<dyn Approver>);
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
                    L::TextDelta(_) | L::ThinkingDelta(_) | L::ToolCallDelta { .. } | L::Usage(_) => self.counted = false,
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
            E::Compaction(result) => {
                Some(json!({"type": "compaction", "compacted": matches!(result, marv_session::CompactResult::Compacted { .. })}))
            }
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
}

/// Runs one headless turn and writes the result to stdout. Returns the exit code.
pub async fn run_headless(options: HeadlessOptions) -> ExitCode {
    use marv_session::config::{ConfigStore, Env, default_config_dir, migrate_legacy_config, resolve_config};
    let (approve, mesh_grants) = match build_approver(&options.approve) {
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
    };
    let root = match std::env::current_dir() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Failed to create session: {e}");
            return ExitCode::Error;
        }
    };
    let mut builder = marv_session::SessionBuilder::new(root.clone(), provider)
        .sources(&[marv_session::sources::Source::User, marv_session::sources::Source::Project])
        .max_steps(options.max_steps.unwrap_or(config.max_steps))
        .yolo(config.yolo)
        // Marv exits after the reply and stops its background processes (main.rs): the process tool says so (#86).
        .one_shot(true)
        .persist(marv_session::sdk::Store::Default)
        .trajectories(if config.trajectories { marv_session::sdk::Store::Default } else { marv_session::sdk::Store::Off });
    if let Some(approve) = approve {
        builder = builder.approve(approve);
    }
    // gh with the user's login, when it's there (MARV_GITHUB=off hides it): reads run, writes need --approve github.
    // Asked here, before the mesh starts: another Marv lists this one once its main agent's plug is bound, and gh
    // auth status can take up to 2 s. No host_commands: nobody approves each command here (#52).
    let github = marv_tools::github::github_wanted(env.get("MARV_GITHUB").map(String::as_str)) && {
        let pairs: Vec<(String, String)> = env.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        marv_tools::github::github_available(&pairs, marv_tools::github::AUTH_CHECK).await
    };
    (builder.options.github, builder.options.github_known) = (github, Some(github));
    // The mesh, when asked for (`--approve mesh:<r>` or `MARV_MESH_NAME`): otherwise a one-off `marv -p` isn't on it
    // and its model isn't offered the mesh tools. Wires are granted only for the listed receptacles, from this run's
    // own agents or from another Marv (its door).
    let wants_mesh = !mesh_grants.is_empty() || env.get("MARV_MESH_NAME").is_some_and(|n| !n.is_empty());
    let mut mesh = None;
    if wants_mesh {
        match crate::mesh_cmd::start(&env, &root) {
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
                eprintln!("marv: the mesh needs XDG_RUNTIME_DIR under /run, /tmp or your home folder; running without it.")
            }
            Err(e) => eprintln!("marv: the mesh: {}", printable(&e)),
        }
    }
    if options.resume_latest {
        builder = builder.resume(marv_session::ResumeTarget::Latest);
    }
    let o = &mut builder.options;
    (o.sandbox, o.thinking, o.diagnostics, o.checks, o.mcp_tools, o.config_dir) =
        (config.sandbox, config.thinking, config.diagnostics, config.checks, config.mcp_tools, Some(config_dir));
    o.checkpoints = config.checkpoints;
    // To stderr, so stdout stays the reply: an MCP server that needs a login, a trajectory that can't be written.
    o.on_warning = Some(Arc::new(|text: String| eprintln!("marv: {}", printable(&text))));
    let session = match builder.build().await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Failed to create session: {}", printable(&e.to_string()));
            return ExitCode::Error;
        }
    };
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
        let (a, _) = build_approver(&["github".into()]).unwrap();
        let a = a.unwrap();
        assert_eq!(a.approve(request("github:pr merge")).await, Decision::Yes);
        assert_eq!(a.approve(request("github:api sensitive")).await, Decision::Yes);
        assert_eq!(a.approve(request("bash:echo")).await, Decision::Unavailable);
        assert_eq!(a.approve(request("bash:host:echo")).await, Decision::Unavailable);
        let (none, _) = build_approver(&["network".into()]).unwrap();
        assert_eq!(none.unwrap().approve(request("github:pr merge")).await, Decision::Unavailable);
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
        let (none, _) = build_approver(&[]).unwrap();
        assert!(none.is_none());
        let (a, _) = build_approver(&["git_write".into()]).unwrap();
        let a = a.unwrap();
        assert_eq!(a.approve(request("bash:git:commit")).await, Decision::Yes);
        assert_eq!(a.approve(request("bash:net:git:push")).await, Decision::Yes);
        assert_eq!(a.approve(request("bash:net:curl")).await, Decision::Unavailable);
        let (a, _) = build_approver(&["files".into(), "web".into()]).unwrap();
        let a = a.unwrap();
        assert_eq!(a.approve(request("files")).await, Decision::Yes);
        assert_eq!(a.approve(request("web:docs.rs")).await, Decision::Yes);
        assert_eq!(a.approve(request("memory")).await, Decision::Unavailable);
    }

    #[tokio::test]
    async fn approve_process_approves_starting_a_process_not_one_on_the_users_network_without_network_too() {
        let approver =
            |names: &[&str]| build_approver(&names.iter().map(|n| n.to_string()).collect::<Vec<_>>()).unwrap().0.unwrap();
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
        assert!(build_approver(&["custom_scope".into()]).err().unwrap().contains("Unknown --approve scope \"custom_scope\""));
        let (approver, mesh) = build_approver(&["mesh:agent.parent.inbox".into()]).unwrap();
        assert_eq!(mesh.len(), 1);
        // The receptacle's own mesh calls are approved, nothing else.
        let approver = approver.unwrap();
        assert_eq!(approver.approve(request("mesh:agent.parent.inbox:send")).await, Decision::Yes);
        assert_eq!(approver.approve(request("mesh:agent.parent.inbox2:send")).await, Decision::Unavailable);
        assert_eq!(approver.approve(request("bash:net:curl")).await, Decision::Unavailable);
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
