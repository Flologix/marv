//! Configuration: presets, the file on disk, env overrides, and the provider factory. Ports Marv's
//! `src/config/config.ts` (and `providerFactory` from `src/provider/factory.ts`, which needs `resolve_config`).
//!
//! Two layers, later wins: `~/.marv/config.json` (validated, written 0600 because it can hold the API key), then env
//! vars (`OPENROUTER_API_KEY`, `MARV_MODEL`, `OLLAMA_HOST`, `MARV_MAX_STEPS`). [`FileConfig`] is what's on disk;
//! [`resolve_config`] produces the [`Config`] the app runs on.
//!
//! **Security (docs/SECURITY.md):** the config folder comes only from `~/.marv` or `MARV_CONFIG_DIR` *in the real
//! process environment* ([`default_config_dir`] is handed `std::env::vars()` by the CLI). It is never read from a
//! project file (a repository's `.env`, `bunfig.toml`…), which could otherwise point Marv at a planted config with
//! its own MCP servers and memory.
//!
//! Port notes:
//! - zod's schema is a hand check in [`FileConfig::from_value`]: an unknown provider (the old "echo") is dropped, not
//!   rejected, so setup opens instead of Marv refusing to start; error wording differs from `z.prettifyError`.
//! - `providerFactory` can't fail on an unknown kind here: [`ProviderOption`] is an enum.

use std::collections::BTreeMap;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use marv_mcp::deferred::McpToolsSetting;
use marv_provider::factory::{ConfigFactory, CustomFactory};
use marv_provider::{ProviderConfig, ProviderFactory, ProviderKind, ProviderOption};
use serde_json::{Map, Value};

/// Marv's own environment, as a map (the CLI passes the real one).
pub type Env = BTreeMap<String, String>;

/// The agent loop's default step limit.
pub const DEFAULT_MAX_STEPS: u32 = marv_agent::agent::DEFAULT_MAX_STEPS;
/// 32k fits fully on a 12 GB GPU for 9-12B models and holds a fair amount of code.
pub const DEFAULT_CONTEXT_LENGTH: u64 = 32_768;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Preset {
    pub label: &'static str,
    /// Where the API lives.
    pub base_url: &'static str,
    /// Env var that overrides the saved key; `None` when no key is needed.
    pub key_env: Option<&'static str>,
    pub key_url: Option<&'static str>,
    /// Used until a model is picked; empty means "must pick one in setup".
    pub default_model: &'static str,
}

/// `PRESETS`: OpenRouter through the OpenAI-compatible adapter, Ollama through its native API.
pub fn preset(kind: ProviderKind) -> Preset {
    match kind {
        ProviderKind::OpenRouter => Preset {
            label: "OpenRouter",
            base_url: "https://openrouter.ai/api/v1",
            key_env: Some("OPENROUTER_API_KEY"),
            key_url: Some("openrouter.ai/settings/keys"),
            default_model: "anthropic/claude-sonnet-5.5",
        },
        ProviderKind::Ollama => {
            Preset { label: "Ollama", base_url: "http://localhost:11434", key_env: None, key_url: None, default_model: "" }
        }
    }
}

/// The saved step setting: a whole number (0 never asks, 1 is refused) or "off".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaxStepsSetting {
    Every(u32),
    Off,
}

/// Exactly what is stored on disk (`~/.marv/config.json`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FileConfig {
    pub provider: Option<ProviderKind>,
    pub model: Option<String>,
    /// The OpenRouter key.
    pub api_key: Option<String>,
    /// Overrides the preset endpoint, e.g. Ollama on another machine.
    pub base_url: Option<String>,
    pub thinking: Option<bool>,
    /// Ollama's context window (num_ctx), at least 2048.
    pub context_length: Option<u64>,
    pub sandbox: Option<bool>,
    pub yolo: Option<bool>,
    pub trajectories: Option<bool>,
    pub diagnostics: Option<bool>,
    pub checks: Option<bool>,
    pub mcp_tools: Option<McpToolsSetting>,
    pub max_steps: Option<MaxStepsSetting>,
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct ConfigError(pub String);

