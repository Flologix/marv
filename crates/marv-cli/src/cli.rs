//! The command line. Ports the argument handling of Marv's `src/cli.tsx` (and `bin/marv`).
//!
//! ```text
//! marv              start an interactive session
//! marv -c / -r      continue the latest session here / pick one to resume
//! marv -p <prompt>  one non-interactive turn (`-p -` reads stdin); `marv run <prompt>` is the same
//! marv mesh ls      the live Marvs on this machine (mesh v2)
//! marv mesh wire 'alpha -> beta:inbox'
//! marv mesh plug alpha --as claude   (an MCP server command: another agent plugs into alpha's switchboard)
//! ```
//!
//! Port notes:
//! - `bin/marv` existed only to rename the process from `bun` to `marv`; a native binary is already `marv`.
//! - Marv parsed argv by hand (last `-p` wins, `--flag value` anywhere); clap gives the same flags. **Argument errors
//!   exit 1** (Marv's code for a bad option), not clap's default 2, which `marv -p` uses for "stopped".
//! - `--version`/`-v` and `--help`/`-h` exit 0, as in Marv.
//! - `__html-convert` is hidden: the web reader re-executes `marv __html-convert` to convert a page in a process of
//!   its own (a conversion that crashes is an answer, not a crash).
//! - `__serve <folder> <port>` is hidden: the process tool's static file server (`marv_tools::serve`).
//! - `__guard <parent-pid> -- <command…>` is hidden: Seatbelt's process supervisor (#165, `marv_tools::guard`).

use clap::{ArgAction, Parser, Subcommand, ValueEnum};

pub const AFTER_HELP: &str = "Config:
  ~/.marv/config.json   created by the setup screen (/setup to change it)
  ~/.marv/sessions/     saved conversations, one folder per project
  ~/.marv/trajectories/ every turn, step by step, with your ratings (/trajectories)
  ~/.marv/agents/       your subagent types (a project's go in .marv/agents/)
  ~/.marv/mcp.json      your MCP servers (a project's go in .mcp.json; /mcp)
  ~/.marv/worktrees/    subagents' worktrees while they run
  OPENROUTER_API_KEY    overrides the saved OpenRouter key
  ZAI_API_KEY           overrides the saved Z.ai Coding Plan key
  KIMI_API_KEY          overrides the saved Kimi Code key
  DEEPSEEK_API_KEY      overrides the saved DeepSeek key
  QWEN_API_KEY          overrides the saved Qwen Coding Plan key
  OLLAMA_HOST           where Ollama runs (default localhost:11434)
  MARV_MODEL            overrides the saved model
  MARV_MAX_STEPS        overrides the saved step limit (0 or off: never ask)
  MARV_CONFIG_DIR       use a different config directory (read from the environment only)

Exit codes for -p: 0 finished, 1 error, 2 stopped (step limit, output limit, loop), 3 an approval it couldn't get";

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum OutputFormat {
    /// The final reply as text (default).
    Text,
    /// A result object as JSON.
    Json,
    /// Each event as a JSON line.
    StreamJson,
}

#[derive(Debug, Parser)]
#[command(
    name = "marv",
    about = "Marv: a terminal coding agent",
    version,
    disable_version_flag = true,
    after_help = AFTER_HELP
)]
pub struct Cli {
    /// Print the version.
    #[arg(short = 'v', long = "version", action = ArgAction::Version)]
    pub version: Option<bool>,

    /// Run one non-interactive turn and print the reply (`-` reads the prompt from stdin).
    #[arg(short = 'p', long = "prompt", value_name = "PROMPT", allow_hyphen_values = true)]
    pub prompt: Option<String>,

    /// How -p prints its result.
    #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
    pub output: OutputFormat,

    /// Auto-approve a scope for this session (git_write, network, web, mcp, files, memory, agent_worktree, process,
    /// github, mesh:<receptacle>) — like answering "always" at the start. Works in -p and the TUI; the config's
    /// "approve" list is the same thing.
    #[arg(long = "approve", value_name = "SCOPE")]
    pub approve: Vec<String>,

    /// With --approve, in -p: also approve egress (web, network, MCP, mesh, GitHub) after the session read
    /// untrusted content. Without it, those calls are refused.
    #[arg(long = "approve-tainted")]
    pub approve_tainted: bool,

    /// With --approve files, in -p: also approve writes to protected paths (.git, .env*, AGENTS.md, .marv/…).
    /// Without it, those writes are refused.
    #[arg(long = "approve-protected")]
    pub approve_protected: bool,

    /// In -p: use this project's instruction files (AGENTS.md, CLAUDE.md), skills and agent types as yours for this
    /// run, as if you'd trusted them (/trust in the TUI). Without it, they count only where you trusted exactly these
    /// files; otherwise the model reads the instruction files as the project's data, and its skills and agent types
    /// aren't offered.
    #[arg(long = "trust-project")]
    pub trust_project: bool,

    /// Steps before the turn stops (0 never stops; default: your config's maxSteps).
    #[arg(long = "max-steps", value_name = "N")]
    pub max_steps: Option<u32>,

