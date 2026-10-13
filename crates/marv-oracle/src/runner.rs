//! Runs one scenario through a `marv` binary: `marv -p <prompt> --output stream-json` in a fresh
//! copy of `project/`, against the fake server, and collects what it did.
//!
//! Ports the workspace half of Marv's `evals/run.ts` (`workspace()`: a temp copy of the task's repo,
//! committed, so glob and grep see it through `git ls-files`). The rest is new: the binary is a
//! parameter, so the same scenario runs through TypeScript Marv (to record) and marv-rs (to check).
//!
//! Isolation, so a run never touches the user's setup or reaches a real model:
//! - a throwaway `MARV_CONFIG_DIR` whose `config.json` selects provider `openrouter` with the fake
//!   server's `baseUrl` and a dummy key;
//! - a throwaway `HOME` (personal skills and agents live in `~/.marv`, and would change the system
//!   prompt), and git told to ignore the user's and the system's config;
//! - a cleared environment: `OPENROUTER_API_KEY`, `MARV_MODEL` and `MARV_MAX_STEPS` beat the config
//!   file in Marv's `resolveConfig`, so an inherited one would send the run elsewhere.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};

use crate::normalize::Normalizer;
use crate::scenario::{DEFAULT_TIMEOUT_SECS, Scenario, mcp_root, real_path};
use crate::server::{FakeServer, Recorded};

/// The model every scenario's config names. Nothing serves it: the fake server answers any model.
pub const MODEL: &str = "oracle/scripted";

/// `config.json` before a scenario's `config` is laid over it. Trajectories, the typecheck after
/// edits and project checks are off: they write outside the project or run tools the script didn't
/// ask for, and a scenario that wants one turns it on.
pub fn default_config(base_url: &str) -> Map<String, Value> {
    let config = json!({
        "provider": "openrouter",
        "baseUrl": base_url,
        "apiKey": "oracle-dummy-key",
        "model": MODEL,
        "trajectories": false,
        "diagnostics": false,
        "checks": false,
    });
    config.as_object().cloned().unwrap_or_default()
}

/// What a run did, normalized.
#[derive(Debug, Clone, PartialEq)]
pub struct Outcome {
    /// `events.jsonl`: the `stream-json` log, one canonical JSON line per event.
    pub events: String,
    /// `requests.jsonl`: each chat request's body as canonical JSON (see [`canonical_request`]).
    pub requests: String,
    /// The same bodies, bytes as sent (after scrubbing): what the prefix rule reads.
    pub raw_requests: Vec<String>,
    /// The project afterwards (`.git` left out), by `/`-separated path.
    pub files: BTreeMap<String, Vec<u8>>,
    /// `None` when the binary was killed (timeout) or died of a signal.
    pub exit_code: Option<i32>,
    /// Not compared: what to read when a run goes wrong.
    pub stderr: String,
    /// Requests other than chat (`GET /models`, …), `METHOD path`. Not compared.
    pub other_requests: Vec<String>,
    /// Scripted replies left unused.
    pub unused_replies: usize,
    pub timed_out: bool,
}

/// Where the tracker's comments land in the project, as in Marv's evals (`evals/run.ts`).
pub const TRACKER_COMMENTS: &str = "__tracker_comments__.json";

