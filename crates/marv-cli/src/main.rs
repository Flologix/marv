//! The `marv` binary. Ports Marv's `src/cli.tsx` (dispatch), `src/headless.ts` and `bin/marv`.
//!
//! - no arguments, `-c`, `-r`: the TUI ([`tui`]);
//! - `-p <prompt>` / `run <prompt>`: one headless turn ([`headless`]), exit codes 0/1/2/3;
//! - `mesh ls|wire|plug`: the mesh ([`mesh_cmd`]);
//! - `__html-convert` (hidden): the web reader's converter process (`marv_tools::web::html_convert_cli::main`);
//! - `__run-checks <spec>` (hidden): the project checks' runner, inside the sandbox (`marv_session::checks::runner`).
//!
//! Both hidden ones run before the Tokio runtime starts: they're short processes of their own.

mod cli;
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
    let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("marv: couldn't start: {e}");
            return ExitCode::from(1);
        }
    };
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
                };
                headless::run_headless(options).await as i32
            }
            Mode::MeshLs => mesh_cmd::ls().await,
            Mode::MeshWire { spec } => mesh_cmd::wire(&spec).await,
            Mode::MeshPlug { session, name } => mesh_cmd::plug(&session, name.as_deref()).await,
            Mode::Interactive { resume } => match tui::run(resume).await {
                Ok(code) => code,
                Err(e) => {
                    eprintln!("marv: {}", marv_core::printable(&e.to_string()));
                    1
                }
            },
            Mode::HtmlConvert | Mode::RunChecks { .. } => unreachable!("handled before the runtime starts"),
        }
    });
    // An HTML converter still running (a turn ended mid-fetch) would outlive Marv: stop it, as Marv's exit handler did.
    marv_tools::web::convert::kill_running();
    ExitCode::from(u8::try_from(code).unwrap_or(1))
}
