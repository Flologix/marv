//! The built-in tools as a contract: what the model and saved sessions depend on can't change by accident.
//!
//! - `docs/TOOLS.md` is generated from the code (`marv_tools::table`) and must match it.
//! - The built-in specs' bytes (names, order, descriptions, schemas) match `golden/tool_specs.jsonl`: the prompt
//!   cache and saved sessions depend on them.
//! - A tool name is never reused: every name ever offered is a tool today or has a tombstone.
//!
//! A golden that differs fails with the command that writes it again:
//! `UPDATE_GOLDEN=1 cargo test -p marv-guards --test tools`. Review the diff before committing it.

use std::path::{Path, PathBuf};

/// The repository's root (this crate is `tests/guards`).
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().expect("the repository root")
}

/// Compares `actual` with the checked-in `path` (relative to the repository root), or writes it with
/// `UPDATE_GOLDEN=1`.
fn golden(path: &str, actual: &str) {
    let file = repo_root().join(path);
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        if let Some(dir) = file.parent() {
            std::fs::create_dir_all(dir).expect("the golden's folder");
        }
        std::fs::write(&file, actual).expect("write the golden");
        return;
    }
    let expected = std::fs::read_to_string(&file).unwrap_or_default();
    if expected != actual {
        let line = expected
            .lines()
            .zip(actual.lines())
            .position(|(a, b)| a != b)
            .unwrap_or_else(|| expected.lines().count().min(actual.lines().count()));
        panic!(
            "{path} is out of date (first difference at line {}). If the change is deliberate, write it again with\n\n    \
             UPDATE_GOLDEN=1 cargo test -p marv-guards --test tools\n\nand review the diff.\n\nexpected: {:?}\n  actual: {:?}",
            line + 1,
            expected.lines().nth(line).unwrap_or(""),
            actual.lines().nth(line).unwrap_or(""),
        );
    }
}

/// **The tools table is the code, printed.** `docs/TOOLS.md` is generated from the `Tool` trait of every built-in
/// tool, so a change to a tool's kind, source, approval, network or persistence shows up in review as a table diff.
///
/// Proven: changed `Glob::kind` to `Write` → this failed (line of `glob`).
#[test]
fn tools_md_matches_the_code() {
    golden("docs/TOOLS.md", &marv_tools::table::tools_table());
}

/// The built-in specs exactly as the model gets them (`tool_specs`, every switch on): one compact JSON line each,
/// in the order sent. The bytes the prompt cache keys on.
fn spec_bytes() -> String {
    marv_tools::tool_specs().iter().map(|s| serde_json::to_string(s).expect("a spec") + "\n").collect()
}

/// **The specs the model gets change only on purpose.** A tool's name, order, description or schema is what the
/// model and every saved session depend on, and the prompt cache keys on their bytes: any change to them, even a
/// reworded description or a reordered list, fails here until the golden is written again, so it's always
/// deliberate (and noted in the changelog). Schemas change additively: a new optional field is fine; removing,
/// renaming or narrowing one makes a new tool.
///
/// Proven: added a word to `Glob`'s description → this failed (line of `glob`).
#[test]
fn builtin_specs_match_the_golden() {
    golden("tests/guards/golden/tool_specs.jsonl", &spec_bytes());
}

/// **A session's specs are what `tool_specs_for` filters, never reordered.** Every switch combination keeps the
/// full list's order, so turning skills, plan or github off drops a tool without moving the others.
#[test]
fn every_tool_list_keeps_the_specs_order() {
    let full: Vec<String> = marv_tools::tool_specs().iter().map(|s| s.name.clone()).collect();
    for bits in 0..8u8 {
        let names: Vec<String> =
            marv_tools::tool_specs_for(bits & 1 != 0, bits & 2 != 0, bits & 4 != 0).into_iter().map(|s| s.name).collect();
        let positions: Vec<usize> = names.iter().map(|n| full.iter().position(|f| f == n).expect("a built-in")).collect();
        assert!(positions.windows(2).all(|w| w[0] < w[1]), "{names:?} out of order");
    }
}

/// Every built-in tool name Marv has ever offered, one per line, in the order they appeared. Append-only.
const TOOL_NAMES: &str = "tests/guards/golden/tool_names.txt";

/// **A tool name is never reused.** Every name Marv ever offered stays in `tool_names.txt`: each is either a
/// built-in tool today or has a tombstone (`REMOVED_TOOLS`), so a saved session's call of a removed tool gets "was
/// removed" rather than another tool's behaviour; and no tombstoned name is offered again. A new tool is added to
/// the file by `UPDATE_GOLDEN=1`; a removed one stays.
///
/// Proven: removed `memory` from `builtin_tools` → this failed (no tombstone); added a `Tombstone` named `glob` →
/// this failed (reused).
#[test]
fn a_removed_tool_keeps_its_name_and_tombstone() {
    let file = repo_root().join(TOOL_NAMES);
    let mut known: Vec<String> =
        std::fs::read_to_string(&file).unwrap_or_default().lines().filter(|l| !l.is_empty()).map(str::to_string).collect();
    let builtin: Vec<&str> = marv_tools::builtin_tools().iter().map(|t| t.name()).collect();
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        for name in &builtin {
            if !known.iter().any(|k| k == name) {
                known.push(name.to_string());
            }
        }
        std::fs::create_dir_all(file.parent().expect("a folder")).expect("the golden's folder");
        std::fs::write(&file, known.iter().map(|n| format!("{n}\n")).collect::<String>()).expect("write the names");
    }
    for name in &builtin {
        assert!(
            known.iter().any(|k| k == name),
            "{name} isn't in {TOOL_NAMES}: add it with UPDATE_GOLDEN=1 cargo test -p marv-guards --test tools"
        );
    }
    for tomb in marv_tools::REMOVED_TOOLS {
        assert!(
            !builtin.contains(&tomb.name),
            "{} has a tombstone and is offered again: give the new tool another name",
            tomb.name
        );
        assert!(known.iter().any(|k| k == tomb.name), "{} has a tombstone but was never in {TOOL_NAMES}", tomb.name);
    }
    for name in &known {
        assert!(
            builtin.contains(&name.as_str()) || marv_tools::REMOVED_TOOLS.iter().any(|t| t.name == name),
            "{name} was removed without a tombstone: add it to marv_tools::REMOVED_TOOLS with the version that drops it"
        );
    }
}
