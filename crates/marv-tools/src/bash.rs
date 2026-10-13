//! `bash`: run a shell command in the project folder, in the sandbox.
//!
//! Ports Marv's `src/tools/bash.ts`. The sandbox (bwrap arguments, `.git` guard, notes) is marv-sandbox.
//!
//! - stdout and stderr combined in order (`exec 2>&1` before the command); both pipes read at once so neither fills
//!   and blocks; at most [`MAX_CAPTURE`] = 2M chars kept in memory; the model sees 30k (10k head + 20k tail,
//!   [`crate::output`]) and how it ended (`[exit code N]`, `[timed out after Ns]`, `[stopped by the user]`).
//! - If bwrap itself fails to start the sandbox, its stderr is shown as `[sandbox failed to start: …]` instead of
//!   empty output.
//! - Timeout default 120 s, max 600 s ([`MAX_TIMEOUT_S`]); a timeout given in milliseconds is repaired (issue #10).
//! - Sandboxed: bwrap, killed with SIGKILL (it takes its PID namespace down with it). Not sandboxed: under `setsid`,
//!   so a timeout or ctrl+c kills the whole process group; still a minimal environment (no API keys).
//! - Confined (yolo): `.git` read-only or an empty placeholder (`git_guard`; the placeholder's mount point is
//!   `rmdir`ed afterwards). A confined command that fails on the read-only `.git` gets `GIT_HINT`. An approved command
//!   in a worktree gets the checked worktree folders writable, except folders the caller made read-only.
//! - Notes after the output: the stand-in home (`touches_hidden_home`), the missing network (not for 126/127; a
//!   timeout counts) and, for `gh`, the GitHub MCP server instead (#23, #27).
//! - Scope `bash:[net:][git:]<command>` ("this exact command"); `auto_safe` only sandboxed, without network or
//!   `git_write`. The description tells the model not to write files with heredocs: a model put the terminator on the
//!   same line as `&& …`, read bash's warning as truncation, and retried for 10 steps.
//! - marv-rs: `uses_network` calls ask after taint even in yolo; the approval for a network command will list the
//!   scripts written this session that the command runs (docs/SECURITY.md).

use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::output::cut_for_model;
use crate::repair::repair_timeout_field;
use crate::types::{Preview, Repaired, Scope, Tool, ToolContext, ToolError, ToolOutput};

pub const DEFAULT_TIMEOUT_S: u64 = 120;
pub const MAX_TIMEOUT_S: u64 = 600;
/// Kept in memory per command; anything beyond is dropped (the middle is cut for the model anyway).
pub const MAX_CAPTURE: usize = 2_000_000;

/// How a command ended.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommandResult {
    pub output: String,
    /// None when the command was killed.
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub aborted: bool,
    pub timeout: Option<Duration>,
}

/// One command to run.
#[derive(Debug, Clone, Default)]
pub struct RunOptions {
    pub command: String,
    pub root: PathBuf,
    pub sandbox: bool,
    pub network: bool,
    pub timeout: Duration,
    pub read_only: Vec<PathBuf>,
    pub writable: Vec<PathBuf>,
    pub placeholders: Vec<PathBuf>,
    pub project_read_only: bool,
}

/// Runs a shell command in the project folder with stdout and stderr combined (also used by diagnostics and checks).
pub async fn run_command(_options: RunOptions, _sandbox: &marv_sandbox::Sandbox, _cancel: &tokio_util::sync::CancellationToken) -> CommandResult {
    todo!("port: runCommand: Sandbox::command or setsid bash -c, env cleared, capture both pipes to MAX_CAPTURE, timeout/abort → SIGKILL (process group outside bwrap), sandbox-failed-to-start message")
}

