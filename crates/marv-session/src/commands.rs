//! Slash commands, engine side. Ports Marv's `src/commands/index.ts` (and `sandbox-notice.ts`).
//!
//! Commands are handled locally and never reach the model. Each is a pure function returning a [`CommandAction`];
//! the client (the TUI) applies it. That keeps this module free of UI and I/O, and easy to test. Add new commands to
//! [`commands`]; they show up in the `/` menu automatically. A `/<skill>` runs that skill, but built-in names win.
//! Text that quotes the project, a server or a check goes through `printable()`.
//!
//! Port note: `/sandbox`'s "is bubblewrap here?" is a field of [`CommandContext`] (`sandbox_available`), not a call,
//! so every command stays pure.

use marv_core::printable;
use marv_mcp::config::ServerSource;
use marv_mcp::manager::{McpServerStatus, McpState};

use crate::agents::{AgentType, SUBAGENT_TOOLS};
use crate::checks::{CHECKS_DIR, ProjectCheck, ProjectCheckResult};
use crate::config::{Config, KeySource, mask_key, preset};
use crate::session::CheckpointProblem;
use crate::skills::{LoadedSkill, SkillSource};
use crate::usage::{Totals, usage_report};

#[derive(Debug, Clone, PartialEq)]
pub enum MemoryTarget {
    Personal,
    Project,
}

#[derive(Debug, Clone, PartialEq)]
pub enum CommandAction {
    Print {
        text: String,
        is_error: bool,
        markdown: bool,
    },
    Clear,
    Setup,
    /// With an id: switch straight to it. Without: open the model picker.
    Model {
        id: Option<String>,
    },
    Thinking {
        on: bool,
    },
    Sandbox {
        on: bool,
    },
    Yolo {
        on: bool,
    },
    Trajectories {
        on: bool,
    },
    Diagnostics {
        on: bool,
    },
    Checks {
        on: bool,
    },
    Checkpoints {
        on: bool,
    },
    /// Show the last turn's changes and, on a yes, put its files back (#16).
    Undo,
    /// The step limit: set it (0 never asks). Showing it is a `Print`.
    Steps {
        every: u32,
    },
    McpTrust,
    McpAdd {
        args: String,
    },
    McpSearch {
        words: String,
    },
    McpRemove {
        name: String,
    },
    McpLogin {
        server: String,
    },
    McpLogout {
        server: String,
    },
    /// Rate or tag the last turn: 1 good, -1 bad, 0 labels only.
    Feedback {
        score: i8,
        note: Option<String>,
        labels: Option<Vec<String>>,
    },
    Resume,
    Remember {
        scope: MemoryTarget,
        text: String,
    },
    Forget {
        text: String,
    },
    Memory,
    Compact {
        focus: Option<String>,
    },
    /// marv-rs #78: let a message on a wired inbox start a turn while idle (`mesh.wake`). Showing it is a `Print`.
    MeshWake {
        on: bool,
    },
    /// Stop one of the session's background processes (Marv #51).
    Processes {
        stop: String,
    },
    /// The user ran a skill: `/<name> <args>`.
    Skill {
        name: String,
        args: String,
    },
    Exit,
}

fn print(text: impl Into<String>) -> CommandAction {
    CommandAction::Print { text: text.into(), is_error: false, markdown: false }
}
fn markdown(text: impl Into<String>) -> CommandAction {
    CommandAction::Print { text: text.into(), is_error: false, markdown: true }
}
fn error(text: impl Into<String>) -> CommandAction {
    CommandAction::Print { text: text.into(), is_error: true, markdown: false }
}

/// The mesh as `/mesh` shows it (marv-rs): who this Marv is on it and what's wired to its inbox.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MeshStatus {
    /// Its name on the machine's mesh (others reach it at `peer.<name>.agent.parent.inbox`), when its door is open.
    pub name: Option<String>,
    /// The wires into the main agent's inbox, as `from -> receptacle`: the only messages that can wake it.
    pub wired: Vec<String>,
}

/// Read-only facts a command may need.
#[derive(Debug, Clone)]
pub struct CommandContext<'a> {
    pub config: &'a Config,
    pub config_path: String,
    pub skills: &'a [LoadedSkill],
    pub agents: &'a [AgentType],
    pub agent_problems: &'a [String],
    pub skill_problems: &'a [String],
    /// MCP servers as they are now, and config problems.
    pub mcp: Option<(&'a [McpServerStatus], &'a [String])>,
    pub trajectories_path: Option<String>,
    /// Totals, the last request, the context window.
    pub usage: Option<(&'a Totals, Option<&'a marv_core::Usage>, Option<u64>)>,
    /// The project's checks on disk, problems, the session's last pass.
    pub checks: Option<(&'a [ProjectCheck], &'a [String], Option<&'a ProjectCheckResult>)>,
    /// Why the last checkpoint failed, or why checkpoints are off for this project, for /checkpoints.
    pub checkpoints: Option<&'a CheckpointProblem>,
    /// PATH folders under the home folder the sandbox can't see, shortened for display.
    pub hidden_path_folders: &'a [String],
    pub sandbox_available: bool,
    /// The session's background processes, for /processes; None when the session doesn't offer the process tool.
    pub processes: Option<&'a [marv_tools::ProcessInfo]>,
    /// The session's switchboard as `/mesh` shows it; None when this Marv isn't on the mesh.
    pub mesh: Option<&'a MeshStatus>,
}

pub struct Command {
    pub name: &'static str,
    pub description: &'static str,
    pub run: fn(&str, &CommandContext) -> CommandAction,
}

