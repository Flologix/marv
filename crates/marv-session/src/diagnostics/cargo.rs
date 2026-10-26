//! Rust's checker, `cargo check`, after a step's file changes (marv-rs, #187; `docs/specs/2026-10-25-lsp-checker.md`
//! §4 "Rust", §6). New in marv-rs: TypeScript Marv has only tsc; nothing here is a port.
//!
//! - **Applies** when the project root has a `Cargo.toml` (a package or a workspace). Nothing else is looked at on the
//!   host: whether cargo is installed, and which toolchain `rust-toolchain.toml` picks, is learned from the sandboxed
//!   run itself. No cargo or rustup ever runs on the host (`docs/SECURITY.md`; the no-host-spawn test pins it).
//! - **The command:** `cargo check --workspace --all-targets --offline --message-format=json-diagnostic-short
//!   --target-dir <key folder>/target`, stderr dropped (cargo's progress text there would break the JSON lines), at low
//!   priority ([`low_priority`]), with `CARGO_TERM_COLOR=never RUSTUP_AUTO_INSTALL=0 CARGO_BUILD_JOBS=<half the
//!   cores>` set in the command (the sandbox's environment is built from scratch). `--target-dir` beats the project's
//!   `build.target-dir`, so the project's own `target/` is never touched.
//! - **The cache** is the check cache's key folder ([`super::cache`]), warmed up at the session's first edit: a cold
//!   workspace takes minutes, which the 60 s check timeout would otherwise cut off twice.
//! - **What the project steers** (its `.cargo/config.toml`, `rust-toolchain.toml`, `build.rs`, proc macros) runs only
//!   inside the sandbox, with the project read-only, no network, and only the key folder writable.
//! - **Reading it** ([`parse_cargo`]): errors only; an error without a primary span (rustc's "aborting due to N previous
//!   errors", whose count changes every step) is dropped; the same error twice in one run (`--all-targets` checks
//!   `src/lib.rs` for the lib and its tests) is told once; no `build-finished` line, a failed build with no error, or
//!   any line that isn't JSON makes the run unreadable. Each message carries its primary span's label ("mismatched
//!   types: expected `u32`, found `&str`"), cut at [`MAX_MESSAGE`] characters.
//! - **Not installed:** cargo missing in the sandbox (exit 127, nothing printed) turns the kind off for the session
//!   quietly, as the formatter skips a missing rustfmt.

use std::collections::HashSet;
use std::path::Path;

use serde_json::Value;

use super::Diagnostic;
use super::tsc::{MAX_REPORTED, format_error, ts_error_key};
use super::turn::{CheckKind, CheckPass, MAX_UNREADABLE, build_jobs, low_priority, shell_quote};

/// Its name in notices, `/diagnostics` and the note.
pub const LABEL: &str = "cargo check";
/// A message (with its label) is cut here: rustc's can be long, and the model can run `cargo check` for the rest.
pub const MAX_MESSAGE: usize = 400;

/// Whether the project is a Rust package or workspace: a `Cargo.toml` at its root. Only looks.
pub fn applies(root: &Path) -> bool {
    root.join("Cargo.toml").is_file()
}

/// The command, run in the sandbox with the project as its folder; `cache` is the kind's key folder.
pub fn check_command(cache: &Path) -> String {
    let target = cache.join("target");
    let cargo = format!(
        "cargo check --workspace --all-targets --offline --message-format=json-diagnostic-short --target-dir {} 2>/dev/null",
        shell_quote(&target.to_string_lossy())
    );
    low_priority(
        &[
            ("CARGO_TERM_COLOR", "never".into()),
            ("RUSTUP_AUTO_INSTALL", "0".into()),
            ("CARGO_BUILD_JOBS", build_jobs().to_string()),
        ],
        &cargo,
    )
}

/// `s`, cut at [`MAX_MESSAGE`] characters (an ellipsis marks the cut).
fn cut(s: String) -> String {
    match s.char_indices().nth(MAX_MESSAGE) {
        Some((at, _)) => format!("{}…", &s[..at]),
        None => s,
    }
}

