//! Custom providers (#275): `"providers"` in `~/.marv/config.json`, endpoints the user names (a company gateway,
//! LiteLLM, vLLM, a proxy), chosen with `"provider": "<name>"` like a built-in one.
//!
//! ```json
//! "providers": {
//!   "acme-gateway": {
//!     "baseUrl": "https://llm.acme.internal/v1",
//!     "wire": "openai-chat",
//!     "key": "env:ACME_LLM_KEY",
//!     "headers": {"X-Team": "platform"},
//!     "extraBody": {"service_tier": "priority"},
//!     "models": ["acme-large"]
//!   }
//! }
//! ```
//!
//! - **Only from the user's own file**, like `hooks` and `mesh`: there is no project config layer, so a repository
//!   can't add an endpoint or point a provider elsewhere ([`crate::config`]'s header).
//! - **The key** is `env:NAME` (that variable), `command:...` (a helper run once per process on the host, with the
//!   host door's environment, that prints the key: a password manager, `gcloud auth print-access-token`), or the key
//!   itself. It goes only to the endpoint's `baseUrl` ([`marv_provider::endpoint`]), never into the sandbox, whose
//!   environment is cleared.
//! - `wire` defaults to `openai-chat`. `anthropic` and `gemini` come with those providers (#276, #277).

use std::collections::BTreeMap;
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use marv_provider::endpoint::{Endpoint, Wire};
use serde_json::{Map, Value};

use crate::config::Env;

/// How long a `command:` key helper may take (a password manager may wait for its unlock).
const KEY_COMMAND_TIMEOUT: Duration = Duration::from_secs(30);

/// One entry of `"providers"`, as written.
#[derive(Debug, Clone, PartialEq)]
pub struct CustomProvider {
    pub base_url: String,
    pub wire: Wire,
    /// `env:NAME`, `command:...`, or the key itself; `None`: no key (a local server).
    pub key: Option<String>,
    pub headers: Vec<(String, String)>,
    pub extra_body: Map<String, Value>,
    pub models: Vec<String>,
}

/// Whether `name` can name a custom provider: letters, digits, `.`, `_` and `-`, and not a built-in provider's id.
pub fn valid_name(name: &str) -> bool {
    const TAKEN: &[&str] = &["openrouter", "ollama", "zai", "chatgpt", "kimi", "deepseek", "qwen", "custom"];
    !name.is_empty()
        && name.len() <= 64
        && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        && !TAKEN.contains(&name.to_ascii_lowercase().as_str())
}

/// A header name as HTTP allows it (a token), and a value without line breaks.
fn valid_header(name: &str, value: &str) -> bool {
    !name.is_empty()
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
        && !value.contains(['\r', '\n'])
}

/// `"providers"` read from the file; each problem names the field.
pub fn parse(raw: &Value) -> Result<BTreeMap<String, CustomProvider>, Vec<String>> {
    let Value::Object(entries) = raw else {
        return Err(vec!["providers: expected an object, like {\"acme-gateway\": {\"baseUrl\": \"https://…/v1\"}}".into()]);
    };
    let mut problems = Vec::new();
    let mut out = BTreeMap::new();
    for (name, entry) in entries {
        let at = format!("providers.{name}");
        if !valid_name(name) {
            problems.push(format!(
                "{at}: a name is letters, digits, \".\", \"_\" and \"-\", and not a built-in provider's"
            ));
            continue;
        }
        let Value::Object(entry) = entry else {
            problems.push(format!("{at}: expected an object with \"baseUrl\""));
            continue;
        };
        const KEYS: &[&str] = &["baseUrl", "wire", "key", "headers", "extraBody", "models"];
        for key in entry.keys().filter(|k| !KEYS.contains(&k.as_str())) {
            problems.push(format!("{at}: unknown key \"{key}\" (only {})", KEYS.join(", ")));
        }
        let base_url = match entry.get("baseUrl").and_then(Value::as_str) {
            Some(url) if url.starts_with("http://") || url.starts_with("https://") => url.trim_end_matches('/').to_string(),
            _ => {
                problems.push(format!("{at}.baseUrl: expected an http:// or https:// URL"));
                String::new()
            }
        };
        let wire = match entry.get("wire") {
            None | Some(Value::Null) => Wire::OpenAiChat,
            Some(v) => match v.as_str().and_then(Wire::parse) {
                Some(wire) => wire,
                None => {
                    problems.push(format!("{at}.wire: expected \"openai-chat\" or \"openai-responses\""));
                    Wire::OpenAiChat
                }
            },
        };
        let key = match entry.get("key") {
            None | Some(Value::Null) => None,
            Some(Value::String(key)) if !key.trim().is_empty() && key != "env:" && key != "command:" => Some(key.clone()),
            Some(_) => {
                problems.push(format!("{at}.key: expected \"env:NAME\", \"command:...\" or the key"));
                None
            }
        };
        let mut headers = Vec::new();
        match entry.get("headers") {
            None | Some(Value::Null) => {}
            Some(Value::Object(map)) => {
                for (name, value) in map {
                    match value.as_str().filter(|v| valid_header(name, v)) {
                        Some(value) => headers.push((name.clone(), value.to_string())),
                        None => problems.push(format!("{at}.headers.{name}: expected a header name and a one-line string")),
                    }
                }
            }
            Some(_) => problems.push(format!("{at}.headers: expected an object of strings")),
        }
        let extra_body = match entry.get("extraBody") {
            None | Some(Value::Null) => Map::new(),
            Some(Value::Object(map)) => map.clone(),
            Some(_) => {
                problems.push(format!("{at}.extraBody: expected an object"));
                Map::new()
            }
        };
        let models = match entry.get("models") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(items)) if items.iter().all(|i| i.as_str().is_some_and(|s| !s.is_empty())) => {
                items.iter().filter_map(Value::as_str).map(str::to_string).collect()
            }
            Some(_) => {
                problems.push(format!("{at}.models: expected a list of model ids"));
                Vec::new()
            }
        };
        out.insert(name.clone(), CustomProvider { base_url, wire, key, headers, extra_body, models });
    }
    if problems.is_empty() { Ok(out) } else { Err(problems) }
}