fn on_off(args: &str, usage: &str) -> Result<Option<bool>, CommandAction> {
    match args.to_lowercase().as_str() {
        "" => Ok(None),
        "on" => Ok(Some(true)),
        "off" => Ok(Some(false)),
        _ => Err(error(usage)),
    }
}

/// Every built-in command, in `/help` order.
pub fn commands() -> &'static [Command] {
    static COMMANDS: &[Command] = &[
        Command { name: "help", description: "Show available commands", run: |_, _| print(help_text()) },
        Command { name: "config", description: "Show the current configuration", run: |_, ctx| print(config_text(ctx)) },
        Command { name: "setup", description: "Change provider, model, or API key", run: |_, _| CommandAction::Setup },
        Command {
            name: "model",
            description: "Switch model (/model to pick, /model <id> to set)",
            run: |args, _| CommandAction::Model { id: (!args.is_empty()).then(|| args.to_string()) },
        },
        Command {
            name: "think",
            description: "Toggle model thinking (/think on, /think off)",
            run: |args, ctx| match on_off(args, "Usage: /think, /think on, or /think off") {
                Ok(on) => CommandAction::Thinking { on: on.unwrap_or(!ctx.config.thinking) },
                Err(e) => e,
            },
        },
        Command { name: "memory", description: "Show what Marv remembers", run: |_, _| CommandAction::Memory },
        Command {
            name: "remember",
            description: "Save a note to memory (/remember <note>, or /remember project: <note>)",
            run: |args, _| {
                let lower = args.to_lowercase();
                let (scope, text) = match lower.strip_prefix("project:") {
                    Some(_) => (MemoryTarget::Project, args["project:".len()..].trim_start()),
                    None => (MemoryTarget::Personal, args),
                };
                if text.trim().is_empty() {
                    return error("Usage: /remember <note> (personal), or /remember project: <note>");
                }
                CommandAction::Remember { scope, text: text.to_string() }
            },
        },
        Command {
            name: "forget",
            description: "Remove a memory (/forget <text from it>)",
            run: |args, _| {
                if args.trim().is_empty() {
                    error("Usage: /forget <text from the memory>")
                } else {
                    CommandAction::Forget { text: args.into() }
                }
            },
        },
        Command {
            name: "compact",
            description: "Summarize the conversation to free up context (/compact <what to keep>)",
            run: |args, _| CommandAction::Compact { focus: (!args.is_empty()).then(|| args.to_string()) },
        },
        Command { name: "resume", description: "Pick up an earlier session in this project", run: |_, _| CommandAction::Resume },
        Command {
            name: "cost",
            description: "Show tokens used and cost so far",
            run: |_, ctx| {
                print(
                    ctx.usage
                        .map_or_else(|| "No requests yet this session.".to_string(), |(t, last, c)| usage_report(t, last, c)),
                )
            },
        },
        Command {
            name: "agents",
            description: "List the subagent types Marv can start",
            run: |_, ctx| markdown(agents_text(ctx.agents, ctx.agent_problems)),
        },
        Command {
            name: "skills",
            description: "List the skills Marv can use",
            run: |_, ctx| markdown(skills_text(ctx.skills, ctx.skill_problems)),
        },
        Command {
            name: "sandbox",
            description: "Show or set the bash sandbox (/sandbox on, /sandbox off)",
            run: |args, ctx| match on_off(args, "Usage: /sandbox, /sandbox on, or /sandbox off") {
                Ok(Some(on)) => CommandAction::Sandbox { on },
                Ok(None) => {
                    print(format!("Sandbox: {}", sandbox_status(ctx.config, ctx.sandbox_available, ctx.hidden_path_folders)))
                }
                Err(e) => e,
            },
        },
        Command {
            name: "yolo",
            description: "Run what the sandbox confines without asking (/yolo on, /yolo off)",
            run: |args, ctx| match on_off(args, "Usage: /yolo, /yolo on, or /yolo off") {
                Ok(on) => CommandAction::Yolo { on: on.unwrap_or(!ctx.config.yolo) },
                Err(e) => e,
            },
        },
        Command {
            name: "mcp",
            description: "Show MCP servers and their tools (/mcp add <package | URL | repo>, search, remove, trust, login, logout)",
            run: |args, ctx| mcp_command(args.trim(), ctx),
        },
        Command {
            name: "good",
            description: "Rate the last turn as good (/good <optional note>)",
            run: |args, _| CommandAction::Feedback {
                score: 1,
                note: Some(args.trim().to_string()).filter(|n| !n.is_empty()),
                labels: None,
            },
        },
        Command {
            name: "bad",
            description: "Rate the last turn as bad (/bad <what went wrong>)",
            run: |args, _| CommandAction::Feedback {
                score: -1,
                note: Some(args.trim().to_string()).filter(|n| !n.is_empty()),
                labels: None,
            },
        },
        Command {
            name: "label",
            description: "Tag the last turn (/label refactor, tests)",
            run: |args, _| {
                let labels: Vec<String> =
                    args.split(|c: char| c.is_whitespace() || c == ',').filter(|s| !s.is_empty()).map(String::from).collect();
                if labels.is_empty() {
                    error("Usage: /label <tag> [more tags]")
                } else {
                    CommandAction::Feedback { score: 0, note: None, labels: Some(labels) }
                }
            },
        },
        Command {
            name: "trajectories",
            description: "Show or set run logging (/trajectories on, /trajectories off)",
            run: |args, ctx| match on_off(args, "Usage: /trajectories, /trajectories on, or /trajectories off") {
                Ok(Some(on)) => CommandAction::Trajectories { on },
                Ok(None) => print(format!("Trajectories: {}", trajectories_status(ctx.config, ctx.trajectories_path.as_deref()))),
                Err(e) => e,
            },
        },
        Command {
            name: "checks",
            description: "The project's checks from .marv/checks/ and their last result (/checks on, /checks off)",
            run: |args, ctx| match on_off(args, "Usage: /checks, /checks on, or /checks off") {
                Ok(Some(on)) => CommandAction::Checks { on },
                Ok(None) => markdown(checks_text(ctx)),
                Err(e) => e,
            },
        },
        Command {
            name: "processes",
            description: "Background processes the model started (/processes stop <name>)",
            run: |args, ctx| {
                let words: Vec<&str> = args.split_whitespace().collect();
                match words[..] {
                    ["stop", name] => return CommandAction::Processes { stop: name.to_string() },
                    [] => {}
                    _ => return error("Usage: /processes, or /processes stop <name>"),
                }
                match ctx.processes {
                    None => print("Background processes aren't available here: they need the bubblewrap sandbox."),
                    Some([]) => print(
                        "No background processes. The model starts one with the process tool (a dev server, a watcher), and you're asked first.",
                    ),
                    Some(list) => {
                        let now = marv_tools::process::now_ms();
                        print(
                            list.iter()
                                .map(|p| printable(&marv_tools::process::process_line(p, now)))
                                .collect::<Vec<_>>()
                                .join("\n"),
                        )
                    }
                }
            },
        },
        Command {
            name: "mesh",
            description: "The mesh: what's wired to Marv's inbox (/mesh wake on lets a message start a turn while idle)",
            run: |args, ctx| {
                let words: Vec<String> = args.split_whitespace().map(str::to_lowercase).collect();
                let words: Vec<&str> = words.iter().map(String::as_str).collect();
                match words[..] {
                    [] => markdown(mesh_text(ctx)),
                    ["wake", "on"] => CommandAction::MeshWake { on: true },
                    ["wake", "off"] => CommandAction::MeshWake { on: false },
                    ["wake"] => print(format!("Mesh wake: {}", mesh_wake_status(ctx.config))),
                    _ => error("Usage: /mesh, /mesh wake, /mesh wake on, or /mesh wake off"),
                }
            },
        },
        Command {
            name: "undo",
            description: "Put back the files the last turn changed (shows them first)",
            run: |args, _| {
                if args.trim().is_empty() {
                    CommandAction::Undo
                } else {
                    error("Usage: /undo (it takes back the last turn not yet undone; again for the one before)")
                }
            },
        },
        Command {
            name: "checkpoints",
            description: "Snapshots before each turn's changes, for /undo (/checkpoints on, /checkpoints off)",
            run: |args, ctx| match on_off(args, "Usage: /checkpoints, /checkpoints on, or /checkpoints off") {
                Ok(Some(on)) => CommandAction::Checkpoints { on },
                Ok(None) => {
                    let problem = match ctx.checkpoints {
                        Some(CheckpointProblem { off: Some(off), .. }) => format!("\nOff for this project: {}.", printable(off)),
                        Some(CheckpointProblem { failure: Some(failure), .. }) => {
                            format!("\nThe last checkpoint failed: {}", printable(failure))
                        }
                        _ => String::new(),
                    };
                    let problem = if ctx.config.checkpoints { problem } else { String::new() };
                    print(format!("Checkpoints: {}{problem}", checkpoints_status(ctx.config)))
                }
                Err(e) => e,
            },
        },
        Command {
            name: "diagnostics",
            description: "Typecheck after the model's edits (/diagnostics on, /diagnostics off)",
            run: |args, ctx| match on_off(args, "Usage: /diagnostics, /diagnostics on, or /diagnostics off") {
                Ok(Some(on)) => CommandAction::Diagnostics { on },
                Ok(None) => print(format!("Diagnostics: {}", diagnostics_status(ctx.config))),
                Err(e) => e,
            },
        },
        Command {
            name: "steps",
            description: "Show or set the step limit (/steps 40, /steps off never asks)",
            run: |args, ctx| {
                let arg = args.trim();
                if arg.eq_ignore_ascii_case("off") {
                    return CommandAction::Steps { every: 0 };
                }
                if !arg.is_empty() && arg.chars().all(|c| c.is_ascii_digit()) {
                    return match arg.parse::<u32>() {
                        Err(_) => error("That's too many steps; use /steps off to never ask."),
                        // 1 would ask after every step; use /steps off for never asking.
                        Ok(1) => error("The smallest interval is 2 steps; /steps off never asks."),
                        Ok(every) => CommandAction::Steps { every },
                    };
                }
                if !arg.is_empty() {
                    return error("Usage: /steps, /steps <number>, or /steps off");
                }
                print(steps_status(ctx.config))
            },
        },
        Command { name: "clear", description: "Clear the conversation", run: |_, _| CommandAction::Clear },
        Command { name: "exit", description: "Quit Marv", run: |_, _| CommandAction::Exit },
    ];
    COMMANDS
}

