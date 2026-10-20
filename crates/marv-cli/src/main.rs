//! The `marv` binary. Ports Marv's `src/cli.tsx` (dispatch), `src/headless.ts` and `bin/marv`.
//!
//! - no arguments, `-c`, `-r`: the TUI ([`tui`]);
//! - `-p <prompt>` / `run <prompt>`: one headless turn ([`headless`]), exit codes 0/1/2/3;
//! - `mesh ls|wire|plug`: the mesh ([`mesh_cmd`]);
//! - `__html-convert` (hidden): the web reader's converter process (`marv_tools::web::html_convert_cli::main`);
//! - `__run-checks <spec>` (hidden): the project checks' runner, inside the sandbox (`marv_session::checks::runner`);
//! - `__relay <spec>` (hidden): the process door's relay, inside a sandbox (`marv_tools::relay`, Marv #51);
//! - `__guard <parent-pid> -- <command…>` (hidden): runs a sandboxed command in a session of its own and kills its
//!   process group when Marv dies, for a backend without a PID namespace ([`guard`], #165).
//!
//! The hidden ones run before the Tokio runtime starts: they're processes of their own.
//!
//! Background processes (Marv #51) never outlive Marv: the TUI and `-p` stop them on the way out (SIGTERM, then
//! SIGKILL), and whatever is left is SIGKILLed when `main` ends, a panic included
//! (`marv_session::processes::kill_running`); if Marv is killed harder, bwrap's `--die-with-parent` takes them.

mod cli;
mod guard;
mod headless;
mod mesh_cmd;
mod tui;

use std::io::Read;
use std::process::ExitCode;

use clap::Parser;
use clap::error::ErrorKind;

use crate::cli::{Cli, Mode};

fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(err) => {
            let _ = err.print();
            // Help and version are answers (exit 0); a bad argument is an error (exit 1, as Marv's), never clap's 2,
            // which `marv -p` uses for "stopped".
            return match err.kind() {
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion | ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand => {
                    ExitCode::SUCCESS
                }
                _ => ExitCode::from(1),
            };
        }
    };
    let mode = cli.mode();
    if mode == Mode::HtmlConvert {
        return match marv_tools::web::html_convert_cli::main() {
            Ok(()) => ExitCode::SUCCESS,
            Err(_) => ExitCode::from(1),
        };
    }
    if let Mode::RunChecks { spec } = &mode {
        return marv_session::checks::runner::main(spec);
    }
    if let Mode::Relay { spec } = &mode {
        return marv_tools::relay::main(spec);
    }
    if let Mode::Guard { parent, command } = &mode {
        return guard::main(*parent, command);
    }
    let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("marv: couldn't start: {e}");
            return ExitCode::from(1);
        }
    };
    // Dropped last thing before the runtime, or while a panic unwinds out of it.
    let _processes = StopProcesses;
    let code = runtime.block_on(async move {
        match mode {
            Mode::Headless { prompt } => {
                let prompt = if prompt == "-" {
                    let mut text = String::new();
                    if std::io::stdin().read_to_string(&mut text).is_err() {
                        eprintln!("Couldn't read the prompt from stdin.");
                        return 1;
                    }
                    text
                } else {
                    prompt
                };
                if prompt.is_empty() {
                    eprintln!("No prompt given. Usage: marv -p \"<prompt>\" or marv run <prompt>");
                    return 1;
                }
                let options = headless::HeadlessOptions {
                    prompt,
                    output: cli.output,
                    resume_latest: cli.continue_,
                    model: cli.model,
                    max_steps: cli.max_steps,
                    approve: cli.approve,
                    must_ask: headless::MustAsk { tainted: cli.approve_tainted, protected: cli.approve_protected },
                };
                headless::run_headless(options).await as i32
            }
            Mode::MeshLs => mesh_cmd::ls().await,
            Mode::MeshWire { spec } => mesh_cmd::wire(&spec).await,
            Mode::MeshPlug { session, name } => mesh_cmd::plug(&session, name.as_deref()).await,
            Mode::Interactive { resume, approve } => match tui::run(resume, approve).await {
                Ok(code) => code,
                Err(e) => {
                    eprintln!("marv: {}", marv_core::printable(&e.to_string()));
                    1
                }
            },
            Mode::HtmlConvert | Mode::RunChecks { .. } | Mode::Relay { .. } | Mode::Guard { .. } => {
                unreachable!("handled before the runtime starts")
            }
        }
    });
    // An HTML converter still running (a turn ended mid-fetch) would outlive Marv: stop it, as Marv's exit handler did.
    marv_tools::web::convert::kill_running();
    ExitCode::from(u8::try_from(code).unwrap_or(1))
}

/// SIGKILLs the background processes still running when it's dropped: at the end of `main`, or on a panic.
struct StopProcesses;

impl Drop for StopProcesses {
    fn drop(&mut self) {
        marv_session::processes::kill_running();
    }
}
