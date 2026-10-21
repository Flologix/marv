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
//! - `tests/worktree.rs`: worktree subagents (`.git` read-only, Marv commits for them);
//! - `tests/mcp.rs`: trust, package runners, ignored `instructions`;
//! - `tests/mesh.rs`: the mesh from inside a sandbox (unwired calls refused and logged, a subagent reaches the parent
//!   only over its wire; a message that wakes an idle Marv is fenced and taints, #78);
//! - `tests/web.rs`: what a reader doesn't see never reaches the model (hidden text in web pages);
//! - `tests/images.rs`: images (not built yet: ignored);
//! - `tests/github.rs`, `tests/host.rs`: the doors out of the sandbox (Marv #47, #52): what gh can never do, what
//!   asks, tokens kept out of everything; host commands asked every time and only where someone approves each one.
//! - `tests/herdr.rs`: the Herdr door (#74): the sandbox can't reach Herdr, reads fenced and tainting, commands and
//!   spawns asked every time and only at a terminal, a worker no stronger than its parent, the peer limit.
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
    /// A folder for sockets (a runtime folder, a plug folder, a mesh folder): on macOS a short `/tmp/mx-<pid>-<n>`,
    /// as macOS's temp folder (`/var/folders/…/T`) leaves too little of `sun_path`'s 104 bytes (#150), kept as
    /// `/tmp/…` rather than its real `/private/tmp/…` (8 bytes more). Elsewhere the same as [`TempDir::new`].
    pub fn short(tag: &str) -> Self {
        if cfg!(target_os = "macos") {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let dir = PathBuf::from(format!("/tmp/mx-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            return Self(dir);
        }
        Self::new(tag)
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

/// The variable that puts a child Marv's runtime folder at `<dir>/marv`, and its value: `XDG_RUNTIME_DIR=<dir>` on
/// Linux, `MARV_RUNTIME_DIR=<dir>/marv` on macOS, which has no `XDG_RUNTIME_DIR` (`marv_sandbox::runtime_dir`).
pub fn runtime_var(dir: &Path) -> (&'static str, PathBuf) {
    if cfg!(target_os = "macos") {
        (marv_sandbox::runtime::RUNTIME_DIR_VAR, dir.join("marv"))
    } else {
        ("XDG_RUNTIME_DIR", dir.to_path_buf())
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
        hidden: Vec::new(),
        writable: Vec::new(),
        config_dir: Some(home.join(".marv")),
    }
}

/// What a sandboxed script printed (stdout and stderr together) and its exit code.
pub struct Ran {
    pub output: String,
    pub code: Option<i32>,
}

/// Runs `script` with real bwrap in `root`, spawned as Marv spawns it (`Sandbox::prepare`, `SandboxCommand::to_command`:
/// only the backend's environment). `env` is added to that environment for bwrap itself, to show the sandbox clears
/// it from the command's.
pub fn run_sandboxed(sandbox: &Sandbox, root: &Path, mounts: &CommandMounts, script: &str, env: &[(&str, &str)]) -> Ran {
    let inner: [OsString; 3] = ["bash".into(), "-c".into(), format!("exec 2>&1\n{script}").into()];
    let mut prepared = sandbox.prepare(root, mounts, &inner).expect("sandbox args");
    let out =
        prepared.to_command(root).envs(env.iter().copied()).stdin(std::process::Stdio::null()).output().expect("bwrap runs");
    // What bwrap left behind (a placeholder's empty mount point), as Marv removes it after a command.
    prepared.finish();
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
        self.decision.clone()
    }
}

/// A tool call with JSON arguments.
pub fn call(name: &str, args: serde_json::Value) -> ToolCall {
    ToolCall { id: "call_guard".into(), name: name.into(), arguments: args.to_string() }
}

/// A local web server (the guards never fetch the internet): every request gets `body` as `content_type`. Returns
/// `http://localhost:<port>`. It runs until the test process ends.
pub fn serve(body: &'static str, content_type: &'static str) -> String {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let mut head = Vec::new();
            let mut buf = [0u8; 4096];
            while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                match stream.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => head.extend_from_slice(&buf[..n]),
                }
            }
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
        }
    });
    format!("http://localhost:{port}")
}