pub fn is_command(input: &str) -> bool {
    input.starts_with('/')
}

/// Runs a `/command args` line.
pub fn run_command(input: &str, ctx: &CommandContext) -> CommandAction {
    let mut words = input[1..].split_whitespace();
    let name = words.next().unwrap_or("").to_lowercase();
    let rest = words.collect::<Vec<_>>().join(" ");
    if let Some(command) = commands().iter().find(|c| c.name == name) {
        return (command.run)(&rest, ctx);
    }
    if ctx.skills.iter().any(|s| s.skill.name == name) {
        return CommandAction::Skill { name, args: rest };
    }
    error(format!("Unknown command: /{name}. Type /help for a list."))
}

pub fn help_text() -> String {
    let width = commands().iter().map(|c| c.name.len()).max().unwrap_or(0) + 2;
    let mut lines = vec!["Commands:".to_string()];
    lines.extend(commands().iter().map(|c| format!("  /{:<width$}{}", c.name, c.description)));
    lines.extend(
        [
            "",
            "Shortcuts:",
            "  ↑/↓       input history",
            "  esc       stop a reply or tool",
            "  ctrl+o    show subagents' steps and whole diffs",
            "  ctrl+c    clear input · press twice to exit",
        ]
        .map(String::from),
    );
    lines.join("\n")
}