    /// Use a different model.
    #[arg(short = 'm', long)]
    pub model: Option<String>,

    /// Continue the latest session in this folder.
    #[arg(short = 'c', long = "continue")]
    pub continue_: bool,

    /// Pick an earlier session to resume.
    #[arg(short = 'r', long)]
    pub resume: bool,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Alias for -p: run one non-interactive turn (no prompt: read stdin).
    Run {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        prompt: Vec<String>,
    },
    /// The mesh: wires between sandboxed agents.
    #[command(subcommand)]
    Mesh(MeshCommand),
    /// Converts one web page: `{html, url}` JSON on stdin, the result on stdout (the web reader's own process).
    #[command(name = "__html-convert", hide = true)]
    HtmlConvert,
    /// Runs the project's checks in the current folder (inside the sandbox): `marv_session::checks::runner`.
    #[command(name = "__run-checks", hide = true)]
    RunChecks { spec: String },
    /// The process door's relay, inside a sandbox (Marv #51): `marv_tools::relay`.
    #[command(name = "__relay", hide = true)]
    Relay { spec: String },
    /// The process tool's static file server for a project folder, inside a sandbox: `marv_tools::serve`.
    #[command(name = marv_tools::serve::SERVE_SUBCOMMAND, hide = true)]
    Serve { folder: String, port: u16 },
    /// Runs a command in a session of its own and kills its process group when `parent` dies (#165): `guard`.
    #[command(name = marv_sandbox::seatbelt_backend::GUARD_SUBCOMMAND, hide = true)]
    Guard {
        parent: u32,
        #[arg(last = true, required = true)]
        command: Vec<std::ffi::OsString>,
    },
}

#[derive(Debug, Subcommand)]
pub enum MeshCommand {
    /// List the live Marv switchboards on this machine.
    Ls,
    /// Ask for a wire, e.g. `marv mesh wire 'alpha -> beta:inbox'`; each Marv's user approves it.
    Wire {
        /// `<marv>[/<agent>] -> <marv>[/<agent>]:inbox`, or `<marv>[/<agent>] -> <receptacle>[:<mode>]`
        #[arg(num_args = 1.., trailing_var_arg = true)]
        spec: Vec<String>,
    },
    /// Plug another agent into a Marv's switchboard over stdin/stdout (use it as an MCP server command).
    Plug {
        /// The Marv's name (`marv mesh ls`).
        session: String,
        /// The plug's name (default `ext-<pid>`).
        #[arg(long = "as", value_name = "NAME")]
        name: Option<String>,
    },
}

/// What the process should do, resolved from the arguments.
#[derive(Debug, PartialEq, Eq)]
pub enum Mode {
    /// The TUI. `resume`: `Some(true)` for -c (latest), `Some(false)` for -r (pick). `approve`: the `--approve`
    /// scopes (#98), which seed "always" like the config's list.
    Interactive {
        resume: Option<bool>,
        approve: Vec<String>,
    },
    /// `-p`/`run`. `prompt` is `-` for stdin.
    Headless {
        prompt: String,
    },
    MeshLs,
    MeshWire {
        spec: String,
    },
    MeshPlug {
        session: String,
        name: Option<String>,
    },
    HtmlConvert,
    RunChecks {
        spec: String,
    },
    Relay {
        spec: String,
    },
    Serve {
        folder: String,
        port: u16,
    },
    Guard {
        parent: u32,
        command: Vec<std::ffi::OsString>,
    },
}