/// Runs the test `name` (an integration test's function name) again in a child process with `env` added, asserts it
/// passed there, and returns false; in that child it returns true, and the test's body runs. The doors read the real
/// environment (a fake gh first on PATH), and Rust can't change its own environment without `unsafe`.
pub fn in_child_with_env(name: &str, env: &[(&str, &str)]) -> bool {
    if std::env::var("MARV_GUARD_CHILD").as_deref() == Ok(name) {
        return true;
    }
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args([name, "--exact", "--nocapture", "--test-threads=1"])
        .env("MARV_GUARD_CHILD", name)
        .envs(env.iter().copied())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success() && stdout.contains("1 passed"), "{name} in a child:\n{stdout}\n{stderr}");
    false
}

/// A model that replays one scripted reply per request (Marv's `ScriptedProvider`) and records each request.
#[derive(Default)]
pub struct Scripted {
    replies: Mutex<std::collections::VecDeque<Vec<marv_core::ProviderEvent>>>,
    pub requests: Mutex<Vec<(Vec<marv_core::ChatTurn>, marv_provider::StreamOptions)>>,
}

impl Scripted {
    pub fn new(replies: Vec<Vec<marv_core::ProviderEvent>>) -> Arc<Self> {
        Arc::new(Self { replies: Mutex::new(replies.into()), requests: Mutex::default() })
    }
}

impl marv_provider::Provider for Scripted {
    fn name(&self) -> &str {
        "scripted"
    }
    fn context_length(&self) -> Option<u64> {
        None
    }
    fn stream(&self, history: &[marv_core::ChatTurn], options: marv_provider::StreamOptions) -> marv_provider::EventStream {
        self.requests.lock().unwrap().push((history.to_vec(), options));
        let reply = self.replies.lock().unwrap().pop_front().unwrap_or_else(|| say("(script ended)"));
        Box::pin(futures::stream::iter(reply))
    }
}

/// A reply that says `text` and stops.
pub fn say(text: &str) -> Vec<marv_core::ProviderEvent> {
    vec![marv_core::ProviderEvent::TextDelta(text.into()), marv_core::ProviderEvent::Done { reason: Some("stop".into()) }]
}

/// A reply that makes these calls.
pub fn use_tools(calls: Vec<ToolCall>) -> Vec<marv_core::ProviderEvent> {
    let mut events: Vec<marv_core::ProviderEvent> = calls.into_iter().map(marv_core::ProviderEvent::ToolCall).collect();
    events.push(marv_core::ProviderEvent::Done { reason: Some("tool_calls".into()) });
    events
}

/// A tool call with its own id.
pub fn tool_call(id: &str, name: &str, args: serde_json::Value) -> ToolCall {
    ToolCall { id: id.into(), name: name.into(), arguments: args.to_string() }
}

/// What one SDK turn did: every event, and the main agent's call results in order.
pub struct TurnRun {
    pub events: Vec<marv_session::session::SessionEvent>,
    pub results: Vec<marv_core::ToolResult>,
}

/// One SDK turn (yolo on, the default) in `root` with `model`, Marv's files in `config_dir`; `edit` sets the rest.
pub async fn sdk_turn(
    root: &Path,
    config_dir: &Path,
    model: Arc<Scripted>,
    edit: impl FnOnce(&mut marv_session::sdk::SessionOptions),
) -> TurnRun {
    use futures::StreamExt;
    let mut builder = marv_session::SessionBuilder::new(root, marv_provider::ProviderOption::Custom(model));
    builder.options.config_dir = Some(config_dir.to_path_buf());
    builder.options.diagnostics = false;
    builder.options.checks = false;
    edit(&mut builder.options);
    let session = builder.build().await.expect("a session");
    let mut turn = session.send("check the project", marv_session::SendOptions::default()).expect("a turn");
    let mut events = Vec::new();
    while let Some(event) = turn.next().await {
        events.push(event);
    }
    session.flush().await;
    session.close().await;
    let results = events
        .iter()
        .filter_map(|e| match e {
            marv_session::session::SessionEvent::Loop(marv_agent::LoopEvent::ToolEnd { run, .. }) => Some(run.result.clone()),
            _ => None,
        })
        .collect();
    TurnRun { events, results }
}