/// The errors in `cargo check --message-format=json-diagnostic-short`'s stdout, or `None` when the run can't be read
/// (see the module doc). The exit code isn't needed: `build-finished` says how the build went.
pub fn parse_cargo(output: &str, _exit_code: Option<i32>) -> Option<Vec<Diagnostic>> {
    let mut errors = Vec::new();
    let mut seen = HashSet::new();
    let mut finished = None;
    for line in output.lines() {
        let line = line.trim_end_matches('\r');
        if line.trim().is_empty() {
            continue;
        }
        let value: Value = serde_json::from_str(line).ok()?;
        match value.get("reason").and_then(Value::as_str) {
            Some("build-finished") => finished = Some(value.get("success").and_then(Value::as_bool)?),
            Some("compiler-message") => {
                let Some(error) = error_of(&value["message"]) else { continue };
                let at = (error.file.clone(), error.line, error.column, error.code.clone(), error.message.clone());
                if seen.insert(at) {
                    errors.push(error);
                }
            }
            // compiler-artifact, build-script-executed and the like: not about errors.
            _ => {}
        }
    }
    match finished? {
        false if errors.is_empty() => None,
        _ => Some(errors),
    }
}

/// A rustc diagnostic as a [`Diagnostic`], if it's an error with a primary span.
fn error_of(message: &Value) -> Option<Diagnostic> {
    if message.get("level").and_then(Value::as_str) != Some("error") {
        return None;
    }
    let spans = message.get("spans").and_then(Value::as_array)?;
    let primary = spans.iter().find(|s| s.get("is_primary").and_then(Value::as_bool) == Some(true))?;
    let text = message.get("message").and_then(Value::as_str)?;
    let text = match primary.get("label").and_then(Value::as_str).filter(|l| !l.is_empty()) {
        Some(label) => format!("{text}: {label}"),
        None => text.to_string(),
    };
    Some(Diagnostic {
        file: primary.get("file_name").and_then(Value::as_str).map(String::from),
        line: primary.get("line_start").and_then(Value::as_u64),
        column: primary.get("column_start").and_then(Value::as_u64),
        code: message.get("code").and_then(|c| c.get("code")).and_then(Value::as_str).unwrap_or("").to_string(),
        message: cut(text),
    })
}

/// Cargo isn't in the sandbox: the shell's "command not found" (exit 127), with nothing on stdout (stderr is dropped).
/// Cargo itself never exits 127.
pub fn not_installed(output: &str, exit_code: Option<i32>) -> bool {
    exit_code == Some(127) && output.trim().is_empty()
}

/// What the model is told: Marv's sentence, then the new errors fenced as the project's data.
pub fn cargo_note(added: &[Diagnostic]) -> String {
    let mut body: Vec<String> = added.iter().take(MAX_REPORTED).map(format_error).collect();
    if added.len() > MAX_REPORTED {
        body.push(format!("and {} more.", added.len() - MAX_REPORTED));
    }
    let n = added.len();
    crate::provenance::project_note(
        &if n == 1 {
            "cargo check found 1 error that wasn't in the previous check.".to_string()
        } else {
            format!("cargo check found {n} errors that weren't in the previous check.")
        },
        LABEL,
        &body.join("\n"),
    )
}

/// The `cargo check` kind.
pub struct Cargo;