impl Cli {
    /// `marv run <prompt>` is `marv -p <prompt>` (no prompt: stdin).
    pub fn mode(&self) -> Mode {
        match &self.command {
            Some(Command::Run { prompt }) => {
                Mode::Headless { prompt: if prompt.is_empty() { "-".into() } else { prompt.join(" ") } }
            }
            Some(Command::Mesh(MeshCommand::Ls)) => Mode::MeshLs,
            Some(Command::Mesh(MeshCommand::Wire { spec })) => Mode::MeshWire { spec: spec.join(" ") },
            Some(Command::Mesh(MeshCommand::Plug { session, name })) => {
                Mode::MeshPlug { session: session.clone(), name: name.clone() }
            }
            Some(Command::HtmlConvert) => Mode::HtmlConvert,
            Some(Command::RunChecks { spec }) => Mode::RunChecks { spec: spec.clone() },
            Some(Command::Relay { spec }) => Mode::Relay { spec: spec.clone() },
            Some(Command::Serve { folder, port }) => Mode::Serve { folder: folder.clone(), port: *port },
            Some(Command::Guard { parent, command }) => Mode::Guard { parent: *parent, command: command.clone() },
            None => match &self.prompt {
                Some(prompt) => Mode::Headless { prompt: prompt.clone() },
                None => Mode::Interactive {
                    resume: if self.continue_ {
                        Some(true)
                    } else if self.resume {
                        Some(false)
                    } else {
                        None
                    },
                    approve: self.approve.clone(),
                },
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Cli {
        Cli::try_parse_from(std::iter::once("marv").chain(args.iter().copied())).unwrap()
    }

    #[test]
    fn modes() {
        assert_eq!(parse(&[]).mode(), Mode::Interactive { resume: None, approve: vec![] });
        assert_eq!(parse(&["-c"]).mode(), Mode::Interactive { resume: Some(true), approve: vec![] });
        assert_eq!(parse(&["-r"]).mode(), Mode::Interactive { resume: Some(false), approve: vec![] });
        // #98: --approve is accepted in the TUI too, not only -p.
        assert_eq!(
            parse(&["--approve", "web", "-c"]).mode(),
            Mode::Interactive { resume: Some(true), approve: vec!["web".into()] }
        );
        assert_eq!(parse(&["-p", "fix it"]).mode(), Mode::Headless { prompt: "fix it".into() });
        assert_eq!(parse(&["-p", "-"]).mode(), Mode::Headless { prompt: "-".into() });
        assert_eq!(parse(&["run", "fix", "it"]).mode(), Mode::Headless { prompt: "fix it".into() });
        assert_eq!(parse(&["run"]).mode(), Mode::Headless { prompt: "-".into() });
        assert_eq!(
            parse(&["mesh", "wire", "alpha", "->", "beta:inbox"]).mode(),
            Mode::MeshWire { spec: "alpha -> beta:inbox".into() }
        );
        assert_eq!(
            parse(&["mesh", "plug", "alpha", "--as", "claude"]).mode(),
            Mode::MeshPlug { session: "alpha".into(), name: Some("claude".into()) }
        );
        assert_eq!(parse(&["__html-convert"]).mode(), Mode::HtmlConvert);
        assert_eq!(parse(&["__run-checks", "{}"]).mode(), Mode::RunChecks { spec: "{}".into() });
        assert_eq!(parse(&["__relay", "{}"]).mode(), Mode::Relay { spec: "{}".into() });
        assert_eq!(parse(&["__serve", "dist", "4173"]).mode(), Mode::Serve { folder: "dist".into(), port: 4173 });
        assert_eq!(
            parse(&["__guard", "42", "--", "bash", "-c", "--x"]).mode(),
            Mode::Guard { parent: 42, command: vec!["bash".into(), "-c".into(), "--x".into()] }
        );
    }

    /// #124: the radio is the user's own config (or `/radio on`) only: no flag turns it on, so nothing that starts Marv
    /// for them (a script, a project's tooling) can.
    #[test]
    fn no_flag_turns_the_radio_on() {
        for args in [&["--radio"][..], &["--radio", "on"], &["--mesh-radio"], &["-p", "x", "--radio"]] {
            assert!(Cli::try_parse_from(std::iter::once("marv").chain(args.iter().copied())).is_err(), "{args:?}");
        }
    }

    #[test]
    fn headless_flags() {
        let cli = parse(&[
            "-p",
            "x",
            "--output",
            "stream-json",
            "--approve",
            "git_write",
            "--approve",
            "mesh:agent.parent.inbox",
            "--max-steps",
            "0",
            "-m",
            "m",
            "-c",
        ]);
        assert_eq!(cli.output, OutputFormat::StreamJson);
        assert_eq!(cli.approve, vec!["git_write", "mesh:agent.parent.inbox"]);
        assert!(!cli.approve_tainted && !cli.approve_protected);
        let opted = parse(&["-p", "x", "--approve", "web", "--approve-tainted", "--approve-protected"]);
        assert!(opted.approve_tainted && opted.approve_protected);
        assert!(!opted.trust_project && parse(&["-p", "x", "--trust-project"]).trust_project);
        assert_eq!((cli.max_steps, cli.model.as_deref(), cli.continue_), (Some(0), Some("m"), true));
    }

    #[test]
    fn bad_values_are_errors() {
        let bad = |args: &[&str]| Cli::try_parse_from(std::iter::once("marv").chain(args.iter().copied())).is_err();
        assert!(bad(&["-p", "x", "--output", "yaml"]));
        assert!(bad(&["-p", "x", "--max-steps", "-1"]));
        assert!(bad(&["-p", "x", "--max-steps", "2.5"]));
        assert!(bad(&["-p"]));
        // The guard's command comes after `--` only, and there must be one.
        assert!(bad(&["__guard", "42"]));
        assert!(bad(&["__guard", "42", "--"]));
        assert!(bad(&["__guard", "42", "bash"]));
        assert!(bad(&["__guard", "x", "--", "bash"]));
    }

    #[test]
    fn the_command_line_is_well_formed() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }

    // The flag's help names the scope names by hand; it must not drift from the real list (#98).
    #[test]
    fn the_help_names_every_approve_scope() {
        let help: String = {
            use clap::CommandFactory as _;
            Cli::command()
                .get_arguments()
                .find(|a| a.get_long() == Some("approve"))
                .map(|a| a.get_long_help().or_else(|| a.get_help()).map(|h| h.to_string()).unwrap_or_default())
                .unwrap_or_default()
        };
        for (name, _) in marv_session::approvals::APPROVE_PREFIXES {
            assert!(help.contains(name), "the --approve help doesn't name \"{name}\": {help}");
        }
        assert!(help.contains("TUI"), "the help should say --approve works in the TUI too: {help}");
    }
}