/// `"providers"` as saved: as it was read (setup rewrites the file and must keep them).
pub fn to_value(providers: &BTreeMap<String, CustomProvider>) -> Value {
    let entries = providers.iter().map(|(name, p)| {
        let mut entry = Map::new();
        entry.insert("baseUrl".into(), Value::from(p.base_url.clone()));
        entry.insert("wire".into(), Value::from(p.wire.as_str()));
        if let Some(key) = &p.key {
            entry.insert("key".into(), Value::from(key.clone()));
        }
        if !p.headers.is_empty() {
            let headers = p.headers.iter().map(|(k, v)| (k.clone(), Value::from(v.clone()))).collect();
            entry.insert("headers".into(), Value::Object(headers));
        }
        if !p.extra_body.is_empty() {
            entry.insert("extraBody".into(), Value::Object(p.extra_body.clone()));
        }
        if !p.models.is_empty() {
            entry.insert("models".into(), Value::from(p.models.clone()));
        }
        (name.clone(), Value::Object(entry))
    });
    Value::Object(entries.collect())
}

/// Runs a `command:` key helper: `sh -c`, on the host, with the host door's environment (Marv's own keys left
/// out), stdin closed, at most [`KEY_COMMAND_TIMEOUT`]. Its first line of output is the key.
pub type RunKeyCommand = dyn Fn(&str) -> Result<String, String> + Send + Sync;

fn run_key_command(command: &str) -> Result<String, String> {
    use std::io::Read;
    use std::process::{Command, Stdio};
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(command)
        .env_clear()
        .envs(marv_sandbox::host_env::host_env_now())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("couldn't run it: {e}"))?;
    let started = std::time::Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
            break status;
        }
        if started.elapsed() > KEY_COMMAND_TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("it took more than {} s", KEY_COMMAND_TIMEOUT.as_secs()));
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let mut out = String::new();
    if let Some(mut stdout) = child.stdout.take() {
        let _ = stdout.read_to_string(&mut out);
    }
    if !status.success() {
        return Err(format!("it exited with {status}"));
    }
    out.lines().next().map(str::trim).filter(|k| !k.is_empty()).map(str::to_string).ok_or_else(|| "it printed nothing".into())
}

/// A `command:` helper's answers, by command: each runs once per process (the config is resolved more than once).
static KEY_COMMANDS: LazyLock<Mutex<BTreeMap<String, Result<String, String>>>> = LazyLock::new(Mutex::default);

fn cached_key_command(command: &str) -> Result<String, String> {
    let mut cache = KEY_COMMANDS.lock().unwrap_or_else(|e| e.into_inner());
    cache.entry(command.to_string()).or_insert_with(|| run_key_command(command)).clone()
}

/// The key a `key` field stands for; `Err` says why there is none, as a sentence naming the provider.
pub fn resolve_key(name: &str, key: &str, env: &Env, run: &RunKeyCommand) -> Result<String, String> {
    if let Some(var) = key.strip_prefix("env:") {
        return env
            .get(var.trim())
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .ok_or_else(|| format!("{name}: its key is read from ${}, which isn't set.", var.trim()));
    }
    if let Some(command) = key.strip_prefix("command:") {
        return run(command.trim()).map_err(|why| format!("{name}: its key command failed ({why})."));
    }
    Ok(key.to_string())
}

