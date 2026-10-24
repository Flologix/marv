//! `process`: servers, watchers and anything else that should keep running after a turn (Marv #51), each in its own
//! sandbox, with the ports it listens on forwarded to the user's 127.0.0.1 (marv-session's `processes`).
//!
//! Ports Marv's `src/tools/process.ts`.
//!
//! - Starting one asks (a door out of the sandbox: it outlives the turn and the user can reach it), even in yolo (no
//!   `auto_safe`); reading its logs, listing and stopping don't (stopping your own process can't hurt).
//! - Scope `process:[net:]<command>` ("starting this exact command in the background"); `door` is `process` for a
//!   start. A start with `network: true` uses the network (it shares the user's).
//! - The door is the context's to open (set for the main agent only), not the approver's: without it a start is
//!   refused in `preview`, before anyone is asked, and again in `run`, so no path skips it ([`NO_PROCESS_DOOR`]).

use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::types::{Preview, ProcessInfo, ProcessState, Scope, Tool, ToolContext, ToolError, ToolOutput};

/// Said when a call has no process door: subagents (their process would outlive their report), or a session without
/// one.
pub const NO_PROCESS_DOOR: &str = "Background processes can only be started by the main agent, in a session that offers them.";
/// The most lines `logs` returns.
pub const MAX_LOG_LINES: usize = 400;
/// What `logs` returns without `lines`.
pub const DEFAULT_LOG_LINES: usize = 50;

/// Deliberate difference (marv-rs #81): Marv's ends at "network: true". GLM reached for `start` to run a one-off
/// clone-and-count (it sent no command, see [`missing_command`]): the last sentence says what start is not for. The
/// oracle maps it back to Marv's (`marv_oracle::runner::MARV_RS_TEXT`).
pub const PROCESS_DESCRIPTION: &str = "Run something that should keep running (a dev server, a watcher) in its own sandbox, across turns, until stopped or Marv exits. start runs a shell command and waits up to 10 s for it to listen on a port: the ports it listens on (1024 and up) are forwarded to the user's localhost, and your bash commands can reach them too (curl localhost:<port>). logs reads its output, stop ends it and everything it started, list shows them all. The user is asked before each start. Like bash's sandbox: only the project folder is writable, no network unless network: true. start always needs command; a command that finishes on its own belongs in bash.";

