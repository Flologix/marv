//! Local models through llama.cpp (#278): a GGUF file, served by a `llama-server` Marv starts on first use and
//! stops when it exits, spoken to as an OpenAI-compatible endpoint ([`marv_provider::endpoint`]).
//!
//! - **Bring your own `llama-server`** for now: `"llamaCpp": {"server": "/path/to/llama-server"}`, else the first
//!   `llama-server` on `PATH`. A pinned, checksum-checked download can come later (the issue's "managed" option).
//! - **The model** is the config's `model` (or `marv --local-model <gguf>` for one run), a path to a `.gguf` file;
//!   the window is `contextLength` (default 32768), passed as `-c` and told to the session, so it compacts in time.
//! - **One server per model and settings, per Marv process**, on a free port of 127.0.0.1 only, with a random
//!   `--api-key`: a sandboxed command with the network shares the loopback, and mustn't use (or load models into) the
//!   user's server. `--jinja` so the model's own chat template parses its tool calls. Its output goes to a private
//!   log file, whose last lines are the error when it exits while loading.
//! - **Each request waits** until `GET /health` answers 200 (503 while the model loads), up to
//!   [`LOAD_TIMEOUT`]; [`stop_all`] stops every server when Marv exits (main's `StopProcesses`).
//! - Ollama stays as it is (`ollama.rs`): this is another provider, `"provider": "llamacpp"`.
//! - Known limits: a Marv killed with SIGKILL leaves its server running (nothing outlives a normal exit); the server
//!   runs outside the sandbox, as Ollama's daemon does, and reads what the user can.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use futures::future::BoxFuture;
use marv_provider::endpoint::{Endpoint, Readiness, Ready, Wire};
use serde_json::{Map, Value};
use tokio_util::sync::CancellationToken;

use crate::config::Env;

/// How long a model may take to load before its requests give up (a big model from a slow disk).
pub const LOAD_TIMEOUT: Duration = Duration::from_secs(600);
/// What the endpoint is called in the status bar and recorded with sessions.
pub const NAME: &str = "llamacpp";

/// `"llamaCpp"` in the config: where `llama-server` is, and arguments added after Marv's (`["-ngl", "99"]`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LlamaCppSetting {
    pub server: Option<String>,
    pub args: Vec<String>,
}

impl LlamaCppSetting {
    pub fn from_value(raw: &Value) -> Result<Self, Vec<String>> {
        let Value::Object(map) = raw else {
            return Err(vec!["llamaCpp: expected an object, like {\"server\": \"/usr/local/bin/llama-server\"}".into()]);
        };
        let mut problems = Vec::new();
        if let Some(key) = map.keys().find(|k| !["server", "args"].contains(&k.as_str())) {
            problems.push(format!("llamaCpp: unknown key \"{key}\" (only \"server\" and \"args\")"));
        }
        let server = match map.get("server") {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) if !s.trim().is_empty() => Some(s.clone()),
            Some(_) => {
                problems.push("llamaCpp.server: expected the path to llama-server".into());
                None
            }
        };
        let args = match map.get("args") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(items)) if items.iter().all(Value::is_string) => {
                items.iter().filter_map(Value::as_str).map(str::to_string).collect()
            }
            Some(_) => {
                problems.push("llamaCpp.args: expected a list of strings, like [\"-ngl\", \"99\"]".into());
                Vec::new()
            }
        };
        if problems.is_empty() { Ok(Self { server, args }) } else { Err(problems) }
    }

    pub fn to_value(&self) -> Value {
        let mut map = Map::new();
        if let Some(server) = &self.server {
            map.insert("server".into(), Value::from(server.clone()));
        }
        if !self.args.is_empty() {
            map.insert("args".into(), Value::from(self.args.clone()));
        }
        Value::Object(map)
    }
}

/// `~/` at the start of a path, from `env`'s HOME.
fn expand_home(path: &str, env: &Env) -> PathBuf {
    match (path.strip_prefix("~/"), env.get("HOME")) {
        (Some(rest), Some(home)) => Path::new(home).join(rest),
        _ => PathBuf::from(path),
    }
}

