//! The built-in tools as a contract: what the model and saved sessions depend on can't change by accident.
//!
//! - `docs/TOOLS.md` is generated from the code (`marv_tools::table`) and must match it.
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