impl CheckKind<Diagnostic> for Cargo {
    fn label(&self) -> &'static str {
        LABEL
    }
    fn source(&self) -> Option<&'static str> {
        Some("cargo")
    }
    fn cache(&self) -> bool {
        true
    }
    fn warm_up(&self, _cache: &Path) -> bool {
        true
    }
    fn max_unreadable(&self) -> Option<u32> {
        Some(MAX_UNREADABLE)
    }
    fn prepare(&self, root: &Path) -> bool {
        applies(root)
    }
    /// Every pass is the whole workspace (a changed signature breaks callers in other files), so the whole baseline is
    /// re-checked.
    fn pass(&self, _root: &Path, cache: Option<&Path>, _changed: Option<&[String]>) -> Option<CheckPass<Diagnostic>> {
        Some(CheckPass { command: check_command(cache?), covers: None, read_only: Vec::new() })
    }
    fn parse(&self, output: &str, exit_code: Option<i32>) -> Option<Vec<Diagnostic>> {
        parse_cargo(output, exit_code)
    }
    fn missing(&self, _output: &str, _exit_code: Option<i32>) -> Option<String> {
        None
    }
    fn not_installed(&self, output: &str, exit_code: Option<i32>) -> bool {
        not_installed(output, exit_code)
    }
    /// File, code, message and primary label; not the line, which edits move.
    fn key(&self, problem: &Diagnostic) -> String {
        ts_error_key(problem)
    }
    fn note(&self, added: &[Diagnostic]) -> String {
        cargo_note(added)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real `cargo check --workspace --all-targets` output (cargo 1.9x) on a crate with two errors in `src/lib.rs` and
    /// one in its `#[cfg(test)]` module; paths shortened, the codes' long explanations left out.
    const ERRORS: &str = include_str!("fixtures/cargo-errors.jsonl");
    /// The same crate with only an unused import: a warning, and a successful build.
    const WARNINGS: &str = include_str!("fixtures/cargo-warnings.jsonl");
    /// rustc's own span-less summary, in cargo's envelope (older cargos pass it through).
    const SUMMARY: &str = r#"{"reason":"compiler-message","package_id":"path+file:///work/fixture#fixture@0.1.0","manifest_path":"/work/fixture/Cargo.toml","target":{"kind":["lib"],"crate_types":["lib"],"name":"fixture","src_path":"/work/fixture/src/lib.rs","edition":"2021","doc":true,"doctest":true,"test":true},"message":{"rendered":"error: aborting due to 2 previous errors\n","$message_type":"diagnostic","children":[],"level":"error","message":"aborting due to 2 previous errors","spans":[],"code":null}}"#;
    const FAILED: &str = r#"{"reason":"build-finished","success":false}"#;

    #[test]
    fn errors_are_kept_lib_and_lib_test_duplicates_folded_with_the_primary_label() {
        let errors = parse_cargo(ERRORS, Some(101)).unwrap();
        let shown: Vec<String> = errors.iter().map(format_error).collect();
        assert_eq!(
            shown,
            [
                "src/lib.rs:2:5 E0308 mismatched types: expected `u32`, found `&str`",
                "src/lib.rs:4:21 E0425 cannot find function `missing_fn` in this scope: not found in this scope",
                "src/lib.rs:6:42 E0308 mismatched types: expected `u8`, found `&str`",
            ]
        );
        // The fixture really has each lib error twice (the lib, then the lib's tests).
        assert_eq!(ERRORS.matches("\"level\":\"error\"").count(), 5);
    }

    #[test]
    fn warnings_are_ignored_and_a_clean_build_has_no_errors() {
        assert!(WARNINGS.contains("\"level\":\"warning\""));
        assert_eq!(parse_cargo(WARNINGS, Some(0)), Some(vec![]));
    }

    #[test]
    fn the_spanless_summary_is_dropped() {
        let out = format!("{ERRORS}{SUMMARY}\n");
        assert_eq!(parse_cargo(&out, Some(101)).unwrap().len(), 3);
        // Alone with a failed build it leaves no error: unreadable, not "clean".
        assert_eq!(parse_cargo(&format!("{SUMMARY}\n{FAILED}\n"), Some(101)), None);
    }

    #[test]
    fn without_build_finished_the_run_cant_be_read() {
        // Cargo stopped before building (a lockfile to update offline, a bad manifest): nothing on stdout.
        assert_eq!(parse_cargo("", Some(101)), None);
        let cut_off: String = ERRORS.lines().filter(|l| !l.contains("build-finished")).map(|l| format!("{l}\n")).collect();
        assert_eq!(parse_cargo(&cut_off, Some(101)), None);
    }

    #[test]
    fn a_failed_build_without_an_error_cant_be_read() {
        // A build script that failed, a toolchain that isn't installed.
        assert_eq!(parse_cargo(&format!("{FAILED}\n"), Some(101)), None);
    }

    #[test]
    fn a_line_that_isnt_json_makes_the_run_unreadable() {
        assert_eq!(parse_cargo(&format!("   Compiling fixture v0.1.0\n{WARNINGS}"), Some(0)), None);
        assert_eq!(parse_cargo(&format!("{ERRORS}thread 'main' panicked\n"), Some(101)), None);
        assert_eq!(parse_cargo("{\"reason\":\"build-finished\"}\n", Some(0)), None);
    }

    #[test]
    fn the_key_has_the_label_not_the_line() {
        let errors = parse_cargo(ERRORS, Some(101)).unwrap();
        // Two E0308 "mismatched types" in one file are told apart by their labels.
        assert_ne!(Cargo.key(&errors[0]), Cargo.key(&errors[2]));
        let moved = Diagnostic { line: Some(40), column: Some(1), ..errors[0].clone() };
        assert_eq!(Cargo.key(&errors[0]), Cargo.key(&moved));
        assert!(Cargo.key(&errors[0]).contains("expected `u32`, found `&str`"));
    }

    #[test]
    fn a_long_message_is_cut_at_400_characters() {
        let long = ERRORS.replace("expected `u32`, found `&str`", &"é".repeat(600));
        let errors = parse_cargo(&long, Some(101)).unwrap();
        assert_eq!(errors[0].message.chars().count(), MAX_MESSAGE + 1);
        assert!(errors[0].message.starts_with("mismatched types: éé") && errors[0].message.ends_with('…'));
        assert!(cargo_note(&errors).contains(&errors[0].message));
    }

    #[test]
    fn the_note_is_marvs_sentence_then_the_errors_fenced_at_most_max_reported() {
        let errors = parse_cargo(ERRORS, Some(101)).unwrap();
        let note = cargo_note(&errors[..1]);
        assert!(
            note.starts_with(
                "(Automatic note from Marv, the program running you) cargo check found 1 error that wasn't in the previous check."
            ),
            "{note}"
        );
        assert!(note.find("src/lib.rs:2:5 E0308").unwrap() > note.find("<untrusted-data").unwrap());
        let many: Vec<Diagnostic> =
            (0..MAX_REPORTED + 12).map(|i| Diagnostic { message: format!("m{i}"), ..errors[0].clone() }).collect();
        let note = cargo_note(&many);
        assert!(note.contains(&format!("found {} errors that weren't in the previous check.", MAX_REPORTED + 12)));
        assert!(note.contains(" m19\n") && !note.contains(" m20\n"));
        assert!(note.contains("\nand 12 more.\n"));
    }

    #[test]
    fn the_command_pins_the_flags_and_the_environment() {
        // The cargo command is quoted inside `low_priority`'s: read it unquoted.
        let command = check_command(Path::new("/cache/checks/repo-abc")).replace("'\\''", "'");
        for part in [
            "cargo check --workspace --all-targets --offline --message-format=json-diagnostic-short",
            "--target-dir '/cache/checks/repo-abc/target' 2>/dev/null",
        ] {
            assert!(command.contains(part), "{command}");
        }
        assert!(command.contains("export RUSTUP_AUTO_INSTALL='0';"), "{command}");
        assert!(command.contains("export CARGO_TERM_COLOR='never';"), "{command}");
        assert!(command.contains(&format!("export CARGO_BUILD_JOBS='{}';", build_jobs())), "{command}");
        assert!(command.contains("nice -n 10"), "{command}");
        // The cache is the key folder's `target/`, never the project's.
        let pass = Cargo.pass(Path::new("/project"), Some(Path::new("/cache/k")), None).unwrap();
        assert!(pass.command.replace("'\\''", "'").contains("--target-dir '/cache/k/target'") && pass.covers.is_none());
        assert!(Cargo.pass(Path::new("/project"), None, None).is_none());
    }

    #[test]
    fn it_applies_with_a_cargo_toml_at_the_root_and_knows_a_missing_cargo() {
        let dir = crate::checks::Scratch::new();
        assert!(!Cargo.prepare(&dir.0));
        dir.put("crates/a/Cargo.toml", "");
        assert!(!Cargo.prepare(&dir.0));
        dir.put("Cargo.toml", "[workspace]\n");
        assert!(Cargo.prepare(&dir.0));
        assert!(Cargo.not_installed("", Some(127)));
        assert!(!Cargo.not_installed("", Some(101)));
        assert!(!Cargo.not_installed(FAILED, Some(127)));
    }
}