/// The `llama-server` to run: the setting's, else the first on `env`'s PATH.
pub fn find_server(setting: Option<&LlamaCppSetting>, env: &Env) -> Result<PathBuf, String> {
    if let Some(server) = setting.and_then(|s| s.server.as_deref()) {
        let path = expand_home(server, env);
        return if path.is_file() {
            Ok(path)
        } else {
            Err(format!(
                "llama.cpp: there is no llama-server at {} (\"llamaCpp\": {{\"server\"}} in ~/.marv/config.json).",
                path.display()
            ))
        };
    }
    env.get("PATH")
        .into_iter()
        .flat_map(|path| path.split(':'))
        .filter(|dir| !dir.is_empty())
        .map(|dir| Path::new(dir).join("llama-server"))
        .find(|p| is_executable(p))
        .ok_or_else(|| {
            "llama.cpp: llama-server isn't on your PATH. Install llama.cpp (github.com/ggml-org/llama.cpp), or set \
             \"llamaCpp\": {\"server\": \"/path/to/llama-server\"} in ~/.marv/config.json."
                .to_string()
        })
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// The GGUF files in the usual places (`~/.cache/llama.cpp`, where `llama-server -hf` downloads; `~/models`), for
/// `/model`: paths, newest first. Any other path can be typed.
pub fn gguf_files(env: &Env) -> Vec<String> {
    let Some(home) = env.get("HOME") else { return Vec::new() };
    let cache = env.get("LLAMA_CACHE").map(PathBuf::from).unwrap_or_else(|| Path::new(home).join(".cache/llama.cpp"));
    let mut found: Vec<(std::time::SystemTime, String)> = Vec::new();
    for dir in [cache, Path::new(home).join("models")] {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten().take(500) {
            let path = entry.path();
            // A split model's later parts (`-00002-of-00003.gguf`) load with the first: only that one is listed.
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default();
            if name.ends_with(".gguf") && !(name.contains("-of-") && !name.contains("-00001-of-")) {
                let modified = entry.metadata().and_then(|m| m.modified()).unwrap_or(std::time::UNIX_EPOCH);
                found.push((modified, path.display().to_string()));
            }
        }
    }
    found.sort_by_key(|entry| std::cmp::Reverse(entry.0));
    found.into_iter().map(|(_, path)| path).collect()
}

/// What a server is started with: one server per distinct set, per process.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Spec {
    server: PathBuf,
    model: PathBuf,
    context: u64,
    args: Vec<String>,
}

/// A `llama-server` this Marv started.
struct Server {
    child: Mutex<Option<Child>>,
    port: u16,
    key: String,
    log: PathBuf,
    ready: AtomicBool,
}

static SERVERS: LazyLock<Mutex<BTreeMap<Spec, Arc<Server>>>> = LazyLock::new(Mutex::default);

/// A free port on 127.0.0.1 (closed again at once, for the server to take).
fn free_port() -> std::io::Result<u16> {
    Ok(std::net::TcpListener::bind(("127.0.0.1", 0))?.local_addr()?.port())
}

/// Where a server's output goes: Marv's cache folder, private.
fn log_path(env: &Env, port: u16) -> PathBuf {
    let base = env
        .get("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| env.get("HOME").map(|h| Path::new(h).join(".cache")))
        .unwrap_or_else(std::env::temp_dir);
    base.join("marv").join(format!("llama-server-{port}.log"))
}

fn start(spec: &Spec, env: &Env) -> Result<Server, String> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    use std::os::unix::process::CommandExt;
    let port = free_port().map_err(|e| format!("llama.cpp: no free local port: {e}"))?;
    let key = marv_provider::chatgpt::random_token();
    let log = log_path(env, port);
    if let Some(dir) = log.parent() {
        let _ = std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir);
    }
    let out = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .open(&log)
        .map_err(|e| format!("llama.cpp: couldn't open its log {}: {e}", log.display()))?;
    let err = out.try_clone().map_err(|e| e.to_string())?;
    let child = Command::new(&spec.server)
        .arg("-m")
        .arg(&spec.model)
        .args(["--host", "127.0.0.1", "--port", &port.to_string(), "--api-key", &key, "-c", &spec.context.to_string()])
        .arg("--jinja")
        .args(&spec.args)
        // Its own process group: a ctrl+c in the terminal is Marv's, and stop_all stops the whole server.
        .process_group(0)
        .env_clear()
        .envs(marv_sandbox::host_env::host_env_now())
        .stdin(Stdio::null())
        .stdout(Stdio::from(out))
        .stderr(Stdio::from(err))
        .spawn()
        .map_err(|e| format!("llama.cpp: couldn't start {}: {e}", spec.server.display()))?;
    Ok(Server { child: Mutex::new(Some(child)), port, key, log, ready: AtomicBool::new(false) })
}