/// #81: a start without `command` says what to send and what did arrive. On GLM via OpenRouter the call came as
/// `{"action": "start"}` although the model's reasoning had written a command too; "start needs a command" alone
/// left it guessing whether its fields reach Marv at all.
fn missing_command(input: &ProcessInput) -> String {
    let arrived: Vec<&str> =
        [Some("action"), input.name.as_ref().map(|_| "name"), input.network.map(|_| "network"), input.lines.map(|_| "lines")]
            .into_iter()
            .flatten()
            .collect();
    format!(
        "command: start needs a command, the shell command to keep running, in the same call: {{\"action\": \"start\", \"command\": \"bun run dev\"}}. This call had only: {}. For a command that finishes on its own (a build, a clone, a count), use bash instead.",
        arrived.join(", ")
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProcessAction {
    Start,
    Logs,
    Stop,
    List,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessInput {
    pub action: ProcessAction,
    pub command: Option<String>,
    pub name: Option<String>,
    pub network: Option<bool>,
    pub lines: Option<u64>,
}

fn url(port: u16) -> String {
    format!("http://localhost:{port}")
}

/// "running · http://localhost:4789" / "exited (code 1)".
fn state(info: &ProcessInfo) -> String {
    match info.state {
        ProcessState::Exited => format!("exited (code {})", code(info)),
        ProcessState::Running => {
            std::iter::once("running".to_string()).chain(info.ports.iter().map(|p| url(*p))).collect::<Vec<_>>().join(" · ")
        }
    }
}

fn code(info: &ProcessInfo) -> String {
    info.exit_code.map_or("?".into(), |c| c.to_string())
}

fn uptime(info: &ProcessInfo, now: u64) -> String {
    // Math.round, as Marv.
    let s = (now.saturating_sub(info.started_at) + 500) / 1000;
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m", s / 60)
    } else {
        format!("{}h{}m", s / 3600, (s % 3600) / 60)
    }
}

pub fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

/// One line per process, for list and `/processes`.
pub fn process_line(info: &ProcessInfo, now: u64) -> String {
    let net = if info.network { " · on the user's network" } else { "" };
    format!("{}: {}{net} · up {} · $ {}", info.name, state(info), uptime(info, now), info.command)
}

fn plural(n: usize, word: &str, suffix: &str) -> String {
    format!("{n} {word}{}", if n == 1 { "" } else { suffix })
}

pub struct ProcessTool;

#[async_trait]
impl Tool for ProcessTool {
    type Input = ProcessInput;

    fn name(&self) -> &str {
        "process"
    }
    fn description(&self) -> &str {
        PROCESS_DESCRIPTION
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ["start", "logs", "stop", "list"], "description": "start a process, read its logs, stop it, or list them all."},
                "command": {"description": "start: the shell command (bash), run in the project folder, e.g. bun run dev.", "type": "string", "minLength": 1},
                "name": {"description": "start: a short name for it (default: from the command). logs, stop: which process.", "type": "string", "minLength": 1},
                "network": {"description": "start: share the user's network (it can reach out, and its ports are the user's own, not forwarded). Off by default.", "type": "boolean"},
                "lines": {"description": format!("logs: how many of the last lines (default {DEFAULT_LOG_LINES}, at most {MAX_LOG_LINES})."), "type": "integer", "minimum": 1, "maximum": MAX_LOG_LINES}
            },
            "required": ["action"],
            "additionalProperties": false
        })
    }
    fn kind(&self) -> marv_core::ToolKind {
        marv_core::ToolKind::Execute
    }
    fn check(&self, input: &ProcessInput) -> Result<(), String> {
        if input.command.as_deref() == Some("") {
            return Err("command: must not be empty".into());
        }
        if input.name.as_deref() == Some("") {
            return Err("name: must not be empty".into());
        }
        if input.lines.is_some_and(|l| !(1..=MAX_LOG_LINES as u64).contains(&l)) {
            return Err(format!("lines: must be between 1 and {MAX_LOG_LINES}"));
        }
        match input.action {
            ProcessAction::Start if input.command.is_none() => Err(missing_command(input)),
            ProcessAction::Logs | ProcessAction::Stop if input.name.is_none() => {
                let action = if input.action == ProcessAction::Logs { "logs" } else { "stop" };
                Err(format!("name: {action} needs the process's name (list shows them)"))
            }
            _ => Ok(()),
        }
    }
    fn label(&self, input: &ProcessInput) -> String {
        match input.action {
            ProcessAction::Start => input.command.clone().unwrap_or_else(|| "start".into()),
            ProcessAction::List => "list".into(),
            ProcessAction::Logs => format!("logs {}", input.name.as_deref().unwrap_or("")).trim().to_string(),
            ProcessAction::Stop => format!("stop {}", input.name.as_deref().unwrap_or("")).trim().to_string(),
        }
    }
    /// Only starting asks: a door out of the sandbox. Logs, list and stop touch nothing but the session's own
    /// processes.
    fn needs_approval(&self, input: &ProcessInput) -> Option<bool> {
        Some(input.action == ProcessAction::Start)
    }
    /// A process's output and Marv's lines about it: the project's.
    fn provenance(&self) -> marv_core::Provenance {
        marv_core::Provenance::Project
    }
    /// What a process with the network prints (its first output on start, its logs) can be whatever a server sent
    /// it: outside text, fenced, and it taints the conversation.
    fn provenance_in(&self, input: &ProcessInput, ctx: &ToolContext) -> marv_core::Provenance {
        let networked = match input.action {
            ProcessAction::Start => input.network.unwrap_or(false),
            ProcessAction::Logs => ctx
                .processes
                .as_ref()
                .is_some_and(|p| p.list().iter().any(|info| Some(info.name.as_str()) == input.name.as_deref() && info.network)),
            ProcessAction::List | ProcessAction::Stop => false,
        };
        if networked { marv_core::Provenance::Network } else { marv_core::Provenance::Project }
    }
    fn uses_network(&self, input: &ProcessInput) -> bool {
        input.action == ProcessAction::Start && input.network.unwrap_or(false)
    }
    fn scope(&self, input: &ProcessInput) -> Option<Scope> {
        let net = if input.network.unwrap_or(false) { "net:" } else { "" };
        Some(Scope {
            key: format!("process:{net}{}", input.command.as_deref().unwrap_or("")),
            description: "starting this exact command in the background".into(),
        })
    }
    fn door(&self, input: &ProcessInput, _ctx: &ToolContext) -> Option<marv_core::Door> {
        (input.action == ProcessAction::Start).then_some(marv_core::Door::Process)
    }
    // No auto_safe: yolo never starts one unasked (it keeps running, and the user can reach it).

    async fn preview(&self, input: &ProcessInput, ctx: &ToolContext) -> Result<Preview, ToolError> {
        // Refused before any ask: the door is the context's to open (set for the main agent only), not the approver's.
        if !ctx.doors.process || ctx.processes.is_none() {
            return Err(ToolError::new(NO_PROCESS_DOOR));
        }
        let note = if input.network.unwrap_or(false) {
            "its own sandbox · shares your network: it can reach out, and its ports are your machine's"
        } else {
            "its own sandbox · no network · its ports forwarded to your 127.0.0.1"
        };
        Ok(Preview {
            title: match &input.name {
                Some(name) => format!("Start a background process ({name})"),
                None => "Start a background process".into(),
            },
            command: input.command.clone(),
            note: Some(note.into()),
            ..Preview::default()
        })
    }

    async fn run(&self, input: &ProcessInput, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let Some(processes) = &ctx.processes else { return Err(ToolError::new(NO_PROCESS_DOOR)) };
        let name = input.name.as_deref().unwrap_or("");
        match input.action {
            ProcessAction::List => {
                let all = processes.list();
                if all.is_empty() {
                    return Ok(ToolOutput::ok("No background processes.", "none"));
                }
                let now = now_ms();
                let lines: Vec<String> = all.iter().map(|p| format!("- {}", process_line(p, now))).collect();
                Ok(ToolOutput::ok(lines.join("\n"), plural(all.len(), "process", "es")))
            }
            ProcessAction::Logs => {
                let lines = input.lines.map_or(DEFAULT_LOG_LINES, |l| l as usize);
                let text = processes.logs(name, lines)?;
                let info = processes.list().into_iter().find(|p| p.name == name);
                let shown = if text.is_empty() { 0 } else { text.split('\n').count() };
                let shown_text = plural(shown, "line", "s");
                let output = format!(
                    "Logs of {name} ({}), last {shown_text}:\n{}",
                    info.as_ref().map_or("gone".into(), state),
                    if text.is_empty() { "(no output yet)" } else { &text }
                );
                Ok(ToolOutput::ok(output, shown_text))
            }
            ProcessAction::Stop => {
                if !processes.stop(name).await {
                    let names: Vec<String> = processes.list().into_iter().map(|p| p.name).collect();
                    let rest =
                        if names.is_empty() { " None is running.".into() } else { format!(" Processes: {}.", names.join(", ")) };
                    return Err(ToolError::new(format!("No process named \"{name}\".{rest}")));
                }
                Ok(ToolOutput::ok(format!("Stopped {name} and everything it started."), "stopped"))
            }
            ProcessAction::Start => {
                // Checked again here, so no path to run() skips the door (preview refuses first).
                if !ctx.doors.process {
                    return Err(ToolError::new(NO_PROCESS_DOOR));
                }
                let command = input.command.as_deref().unwrap_or("");
                let started = processes.start(command, input.name.as_deref(), input.network.unwrap_or(false)).await?;
                Ok(start_output(&started.info, &started.output, processes.one_shot()))
            }
        }
    }
}

