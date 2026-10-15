//! A fake `gh` for tests (this crate's and the guard suite's): never run the real gh against GitHub in a test. See
//! `fake_gh.sh` for what it answers. Ports Marv's `tests/fixtures/fake-gh.ts`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const SCRIPT: &str = include_str!("fake_gh.sh");

/// Writes an executable `gh` into `dir` (to put first on PATH); returns `dir`.
pub fn install_fake_gh(dir: &Path) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    write_executable(&dir.join("gh"), SCRIPT);
    dir.to_path_buf()
}

/// Writes `contents` to `path` (making its folder) as an executable (0755) script, for a test's fake program. Every test in the workspace
/// that writes a script it (or Marv) then runs goes through this.
///
/// The file is written by a child `sh`, never by this process (#92): while a test thread here has a file open for
/// writing, another thread may fork to spawn its own child, and that child holds a copy of the write handle until it
/// execs. Linux refuses to run a file open for writing ("Text file busy", ETXTBSY), so under load the first run of a
/// script written with `std::fs::write` can fail. A handle that only ever lives in the child (gone once it has been
/// waited for) can't be copied that way, so the script can always run once this returns.
pub fn write_executable(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    let status = std::process::Command::new("sh")
        .args(["-c", "printf '%s' \"$2\" > \"$1\" && chmod 755 \"$1\"", "sh"])
        .arg(path)
        .arg(contents)
        .status()
        .unwrap();
    assert!(status.success(), "couldn't write {}: {status}", path.display());
}

/// One logged call: its argv (without "gh"), its folder, and the logged variables that were set.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FakeGhCall {
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub env: BTreeMap<String, String>,
}

/// The calls the fake logged, in order (none if it never ran).
pub fn fake_gh_calls(log: &Path) -> Vec<FakeGhCall> {
    let Ok(text) = std::fs::read_to_string(log) else { return Vec::new() };
    let mut calls = Vec::new();
    let mut call = FakeGhCall::default();
    for line in text.lines() {
        if line == "call" {
            call = FakeGhCall::default();
        } else if line == "end" {
            calls.push(std::mem::take(&mut call));
        } else if let Some(cwd) = line.strip_prefix("cwd ") {
            call.cwd = PathBuf::from(cwd);
        } else if let Some(arg) = line.strip_prefix("arg ") {
            call.args.push(arg.to_string());
        } else if let Some((name, value)) = line.strip_prefix("env ").and_then(|v| v.split_once('=')) {
            call.env.insert(name.to_string(), value.to_string());
        }
    }
    calls
}