fn config_text(ctx: &CommandContext) -> String {
    let c = ctx.config;
    let p = preset(c.provider);
    let key = match (p.key_env, &c.api_key) {
        (None, _) => "not needed".to_string(),
        (Some(env), Some(k)) => {
            format!("{} (from {})", mask_key(k), if c.api_key_source == Some(KeySource::Env) { env } else { "config file" })
        }
        (Some(_), None) => "not set".into(),
    };
    let mut lines = vec![
        format!("Provider:  {}", p.label),
        format!("Model:     {}", c.model),
        format!("Thinking:  {}", if c.thinking { "on" } else { "off" }),
        format!("Sandbox:   {}", sandbox_status(c, ctx.sandbox_available, &[])),
        format!("Yolo:      {} (/yolo)", if c.yolo { "on" } else { "off" }),
        format!("Trajectories: {} (/trajectories)", if c.trajectories { "on" } else { "off" }),
        format!("Diagnostics: {} (/diagnostics)", if c.diagnostics { "on" } else { "off" }),
        format!("Checks:    {} (/checks)", if c.checks { "on" } else { "off" }),
        format!("Checkpoints: {} (/checkpoints, /undo)", if c.checkpoints { "on" } else { "off" }),
        format!("Steps: {}", steps_status(c)),
        format!("Mesh wake: {} (/mesh)", if c.mesh_wake { "on" } else { "off" }),
        format!("Endpoint:  {}", c.base_url),
    ];
    if c.provider == marv_provider::ProviderKind::Ollama {
        lines.push(format!("Context:   {} tokens (contextLength in the config file)", c.context_length));
    }
    lines.push(format!("API key:   {key}"));
    lines.push(format!("File:      {}", ctx.config_path));
    lines.join("\n")
}

pub fn diagnostics_status(config: &Config) -> String {
    if config.diagnostics {
        "on: after the model changes files in a TypeScript project, Marv runs the project's typecheck (in the sandbox, read-only) and tells the model about new errors".into()
    } else {
        "off".into()
    }
}

/// `/mesh wake` (#78).
pub fn mesh_wake_status(config: &Config) -> String {
    if config.mesh_wake {
        format!(
            "on: a message on a wired inbox starts a turn while Marv is idle (at most {} a minute per wire; never during a turn or while an approval waits, and never in marv -p). The message is read as data from another agent, never as your instructions, and the conversation counts as tainted",
            crate::mesh::WAKES_PER_MINUTE
        )
    } else {
        "off: messages wait in the inbox until your next turn (/mesh wake on lets one start a turn while Marv is idle)".into()
    }
}

fn mesh_text(ctx: &CommandContext) -> String {
    let mut parts = Vec::new();
    match ctx.mesh {
        None => parts.push("**Mesh**: this Marv isn't on the mesh.".to_string()),
        Some(mesh) => {
            parts.push(match &mesh.name {
                Some(name) => {
                    let name = printable(name);
                    format!("**Mesh**: on the mesh as `{name}` (others reach you at `peer.{name}.agent.parent.inbox`).")
                }
                None => "**Mesh**: this session's own switchboard (not on the machine's mesh).".to_string(),
            });
            parts.push(if mesh.wired.is_empty() {
                "Nothing is wired to Marv's inbox.".to_string()
            } else {
                let list = mesh.wired.iter().map(|w| format!("- `{}`", printable(w))).collect::<Vec<_>>().join("\n");
                format!("Wired to Marv's inbox:\n{list}")
            });
        }
    }
    parts.push(format!("**Wake**: {}.", mesh_wake_status(ctx.config)));
    parts.join("\n\n")
}

pub fn checkpoints_status(config: &Config) -> String {
    if config.checkpoints {
        "on: before a turn's first change Marv snapshots the project's files (git, under a hidden ref; never your branch, index or stash), so /undo can put them back".into()
    } else {
        "off".into()
    }
}

pub fn checks_status(config: &Config) -> String {
    if config.checks {
        format!(
            "on: after the model changes files, Marv runs the project's checks ({CHECKS_DIR}/) that concern them (in the sandbox, read-only) and tells the model about new violations"
        )
    } else {
        "off".into()
    }
}

