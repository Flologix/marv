//! The oracle's scenarios, run through a `marv` binary (`MARV_ORACLE_BIN`, default this workspace's
//! `target/debug/marv`). Ignored by default: it needs a working `marv -p`.
//!
//! ```sh
//! MARV_ORACLE_BIN=~/Projects/marv/bin/marv cargo test -p marv-oracle -- --ignored --nocapture
//! MARV_ORACLE_RECORD=1 MARV_ORACLE_BIN=~/Projects/marv/bin/marv cargo test -p marv-oracle -- --ignored   # re-record
//! ```
//!
//! `MARV_ORACLE_TARGET` (`marv` or `marv-rs`) says which one the binary is: by default checking means
//! marv-rs and recording TypeScript Marv (see `marv_oracle::scenario::Target`).

use marv_oracle::scenario::Target;
use marv_oracle::{Verdict, binary, check_all, report, scenario};

fn scenarios() -> Vec<scenario::Scenario> {
    scenario::load_all(&scenario::default_root()).expect("the scenarios load")
}

/// Every scenario parses and has a complete recording, so `--ignored` has something to check.
#[test]
fn every_scenario_is_well_formed_and_recorded() {
    let all = scenarios();
    assert!(all.len() >= 10, "only {} scenarios", all.len());
    for s in &all {
        assert!(s.project().is_dir(), "{}: no project/", s.name);
        assert!(!s.replies.is_empty(), "{}: empty script", s.name);
        // A deliberate marv-rs difference has its own complete recording, checked the same way.
        let expects = [s.expect(), s.expect_for(Target::MarvRs)];
        let expects = if expects[0] == expects[1] { &expects[..1] } else { &expects[..] };
        for dir in expects {
            let expect = marv_oracle::compare::load_expect(dir).unwrap_or_else(|err| panic!("{}: {err}", s.name));
            // The recordings keep the prefix rule too (canonical JSON keeps each message's bytes stable
            // from one request to the next, as long as the sender did).
            let bodies: Vec<String> = expect.requests.lines().map(String::from).collect();
            assert_eq!(marv_oracle::prefix::prefix_breaks(&bodies, &s.run.prefix_resets), [], "{}", s.name);
        }
        if let Some(d) = &s.run.deliberate {
            assert!(
                d.well_formed(),
                "{}: a deliberate difference needs one reason (an issue or a SECURITY.md section) and why",
                s.name
            );
        }
        // A .gitignore in this repository would apply to marv-rs itself.
        for dir in [s.project(), s.expect(), s.expect_for(Target::MarvRs)] {
            if !dir.exists() {
                continue;
            }
            assert!(
                !marv_oracle::runner::read_tree(&dir, false).unwrap().keys().any(|k| k.ends_with(".gitignore")),
                "{}: a .gitignore in {}",
                s.name,
                dir.display()
            );
        }
    }
}

#[test]
#[ignore = "runs a marv binary: set MARV_ORACLE_BIN"]
fn oracle() {
    let bin = binary();
    assert!(bin.exists(), "no binary at {} (build marv, or set MARV_ORACLE_BIN)", bin.display());
    let record = std::env::var_os("MARV_ORACLE_RECORD").is_some();
    let target = Target::from_env(record).unwrap();
    let results = check_all(&scenarios(), &bin, record, target);
    let mut failed = Vec::new();
    for (name, verdict) in &results {
        print!("{}", report(name, verdict));
        if !matches!(verdict, Verdict::Pass | Verdict::Recorded) {
            failed.push(name.as_str());
        }
    }
    assert!(failed.is_empty(), "{} of {} scenarios differ from expect/: {}", failed.len(), results.len(), failed.join(", "));
}

/// A binary that does the wrong thing fails, with a diff of each part and a note on the unused script.
#[test]
fn a_wrong_binary_fails_with_a_diff() {
    let dir = std::env::temp_dir().join(format!("marv-oracle-wrong-bin-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let bin = dir.join("marv");
    // Answers without asking the model, and leaves a file behind.
    std::fs::write(&bin, "#!/bin/sh\necho '{\"type\":\"done\",\"reason\":\"end\"}'\necho stray > stray.txt\nexit 0\n").unwrap();
    std::fs::set_permissions(&bin, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let scenario = scenarios().into_iter().find(|s| s.name == "step-limit").unwrap();
    let verdict = marv_oracle::check(&scenario, &bin, false, Target::MarvRs);
    std::fs::remove_dir_all(&dir).unwrap();
    let Verdict::Fail { mismatches, notes, .. } = &verdict else { panic!("expected a failure, got {verdict:?}") };
    let parts: Vec<&str> = mismatches.iter().map(|m| m.part.as_str()).collect();
    assert_eq!(parts, ["exit_code", "events.jsonl", "requests.jsonl", "files/stray.txt"]);
    assert!(notes.iter().any(|n| n == "3 scripted replies were never asked for"), "{notes:?}");
    let text = report(&scenario.name, &verdict);
    assert!(
        text.contains("FAIL     step-limit")
            && text.contains("expected 2, got 0")
            && text.contains("+{\"reason\":\"end\",\"type\":\"done\"}"),
        "{text}"
    );
}