impl FileConfig {
    /// Validates a parsed config file (zod's `FileConfigSchema`). Unknown keys are ignored, like zod's object.
    pub fn from_value(raw: &Value) -> Result<Self, String> {
        let Value::Object(map) = raw else {
            return Err("expected an object".into());
        };
        let mut problems = Vec::new();
        let string = |key: &str, problems: &mut Vec<String>| match map.get(key) {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
            Some(_) => {
                problems.push(format!("{key}: expected a non-empty string"));
                None
            }
        };
        let boolean = |key: &str, problems: &mut Vec<String>| match map.get(key) {
            None | Some(Value::Null) => None,
            Some(Value::Bool(b)) => Some(*b),
            Some(_) => {
                problems.push(format!("{key}: expected a boolean"));
                None
            }
        };
        let provider = match map.get("provider").and_then(Value::as_str) {
            Some("openrouter") => Some(ProviderKind::OpenRouter),
            Some("ollama") => Some(ProviderKind::Ollama),
            _ => None, // a provider Marv no longer has is dropped, so setup opens
        };
        let base_url = string("baseUrl", &mut problems);
        if let Some(url) = &base_url
            && !(url.contains("://") && url.split("://").next().is_some_and(|s| !s.is_empty()))
        {
            problems.push("baseUrl: expected a URL".into());
        }
        let context_length = match map.get("contextLength") {
            None | Some(Value::Null) => None,
            Some(v) => match v.as_u64() {
                Some(n) if n >= 2048 => Some(n),
                _ => {
                    problems.push("contextLength: expected a whole number of at least 2048".into());
                    None
                }
            },
        };
        let mcp_tools = match map.get("mcpTools") {
            None | Some(Value::Null) => None,
            Some(v) => match serde_json::from_value::<McpToolsSetting>(v.clone()) {
                Ok(s) => Some(s),
                Err(_) => {
                    problems.push("mcpTools: expected auto, deferred or eager".into());
                    None
                }
            },
        };
        let max_steps = match map.get("maxSteps") {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) if s == "off" => Some(MaxStepsSetting::Off),
            // 1 is useless (it would ask after every step), so the floor is 2.
            Some(v) => match v.as_u64() {
                Some(1) => {
                    problems.push("maxSteps: too small: the smallest interval is 2".into());
                    None
                }
                Some(n) if n <= u32::MAX as u64 => Some(MaxStepsSetting::Every(n as u32)),
                _ => {
                    problems.push("maxSteps: expected a whole number of at least 0, or \"off\"".into());
                    None
                }
            },
        };
        let config = Self {
            provider,
            model: string("model", &mut problems),
            api_key: string("apiKey", &mut problems),
            base_url,
            thinking: boolean("thinking", &mut problems),
            context_length,
            sandbox: boolean("sandbox", &mut problems),
            yolo: boolean("yolo", &mut problems),
            trajectories: boolean("trajectories", &mut problems),
            diagnostics: boolean("diagnostics", &mut problems),
            checks: boolean("checks", &mut problems),
            mcp_tools,
            max_steps,
        };
        if problems.is_empty() { Ok(config) } else { Err(problems.join("\n")) }
    }

    /// As saved: Marv's key names, absent fields left out.
    pub fn to_value(&self) -> Value {
        let mut map = Map::new();
        let mut put = |k: &str, v: Option<Value>| {
            if let Some(v) = v {
                map.insert(k.into(), v);
            }
        };
        put("provider", self.provider.map(|p| Value::from(p.id())));
        put("model", self.model.clone().map(Value::from));
        put("apiKey", self.api_key.clone().map(Value::from));
        put("baseUrl", self.base_url.clone().map(Value::from));
        put("thinking", self.thinking.map(Value::from));
        put("contextLength", self.context_length.map(Value::from));
        put("sandbox", self.sandbox.map(Value::from));
        put("yolo", self.yolo.map(Value::from));
        put("trajectories", self.trajectories.map(Value::from));
        put("diagnostics", self.diagnostics.map(Value::from));
        put("checks", self.checks.map(Value::from));
        put("mcpTools", self.mcp_tools.and_then(|m| serde_json::to_value(m).ok()));
        put(
            "maxSteps",
            self.max_steps.map(|m| match m {
                MaxStepsSetting::Off => Value::from("off"),
                MaxStepsSetting::Every(n) => Value::from(n),
            }),
        );
        Value::Object(map)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeySource {
    Env,
    File,
}

/// What the app actually runs with, after env overrides are applied.
#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub provider: ProviderKind,
    pub model: String,
    pub base_url: String,
    pub api_key: Option<String>,
    pub api_key_source: Option<KeySource>,
    pub thinking: bool,
    pub context_length: u64,
    pub sandbox: bool,
    pub yolo: bool,
    pub trajectories: bool,
    pub diagnostics: bool,
    pub checks: bool,
    pub mcp_tools: McpToolsSetting,
    /// 0 never asks; 1 can't happen. Subagents' own hard limit (50) is not touched by 0.
    pub max_steps: u32,
}

