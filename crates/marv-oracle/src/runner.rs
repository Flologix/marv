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
use crate::scenario::{DEFAULT_TIMEOUT_SECS, Scenario};
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
    /// `requests.jsonl`: each chat request's body, bytes as sent (after scrubbing).
    pub requests: String,
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
    copy_dir(&scenario.project(), &project)?;
    if scenario.run.git {
        git_init(&project, &home)?;
    }

    let server = FakeServer::start(scenario.replies.clone())?;
    let mut config = default_config(&server.base_url());
    config.extend(scenario.run.config.clone());
    std::fs::write(config_dir.join("config.json"), serde_json::to_string_pretty(&Value::Object(config))? + "\n")?;

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

    let mut normalizer = Normalizer::new();
    normalizer.add(&project.to_string_lossy(), "<project>");
    normalizer.add(&home.to_string_lossy(), "<home>");
    normalizer.add(&config_dir.to_string_lossy(), "<config>");
    normalizer.add(&temp.0.to_string_lossy(), "<tmp>");
    normalizer.add(&format!("127.0.0.1:{}", server.port()), "127.0.0.1:<port>");

    let recorded = server.requests();
    let is_chat = |r: &Recorded| r.method == "POST" && r.path.split('?').next().unwrap_or("").ends_with("/chat/completions");
    let requests: String = recorded.iter().filter(|r| is_chat(r)).map(|r| normalizer.text(&r.body) + "\n").collect();
    let other_requests = recorded.iter().filter(|r| !is_chat(r)).map(|r| format!("{} {}", r.method, r.path)).collect();

    Ok(Outcome {
        events: normalizer.events(&stdout),
        requests,
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
        let path = to.join(&rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, bytes)?;
    }
    Ok(())
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
    fn config_points_at_the_server_with_a_dummy_key() {
        let config = default_config("http://127.0.0.1:1/v1");
        assert_eq!(config["provider"], "openrouter");
        assert_eq!(config["baseUrl"], "http://127.0.0.1:1/v1");
        assert_eq!(config["apiKey"], "oracle-dummy-key");
    }
}
