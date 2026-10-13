//! The oracle's scenarios, run through a `marv` binary (`MARV_ORACLE_BIN`, default this workspace's
//! `target/debug/marv`). Ignored by default: it needs a working `marv -p`.
//!
//! ```sh
//! MARV_ORACLE_BIN=~/Projects/marv/bin/marv cargo test -p marv-oracle -- --ignored --nocapture
//! MARV_ORACLE_RECORD=1 MARV_ORACLE_BIN=~/Projects/marv/bin/marv cargo test -p marv-oracle -- --ignored   # re-record
//! ```

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
        marv_oracle::compare::load_expect(&s.expect()).unwrap_or_else(|err| panic!("{}: {err}", s.name));
    }
}

#[test]
#[ignore = "runs a marv binary: set MARV_ORACLE_BIN"]
fn oracle() {
    let bin = binary();
    assert!(bin.exists(), "no binary at {} (build marv, or set MARV_ORACLE_BIN)", bin.display());
    let record = std::env::var_os("MARV_ORACLE_RECORD").is_some();
    let results = check_all(&scenarios(), &bin, record);
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
    let verdict = marv_oracle::check(&scenario, &bin, false);
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
