//! The `marv` binary. Ports Marv's `src/cli.tsx` (dispatch), `src/headless.ts` and `bin/marv`.
//!
//! - no arguments, `-c`, `-r`: the TUI ([`tui`]);
//! - `-p <prompt>` / `run <prompt>`: one headless turn ([`headless`]), exit codes 0/1/2/3;
//! - `mesh ls|wire|plug`: the mesh ([`mesh_cmd`]);
//! - `__html-convert` (hidden): the web reader's converter process (`marv_tools::web::html_convert_cli::main`);
//! - `__run-checks <spec>` (hidden): the project checks' runner, inside the sandbox (`marv_session::checks::runner`);
//! - `__relay <spec>` (hidden): the process door's relay, inside a sandbox (`marv_tools::relay`, Marv #51);
//! - `__guard <parent-pid> -- <command…>` (hidden): runs a sandboxed command in a session of its own and kills its
//!   process group when Marv dies, for a backend without a PID namespace (`marv_tools::guard`, #165).
//!
//! The hidden ones run before the Tokio runtime starts: they're processes of their own.
//!
//! Background processes (Marv #51) never outlive Marv: the TUI and `-p` stop them on the way out (SIGTERM, then
//! SIGKILL), and whatever is left is SIGKILLed when `main` ends, a panic included
//! (`marv_session::processes::kill_running`); if Marv is killed harder, bwrap's `--die-with-parent` takes them.
//!
//! SIGHUP (the terminal closed) and SIGTERM end the TUI and `-p` as `/exit` would, within a deadline ([`signals`],
//! #196); the exit code is then 128 + the signal's number.

mod cli;
mod headless;
mod mesh_cmd;
mod signals;
mod tui;

use std::io::{Read, Write};
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
    let mut mode = cli.mode();
    // Read before SIGTERM is caught (#196), which a blocking read here would hold off.
    if let Mode::Headless { prompt } = &mut mode
        && prompt == "-"
    {
        let mut text = String::new();
        if std::io::stdin().read_to_string(&mut text).is_err() {
            eprintln!("Couldn't read the prompt from stdin.");
            return ExitCode::from(1);
        }
        *prompt = text;
    }
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
        return marv_tools::guard::main(*parent, command);
    }
    let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("marv: couldn't start: {e}");
            return ExitCode::from(1);
        }
    };
    // Dropped last thing before the runtime, or while a panic unwinds out of it.
    let processes = StopProcesses;
    let (code, signalled) = runtime.block_on(async move {
        // The TUI and -p leave on SIGHUP or SIGTERM as on /exit (#196); the mesh commands keep the default (they end).
        let quit = matches!(mode, Mode::Headless { .. } | Mode::Interactive { .. }).then(signals::Quit::listen);
        let work = run_mode(cli, mode, quit.clone().unwrap_or_default());
        let Some(quit) = quit else { return (work.await, false) };
        tokio::select! {
            code = work => (quit.exit_code().unwrap_or(code), quit.signal().is_some()),
            // The way out took too long: it's dropped, and the runtime's shutdown takes what's left.
            () = quit.expired() => (quit.exit_code().unwrap_or(1), true),
        }
    });
    // An HTML converter still running (a turn ended mid-fetch) would outlive Marv: stop it, as Marv's exit handler did.
    marv_tools::web::convert::kill_running();
    if signalled {
        drop(processes);
        // Dropping the runtime waits for every blocking task: one stuck writing to a terminal that's gone would hold
        // Marv forever. Its tasks are dropped either way (a checkpoint's git with them, its process group killed).
        runtime.shutdown_timeout(signals::RUNTIME_SHUTDOWN);
    }
    ExitCode::from(u8::try_from(code).unwrap_or(1))
}

/// Runs a mode that needs the runtime; `quit` is SIGHUP or SIGTERM (#196), for the TUI and `-p`.
async fn run_mode(cli: Cli, mode: Mode, quit: signals::Quit) -> i32 {
    match mode {
        Mode::Headless { prompt } => {
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
                trust_project: cli.trust_project,
            };
            headless::run_headless(options, quit).await as i32
        }
        Mode::MeshLs => mesh_cmd::ls().await,
        Mode::MeshWire { spec } => mesh_cmd::wire(&spec).await,
        Mode::MeshPlug { session, name } => mesh_cmd::plug(&session, name.as_deref()).await,
        Mode::Interactive { resume, approve } => match tui::run(resume, approve, quit).await {
            Ok(code) => code,
            Err(e) => {
                // Not eprintln!, which panics when stderr is a terminal that's gone (#196: it closed).
                let _ = writeln!(std::io::stderr(), "marv: {}", marv_core::printable(&e.to_string()));
                1
            }
        },
        Mode::HtmlConvert | Mode::RunChecks { .. } | Mode::Relay { .. } | Mode::Guard { .. } => {
            unreachable!("handled before the runtime starts")
        }
    }
}

/// SIGKILLs the background processes still running when it's dropped: at the end of `main`, or on a panic.
struct StopProcesses;

impl Drop for StopProcesses {
    fn drop(&mut self) {
        marv_session::processes::kill_running();
    }
}