impl Config {
    /// The provider part, for `marv_provider::create_provider` and `ConfigFactory`.
    pub fn provider_config(&self) -> ProviderConfig {
        ProviderConfig {
            provider: self.provider,
            model: self.model.clone(),
            base_url: self.base_url.clone(),
            api_key: self.api_key.clone(),
            context_length: self.context_length,
            thinking: self.thinking,
        }
    }
}

/// `~/.marv`, or `MARV_CONFIG_DIR` from the **real** environment (never a project file).
pub fn default_config_dir(env: &Env, home: &Path) -> PathBuf {
    env.get("MARV_CONFIG_DIR").map(PathBuf::from).unwrap_or_else(|| home.join(".marv"))
}

/// `MARV_MAX_STEPS`, like `MARV_MODEL`: an env var beats the saved file, so an orchestrator can start workers that
/// never stop without editing anyone's config. "off" (any case) and "0" are 0; anything unparsable, negative or 1 is
/// ignored (the file value, or the default, stands).
pub fn max_steps_from_env(value: Option<&str>, fallback: u32) -> u32 {
    let Some(text) = value.map(str::trim).filter(|t| !t.is_empty()) else {
        return fallback;
    };
    if text.eq_ignore_ascii_case("off") {
        return 0;
    }
    match text.parse::<u32>() {
        Ok(1) | Err(_) => fallback,
        Ok(n) => n,
    }
}

/// `OLLAMA_HOST` is Ollama's own setting ("127.0.0.1:11434" or a full URL); honor it.
pub fn ollama_url(host: Option<&str>) -> String {
    let Some(host) = host.filter(|h| !h.trim().is_empty()) else {
        return preset(ProviderKind::Ollama).base_url.into();
    };
    let url =
        if host.starts_with("http://") || host.starts_with("https://") { host.to_string() } else { format!("http://{host}") };
    url.trim_end_matches('/').to_string()
}

pub fn resolve_config(file: Option<&FileConfig>, env: &Env) -> Config {
    let provider = file.and_then(|f| f.provider).unwrap_or(ProviderKind::OpenRouter);
    let preset = preset(provider);
    let env_key = preset.key_env.and_then(|k| env.get(k)).map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
    let api_key = if preset.key_env.is_some() { env_key.clone().or_else(|| file.and_then(|f| f.api_key.clone())) } else { None };
    let file_steps = match file.and_then(|f| f.max_steps) {
        Some(MaxStepsSetting::Off) => 0,
        Some(MaxStepsSetting::Every(n)) => n,
        None => DEFAULT_MAX_STEPS,
    };
    Config {
        provider,
        model: env
            .get("MARV_MODEL")
            .map(|m| m.trim().to_string())
            .filter(|m| !m.is_empty())
            .or_else(|| file.and_then(|f| f.model.clone()))
            .unwrap_or_else(|| preset.default_model.into()),
        base_url: file.and_then(|f| f.base_url.clone()).unwrap_or_else(|| match provider {
            ProviderKind::Ollama => ollama_url(env.get("OLLAMA_HOST").map(String::as_str)),
            ProviderKind::OpenRouter => preset.base_url.into(),
        }),
        api_key_source: if env_key.is_some() {
            Some(KeySource::Env)
        } else if api_key.is_some() {
            Some(KeySource::File)
        } else {
            None
        },
        api_key,
        thinking: file.and_then(|f| f.thinking).unwrap_or(false),
        context_length: file.and_then(|f| f.context_length).unwrap_or(DEFAULT_CONTEXT_LENGTH),
        sandbox: file.and_then(|f| f.sandbox).unwrap_or(true),
        yolo: file.and_then(|f| f.yolo).unwrap_or(true),
        trajectories: file.and_then(|f| f.trajectories).unwrap_or(true),
        diagnostics: file.and_then(|f| f.diagnostics).unwrap_or(true),
        checks: file.and_then(|f| f.checks).unwrap_or(true),
        mcp_tools: file.and_then(|f| f.mcp_tools).unwrap_or_default(),
        max_steps: max_steps_from_env(env.get("MARV_MAX_STEPS").map(String::as_str), file_steps),
    }
}