/// A temp directory removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> anyhow::Result<TempDir> {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("marv-oracle-{label}-{}-{n}", std::process::id()));
        if dir.exists() {
            std::fs::remove_dir_all(&dir)?;
        }
        std::fs::create_dir_all(&dir)?;
        // Canonical, so the paths the binary prints (after realpath) are the ones scrubbed.
        Ok(TempDir(dir.canonicalize()?))
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        if std::env::var_os("MARV_ORACLE_KEEP").is_none() {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

/// Runs `scenario` through `bin`.
pub fn run(scenario: &Scenario, bin: &Path) -> anyhow::Result<Outcome> {
    let temp = TempDir::new(&scenario.name)?;
    let (home, config_dir, project) = (temp.0.join("home"), temp.0.join("config"), temp.0.join("project"));
    for dir in [&home, &config_dir, &project] {
        std::fs::create_dir_all(dir)?;
    }
    for (rel, bytes) in read_tree(&scenario.project(), false)? {
        write_file(&project.join(real_path(&rel)), &bytes)?;
    }
    if scenario.run.git {
        git_init(&project, &home)?;
    }

    let server = FakeServer::start(scenario.replies.clone(), scenario.run.models.clone())?;
    let mut config = default_config(&server.base_url());
    config.extend(scenario.run.config.clone());
    std::fs::write(config_dir.join("config.json"), serde_json::to_string_pretty(&Value::Object(config))? + "\n")?;

    // The tracker's data and output live beside the project, not in it: the model can only reach
    // them through the server (as in Marv's evals).
    let mut mcp_out = None;
    let oracle = oracle_binary();
    if let Some(fixture) = &scenario.run.mcp {
        anyhow::ensure!(fixture.server == "tracker", "{}: the only MCP fixture is \"tracker\"", scenario.name);
        let oracle = oracle.as_ref().map_err(|err| anyhow::anyhow!("{err}"))?;
        let dir = temp.0.join("tracker");
        std::fs::create_dir_all(&dir)?;
        let data = dir.join(&fixture.data);
        std::fs::copy(mcp_root().join(&fixture.data), &data)?;
        let out = dir.join("out.json");
        let servers = json!({"mcpServers": {"tracker": {"command": oracle, "args": ["tracker", data, out]}}});
        std::fs::write(config_dir.join("mcp.json"), serde_json::to_string_pretty(&servers)? + "\n")?;
        mcp_out = Some(out);
    }

    let mut command = Command::new(bin);
    command.args(["-p", &scenario.run.prompt, "--output", "stream-json"]);
    if let Some(steps) = scenario.run.max_steps {
        command.args(["--max-steps", &steps.to_string()]);
    }
    for name in &scenario.run.approve {
        command.args(["--approve", name]);
    }
    command
        .current_dir(&project)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", &home)
        .env("MARV_CONFIG_DIR", &config_dir)
        .env("TMPDIR", &temp.0)
        .env("TERM", "dumb")
        .env("LANG", "C.UTF-8")
        .env("NO_COLOR", "1")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().map_err(|err| anyhow::anyhow!("can't start {}: {err}", bin.display()))?;

    // Drained on threads, so a chatty binary can't fill a pipe and stall.
    let drain = |mut pipe: Box<dyn Read + Send>| {
        std::thread::spawn(move || {
            let mut out = Vec::new();
            let _ = pipe.read_to_end(&mut out);
            String::from_utf8_lossy(&out).into_owned()
        })
    };
    let stdout = drain(Box::new(child.stdout.take().expect("piped")));
    let stderr = drain(Box::new(child.stderr.take().expect("piped")));
    let deadline = Instant::now() + Duration::from_secs(scenario.run.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS));
    let mut timed_out = false;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            timed_out = true;
            let _ = child.kill();
            break child.wait()?;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let (stdout, stderr) = (stdout.join().unwrap_or_default(), stderr.join().unwrap_or_default());
    if let Some(out) = &mcp_out {
        // What the tracker recorded, over anything the model wrote under that name itself.
        let comments = std::fs::read(out).unwrap_or_else(|_| b"[]".to_vec());
        write_file(&project.join(TRACKER_COMMENTS), &comments)?;
    }

    let mut normalizer = Normalizer::new();
    normalizer.add(&project.to_string_lossy(), "<project>");
    normalizer.add(&home.to_string_lossy(), "<home>");
    normalizer.add(&config_dir.to_string_lossy(), "<config>");
    normalizer.add(&temp.0.to_string_lossy(), "<tmp>");
    normalizer.add(&format!("127.0.0.1:{}", server.port()), "127.0.0.1:<port>");
    if let Ok(oracle) = &oracle {
        normalizer.add(&oracle.to_string_lossy(), "<oracle>");
    }

    let recorded = server.requests();
    let is_chat = |r: &Recorded| r.method == "POST" && r.path.split('?').next().unwrap_or("").ends_with("/chat/completions");
    let raw_requests: Vec<String> = recorded.iter().filter(|r| is_chat(r)).map(|r| normalizer.text(&r.body)).collect();
    let requests = raw_requests.iter().map(|body| canonical_request(body) + "\n").collect();
    let other_requests = recorded.iter().filter(|r| !is_chat(r)).map(|r| format!("{} {}", r.method, r.path)).collect();

    Ok(Outcome {
        events: normalizer.events(&stdout),
        requests,
        raw_requests,
        files: read_tree(&project, true)?,
        exit_code: status.code(),
        stderr: normalizer.text(&stderr),
        other_requests,
        unused_replies: server.unused(),
        timed_out,
    })
}

/// The files under `root` by `/`-separated relative path; `skip_git` leaves out a top-level `.git`.
pub fn read_tree(root: &Path, skip_git: bool) -> anyhow::Result<BTreeMap<String, Vec<u8>>> {
    let mut files = BTreeMap::new();
    if !root.exists() {
        return Ok(files);
    }
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            let rel =
                path.strip_prefix(root)?.components().map(|c| c.as_os_str().to_string_lossy()).collect::<Vec<_>>().join("/");
            if skip_git && rel == ".git" {
                continue;
            }
            let kind = entry.file_type()?;
            if kind.is_dir() {
                stack.push(path);
            } else if kind.is_symlink() {
                // A link is recorded as where it points, never followed out of the project.
                files.insert(rel, format!("symlink -> {}", std::fs::read_link(&path)?.display()).into_bytes());
            } else {
                files.insert(rel, std::fs::read(&path)?);
            }
        }
    }
    Ok(files)
}

pub fn copy_dir(from: &Path, to: &Path) -> anyhow::Result<()> {
    for (rel, bytes) in read_tree(from, false)? {
        write_file(&to.join(&rel), &bytes)?;
    }
    Ok(())
}

/// Writes `bytes` to `path`, creating its folders.
pub fn write_file(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, bytes)?;
    Ok(())
}