/// The endpoint `name` stands for, its key resolved (a `command:` helper is run once per process).
pub fn endpoint(name: &str, provider: &CustomProvider, env: &Env) -> Endpoint {
    endpoint_with(name, provider, env, &cached_key_command)
}

/// [`endpoint`] with a given helper runner (tests).
pub fn endpoint_with(name: &str, provider: &CustomProvider, env: &Env, run: &RunKeyCommand) -> Endpoint {
    let (api_key, problem) = match provider.key.as_deref().map(|key| resolve_key(name, key, env, run)) {
        None => (None, None),
        Some(Ok(key)) => (Some(key), None),
        Some(Err(why)) => (None, Some(why)),
    };
    Endpoint {
        name: name.to_string(),
        wire: provider.wire,
        base_url: provider.base_url.clone(),
        api_key,
        problem,
        headers: provider.headers.clone(),
        extra_body: provider.extra_body.clone(),
        models: provider.models.clone(),
        context_length: None,
        ready: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn acme() -> Value {
        json!({"acme-gateway": {
            "baseUrl": "https://llm.acme.internal/v1/",
            "key": "env:ACME_LLM_KEY",
            "headers": {"X-Team": "platform"},
            "extraBody": {"service_tier": "priority"}
        }})
    }

    #[test]
    fn an_entry_is_read_with_its_defaults_and_saved_back() {
        let providers = parse(&acme()).unwrap();
        let acme = &providers["acme-gateway"];
        assert_eq!(acme.base_url, "https://llm.acme.internal/v1");
        assert_eq!(acme.wire, Wire::OpenAiChat);
        assert_eq!(acme.headers, [("X-Team".to_string(), "platform".to_string())]);
        assert_eq!(parse(&to_value(&providers)).unwrap(), providers);
    }

    #[test]
    fn mistakes_are_named_by_field() {
        let err = |v: Value| parse(&v).unwrap_err().join("\n");
        assert!(err(json!({"openrouter": {"baseUrl": "https://x"}})).contains("providers.openrouter: a name"));
        assert!(err(json!({"a b": {"baseUrl": "https://x"}})).contains("providers.a b"));
        assert!(err(json!({"g": {"baseUrl": "ftp://x"}})).contains("providers.g.baseUrl"));
        assert!(err(json!({"g": {"baseUrl": "https://x", "wire": "anthropic"}})).contains("providers.g.wire"));
        assert!(err(json!({"g": {"baseUrl": "https://x", "base_url": "https://y"}})).contains("unknown key \"base_url\""));
        assert!(err(json!({"g": {"baseUrl": "https://x", "headers": {"X-A": "a\r\nX-B: b"}}})).contains("headers.X-A"));
        assert!(err(json!({"g": {"baseUrl": "https://x", "key": "env:"}})).contains("providers.g.key"));
        assert!(err(json!({"g": {"baseUrl": "https://x", "models": ["", "m"]}})).contains("providers.g.models"));
        assert!(err(json!(["g"])).contains("providers: expected an object"));
    }

    #[test]
    fn a_key_comes_from_a_variable_a_command_or_the_field() {
        let env = Env::from([("ACME_LLM_KEY".to_string(), " sk-acme \n".to_string())]);
        let never = |_: &str| -> Result<String, String> { panic!("no command here") };
        assert_eq!(resolve_key("g", "env:ACME_LLM_KEY", &env, &never), Ok("sk-acme".into()));
        assert_eq!(resolve_key("g", "env:MISSING", &env, &never), Err("g: its key is read from $MISSING, which isn't set.".into()));
        assert_eq!(resolve_key("g", "sk-literal", &env, &never), Ok("sk-literal".into()));
        let helper = |c: &str| if c == "pass show acme" { Ok("sk-pass".into()) } else { Err("no".into()) };
        assert_eq!(resolve_key("g", "command: pass show acme", &env, &helper), Ok("sk-pass".into()));
        assert_eq!(resolve_key("g", "command:false", &env, &helper), Err("g: its key command failed (no).".into()));
    }

    #[test]
    fn a_key_command_runs_on_the_host_and_its_first_line_is_the_key() {
        assert_eq!(run_key_command("printf 'sk-one\\nsecond\\n'"), Ok("sk-one".into()));
        assert!(run_key_command("exit 3").unwrap_err().contains("exited"));
        assert_eq!(run_key_command("true"), Err("it printed nothing".into()));
    }

    #[test]
    fn a_key_that_cant_be_read_is_the_endpoints_problem_not_a_missing_header() {
        let providers = parse(&acme()).unwrap();
        let never = |_: &str| -> Result<String, String> { Err("unused".into()) };
        let e = endpoint_with("acme-gateway", &providers["acme-gateway"], &Env::new(), &never);
        assert_eq!(e.api_key, None);
        assert!(e.problem.as_deref().is_some_and(|p| p.contains("$ACME_LLM_KEY")));
    }
}