fn checks_text(ctx: &CommandContext) -> String {
    let (list, problems, last) = ctx.checks.unwrap_or((&[], &[], None));
    let mut parts = vec![format!("**Checks**: {}.", checks_status(ctx.config))];
    if list.is_empty() && problems.is_empty() {
        parts.push(format!("This project has none. Add one rule per file to `{CHECKS_DIR}/`: a `.yaml` with `glob`, `forbid` or `require` (a regular expression), `message` and `why`, or a `.ts` script (`// glob: …` comment first) that prints violations as JSON lines `{{file, line, message, why}}`."));
    } else {
        parts.push(
            list.iter()
                .map(|c| {
                    let what = match &c.body {
                        crate::checks::load::CheckBody::Pattern { rule, message, .. } => format!(
                            "{} a pattern: {}",
                            if matches!(rule, crate::checks::load::CheckRule::Forbid(_)) { "forbids" } else { "requires" },
                            printable(message)
                        ),
                        crate::checks::load::CheckBody::Script => "script".into(),
                    };
                    let not = if c.exclude.is_empty() { String::new() } else { format!(" · not {}", c.exclude.join(", ")) };
                    format!("- `{}` *({what})*\n  {}{not}", c.name, c.glob.join(", "))
                })
                .collect::<Vec<_>>()
                .join("\n"),
        );
    }
    if !problems.is_empty() {
        parts.push("**Couldn't load:**".into());
        parts.push(problems.iter().map(|p| format!("- {}", printable(p))).collect::<Vec<_>>().join("\n"));
    }
    parts.push(format!("**Last result:** {}", last_checks_text(last)));
    parts.join("\n\n")
}

fn last_checks_text(last: Option<&ProjectCheckResult>) -> String {
    use crate::diagnostics::turn::{CheckOutcome, FailReason, OffReason};
    match last {
        None => "none yet this session (checks run after a step that changed a file they concern).".into(),
        Some(CheckOutcome::Off { reason: OffReason::NoSandbox }) => "off: they run only in the sandbox.".into(),
        Some(CheckOutcome::Off { reason: OffReason::Timeouts }) => "off: they timed out twice this session.".into(),
        Some(CheckOutcome::Failed { reason: FailReason::MissingInterpreter, tool, .. }) => {
            format!("failed: {} isn't in the sandbox.", tool.as_deref().unwrap_or("bun"))
        }
        Some(CheckOutcome::Failed { reason, .. }) => {
            format!("failed ({}).", if *reason == FailReason::Timeout { "timeout" } else { "unreadable" })
        }
        Some(CheckOutcome::Done { errors, added, ms, .. }) => {
            let total = format!("{errors} violation{} in all", if *errors == 1 { "" } else { "s" });
            if added.is_empty() {
                format!("nothing new ({total}, {ms} ms).")
            } else {
                let mut lines = vec![format!("{} new ({total}, {ms} ms):", added.len())];
                lines.extend(added.iter().map(|v| format!("- {}", printable(&crate::checks::kind::format_violation(v)))));
                lines.join("\n")
            }
        }
    }
}

pub fn steps_status(config: &Config) -> String {
    if config.max_steps == 0 {
        "off: Marv never asks to keep going, and a turn without an approver doesn't stop at a step limit either (subagents still stop at their own 50).".into()
    } else {
        format!(
            "Keep going? after {} steps (0, or off, never asks). Without someone to ask, the turn stops there.",
            config.max_steps
        )
    }
}

pub fn trajectories_status(config: &Config, path: Option<&str>) -> String {
    if config.trajectories {
        format!("on: every turn is logged to {} (rate turns with /good, /bad, /label)", path.unwrap_or("~/.marv/trajectories"))
    } else {
        "off: turns aren't logged".into()
    }
}

/// What yolo mode does with this config, for the notice when it's turned on.
pub fn yolo_status(config: &Config, sandbox_available: bool) -> String {
    if !config.yolo {
        return "Yolo off: every change asks first.".into();
    }
    if config.sandbox && sandbox_available {
        "Yolo on: sandboxed commands and file edits run without asking. Network access, git changes (git_write), edits inside .git, and memory still ask.".into()
    } else {
        format!(
            "Yolo on: file edits run without asking. Commands still ask: {}, so nothing confines them.",
            if config.sandbox { "bubblewrap isn't available here" } else { "the sandbox is off" }
        )
    }
}

/// The startup message for PATH folders the sandbox can't see.
pub fn hidden_path_folders_notice(folders: &[String]) -> String {
    let one = folders.len() == 1;
    format!(
        "Your PATH has {} folder{} the sandbox can't see: {}. Commands in Marv can't run the tool{} in it, and tsc and other node_modules scripts may not run. Tell Marv's sandbox about it (an issue on the repository) or add the tool to a folder it does show, like ~/.local/bin.",
        folders.len(),
        if one { "" } else { "s" },
        folders.join(", "),
        if one { "" } else { "s" }
    )
}

fn sandbox_status(config: &Config, available: bool, hidden: &[String]) -> String {
    if !config.sandbox {
        return "off: bash commands run directly on your system (after approval)".into();
    }
    if !available {
        return "on, but bubblewrap isn't available here, so bash runs WITHOUT a sandbox".into();
    }
    let hidden = if hidden.is_empty() { String::new() } else { format!(" {}", hidden_path_folders_notice(hidden)) };
    format!("on: bash runs in bubblewrap (project writable, home hidden, no network unless asked).{hidden}")
}

const MCP_USAGE: &str = "Usage: /mcp (list servers), /mcp add <npm package | pypi:<package> | https URL | GitHub repo | registry name> [--name <name>] [--env VAR] [--header \"Name: Bearer ${VAR}\"] [-- <server args>], /mcp search <words>, /mcp remove <name>, /mcp trust, /mcp login <name>, /mcp logout <name>";

const MCP_HELP: &str = "No MCP servers yet. Add one with `/mcp add <npm package | URL | GitHub repo>` (find one with `/mcp search <words>`), or by hand in `.mcp.json` in the project, or `~/.marv/mcp.json` for every project (the same format as Claude Code's), then restart Marv:\n\n```json\n{ \"mcpServers\": {\n    \"files\": { \"command\": \"npx\", \"args\": [\"-y\", \"some-mcp-server\"] },\n    \"docs\": { \"type\": \"http\", \"url\": \"https://example.com/mcp\", \"headers\": { \"Authorization\": \"Bearer ${DOCS_TOKEN}\" } } } }\n```";