/// The last lines of a server's log, for the error when it exits.
fn log_tail(log: &Path) -> String {
    let text = std::fs::read_to_string(log).unwrap_or_default();
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    lines[lines.len().saturating_sub(5)..].join("\n")
}

impl Server {
    /// `Some(why)` once the process has exited.
    fn exited(&self) -> Option<String> {
        let mut child = self.child.lock().unwrap_or_else(|e| e.into_inner());
        match child.as_mut().map(Child::try_wait) {
            None => Some("llama.cpp: its server was stopped.".into()),
            Some(Ok(Some(status))) => Some(format!(
                "llama.cpp: llama-server exited ({status}). Its last lines ({}):\n{}",
                self.log.display(),
                marv_core::printable(&log_tail(&self.log))
            )),
            Some(_) => None,
        }
    }

    fn stop(&self) {
        if let Some(mut child) = self.child.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let group = nix::unistd::Pid::from_raw(child.id() as i32);
            let _ = nix::sys::signal::killpg(group, nix::sys::signal::Signal::SIGTERM);
            let deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < deadline && matches!(child.try_wait(), Ok(None)) {
                std::thread::sleep(Duration::from_millis(20));
            }
            let _ = nix::sys::signal::killpg(group, nix::sys::signal::Signal::SIGKILL);
            let _ = child.wait();
        }
    }
}

/// `GET /health`'s status code, or None when nothing answers yet.
async fn health(port: u16) -> Option<u16> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream =
        tokio::time::timeout(Duration::from_secs(2), tokio::net::TcpStream::connect(("127.0.0.1", port))).await.ok()?.ok()?;
    stream.write_all(b"GET /health HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n").await.ok()?;
    let mut head = [0u8; 64];
    let n = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut head)).await.ok()?.ok()?;
    let line = std::str::from_utf8(&head[..n]).ok()?;
    line.split_whitespace().nth(1)?.parse().ok()
}

/// A server's readiness: its `/health`, while it hasn't exited.
struct Loading(Arc<Server>);

impl Readiness for Loading {
    fn wait(&self, cancel: CancellationToken) -> BoxFuture<'static, Result<(), String>> {
        let server = self.0.clone();
        Box::pin(async move {
            let started = Instant::now();
            loop {
                if let Some(why) = server.exited() {
                    return Err(why);
                }
                if server.ready.load(Ordering::Relaxed) {
                    return Ok(());
                }
                if health(server.port).await == Some(200) {
                    server.ready.store(true, Ordering::Relaxed);
                    return Ok(());
                }
                if started.elapsed() > LOAD_TIMEOUT {
                    return Err(format!(
                        "llama.cpp: the model didn't load in {} minutes (its log: {}).",
                        LOAD_TIMEOUT.as_secs() / 60,
                        server.log.display()
                    ));
                }
                tokio::select! {
                    () = cancel.cancelled() => return Err("Stopped while the model was loading.".into()),
                    () = tokio::time::sleep(Duration::from_millis(250)) => {}
                }
            }
        })
    }
}

