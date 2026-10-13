//! marv-guards: one test per rule in `docs/SECURITY.md`, each proven by reverting its guard.
//!
//! Ports the idea of Marv issue #53 (the guard suite). Marv had no such suite; these rules are marv-rs's, from the
//! 2026-10-12 audit of Marv 0.3.0. The tests sit in one workspace crate (not in each crate's own tests) so the
//! rules read as one list, in `docs/SECURITY.md`'s order:
//!
//! - `tests/launch.rs`: process and launch (config only from the real environment, `printable()` everywhere);
//! - `tests/sandbox.rs`: bubblewrap, the stand-in home, protected paths (asks and read-only mounts, and the scan for
//!   ones a command creates), secrets, realpath containment;
//! - `tests/provenance.rs`: provenance tagging and fencing, taint and egress, `must_ask` vs "always", subagents;
//! - `tests/mcp.rs`: trust, package runners, ignored `instructions`;
//! - `tests/images.rs`: images (not built yet: ignored).
//!
//! Every guard's doc comment says how it was proven: "reverted X → this test failed". A guard whose code isn't
//! written yet is `#[ignore = "needs …"]` with the body it will run.
//!
//! Tests that run real bwrap skip (with a line on stderr) where bubblewrap can't create namespaces; CI installs it.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use marv_core::ToolCall;
use marv_sandbox::{CommandMounts, Sandbox};
use marv_tools::{ApprovalRequest, Approver, Decision};

/// Skips a test that needs bwrap on a machine without it.
#[macro_export]
macro_rules! require_bwrap {
    () => {
        if !marv_sandbox::sandbox_available() {
            eprintln!("skipped: bubblewrap isn't available here");
            return;
        }
    };
}

/// A fresh temp folder (real path), removed on drop.
pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new(tag: &str) -> Self {
        Self::new_in(&std::env::temp_dir(), tag)
    }
    /// A folder the host's user can write outside `/tmp` (which the sandbox replaces with its own): `/var/tmp`. A fake
    /// home there is what a missing home tmpfs would expose; a file there is what a writable `/` would let a command
    /// change. Falls back to the temp folder where `/var/tmp` isn't writable.
    pub fn outside_tmp(tag: &str) -> Self {
        let var_tmp = Path::new("/var/tmp");
        let probe = var_tmp.join(format!(".marv-guards-probe-{}", std::process::id()));
        let writable = std::fs::write(&probe, "").and_then(|()| std::fs::remove_file(&probe)).is_ok();
        let parent = if writable { var_tmp.to_path_buf() } else { std::env::temp_dir() };
        Self::new_in(&parent, tag)
    }
    fn new_in(parent: &Path, tag: &str) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = parent.join(format!("marv-guards-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(std::fs::canonicalize(dir).unwrap())
    }
    pub fn path(&self) -> &Path {
        &self.0
    }
    /// Writes `rel` (creating folders).
    pub fn write(&self, rel: &str, text: &str) -> PathBuf {
        let path = self.0.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, text).unwrap();
        path
    }
    pub fn read(&self, rel: &str) -> Option<String> {
        std::fs::read_to_string(self.0.join(rel)).ok()
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A sandbox whose home is `home` (a fake one), with the host's PATH.
pub fn sandbox_with_home(home: &Path) -> Sandbox {
    Sandbox {
        enabled: true,
        home: home.to_path_buf(),
        path: std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into()),
        lang: "C.UTF-8".into(),
        mesh_socket: None,
    }
}

/// What a sandboxed script printed (stdout and stderr together) and its exit code.
pub struct Ran {
    pub output: String,
    pub code: Option<i32>,
}

/// Runs `script` with real bwrap in `root`, as `Sandbox::command` builds it. `env` is added to *our* environment
/// for bwrap, to show the sandbox clears it.
pub fn run_sandboxed(sandbox: &Sandbox, root: &Path, mounts: &CommandMounts, script: &str, env: &[(&str, &str)]) -> Ran {
    let argv: Vec<OsString> = sandbox.command(root, mounts, &format!("exec 2>&1\n{script}")).expect("sandbox args");
    let out = std::process::Command::new(&argv[0])
        .args(&argv[1..])
        .envs(env.iter().copied())
        .current_dir(root)
        .stdin(std::process::Stdio::null())
        .output()
        .expect("bwrap runs");
    let mut output = String::from_utf8_lossy(&out.stdout).into_owned();
    output.push_str(&String::from_utf8_lossy(&out.stderr));
    Ran { output, code: out.status.code() }
}

/// An approver that records every request and answers `decision`.
pub struct Recorder {
    pub decision: Decision,
    pub requests: Mutex<Vec<ApprovalRequest>>,
}

impl Recorder {
    pub fn new(decision: Decision) -> Arc<Self> {
        Arc::new(Self { decision, requests: Mutex::new(Vec::new()) })
    }
    pub fn requests(&self) -> Vec<ApprovalRequest> {
        self.requests.lock().unwrap().clone()
    }
}

#[async_trait]
impl Approver for Recorder {
    async fn approve(&self, request: ApprovalRequest) -> Decision {
        self.requests.lock().unwrap().push(request);
        self.decision
    }
}

/// A tool call with JSON arguments.
pub fn call(name: &str, args: serde_json::Value) -> ToolCall {
    ToolCall { id: "call_guard".into(), name: name.into(), arguments: args.to_string() }
}
