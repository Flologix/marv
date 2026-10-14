//! A fake `gh` for tests (this crate's and the guard suite's): never run the real gh against GitHub in a test. See
//! `fake_gh.sh` for what it answers. Ports Marv's `tests/fixtures/fake-gh.ts`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const SCRIPT: &str = include_str!("fake_gh.sh");

/// Writes an executable `gh` into `dir` (to put first on PATH); returns `dir`.
pub fn install_fake_gh(dir: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(dir).unwrap();
    let gh = dir.join("gh");
    std::fs::write(&gh, SCRIPT).unwrap();
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
    dir.to_path_buf()
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