/// First run, no provider chosen, a provider that needs a key we don't have, or no model picked yet.
pub fn needs_setup(file: Option<&FileConfig>, config: &Config) -> bool {
    if file.and_then(|f| f.provider).is_none() || config.model.is_empty() {
        return true;
    }
    preset(config.provider).key_env.is_some() && config.api_key.is_none()
}

/// `sk-or-v1-abc…wxyz`: enough to recognise a key without revealing it.
pub fn mask_key(key: &str) -> String {
    let chars: Vec<char> = key.chars().collect();
    if chars.len() > 16 {
        format!("{}…{}", chars[..10].iter().collect::<String>(), chars[chars.len() - 4..].iter().collect::<String>())
    } else {
        "****".into()
    }
}

/// `~/.marv/config.json`.
pub struct ConfigStore {
    pub path: PathBuf,
}

impl ConfigStore {
    pub fn new(dir: &Path) -> Self {
        Self { path: dir.join("config.json") }
    }

    /// `None` on first run (no file yet). A broken file is a [`ConfigError`] naming it.
    pub fn load(&self) -> Result<Option<FileConfig>, ConfigError> {
        let text = match std::fs::read_to_string(&self.path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => {
                return Err(ConfigError(format!("{} can't be read ({e}).", self.path.display())));
            }
        };
        let raw: Value =
            serde_json::from_str(&text).map_err(|_| ConfigError(format!("{} is not valid JSON.", self.path.display())))?;
        FileConfig::from_value(&raw)
            .map(Some)
            .map_err(|why| ConfigError(format!("Invalid config in {}:\n{why}", self.path.display())))
    }

    /// The file can hold an API key, so only the current user may read it (0600 in a 0700 folder).
    pub fn save(&self, config: &FileConfig) -> std::io::Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
        }
        let text = serde_json::to_string_pretty(&config.to_value()).map_err(std::io::Error::other)? + "\n";
        std::fs::write(&self.path, text)?;
        std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o600)) // `mode` only applies on create
    }
}

/// Marv used to be called Ekko and kept its config in `~/.ekko`: copied once to `~/.marv`, privately. Only for the
/// default location. Returns whether it copied.
pub fn migrate_legacy_config(home: &Path) -> std::io::Result<bool> {
    let from = home.join(".ekko").join("config.json");
    let to = home.join(".marv").join("config.json");
    if to.exists() || !from.exists() {
        return Ok(false);
    }
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(home.join(".marv"))?;
    std::fs::copy(&from, &to)?;
    std::fs::set_permissions(&to, std::fs::Permissions::from_mode(0o600))?;
    Ok(true)
}