/// The command's result as the model sees it: output (start and end if long), then how it ended.
pub fn format_output(result: &CommandResult) -> String {
    let trimmed = result.output.trim_end();
    let text = if result.output.trim().is_empty() { "(no output)".to_string() } else { cut_for_model(trimmed) };
    let ending = if result.timed_out {
        // Math.round(ms / 1000), as Marv.
        format!("[timed out after {}s]", result.timeout.map_or(0, |t| (t.as_millis() + 500) / 1000))
    } else if result.aborted {
        "[stopped by the user]".to_string()
    } else {
        format!("[exit code {}]", result.exit_code.map_or("null".to_string(), |c| c.to_string()))
    };
    format!("{text}\n\n{ending}")
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BashInput {
    pub command: String,
    pub timeout: Option<u64>,
    pub network: Option<bool>,
    pub git_write: Option<bool>,
}

pub struct Bash;

#[async_trait]
impl Tool for Bash {
    type Input = BashInput;

    fn name(&self) -> &str {
        "bash"
    }
    fn description(&self) -> &str {
        "Run a shell command in the project folder, e.g. to run tests, a build, git status, or wc -l. It runs in a sandbox: only the project folder is writable, the home folder is an empty stand-in (toolchains like ~/.bun, ~/.local/share/mise, ~/.asdf, git config and ~/.marv/skills show through, read-only; nothing else in it is visible), and there's no network unless you set network: true. The user may be asked to approve a command; they always are for network: true or git_write: true. Not interactive: commands can't prompt for input. Don't write files with it (no heredocs or echo >): use write_file or edit_file. Output (stdout and stderr together) is cut in the middle if very long."
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": {"type": "string", "minLength": 1, "description": "The shell command (bash). Runs in the project folder."},
                "timeout": {"type": "integer", "minimum": 1, "maximum": MAX_TIMEOUT_S, "description": format!("Seconds before it's stopped. Default {DEFAULT_TIMEOUT_S}.")},
                "network": {"type": "boolean", "description": "Allow network access (e.g. to install packages). Off by default."},
                "git_write": {"type": "boolean", "description": "Set when the command changes the git repository (commit, checkout, merge, rebase, stash, branch, tag, init…). Otherwise .git may be read-only."}
            },
            "required": ["command"],
            "additionalProperties": false
        })
    }
    fn kind(&self) -> marv_core::ToolKind {
        marv_core::ToolKind::Execute
    }
    fn check(&self, input: &BashInput) -> Result<(), String> {
        if input.command.is_empty() {
            return Err("command: must not be empty".into());
        }
        if input.timeout.is_some_and(|t| !(1..=MAX_TIMEOUT_S).contains(&t)) {
            return Err(format!("timeout: must be between 1 and {MAX_TIMEOUT_S}"));
        }
        Ok(())
    }
    fn label(&self, input: &BashInput) -> String {
        input.command.clone()
    }
    fn uses_network(&self, input: &BashInput) -> bool {
        input.network.unwrap_or(false)
    }
    fn scope(&self, input: &BashInput) -> Option<Scope> {
        let net = if input.network.unwrap_or(false) { "net:" } else { "" };
        let git = if input.git_write.unwrap_or(false) { "git:" } else { "" };
        Some(Scope { key: format!("bash:{net}{git}{}", input.command), description: "this exact command".into() })
    }
    /// In the sandbox, without network, and (as run_tool confines it) with .git read-only, a command can only change
    /// the project's own files. Without the sandbox, nothing limits it.
    fn auto_safe(&self, input: &BashInput, ctx: &ToolContext) -> bool {
        ctx.sandbox.active() && !input.network.unwrap_or(false) && !input.git_write.unwrap_or(false)
    }
    fn repair(&self, raw: &Value, _ctx: &ToolContext) -> Option<Repaired> {
        repair_timeout_field(raw, MAX_TIMEOUT_S)
    }

    async fn preview(&self, input: &BashInput, ctx: &ToolContext) -> Result<Preview, ToolError> {
        let sandboxed = ctx.sandbox.active();
        let mut notes = Vec::new();
        if sandboxed {
            notes.push("sandboxed".to_string());
            notes.push(if input.network.unwrap_or(false) { "network allowed" } else { "no network" }.to_string());
        }
        if let Some(t) = input.timeout {
            notes.push(format!("timeout {t}s"));
        }
        let warning = if sandboxed {
            None
        } else if ctx.sandbox.enabled {
            Some("bubblewrap isn't available here, so this runs WITHOUT a sandbox".to_string())
        } else {
            Some("the sandbox is off (/sandbox on to turn it back on), so this runs WITHOUT a sandbox".to_string())
        };
        Ok(Preview {
            title: "Run a command".into(),
            command: Some(input.command.clone()),
            note: (!notes.is_empty()).then(|| notes.join(" · ")),
            warning,
            ..Default::default()
        })
    }

    async fn run(&self, _input: &BashInput, _ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        todo!("port: git_guard when confined, worktree_git writable when approved, run_command, rmdir placeholders, \
             GIT_HINT / HOME_NOTE / NETWORK_NOTE / GH_NOTE, summary 'exit N · L lines of output' | 'timed out after Ns' | 'stopped'")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(output: &str, exit_code: Option<i32>) -> CommandResult {
        CommandResult { output: output.into(), exit_code, ..Default::default() }
    }

    #[test]
    fn shows_the_output_and_exit_code() {
        assert_eq!(format_output(&result("ok\n", Some(0))), "ok\n\n[exit code 0]");
        assert_eq!(format_output(&result("", Some(1))), "(no output)\n\n[exit code 1]");
    }

    #[test]
    fn says_when_it_timed_out_or_was_stopped() {
        let timed_out = CommandResult { output: "partial".into(), timed_out: true, timeout: Some(Duration::from_secs(1)), ..Default::default() };
        assert!(format_output(&timed_out).contains("[timed out after 1s]"));
        let stopped = CommandResult { aborted: true, ..Default::default() };
        assert!(format_output(&stopped).ends_with("[stopped by the user]"));
    }

    #[test]
    fn scope_and_label() {
        let input = BashInput { command: "bun test".into(), timeout: None, network: Some(true), git_write: Some(true) };
        assert_eq!(Bash.scope(&input).unwrap().key, "bash:net:git:bun test");
        assert!(Bash.uses_network(&input));
    }
}