fn mcp_command(args: &str, ctx: &CommandContext) -> CommandAction {
    let servers: &[McpServerStatus] = ctx.mcp.map_or(&[], |m| m.0);
    if args.is_empty() {
        return markdown(mcp_text(ctx));
    }
    let mut words = args.split_whitespace();
    let sub = words.next().unwrap_or("");
    let words: Vec<&str> = words.collect();
    let rest = args[sub.len()..].trim();
    let action = match sub.to_lowercase().as_str() {
        "trust" if rest.is_empty() => Some(CommandAction::McpTrust),
        "add" if !rest.is_empty() => Some(CommandAction::McpAdd { args: rest.into() }),
        "search" if !rest.is_empty() => Some(CommandAction::McpSearch { words: rest.into() }),
        "remove" if words.len() == 1 => Some(CommandAction::McpRemove { name: words[0].into() }),
        "login" => Some(login_action(true, &words, servers)),
        "logout" => Some(login_action(false, &words, servers)),
        _ => None,
    };
    action.unwrap_or_else(|| error(MCP_USAGE))
}

/// `/mcp login|logout <server>`: only a server that can log in; otherwise say which can.
fn login_action(login: bool, words: &[&str], servers: &[McpServerStatus]) -> CommandAction {
    let can: Vec<&str> = servers.iter().filter(|s| s.login.is_some()).map(|s| s.name.as_str()).collect();
    if words.len() == 1 && can.contains(&words[0]) {
        let server = words[0].to_string();
        return if login { CommandAction::McpLogin { server } } else { CommandAction::McpLogout { server } };
    }
    let verb = if login { "login" } else { "logout" };
    if can.is_empty() {
        error(format!(
            "Usage: /mcp {verb} <name>. No server here can log in: logins are for remote (http) servers without an Authorization header of their own."
        ))
    } else {
        error(format!(
            "Usage: /mcp {verb} <name>, one of {}",
            can.iter().map(|n| format!("`{n}`")).collect::<Vec<_>>().join(", ")
        ))
    }
}

fn mcp_text(ctx: &CommandContext) -> String {
    let Some((servers, problems)) = ctx.mcp else {
        return MCP_HELP.into();
    };
    if servers.is_empty() && problems.is_empty() {
        return MCP_HELP.into();
    }
    let plural = |n: usize| if n == 1 { "" } else { "s" };
    let mut lines: Vec<String> = servers
        .iter()
        .map(|s| {
            let who =
                format!("- **{}** ({}): ", s.name, if s.source == ServerSource::Project { "this project" } else { "yours" });
            match s.state {
                McpState::Connected => {
                    let tools = s
                        .tools
                        .iter()
                        .map(|t| format!("`{}`", t.replace(&format!("mcp__{}__", s.name), "")))
                        .collect::<Vec<_>>()
                        .join(", ");
                    let login = match &s.login {
                        Some(l) if l.logged_in => format!(
                            " · logged in{} (renewed automatically)",
                            l.host.as_ref().map(|h| format!(" at {h}")).unwrap_or_default()
                        ),
                        _ => String::new(),
                    };
                    let skipped = if s.skipped.is_empty() {
                        String::new()
                    } else {
                        format!(
                            "; not offered (another server's tool has the name): {}",
                            s.skipped.iter().map(|t| format!("`{t}`")).collect::<Vec<_>>().join(", ")
                        )
                    };
                    format!(
                        "{who}connected · {} tool{}{}{login}{skipped}",
                        s.tools.len(),
                        plural(s.tools.len()),
                        if tools.is_empty() { String::new() } else { format!(": {tools}") }
                    )
                }
                McpState::Untrusted => {
                    let reads = if s.reads.is_empty() {
                        String::new()
                    } else {
                        format!(
                            "; reads {} from your environment",
                            s.reads.iter().map(|v| format!("`${v}`")).collect::<Vec<_>>().join(", ")
                        )
                    };
                    let files = if s.runs_project_files.is_empty() {
                        String::new()
                    } else {
                        format!(
                            "; runs this project's {}",
                            s.runs_project_files.iter().map(|f| format!("`{f}`")).collect::<Vec<_>>().join(", ")
                        )
                    };
                    format!("{who}not trusted yet: `{}`{reads}{files} (`/mcp trust` to start it)", s.target)
                }
                McpState::Connecting => format!("{who}starting…"),
                McpState::NeedsLogin => format!("{who}needs login: `/mcp login {}`", s.name),
                McpState::Removed => format!("{who}removed (its tools stay listed, failing, until `/clear` or a restart)"),
                McpState::Failed => {
                    let stderr = s.stderr.as_ref().map(|e| {
                        let lines: Vec<&str> = e.lines().collect();
                        let tail = lines[lines.len().saturating_sub(5)..].join("\n  ");
                        format!("\n\n  ```\n  {}\n  ```", printable(&tail))
                    });
                    format!(
                        "{who}failed: {}{}",
                        printable(s.error.as_deref().unwrap_or("unknown error")),
                        stderr.unwrap_or_default()
                    )
                }
            }
        })
        .collect();
    if !problems.is_empty() {
        lines.push(String::new());
        lines.push("Couldn't load:".into());
        lines.extend(problems.iter().map(|p| format!("- {p}")));
    }
    lines.join("\n")
}