/// What `marv -p` printed as JSON, its exit code, and its stderr.
pub struct Headless {
    pub code: Option<i32>,
    pub result: serde_json::Value,
    pub stderr: String,
}

/// Runs the real `marv -p "do it" --output json` in `root` (the binary `cargo test --workspace` builds, or
/// `MARV_ORACLE_BIN`) against a fake OpenRouter that makes `calls` and then says "Done.". `env` is all the
/// environment it gets (with `MARV_CONFIG_DIR` and `HOME` set here).
pub fn headless(
    root: &Path,
    config_dir: &Path,
    calls: Vec<marv_oracle::script::ScriptCall>,
    approve: &[&str],
    env: &[(&str, &str)],
) -> Headless {
    let args: Vec<&str> = approve.iter().flat_map(|name| ["--approve", name]).collect();
    headless_steps(root, config_dir, vec![calls], &args, env)
}

/// [`headless`] with one model reply per entry of `steps` (each makes its calls; then "Done."), and `args` added to
/// the command line as they are (`["--approve", "web", "--approve-tainted"]`).
pub fn headless_steps(
    root: &Path,
    config_dir: &Path,
    steps: Vec<Vec<marv_oracle::script::ScriptCall>>,
    args: &[&str],
    env: &[(&str, &str)],
) -> Headless {
    headless_configured(root, config_dir, steps, args, env, serde_json::json!({}))
}

/// [`headless_steps`] with `extra` (a JSON object) merged into the config file (`{"yolo": false, "approve": [...]}`).
pub fn headless_configured(
    root: &Path,
    config_dir: &Path,
    steps: Vec<Vec<marv_oracle::script::ScriptCall>>,
    args: &[&str],
    env: &[(&str, &str)],
    extra: serde_json::Value,
) -> Headless {
    use marv_oracle::script::Reply;
    let bin = marv_oracle::binary();
    assert!(bin.exists(), "no marv binary at {} (cargo build -p marv-cli, or set MARV_ORACLE_BIN)", bin.display());
    let mut replies: Vec<Reply> = steps.into_iter().map(|calls| Reply { tool_calls: calls, ..Reply::default() }).collect();
    replies.push(Reply { text: "Done.".into(), ..Reply::default() });
    let server =
        marv_oracle::server::FakeServer::start(replies, vec![serde_json::json!({"id": marv_oracle::runner::MODEL})]).unwrap();
    let mut config = marv_oracle::runner::default_config(&server.base_url());
    if let serde_json::Value::Object(extra) = extra {
        config.extend(extra);
    }
    let config = serde_json::Value::Object(config);
    std::fs::create_dir_all(config_dir).unwrap();
    std::fs::write(config_dir.join("config.json"), config.to_string()).unwrap();
    let home = config_dir.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let mut cmd = std::process::Command::new(bin);
    cmd.args(["-p", "do it", "--output", "json"]).args(args);
    let out = cmd
        .current_dir(root)
        .env_clear()
        .env("HOME", &home)
        .env("MARV_CONFIG_DIR", config_dir)
        .env("NO_COLOR", "1")
        .envs(env.iter().copied())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    let result = serde_json::from_str(&stdout)
        .unwrap_or_else(|_| panic!("marv -p printed no JSON (exit {:?}):\n{stdout}\n{stderr}", out.status.code()));
    Headless { code: out.status.code(), result, stderr }
}