/// The endpoint for `model` (a GGUF path), its server started if this process hasn't yet. What can't be used (no
/// `llama-server`, no such file) is the endpoint's `problem`: every request ends with it.
pub fn endpoint(model: &str, setting: Option<&LlamaCppSetting>, context: u64, env: &Env) -> Endpoint {
    let name = Path::new(model).file_stem().and_then(|s| s.to_str()).unwrap_or(model).to_string();
    let mut endpoint = Endpoint {
        name: NAME.into(),
        wire: Wire::OpenAiChat,
        base_url: String::new(),
        api_key: None,
        problem: None,
        headers: Vec::new(),
        extra_body: Map::new(),
        models: vec![name],
        context_length: Some(context),
        ready: None,
    };
    let path = expand_home(model, env);
    let server = match find_server(setting, env) {
        Ok(server) => server,
        Err(why) => {
            endpoint.problem = Some(why);
            return endpoint;
        }
    };
    let model = match std::fs::canonicalize(&path) {
        Ok(model) if model.is_file() => model,
        _ => {
            endpoint.problem =
                Some(format!("llama.cpp: there's no model file at {}: give a .gguf file's path with /model.", path.display()));
            return endpoint;
        }
    };
    let spec = Spec { server, model, context, args: setting.map(|s| s.args.clone()).unwrap_or_default() };
    let mut servers = SERVERS.lock().unwrap_or_else(|e| e.into_inner());
    let running = match servers.get(&spec) {
        Some(running) => running.clone(),
        None => match start(&spec, env) {
            Ok(server) => servers.entry(spec).or_insert(Arc::new(server)).clone(),
            Err(why) => {
                endpoint.problem = Some(why);
                return endpoint;
            }
        },
    };
    endpoint.base_url = format!("http://127.0.0.1:{}/v1", running.port);
    endpoint.api_key = Some(running.key.clone());
    endpoint.ready = Some(Ready(Arc::new(Loading(running))));
    endpoint
}

/// Stops every server this process started (when Marv exits).
pub fn stop_all() {
    let servers = std::mem::take(&mut *SERVERS.lock().unwrap_or_else(|e| e.into_inner()));
    for server in servers.values() {
        server.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_setting_is_read_and_saved_back() {
        let raw = json!({"server": "~/llama.cpp/build/bin/llama-server", "args": ["-ngl", "99"]});
        let setting = LlamaCppSetting::from_value(&raw).unwrap();
        assert_eq!(setting.args, ["-ngl", "99"]);
        assert_eq!(setting.to_value(), raw);
        assert!(LlamaCppSetting::from_value(&json!({"path": "x"})).unwrap_err()[0].contains("unknown key \"path\""));
        assert!(LlamaCppSetting::from_value(&json!({"args": "-ngl 99"})).is_err());
    }

    #[test]
    fn llama_server_is_found_on_the_path_or_where_the_setting_says() {
        let dir = std::env::temp_dir().join(format!("marv-llama-find-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let bin = dir.join("llama-server");
        std::fs::write(&bin, "#!/bin/sh\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        let env = Env::from([("PATH".to_string(), format!("/nonexistent:{}", dir.display()))]);
        assert_eq!(find_server(None, &env), Ok(bin.clone()));
        assert!(find_server(None, &Env::new()).unwrap_err().contains("isn't on your PATH"));
        let setting = LlamaCppSetting { server: Some("/nonexistent/llama-server".into()), args: Vec::new() };
        assert!(find_server(Some(&setting), &env).unwrap_err().contains("no llama-server at /nonexistent"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_missing_server_or_model_is_the_endpoints_problem() {
        let e = endpoint("/nonexistent/m.gguf", None, 32768, &Env::new());
        assert!(e.problem.as_deref().is_some_and(|p| p.contains("llama-server")), "{e:?}");
        assert_eq!((e.name.as_str(), e.models.as_slice(), e.context_length), (NAME, &["m".to_string()][..], Some(32768)));
        let setting = LlamaCppSetting { server: Some("/bin/sh".into()), args: Vec::new() };
        let e = endpoint("/nonexistent/m.gguf", Some(&setting), 32768, &Env::new());
        assert!(e.problem.as_deref().is_some_and(|p| p.contains("no model file at /nonexistent/m.gguf")), "{e:?}");
        assert!(e.ready.is_none());
    }
}
