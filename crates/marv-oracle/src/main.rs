//! `marv-oracle`: run the oracle's scenarios from the command line.
//!
//! ```sh
//! marv-oracle check  [--bin <marv>] [--target marv|marv-rs] [scenario…]   # diff (exit 1 on any difference)
//! marv-oracle record [--bin <marv>] [--target marv|marv-rs] [scenario…]   # save each run as its expect/
//! marv-oracle tracker <data.json> <out.json>      # the tracker MCP server (stdio), for the MCP scenarios
//! ```
//!
//! The binary defaults to `MARV_ORACLE_BIN`, then this workspace's `target/debug/marv`; scenarios to
//! all of `tests/oracle/scenarios`; the target to `MARV_ORACLE_TARGET`, then marv-rs for `check` and
//! TypeScript Marv for `record` (marv-rs records only `expect.marv-rs/`, for declared differences).

use std::path::PathBuf;
use std::process::ExitCode;

use marv_oracle::scenario::Target;
use marv_oracle::{Verdict, binary, check_all, report, scenario};

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let record = match args.next().as_deref() {
        Some("check") => false,
        Some("record") => true,
        Some("tracker") => {
            let (Some(data), Some(out)) = (args.next(), args.next()) else {
                eprintln!("usage: marv-oracle tracker <data.json> <out.json>");
                return ExitCode::FAILURE;
            };
            return match marv_oracle::tracker::serve(&PathBuf::from(data), &PathBuf::from(out)) {
                Ok(()) => ExitCode::SUCCESS,
                Err(err) => {
                    eprintln!("tracker: {err}");
                    ExitCode::FAILURE
                }
            };
        }
        _ => {
            eprintln!("usage: marv-oracle check|record [--bin <marv>] [scenario…] | tracker <data.json> <out.json>");
            return ExitCode::FAILURE;
        }
    };
    let mut bin = binary();
    let mut target = match Target::from_env(record) {
        Ok(target) => target,
        Err(err) => {
            eprintln!("{err}");
            return ExitCode::FAILURE;
        }
    };
    let mut names = Vec::new();
    while let Some(arg) = args.next() {
        if arg == "--target" {
            let Some(t) = args.next().as_deref().and_then(Target::parse) else {
                eprintln!("--target needs marv or marv-rs");
                return ExitCode::FAILURE;
            };
            target = t;
        } else if arg == "--bin" {
            let Some(path) = args.next() else {
                eprintln!("--bin needs a path");
                return ExitCode::FAILURE;
            };
            bin = PathBuf::from(path);
        } else {
            names.push(arg);
        }
    }
    let all = match scenario::load_all(&scenario::default_root()) {
        Ok(all) => all,
        Err(err) => {
            eprintln!("{err}");
            return ExitCode::FAILURE;
        }
    };
    if let Some(unknown) = names.iter().find(|n| !all.iter().any(|s| &&s.name == n)) {
        eprintln!("no scenario {unknown:?}");
        return ExitCode::FAILURE;
    }
    let chosen: Vec<_> = all.into_iter().filter(|s| names.is_empty() || names.contains(&s.name)).collect();
    let results = check_all(&chosen, &bin, record, target);
    let mut ok = true;
    for (name, verdict) in &results {
        print!("{}", report(name, verdict));
        ok &= matches!(verdict, Verdict::Pass | Verdict::Recorded);
    }
    if ok { ExitCode::SUCCESS } else { ExitCode::FAILURE }
}