fn skills_text(skills: &[LoadedSkill], problems: &[String]) -> String {
    let mut parts = if skills.is_empty() {
        vec![
            "**No skills yet.** Add one as `.marv/skills/<name>/SKILL.md` in the project, or in `~/.marv/skills/` for all projects:".to_string(),
            "```markdown\n---\nname: <name>\ndescription: <what it does, and when to use it>\n---\n\n<instructions>\n```".to_string(),
        ]
    } else {
        vec![
            "**Skills**: run one with `/<name>`, or let Marv pick when a request matches.".to_string(),
            skills
                .iter()
                .map(|s| {
                    format!(
                        "- `/{}`: {}{}",
                        s.skill.name,
                        printable(&s.skill.description),
                        if s.source == SkillSource::Personal { " *(personal)*" } else { "" }
                    )
                })
                .collect::<Vec<_>>()
                .join("\n"),
        ]
    };
    if !problems.is_empty() {
        parts.push("**Couldn't load:**".into());
        parts.push(problems.iter().map(|p| format!("- {p}")).collect::<Vec<_>>().join("\n"));
    }
    parts.join("\n\n")
}

fn agents_text(agents: &[AgentType], problems: &[String]) -> String {
    let list = agents
        .iter()
        .map(|a| {
            let mut details = Vec::new();
            if a.tools.len() != SUBAGENT_TOOLS.len() {
                details.push(format!("tools: {}", a.tools.join(", ")));
            }
            if let Some(m) = &a.model {
                details.push(format!("model: {m}"));
            }
            let first = printable(a.description.lines().next().unwrap_or(""));
            let details = if details.is_empty() { String::new() } else { format!("\n  {}", details.join(" · ")) };
            format!("- `{}` *({})*: {first}{details}", a.name, a.source.label())
        })
        .collect::<Vec<_>>()
        .join("\n");
    let mut parts = vec!["**Agents**: Marv can hand a task to one of these. Add your own as `.marv/agents/<name>.md` (project) or `~/.marv/agents/<name>.md`.".to_string(), list];
    if !problems.is_empty() {
        parts.push("**Couldn't load:**".into());
        parts.push(problems.iter().map(|p| format!("- {p}")).collect::<Vec<_>>().join("\n"));
    }
    parts.join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Env, resolve_config};

    fn with<R>(f: impl FnOnce(&CommandContext) -> R) -> R {
        let config =
            resolve_config(None, &Env::from([("OPENROUTER_API_KEY".to_string(), "sk-or-v1-abcdefghijklmnop-wxyz".to_string())]));
        let ctx = CommandContext {
            config: &config,
            config_path: "~/.marv/config.json".into(),
            skills: &[],
            agents: &[],
            agent_problems: &[],
            skill_problems: &[],
            mcp: None,
            trajectories_path: None,
            usage: None,
            checks: None,
            checkpoints: None,
            hidden_path_folders: &[],
            sandbox_available: true,
            processes: None,
            mesh: None,
        };
        f(&ctx)
    }

    #[test]
    fn mesh_shows_the_wires_and_wake_and_turns_wake_on_and_off() {
        with(|ctx| {
            assert_eq!(run_command("/mesh wake on", ctx), CommandAction::MeshWake { on: true });
            assert_eq!(run_command("/mesh wake OFF", ctx), CommandAction::MeshWake { on: false });
            for bad in ["/mesh wake maybe", "/mesh frob", "/mesh wake on now"] {
                assert!(matches!(run_command(bad, ctx), CommandAction::Print { is_error: true, .. }), "{bad}");
            }
            let CommandAction::Print { text, is_error: false, .. } = run_command("/mesh", ctx) else { panic!() };
            assert!(text.contains("isn't on the mesh") && text.contains("**Wake**: off"), "{text}");
            let CommandAction::Print { text, .. } = run_command("/mesh wake", ctx) else { panic!() };
            assert!(text.starts_with("Mesh wake: off"), "{text}");
            let CommandAction::Print { text, .. } = run_command("/config", ctx) else { panic!() };
            assert!(text.contains("Mesh wake: off (/mesh)"), "{text}");

            let on = Config { mesh_wake: true, ..ctx.config.clone() };
            let status = MeshStatus {
                name: Some("beta".into()),
                wired: vec!["peer-alpha -> agent.parent.inbox".into(), "x\u{1b}[2J -> agent.parent.inbox".into()],
            };
            let shown = CommandContext { config: &on, mesh: Some(&status), ..ctx.clone() };
            let CommandAction::Print { text, .. } = run_command("/mesh", &shown) else { panic!() };
            assert!(text.contains("on the mesh as `beta`") && text.contains("- `peer-alpha -> agent.parent.inbox`"), "{text}");
            assert!(!text.contains('\u{1b}'), "printable: {text}");
            assert!(text.contains("**Wake**: on") && text.contains("at most 3 a minute per wire"), "{text}");
        });
    }

    #[test]
    fn processes_lists_the_sessions_background_processes_and_stop_is_an_action() {
        let now = marv_tools::process::now_ms();
        let web = marv_tools::ProcessInfo {
            name: "web".into(),
            command: "bun run dev".into(),
            network: false,
            state: marv_tools::ProcessState::Running,
            exit_code: None,
            ports: vec![5173],
            notes: vec![],
            started_at: now - 125_000,
        };
        let old = marv_tools::ProcessInfo {
            name: "old".into(),
            state: marv_tools::ProcessState::Exited,
            exit_code: Some(1),
            ports: vec![],
            ..web.clone()
        };
        with(|ctx| {
            let list = [web, old];
            let listed = CommandContext { processes: Some(&list), ..ctx.clone() };
            assert_eq!(
                run_command("/processes", &listed),
                print(
                    "web: running · http://localhost:5173 · up 2m · $ bun run dev\nold: exited (code 1) · up 2m · $ bun run dev"
                )
            );
            let none = CommandContext { processes: Some(&[]), ..ctx.clone() };
            assert!(
                matches!(run_command("/processes", &none), CommandAction::Print { text, .. } if text.contains("No background processes"))
            );
            assert!(
                matches!(run_command("/processes", ctx), CommandAction::Print { text, .. } if text.contains("need the bubblewrap sandbox"))
            );
            assert_eq!(run_command("/processes stop web", ctx), CommandAction::Processes { stop: "web".into() });
            for bad in ["/processes stop", "/processes kill web", "/processes stop a b"] {
                assert!(
                    matches!(run_command(bad, ctx), CommandAction::Print { is_error: true, ref text, .. } if text.contains("Usage")),
                    "{bad}"
                );
            }
        });
    }

    #[test]
    fn commands_map_to_actions() {
        with(|ctx| {
            assert!(is_command("/x") && !is_command("x"));
            assert_eq!(run_command("/CLEAR", ctx), CommandAction::Clear);
            assert_eq!(run_command("/model", ctx), CommandAction::Model { id: None });
            assert_eq!(run_command("/model a/b", ctx), CommandAction::Model { id: Some("a/b".into()) });
            assert_eq!(run_command("/think", ctx), CommandAction::Thinking { on: true });
            assert_eq!(run_command("/yolo off", ctx), CommandAction::Yolo { on: false });
            assert_eq!(
                run_command("/remember project: uses tabs", ctx),
                CommandAction::Remember { scope: MemoryTarget::Project, text: "uses tabs".into() }
            );
            assert_eq!(
                run_command("/label refactor, tests", ctx),
                CommandAction::Feedback { score: 0, note: None, labels: Some(vec!["refactor".into(), "tests".into()]) }
            );
            assert_eq!(
                run_command("/good  nice ", ctx),
                CommandAction::Feedback { score: 1, note: Some("nice".into()), labels: None }
            );
            assert!(matches!(run_command("/nope", ctx), CommandAction::Print { is_error: true, .. }));
        });
    }

    #[test]
    fn steps() {
        with(|ctx| {
            assert_eq!(run_command("/steps 40", ctx), CommandAction::Steps { every: 40 });
            assert_eq!(run_command("/steps OFF", ctx), CommandAction::Steps { every: 0 });
            assert!(matches!(run_command("/steps 1", ctx), CommandAction::Print { is_error: true, .. }));
            assert!(matches!(run_command("/steps 99999999999999", ctx), CommandAction::Print { is_error: true, .. }));
            assert!(matches!(run_command("/steps", ctx), CommandAction::Print { is_error: false, .. }));
        });
    }

    #[test]
    fn help_and_config() {
        with(|ctx| {
            let CommandAction::Print { text, .. } = run_command("/help", ctx) else { panic!() };
            assert!(commands().iter().all(|c| text.contains(&format!("/{}", c.name))));
            let CommandAction::Print { text, .. } = run_command("/config", ctx) else { panic!() };
            assert!(text.contains("sk-or-v1-a…wxyz (from OPENROUTER_API_KEY)"));
        });
    }

    #[test]
    fn undo_and_checkpoints_says_what_it_does_and_the_last_failure_or_turns_it_on_and_off() {
        with(|ctx| {
            assert_eq!(run_command("/undo", ctx), CommandAction::Undo);
            assert!(matches!(run_command("/undo 3", ctx), CommandAction::Print { is_error: true, .. }));
            assert_eq!(run_command("/checkpoints off", ctx), CommandAction::Checkpoints { on: false });
            assert_eq!(run_command("/checkpoints on", ctx), CommandAction::Checkpoints { on: true });
            let CommandAction::Print { text, is_error: false, .. } = run_command("/checkpoints", ctx) else { panic!() };
            assert!(text.contains("/undo"), "{text}");
            assert!(matches!(run_command("/checkpoints maybe", ctx), CommandAction::Print { is_error: true, .. }));
            let failed = CheckpointProblem { failure: Some("disk full".into()), off: None };
            let CommandAction::Print { text, .. } =
                run_command("/checkpoints", &CommandContext { checkpoints: Some(&failed), ..ctx.clone() })
            else {
                panic!()
            };
            assert!(text.contains("disk full"), "{text}");
            let off = Config { checkpoints: false, ..ctx.config.clone() };
            assert_eq!(
                run_command("/checkpoints", &CommandContext { config: &off, ..ctx.clone() }),
                CommandAction::Print { text: "Checkpoints: off".into(), is_error: false, markdown: false }
            );
            let CommandAction::Print { text, .. } = run_command("/config", ctx) else { panic!() };
            assert!(text.contains("Checkpoints: on (/checkpoints, /undo)"));
        });
    }

    #[test]
    fn mcp_subcommands() {
        with(|ctx| {
            assert_eq!(run_command("/mcp trust", ctx), CommandAction::McpTrust);
            assert_eq!(run_command("/mcp add pypi:x --name y", ctx), CommandAction::McpAdd { args: "pypi:x --name y".into() });
            assert_eq!(run_command("/mcp remove gh", ctx), CommandAction::McpRemove { name: "gh".into() });
            assert!(matches!(run_command("/mcp __proto__", ctx), CommandAction::Print { is_error: true, .. }));
            assert!(matches!(run_command("/mcp login gh", ctx), CommandAction::Print { is_error: true, .. }));
        });
    }
}