/// The SDK's way to a provider factory (Marv's `providerFactory`). Environment variables are ignored on this path: a
/// library uses what it's given, and errors name the option to change (`SDK_HINTS`).
pub fn provider_factory(option: ProviderOption, thinking: bool) -> Arc<dyn ProviderFactory> {
    let (file, env) = match option {
        ProviderOption::Custom(provider) => return Arc::new(CustomFactory::new(provider)),
        ProviderOption::OpenRouter { api_key, model, base_url } => (
            FileConfig {
                provider: Some(ProviderKind::OpenRouter),
                api_key: Some(api_key),
                model,
                base_url,
                ..FileConfig::default()
            },
            Env::new(),
        ),
        ProviderOption::Ollama { model, host, context_length } => (
            FileConfig { provider: Some(ProviderKind::Ollama), model: Some(model), context_length, ..FileConfig::default() },
            host.map(|h| Env::from([("OLLAMA_HOST".to_string(), h)])).unwrap_or_default(),
        ),
    };
    let config = Config { thinking, ..resolve_config(Some(&file), &env) };
    Arc::new(ConfigFactory {
        config: config.provider_config(),
        make: Arc::new(|config, info| marv_provider::create_provider(config, info, Some(marv_provider::errors::SDK_HINTS))),
        load: Arc::new(|_kind, _base_url| {
            todo!(
                "port: the SDK's model list: marv_provider::models::list_models needs a reqwest::Client marv-session can't make (upstream gap)"
            )
        }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn file() -> FileConfig {
        FileConfig {
            provider: Some(ProviderKind::OpenRouter),
            model: Some("z-ai/glm-5.3".into()),
            api_key: Some("sk-from-file".into()),
            ..FileConfig::default()
        }
    }
    fn env(pairs: &[(&str, &str)]) -> Env {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn uses_the_file_and_the_preset_endpoint_without_env_vars() {
        let c = resolve_config(Some(&file()), &Env::new());
        assert_eq!(c.base_url, "https://openrouter.ai/api/v1");
        assert_eq!(c.api_key_source, Some(KeySource::File));
        assert_eq!(
            (c.thinking, c.context_length, c.sandbox, c.yolo, c.trajectories, c.diagnostics, c.checks),
            (false, 32768, true, true, true, true, true)
        );
        assert_eq!((c.max_steps, c.mcp_tools), (25, McpToolsSetting::Auto));
    }

    #[test]
    fn env_vars_override_the_file() {
        let c =
            resolve_config(Some(&file()), &env(&[("OPENROUTER_API_KEY", "sk-from-env"), ("MARV_MODEL", "openai/gpt-5.6-luna")]));
        assert_eq!(
            (c.api_key.as_deref(), c.api_key_source, c.model.as_str()),
            (Some("sk-from-env"), Some(KeySource::Env), "openai/gpt-5.6-luna")
        );
    }

    #[test]
    fn max_steps_file_env_and_off() {
        let with = |m| FileConfig { max_steps: Some(m), ..file() };
        assert_eq!(resolve_config(Some(&with(MaxStepsSetting::Every(40))), &Env::new()).max_steps, 40);
        assert_eq!(resolve_config(Some(&with(MaxStepsSetting::Every(0))), &Env::new()).max_steps, 0);
        assert_eq!(resolve_config(Some(&with(MaxStepsSetting::Off)), &Env::new()).max_steps, 0);
        assert_eq!(resolve_config(Some(&file()), &env(&[("MARV_MAX_STEPS", "50")])).max_steps, 50);
        assert_eq!(resolve_config(Some(&with(MaxStepsSetting::Every(40))), &env(&[("MARV_MAX_STEPS", "0")])).max_steps, 0);
        assert_eq!(resolve_config(Some(&file()), &env(&[("MARV_MAX_STEPS", "OFF")])).max_steps, 0);
        for junk in ["junk", "-5", "2.5", "1", ""] {
            assert_eq!(
                resolve_config(Some(&with(MaxStepsSetting::Every(40))), &env(&[("MARV_MAX_STEPS", junk)])).max_steps,
                40,
                "{junk}"
            );
        }
    }

    #[test]
    fn a_bad_max_steps_in_the_file_is_rejected() {
        for bad in [json!(-1), json!(2.5), json!("sometimes"), json!(1)] {
            assert!(FileConfig::from_value(&json!({"provider": "openrouter", "maxSteps": bad})).is_err());
        }
        assert!(FileConfig::from_value(&json!({"mcpTools": "sometimes"})).is_err());
    }

    #[test]
    fn a_provider_marv_no_longer_has_is_dropped_so_setup_opens() {
        let f = FileConfig::from_value(&json!({"provider": "echo", "apiKey": "sk-or-keep"})).unwrap();
        assert_eq!(f, FileConfig { api_key: Some("sk-or-keep".into()), ..FileConfig::default() });
        assert!(needs_setup(Some(&f), &resolve_config(Some(&f), &Env::new())));
    }

    #[test]
    fn defaults_with_no_file_and_ollama_rules() {
        let c = resolve_config(None, &Env::new());
        assert_eq!((c.provider, c.model.as_str()), (ProviderKind::OpenRouter, "anthropic/claude-sonnet-5.5"));
        let ollama = FileConfig {
            provider: Some(ProviderKind::Ollama),
            model: Some("qwen3.5:9b".into()),
            api_key: Some("sk-or".into()),
            ..FileConfig::default()
        };
        let c = resolve_config(Some(&ollama), &env(&[("OPENROUTER_API_KEY", "k")]));
        assert_eq!((c.base_url.as_str(), c.api_key, c.api_key_source), ("http://localhost:11434", None, None));
        let m = FileConfig { provider: Some(ProviderKind::Ollama), model: Some("m".into()), ..FileConfig::default() };
        assert_eq!(resolve_config(Some(&m), &env(&[("OLLAMA_HOST", "10.0.0.5:11434")])).base_url, "http://10.0.0.5:11434");
        assert_eq!(resolve_config(Some(&m), &env(&[("OLLAMA_HOST", "https://box.lan/")])).base_url, "https://box.lan");
        let gpu = FileConfig { base_url: Some("http://gpu:11434".into()), ..m };
        assert_eq!(resolve_config(Some(&gpu), &Env::new()).base_url, "http://gpu:11434");
    }

    #[test]
    fn needs_setup_cases() {
        assert!(needs_setup(None, &resolve_config(None, &env(&[("OPENROUTER_API_KEY", "k")]))));
        let f = FileConfig { provider: Some(ProviderKind::OpenRouter), model: Some("z".into()), ..FileConfig::default() };
        assert!(needs_setup(Some(&f), &resolve_config(Some(&f), &Env::new())));
        assert!(!needs_setup(Some(&f), &resolve_config(Some(&f), &env(&[("OPENROUTER_API_KEY", "k")]))));
        let o = FileConfig { provider: Some(ProviderKind::Ollama), ..FileConfig::default() };
        assert!(needs_setup(Some(&o), &resolve_config(Some(&o), &Env::new())));
    }

    #[test]
    fn mask_key_hides_the_middle() {
        assert_eq!(mask_key("sk-or-v1-abcdefghijklmnop-wxyz"), "sk-or-v1-a…wxyz");
        assert_eq!(mask_key("short"), "****");
    }

    #[test]
    fn the_config_dir_comes_from_the_given_environment_only() {
        let home = Path::new("/home/me");
        assert_eq!(default_config_dir(&Env::new(), home), Path::new("/home/me/.marv"));
        assert_eq!(default_config_dir(&env(&[("MARV_CONFIG_DIR", "/tmp/x")]), home), Path::new("/tmp/x"));
    }

    #[test]
    fn store_round_trips_privately() {
        let dir = std::env::temp_dir().join(format!("marv-config-{}-{}", std::process::id(), crate::private_file::random_hex()));
        let store = ConfigStore::new(&dir.join("nested"));
        assert!(store.load().unwrap().is_none());
        let f = FileConfig { model: Some("openai/gpt-5.6-sol".into()), max_steps: Some(MaxStepsSetting::Off), ..file() };
        store.save(&f).unwrap();
        assert_eq!(store.load().unwrap(), Some(f));
        assert_eq!(std::fs::metadata(&store.path).unwrap().permissions().mode() & 0o777, 0o600);
        std::fs::write(&store.path, "{not json").unwrap();
        assert!(store.load().is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