/// In a one-shot run the process dies with Marv, right after the reply: the model may use it, never promise it (#86).
pub const ONE_SHOT_NOTE: &str = "This is a one-shot run (marv -p): when Marv exits after your reply it stops every process it started, so this one won't be running by the time the user reads it. Use it for your own checks now (curl it, read its logs), then stop it. Don't tell the user it's up or that you left it running: give them the command to run it themselves.";

/// What the model reads after a start.
fn start_output(info: &ProcessInfo, output: &str, one_shot: bool) -> ToolOutput {
    let mut parts = vec![format!("Process {}: {}.", info.name, state(info))];
    if info.state == ProcessState::Exited {
        parts.push("It ended at once: nothing is running. Fix the problem its output shows, then start it again.".into());
    } else if !info.ports.is_empty() {
        let urls = info.ports.iter().map(|p| url(*p)).collect::<Vec<_>>().join(", ");
        parts.push(if info.network {
            format!("It shares the user's network, so it listens on their machine directly: {urls}.")
        } else {
            format!("Forwarded to the user's machine: {urls} (your bash commands reach it there too).")
        });
    } else {
        parts.push("It's running but hasn't listened on a port yet (waited 10 s): check its logs, or list later.".into());
    }
    for note in &info.notes {
        parts.push(format!("Note: {note}"));
    }
    if one_shot && info.state == ProcessState::Running {
        parts.push(ONE_SHOT_NOTE.into());
    }
    parts.push(if output.is_empty() { "No output yet.".into() } else { format!("First output:\n{output}") });
    let summary = match info.state {
        ProcessState::Exited => format!("exited (code {})", code(info)),
        ProcessState::Running => state(info).replacen("http://", "", 1),
    };
    ToolOutput::ok(parts.join("\n"), summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::run_tool;
    use crate::types::{ApprovalRequest, Approver, Decision, Doors, ProcessHost, StartResult};
    use marv_core::{Door, ToolCall};
    use std::sync::{Arc, Mutex};

    fn info(name: &str, state: ProcessState, ports: Vec<u16>) -> ProcessInfo {
        ProcessInfo {
            name: name.into(),
            command: "bun run dev".into(),
            network: false,
            state,
            exit_code: (state == ProcessState::Exited).then_some(1),
            ports,
            notes: vec![],
            started_at: now_ms(),
        }
    }

    /// A host that records what it was asked and starts nothing real.
    #[derive(Default)]
    struct Fake {
        started: Mutex<Vec<(String, Option<String>, bool)>>,
        running: Mutex<Vec<ProcessInfo>>,
    }

    #[async_trait]
    impl ProcessHost for Fake {
        async fn start(&self, command: &str, name: Option<&str>, network: bool) -> Result<StartResult, ToolError> {
            self.started.lock().unwrap().push((command.into(), name.map(String::from), network));
            let mut info = info(name.unwrap_or("bun-run-dev"), ProcessState::Running, vec![4789]);
            info.network = network;
            self.running.lock().unwrap().push(info.clone());
            Ok(StartResult { info, output: "ready".into() })
        }
        fn logs(&self, name: &str, lines: usize) -> Result<String, ToolError> {
            if self.running.lock().unwrap().iter().any(|p| p.name == name) {
                Ok((1..=lines.min(3)).map(|n| format!("line {n}")).collect::<Vec<_>>().join("\n"))
            } else {
                Err(ToolError::new(format!("No process named \"{name}\".")))
            }
        }
        async fn stop(&self, name: &str) -> bool {
            let mut running = self.running.lock().unwrap();
            let before = running.len();
            running.retain(|p| p.name != name);
            running.len() < before
        }
        fn list(&self) -> Vec<ProcessInfo> {
            self.running.lock().unwrap().clone()
        }
    }

    struct Asked(Mutex<Vec<ApprovalRequest>>, Decision);
    #[async_trait]
    impl Approver for Asked {
        async fn approve(&self, request: ApprovalRequest) -> Decision {
            self.0.lock().unwrap().push(request);
            self.1.clone()
        }
    }

    fn ctx(host: &Arc<Fake>, door: bool, asked: &Arc<Asked>) -> ToolContext {
        let mut ctx = ToolContext::new("/project");
        ctx.yolo = true;
        ctx.doors = Doors { process: door, ..Doors::default() };
        ctx.processes = Some(host.clone());
        ctx.approver = Some(asked.clone());
        ctx
    }

    async fn call(ctx: &ToolContext, args: Value) -> ToolOutput {
        let call = ToolCall { id: "c1".into(), name: "process".into(), arguments: args.to_string() };
        let tools: Vec<crate::ToolRef> = vec![Arc::new(ProcessTool)];
        run_tool(&call, ctx, &tools).await.result
    }

    #[tokio::test]
    async fn start_asks_even_in_yolo_with_the_door_and_scope_and_logs_list_stop_dont() {
        let host = Arc::new(Fake::default());
        let asked = Arc::new(Asked(Mutex::default(), Decision::Yes));
        let ctx = ctx(&host, true, &asked);
        let started = call(&ctx, json!({"action": "start", "command": "bun run dev"})).await;
        assert_eq!(started.door, Some(Door::Process));
        assert!(started.output.starts_with("Process bun-run-dev: running · http://localhost:4789."), "{}", started.output);
        assert!(
            started
                .output
                .contains("Forwarded to the user's machine: http://localhost:4789 (your bash commands reach it there too).")
        );
        assert!(started.output.ends_with("First output:\nready"));
        assert_eq!(started.summary, "running · localhost:4789");
        assert_eq!(started.provenance, marv_core::Provenance::Project, "offline: the project's");
        let net = call(&ctx, json!({"action": "start", "command": "sleep 9", "network": true, "name": "n"})).await;
        assert!(net.output.contains("It shares the user's network, so it listens on their machine directly"));
        // With the network, what it prints can be anything a server sent it: outside text, its logs too.
        assert_eq!(net.provenance, marv_core::Provenance::Network);
        assert_eq!(call(&ctx, json!({"action": "logs", "name": "n"})).await.provenance, marv_core::Provenance::Network);
        {
            let asked = asked.0.lock().unwrap();
            let keys: Vec<_> = asked.iter().map(|r| (r.door, r.scope.key.clone(), r.once, r.network)).collect();
            assert_eq!(
                keys,
                [
                    (Some(Door::Process), "process:bun run dev".to_string(), false, false),
                    (Some(Door::Process), "process:net:sleep 9".to_string(), false, true)
                ]
            );
            assert_eq!(
                asked[0].preview.note.as_deref(),
                Some("its own sandbox · no network · its ports forwarded to your 127.0.0.1")
            );
            assert_eq!(asked[1].preview.title, "Start a background process (n)");
        }
        let logs = call(&ctx, json!({"action": "logs", "name": "bun-run-dev", "lines": 2})).await;
        assert_eq!(logs.output, "Logs of bun-run-dev (running · http://localhost:4789), last 2 lines:\nline 1\nline 2");
        assert_eq!(logs.approval, None);
        assert_eq!(logs.provenance, marv_core::Provenance::Project);
        let list = call(&ctx, json!({"action": "list"})).await;
        assert!(
            list.output.starts_with("- bun-run-dev: running · http://localhost:4789 · up 0s · $ bun run dev"),
            "{}",
            list.output
        );
        assert_eq!(list.summary, "2 processes");
        let stopped = call(&ctx, json!({"action": "stop", "name": "n"})).await;
        assert_eq!(stopped.output, "Stopped n and everything it started.");
        let missing = call(&ctx, json!({"action": "stop", "name": "n"})).await;
        assert!(missing.is_error);
        assert_eq!(missing.output, "No process named \"n\". Processes: bun-run-dev.");
        assert_eq!(asked.0.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn without_the_door_a_start_is_refused_before_anyone_is_asked() {
        let host = Arc::new(Fake::default());
        let asked = Arc::new(Asked(Mutex::default(), Decision::Yes));
        let result = call(&ctx(&host, false, &asked), json!({"action": "start", "command": "sleep 100"})).await;
        assert!(result.is_error);
        assert_eq!(result.output, NO_PROCESS_DOOR);
        assert!(asked.0.lock().unwrap().is_empty());
        assert!(host.started.lock().unwrap().is_empty());
        // And run() checks it again, whatever reached it.
        let input =
            ProcessInput { action: ProcessAction::Start, command: Some("x".into()), name: None, network: None, lines: None };
        assert_eq!(ProcessTool.run(&input, &ctx(&host, false, &asked)).await.unwrap_err().0, NO_PROCESS_DOOR);
        // No host at all: every action is refused.
        let mut bare = ToolContext::new("/project");
        bare.doors.process = true;
        assert_eq!(call(&bare, json!({"action": "list"})).await.output, NO_PROCESS_DOOR);
    }

    #[tokio::test]
    async fn a_no_starts_nothing() {
        let host = Arc::new(Fake::default());
        let asked = Arc::new(Asked(Mutex::default(), Decision::No));
        let result = call(&ctx(&host, true, &asked), json!({"action": "start", "command": "sleep 100"})).await;
        assert!(result.declined);
        assert_eq!(result.door, Some(Door::Process));
        assert!(host.started.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn start_needs_a_command_logs_and_stop_a_name_and_lines_are_1_to_400() {
        let host = Arc::new(Fake::default());
        let asked = Arc::new(Asked(Mutex::default(), Decision::Yes));
        let ctx = ctx(&host, true, &asked);
        for (args, error) in [
            (json!({"action": "start"}), "start needs a command"),
            // #81: it says what to send, and which fields did arrive.
            (json!({"action": "start"}), r#"{"action": "start", "command": "bun run dev"}. This call had only: action. "#),
            (json!({"action": "start", "network": true, "name": "n"}), "This call had only: action, name, network."),
            (json!({"action": "start"}), "use bash instead"),
            (json!({"action": "logs"}), "logs needs the process's name (list shows them)"),
            (json!({"action": "stop"}), "stop needs the process's name"),
            (json!({"action": "logs", "name": "a", "lines": 401}), "between 1 and 400"),
            (json!({"action": "restart"}), "unknown variant"),
        ] {
            let result = call(&ctx, args).await;
            assert!(result.is_error && result.output.contains(error), "{}", result.output);
        }
    }

    #[test]
    fn a_process_line_says_its_state_network_uptime_and_command() {
        let mut p = info("web", ProcessState::Exited, vec![]);
        p.started_at = 1_000;
        assert_eq!(process_line(&p, 1_000 + 3_725_000), "web: exited (code 1) · up 1h2m · $ bun run dev");
        p.state = ProcessState::Running;
        p.network = true;
        p.ports = vec![3000, 3001];
        assert_eq!(
            process_line(&p, 1_000 + 90_000),
            "web: running · http://localhost:3000 · http://localhost:3001 · on the user's network · up 1m · $ bun run dev"
        );
    }

    #[test]
    fn a_start_that_ended_at_once_or_didnt_listen_says_so() {
        let exited = start_output(&info("x", ProcessState::Exited, vec![]), "", false);
        assert_eq!(
            exited.output,
            "Process x: exited (code 1).\nIt ended at once: nothing is running. Fix the problem its output shows, then start it again.\nNo output yet."
        );
        assert_eq!(exited.summary, "exited (code 1)");
        let mut quiet = info("q", ProcessState::Running, vec![]);
        quiet.notes = vec!["port 80 isn't forwarded.".into()];
        let quiet = start_output(&quiet, "hi", false);
        assert!(quiet.output.contains("hasn't listened on a port yet (waited 10 s)"));
        assert!(quiet.output.contains("\nNote: port 80 isn't forwarded.\nFirst output:\nhi"));
        assert_eq!(quiet.summary, "running");
    }

    #[test]
    fn a_one_shot_run_says_the_process_ends_with_it() {
        // #86: in `marv -p` the model read "Forwarded to the user's machine" and told the user the server was up; Marv
        // then exited and stopped it.
        let running = info("dev", ProcessState::Running, vec![4789]);
        let once = start_output(&running, "", true);
        assert!(once.output.contains("Forwarded to the user's machine: http://localhost:4789"));
        assert!(once.output.contains(ONE_SHOT_NOTE), "{}", once.output);
        assert!(!start_output(&running, "", false).output.contains(ONE_SHOT_NOTE));
        // Nothing is running to warn about.
        assert!(!start_output(&info("x", ProcessState::Exited, vec![]), "", true).output.contains(ONE_SHOT_NOTE));
    }
}