/// A request body compared with another Marv's: canonical JSON (keys sorted, as serde_json writes
/// a `Value`), with `cache_control` breakpoints (#56) taken out. The prompt cache needs each run's
/// own requests to be byte-stable ([`crate::prefix`]), not marv-rs's bytes to equal Marv's, so
/// key order and breakpoints are left to each implementation; everything else must be the same.
pub fn canonical_request(body: &str) -> String {
    let Ok(mut value) = serde_json::from_str::<Value>(body) else { return body.to_string() };
    if let Some(messages) = value.get_mut("messages").and_then(Value::as_array_mut) {
        for message in messages {
            let Some(content) = message.get_mut("content") else { continue };
            // A marked message carries its text as one part; unmarked it's the plain string.
            if let Some([part]) = content.as_array().map(Vec::as_slice)
                && part.get("type") == Some(&json!("text"))
                && part.get("cache_control").is_some()
                && part.as_object().is_some_and(|p| p.len() == 3)
                && let Some(text) = part.get("text").cloned()
            {
                *content = text;
            }
        }
    }
    strip_cache_control(&mut value);
    value.to_string()
}

fn strip_cache_control(value: &mut Value) {
    match value {
        Value::Object(map) => {
            map.remove("cache_control");
            map.values_mut().for_each(strip_cache_control);
        }
        Value::Array(items) => items.iter_mut().for_each(strip_cache_control),
        _ => {}
    }
}

/// The `marv-oracle` binary, which serves the MCP fixtures (`marv-oracle tracker …`): this process
/// when it is that binary, else the one Cargo built next to this test.
pub fn oracle_binary() -> anyhow::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    let name = format!("marv-oracle{}", std::env::consts::EXE_SUFFIX);
    if exe.file_name().is_some_and(|n| n == name.as_str()) {
        return Ok(exe);
    }
    // Integration tests run from target/<profile>/deps; the binary is in target/<profile>.
    exe.ancestors()
        .skip(1)
        .take(2)
        .map(|dir| dir.join(&name))
        .find(|path| path.is_file())
        .ok_or_else(|| anyhow::anyhow!("no {name} binary next to {} (cargo build -p marv-oracle)", exe.display()))
}

/// `git init` and one commit, with fixed names and dates so the commit is the same every run.
fn git_init(project: &Path, home: &Path) -> anyhow::Result<()> {
    let git = |args: &[&str]| -> anyhow::Result<()> {
        let output = Command::new("git")
            .args([
                "-c",
                "user.name=oracle",
                "-c",
                "user.email=oracle@localhost",
                "-c",
                "commit.gpgSign=false",
                "-c",
                "init.defaultBranch=main",
            ])
            .args(args)
            .current_dir(project)
            .env("HOME", home)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_AUTHOR_DATE", "2026-01-01T00:00:00Z")
            .env("GIT_COMMITTER_DATE", "2026-01-01T00:00:00Z")
            .output()?;
        anyhow::ensure!(output.status.success(), "git {}: {}", args[0], String::from_utf8_lossy(&output.stderr));
        Ok(())
    };
    git(&["init", "-q"])?;
    git(&["add", "-A"])?;
    git(&["commit", "-q", "--allow-empty", "-m", "start"])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tree_round_trips_and_skips_git() {
        let temp = TempDir::new("unit-tree").unwrap();
        let (a, b) = (temp.0.join("a"), temp.0.join("b"));
        std::fs::create_dir_all(a.join("src/deep")).unwrap();
        std::fs::create_dir_all(a.join(".git")).unwrap();
        std::fs::write(a.join("src/deep/x.js"), b"x\r\n").unwrap();
        std::fs::write(a.join(".git/HEAD"), b"ref").unwrap();
        copy_dir(&a, &b).unwrap();
        let tree = read_tree(&b, true).unwrap();
        assert_eq!(tree.keys().collect::<Vec<_>>(), vec!["src/deep/x.js"]);
        assert_eq!(tree["src/deep/x.js"], b"x\r\n");
    }

    #[test]
    fn canonical_requests_sort_keys_and_drop_breakpoints() {
        let marked = r#"{"stream":true,"model":"m","messages":[{"role":"system","content":[{"type":"text","text":"S","cache_control":{"type":"ephemeral"}}]},{"role":"user","content":"hi"}]}"#;
        let plain = r#"{"model":"m","messages":[{"content":"S","role":"system"},{"content":"hi","role":"user"}],"stream":true}"#;
        assert_eq!(canonical_request(marked), canonical_request(plain));
        assert_eq!(
            canonical_request(plain),
            r#"{"messages":[{"content":"S","role":"system"},{"content":"hi","role":"user"}],"model":"m","stream":true}"#
        );
    }

    #[test]
    fn config_points_at_the_server_with_a_dummy_key() {
        let config = default_config("http://127.0.0.1:1/v1");
        assert_eq!(config["provider"], "openrouter");
        assert_eq!(config["baseUrl"], "http://127.0.0.1:1/v1");
        assert_eq!(config["apiKey"], "oracle-dummy-key");
    }
}
