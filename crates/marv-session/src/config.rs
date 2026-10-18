//! Configuration: presets, the file on disk, env overrides, and the provider factory. Ports Marv's
//! `src/config/config.ts` (and `providerFactory` from `src/provider/factory.ts`, which needs `resolve_config`).
//!
//! Two layers, later wins: `~/.marv/config.json` (validated, written 0600 because it can hold the API keys), then env
//! vars (`OPENROUTER_API_KEY`, `ZAI_API_KEY`, `MARV_MODEL`, `OLLAMA_HOST`, `MARV_MAX_STEPS`). [`FileConfig`] is what's on disk;
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
use marv_tools::formatter::FormatterSetting;
use serde_json::{Map, Value};

/// Marv's own environment, as a map (the CLI passes the real one).
pub type Env = BTreeMap<String, String>;

/// The agent loop's default step limit.
pub const DEFAULT_MAX_STEPS: u32 = marv_agent::agent::DEFAULT_MAX_STEPS;
/// 32k fits fully on a 12 GB GPU for 9-12B models and holds a fair amount of code.
pub const DEFAULT_CONTEXT_LENGTH: u64 = 32_768;
/// `reasoningEffort` (#143) when the file doesn't say: medium. On SWE-bench (10 Verified instances, GLM-5.3-flash)
/// it resolved the same 7 as the model's own effort with 37% fewer output tokens and 30% less cost; low was no
/// cheaper and slower.
pub const DEFAULT_REASONING_EFFORT: Option<marv_provider::ReasoningEffort> = Some(marv_provider::ReasoningEffort::Medium);
/// `compactTokens` (#140): compact once the context passes this many tokens, even when the window is far larger.
pub const DEFAULT_COMPACT_TOKENS: u64 = 100_000;
/// The smallest `compactTokens`. After a compaction the context is the system prompt, tools and MCP list (7k–13k
/// seen), the summary and the next tool outputs: a threshold close to that would compact again every step or two.
pub const MIN_COMPACT_TOKENS: u64 = 40_000;

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

/// `PRESETS`: OpenRouter through the OpenAI-compatible adapter, Ollama through its native API, and (#159) Z.ai's GLM
/// Coding Plan through the OpenAI-compatible adapter on the plan's own endpoint: a Coding Plan key gets 401 on Z.ai's
/// general `/api/paas/v4`.
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
        ProviderKind::Zai => Preset {
            label: "Z.ai Coding Plan",
            base_url: marv_provider::models::ZAI_CODING_BASE_URL,
            key_env: Some("ZAI_API_KEY"),
            key_url: Some("z.ai/manage-apikey/apikey-list"),
            default_model: marv_provider::models::ZAI_DEFAULT_MODEL,
        },
        // #159: no key; the person signs in (`marv_provider::chatgpt`). The models are the account's, picked in setup.
        ProviderKind::ChatGpt => Preset {
            label: "ChatGPT",
            base_url: marv_provider::chatgpt::API_BASE_URL,
            key_env: None,
            key_url: None,
            default_model: "",
        },
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
    /// `zaiApiKey` (#159): the Z.ai Coding Plan key. Its own field, so switching providers never sends one vendor's key
    /// to another.
    pub zai_api_key: Option<String>,
    /// Overrides the preset endpoint, e.g. Ollama on another machine.
    pub base_url: Option<String>,
    pub thinking: Option<bool>,
    /// `reasoningEffort` (#143): `minimal`, `low`, `medium` or `high`, sent to OpenRouter models that reason;
    /// `Some(None)` is `"default"` (the model picks). Unset: [`DEFAULT_REASONING_EFFORT`].
    pub reasoning_effort: Option<Option<marv_provider::ReasoningEffort>>,
    /// Ollama's context window (num_ctx), at least 2048.
    pub context_length: Option<u64>,
    /// `compactTokens` (#140): compact at this many tokens of context, or at 85% of the window if that comes first.
    /// `Some(0)` (written `0` or `false`) turns it off: only the window rule is left.
    pub compact_tokens: Option<u64>,
    pub sandbox: Option<bool>,
    pub yolo: Option<bool>,
    pub trajectories: Option<bool>,
    pub diagnostics: Option<bool>,
    pub checks: Option<bool>,
    /// Snapshot the project before each turn's changes, so /undo can put them back (default on). #16.
    pub checkpoints: Option<bool>,
    /// Session-start hooks (#108). Read only from this file, the user's own: no project file ever fills it.
    pub hooks: Option<crate::hooks::Hooks>,
    /// After a turn, suggest the next message in the empty prompt (default on; Tab takes it).
    pub suggestions: Option<bool>,
    pub mcp_tools: Option<McpToolsSetting>,
    pub max_steps: Option<MaxStepsSetting>,
    /// The formatter the edit tools run after a change (#67): `false` (off), `true` (detect), or extension → command.
    pub formatter: Option<FormatterSetting>,
    /// Pre-approved approval scopes (#98), the names `--approve` takes ("web", "network", "github", …). "Always"
    /// for the session, in the TUI and in `-p`; never covers protected paths, lifecycle scripts, mesh wires or
    /// tainted egress.
    pub approve: Option<Vec<String>>,
    /// Extra writable sandbox folders (#98), e.g. `~/Projects/marv/.beads`: bind-mounted read-write so a tool
    /// keeping its state outside the project runs inside the sandbox. Refused at startup for the home, `/`,
    /// `~/.ssh`, `~/.marv`, anything holding credentials (marv_sandbox::writable).
    pub writable: Option<Vec<String>>,
    /// `mesh.wake` (marv-rs #78): a message on a wired inbox starts a turn while the TUI is idle (default off).
    pub mesh_wake: Option<bool>,
    /// `mesh.radio` (marv-rs #124): this Marv is on the mesh radio (default off). Read only from this file, the user's
    /// own: never a project's, an environment variable or a flag.
    pub mesh_radio: Option<RadioSetting>,
    /// `herdr.peers` (#74): how many panes the Herdr door may open (commands and spawned workers) per session
    /// (default [`DEFAULT_HERDR_PEERS`], at most [`MAX_HERDR_PEERS`]; 0: none). The user's own file only.
    pub herdr_peers: Option<u32>,
}

/// `MARV_YOLO=off` (or 0, false, no): yolo stays off for this process (#74).
pub fn yolo_held_off(value: Option<&str>) -> bool {
    matches!(value.map(|v| v.trim().to_lowercase()).as_deref(), Some("0" | "off" | "false" | "no"))
}

/// `MARV_TAINTED=1` (or true, yes, on): this Marv starts tainted (#74: the Herdr door spawns a worker so when the
/// spawning conversation is). It can only add taint, never take it away.
pub fn tainted_at_start(env: &Env) -> bool {
    matches!(env.get("MARV_TAINTED").map(|v| v.trim().to_lowercase()).as_deref(), Some("1" | "true" | "yes" | "on"))
}

/// The Herdr door's default limit on panes it opens per session (#74).
pub const DEFAULT_HERDR_PEERS: u32 = 4;
/// The most `herdr.peers` may allow.
pub const MAX_HERDR_PEERS: u32 = 16;

/// `"mesh": {"radio": true}`, or the object form `{"radio": {"titles": false}}` (on, without sharing the
/// conversation's title in presence). `false` is off. `/radio off` with titles off saves `{"on": false, "titles":
/// false}`, so the choice survives an off and on (`"on"` defaults to true in the object form).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RadioSetting {
    pub on: bool,
    /// Whether presence carries the conversation's title (default on).
    pub titles: bool,
}

impl RadioSetting {
    fn from_value(value: &Value) -> Result<Option<Self>, String> {
        match value {
            Value::Null => Ok(None),
            Value::Bool(on) => Ok(Some(Self { on: *on, titles: true })),
            Value::Object(options) => {
                if let Some(key) = options.keys().find(|k| !matches!(k.as_str(), "titles" | "on")) {
                    return Err(format!("mesh.radio: unknown key \"{key}\" (only \"titles\" and \"on\")"));
                }
                let flag = |key: &str| match options.get(key) {
                    None | Some(Value::Null) => Ok(true),
                    Some(Value::Bool(b)) => Ok(*b),
                    Some(_) => Err(format!("mesh.radio.{key}: expected a boolean")),
                };
                Ok(Some(Self { on: flag("on")?, titles: flag("titles")? }))
            }
            _ => Err("mesh.radio: expected true, false or {\"titles\": false}".into()),
        }
    }

    /// As saved: `true`/`false`, or `{"titles": false}` when on without titles, and `{"on": false, "titles": false}`
    /// when off without them (so `/radio on` doesn't share titles again).
    fn to_value(self) -> Value {
        match (self.on, self.titles) {
            (on, true) => Value::from(on),
            (true, false) => serde_json::json!({ "titles": false }),
            (false, false) => serde_json::json!({ "on": false, "titles": false }),
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct ConfigError(pub String);

impl FileConfig {
    /// The saved key of `kind` (#159: each provider has its own field). `None` for a provider that needs none.
    pub fn key_for(&self, kind: ProviderKind) -> Option<&str> {
        match kind {
            ProviderKind::OpenRouter => self.api_key.as_deref(),
            ProviderKind::Zai => self.zai_api_key.as_deref(),
            ProviderKind::Ollama | ProviderKind::ChatGpt => None,
        }
    }

    /// Saves `key` as `kind`'s key (nothing for a provider that needs none).
    pub fn set_key(&mut self, kind: ProviderKind, key: String) {
        match kind {
            ProviderKind::OpenRouter => self.api_key = Some(key),
            ProviderKind::Zai => self.zai_api_key = Some(key),
            ProviderKind::Ollama | ProviderKind::ChatGpt => {}
        }
    }

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
        let string_list = |key: &str, problems: &mut Vec<String>| match map.get(key) {
            None | Some(Value::Null) => None,
            Some(Value::Array(items)) => {
                let mut out = Vec::new();
                let mut ok = true;
                for item in items {
                    match item.as_str().filter(|s| !s.is_empty()) {
                        Some(s) => out.push(s.to_string()),
                        None => ok = false,
                    }
                }
                if !ok {
                    problems.push(format!("{key}: expected a list of non-empty strings"));
                }
                (!out.is_empty()).then_some(out)
            }
            Some(_) => {
                problems.push(format!("{key}: expected a list of strings"));
                None
            }
        };
        let provider = match map.get("provider").and_then(Value::as_str) {
            Some("openrouter") => Some(ProviderKind::OpenRouter),
            Some("ollama") => Some(ProviderKind::Ollama),
            Some("zai") => Some(ProviderKind::Zai),
            Some("chatgpt") => Some(ProviderKind::ChatGpt),
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
        let reasoning_effort = match map.get("reasoningEffort") {
            None | Some(Value::Null) => None,
            Some(v) if v.as_str() == Some("default") => Some(None),
            Some(v) => match v.as_str().and_then(marv_provider::ReasoningEffort::parse) {
                Some(effort) => Some(Some(effort)),
                None => {
                    problems.push(format!(
                        "reasoningEffort: expected one of {}, or default",
                        marv_provider::ReasoningEffort::NAMES.join(", ")
                    ));
                    None
                }
            },
        };
        let compact_tokens = match map.get("compactTokens") {
            None | Some(Value::Null) => None,
            Some(Value::Bool(false)) => Some(0),
            Some(v) => match v.as_u64() {
                Some(n) if n == 0 || n >= MIN_COMPACT_TOKENS => Some(n),
                _ => {
                    problems.push(format!(
                        "compactTokens: expected a whole number of at least {MIN_COMPACT_TOKENS}, or 0 or false (off)"
                    ));
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
        let formatter = match map.get("formatter") {
            None | Some(Value::Null) => None,
            Some(Value::Bool(false)) => Some(FormatterSetting::Off),
            Some(Value::Bool(true)) => Some(FormatterSetting::Auto),
            Some(Value::Object(entries)) => {
                let mut commands = BTreeMap::new();
                for (ext, command) in entries {
                    match command.as_str().filter(|c| !c.is_empty()) {
                        Some(command) => {
                            commands.insert(ext.to_lowercase(), command.to_string());
                        }
                        None => problems.push(format!("formatter.{ext}: expected a non-empty command string")),
                    }
                }
                Some(FormatterSetting::Custom(commands))
            }
            Some(_) => {
                problems.push("formatter: expected false, true, or an object of extension to command".into());
                None
            }
        };
        // `"mesh": {"wake": true, "radio": true}`: the mesh's settings, one object.
        let (mesh_wake, mesh_radio) = match map.get("mesh") {
            None | Some(Value::Null) => (None, None),
            Some(Value::Object(mesh)) => {
                let wake = match mesh.get("wake") {
                    None | Some(Value::Null) => None,
                    Some(Value::Bool(b)) => Some(*b),
                    Some(_) => {
                        problems.push("mesh.wake: expected a boolean".into());
                        None
                    }
                };
                let radio = RadioSetting::from_value(mesh.get("radio").unwrap_or(&Value::Null))
                    .map_err(|e| problems.push(e))
                    .ok()
                    .flatten();
                (wake, radio)
            }
            Some(_) => {
                problems.push("mesh: expected an object, like {\"wake\": true}".into());
                (None, None)
            }
        };
        // `"herdr": {"peers": 4}`: the Herdr door's settings (#74).
        let herdr_peers = match map.get("herdr") {
            None | Some(Value::Null) => None,
            Some(Value::Object(herdr)) => {
                if let Some(key) = herdr.keys().find(|k| k.as_str() != "peers") {
                    problems.push(format!("herdr: unknown key \"{key}\" (only \"peers\")"));
                }
                match herdr.get("peers") {
                    None | Some(Value::Null) => None,
                    Some(v) => match v.as_u64().filter(|n| *n <= u64::from(MAX_HERDR_PEERS)) {
                        Some(n) => Some(n as u32),
                        None => {
                            problems.push(format!("herdr.peers: expected a whole number from 0 to {MAX_HERDR_PEERS}"));
                            None
                        }
                    },
                }
            }
            Some(_) => {
                problems.push("herdr: expected an object, like {\"peers\": 4}".into());
                None
            }
        };
        let hooks = match map.get("hooks") {
            None | Some(Value::Null) => None,
            Some(v) => crate::hooks::Hooks::from_value(v).map_err(|e| problems.push(e)).ok(),
        };
        let config = Self {
            provider,
            model: string("model", &mut problems),
            api_key: string("apiKey", &mut problems),
            zai_api_key: string("zaiApiKey", &mut problems),
            base_url,
            thinking: boolean("thinking", &mut problems),
            reasoning_effort,
            context_length,
            compact_tokens,
            sandbox: boolean("sandbox", &mut problems),
            yolo: boolean("yolo", &mut problems),
            trajectories: boolean("trajectories", &mut problems),
            diagnostics: boolean("diagnostics", &mut problems),
            checks: boolean("checks", &mut problems),
            checkpoints: boolean("checkpoints", &mut problems),
            hooks,
            suggestions: boolean("suggestions", &mut problems),
            mcp_tools,
            max_steps,
            formatter,
            approve: string_list("approve", &mut problems),
            writable: string_list("writable", &mut problems),
            mesh_wake,
            mesh_radio,
            herdr_peers,
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
        put("zaiApiKey", self.zai_api_key.clone().map(Value::from));
        put("baseUrl", self.base_url.clone().map(Value::from));
        put("thinking", self.thinking.map(Value::from));
        put("reasoningEffort", self.reasoning_effort.map(|e| Value::from(e.map_or("default", |e| e.as_str()))));
        put("contextLength", self.context_length.map(Value::from));
        put("compactTokens", self.compact_tokens.map(Value::from));
        put("sandbox", self.sandbox.map(Value::from));
        put("yolo", self.yolo.map(Value::from));
        put("trajectories", self.trajectories.map(Value::from));
        put("diagnostics", self.diagnostics.map(Value::from));
        put("checks", self.checks.map(Value::from));
        put("checkpoints", self.checkpoints.map(Value::from));
        put("hooks", self.hooks.as_ref().map(crate::hooks::Hooks::to_value));
        put("suggestions", self.suggestions.map(Value::from));
        put("mcpTools", self.mcp_tools.and_then(|m| serde_json::to_value(m).ok()));
        put("approve", self.approve.clone().map(Value::from));
        put("writable", self.writable.clone().map(Value::from));
        put(
            "maxSteps",
            self.max_steps.map(|m| match m {
                MaxStepsSetting::Off => Value::from("off"),
                MaxStepsSetting::Every(n) => Value::from(n),
            }),
        );
        put(
            "formatter",
            self.formatter.as_ref().map(|f| match f {
                FormatterSetting::Off => Value::from(false),
                FormatterSetting::Auto => Value::from(true),
                FormatterSetting::Custom(commands) => serde_json::to_value(commands).unwrap_or(Value::Null),
            }),
        );
        let mut mesh = Map::new();
        if let Some(wake) = self.mesh_wake {
            mesh.insert("wake".into(), Value::from(wake));
        }
        if let Some(radio) = self.mesh_radio {
            mesh.insert("radio".into(), radio.to_value());
        }
        put("mesh", (!mesh.is_empty()).then_some(Value::Object(mesh)));
        put("herdr", self.herdr_peers.map(|n| serde_json::json!({"peers": n})));
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
    /// `reasoningEffort` (#143); `None`: the model's own default.
    pub reasoning_effort: Option<marv_provider::ReasoningEffort>,
    pub context_length: u64,
    /// `compactTokens` (#140): compact at this many tokens of context (or at 85% of the window if sooner); `None`: off.
    pub compact_tokens: Option<u64>,
    pub sandbox: bool,
    pub yolo: bool,
    pub trajectories: bool,
    pub diagnostics: bool,
    pub checks: bool,
    pub checkpoints: bool,
    pub suggestions: bool,
    pub mcp_tools: McpToolsSetting,
    /// 0 never asks; 1 can't happen. Subagents' own hard limit (50) is not touched by 0.
    pub max_steps: u32,
    /// The formatter the edit tools run after a change (#67). On (detect) unless the file turned it off or
    /// configured it.
    pub formatter: FormatterSetting,
    /// Pre-approved scopes (#98), the `--approve` names the config listed. Empty: nothing pre-approved.
    pub approve: Vec<String>,
    /// Extra writable sandbox folders (#98), as the config wrote them (`~` still to expand).
    pub writable: Vec<String>,
    /// `mesh.wake` (#78): off unless the file turns it on.
    pub mesh_wake: bool,
    /// `mesh.radio` (#124): off unless the user's own file turns it on. No environment variable or flag reaches it.
    pub mesh_radio: bool,
    /// `herdr.peers` (#74): the panes the Herdr door may open per session.
    pub herdr_peers: u32,
    /// #159: the ChatGPT sign-in's token file, `chatgpt.json` in the config folder (`MARV_CONFIG_DIR`, else
    /// `$HOME/.marv`); `None` when the environment names neither.
    pub auth_file: Option<PathBuf>,
    /// Whether the radio's presence carries the conversation's title (`{"radio": {"titles": false}}` turns it off).
    pub radio_titles: bool,
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
            reasoning_effort: self.reasoning_effort,
            auth_file: (self.provider == ProviderKind::ChatGpt).then(|| self.auth_file.clone()).flatten(),
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

/// `MARV_DEBUG_RAW` (#90): debug recording for a turn. On, the providers collect the raw
/// response payloads (the trajectory's `request` records carry them as `rawResponse`) — for
/// diagnosing a turn that ended on something the loop may have misparsed. Off by default;
/// "off" (any case) and "0" turn it back off.
pub fn debug_raw() -> bool {
    debug_raw_from(std::env::var("MARV_DEBUG_RAW").ok().as_deref())
}

/// The `MARV_DEBUG_RAW` value's meaning (testable).
pub fn debug_raw_from(value: Option<&str>) -> bool {
    let Some(text) = value.map(str::trim).filter(|t| !t.is_empty()) else {
        return false;
    };
    !text.eq_ignore_ascii_case("off") && text != "0"
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
    // #159: only this provider's own key, from its own variable or field: never another vendor's.
    let api_key = if preset.key_env.is_some() {
        env_key.clone().or_else(|| file.and_then(|f| f.key_for(provider)).map(str::to_string))
    } else {
        None
    };
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
            ProviderKind::OpenRouter | ProviderKind::Zai | ProviderKind::ChatGpt => preset.base_url.into(),
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
        reasoning_effort: file.and_then(|f| f.reasoning_effort).unwrap_or(DEFAULT_REASONING_EFFORT),
        context_length: file.and_then(|f| f.context_length).unwrap_or(DEFAULT_CONTEXT_LENGTH),
        compact_tokens: match file.and_then(|f| f.compact_tokens) {
            Some(0) => None,
            n => Some(n.unwrap_or(DEFAULT_COMPACT_TOKENS)),
        },
        sandbox: file.and_then(|f| f.sandbox).unwrap_or(true),
        // `MARV_YOLO=off` holds yolo off for this process whatever the file says (#74: how the Herdr door starts a Marv
        // that may have no more than the session that spawned it). It can only turn yolo off, never on.
        yolo: file.and_then(|f| f.yolo).unwrap_or(true) && !yolo_held_off(env.get("MARV_YOLO").map(String::as_str)),
        trajectories: file.and_then(|f| f.trajectories).unwrap_or(true),
        diagnostics: file.and_then(|f| f.diagnostics).unwrap_or(true),
        checks: file.and_then(|f| f.checks).unwrap_or(true),
        checkpoints: file.and_then(|f| f.checkpoints).unwrap_or(true),
        suggestions: file.and_then(|f| f.suggestions).unwrap_or(true),
        mcp_tools: file.and_then(|f| f.mcp_tools).unwrap_or_default(),
        max_steps: max_steps_from_env(env.get("MARV_MAX_STEPS").map(String::as_str), file_steps),
        formatter: file.and_then(|f| f.formatter.clone()).unwrap_or(FormatterSetting::Auto),
        approve: file.and_then(|f| f.approve.clone()).unwrap_or_default(),
        writable: file.and_then(|f| f.writable.clone()).unwrap_or_default(),
        mesh_wake: file.and_then(|f| f.mesh_wake).unwrap_or(false),
        mesh_radio: file.and_then(|f| f.mesh_radio).is_some_and(|r| r.on),
        radio_titles: file.and_then(|f| f.mesh_radio).is_none_or(|r| r.titles),
        herdr_peers: file.and_then(|f| f.herdr_peers).unwrap_or(DEFAULT_HERDR_PEERS),
        auth_file: env
            .get("MARV_CONFIG_DIR")
            .map(PathBuf::from)
            .or_else(|| env.get("HOME").map(|home| Path::new(home).join(".marv")))
            .map(|dir| dir.join(marv_provider::chatgpt::AUTH_FILE)),
    }
}

/// First run, no provider chosen, a provider that needs a key we don't have, or no model picked yet.
pub fn needs_setup(file: Option<&FileConfig>, config: &Config) -> bool {
    if file.and_then(|f| f.provider).is_none() || config.model.is_empty() {
        return true;
    }
    // #159: ChatGPT needs a saved sign-in that allows plan usage.
    if config.provider == ProviderKind::ChatGpt {
        return !config
            .auth_file
            .as_ref()
            .is_some_and(|path| marv_provider::chatgpt::AuthStore { path: path.clone() }.signed_in());
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
pub fn provider_factory(
    option: ProviderOption,
    thinking: bool,
    reasoning_effort: Option<marv_provider::ReasoningEffort>,
) -> Arc<dyn ProviderFactory> {
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
        ProviderOption::ChatGpt { model, auth_file } => {
            let file = FileConfig { provider: Some(ProviderKind::ChatGpt), model: Some(model), ..FileConfig::default() };
            let config =
                Config { thinking, reasoning_effort, auth_file: Some(auth_file), ..resolve_config(Some(&file), &Env::new()) };
            return config_factory(config);
        }
        ProviderOption::Zai { api_key, model, base_url } => (
            FileConfig {
                provider: Some(ProviderKind::Zai),
                zai_api_key: Some(api_key),
                model,
                base_url,
                ..FileConfig::default()
            },
            Env::new(),
        ),
    };
    config_factory(Config { thinking, reasoning_effort, ..resolve_config(Some(&file), &env) })
}

/// The SDK's factory for a resolved config.
fn config_factory(config: Config) -> Arc<dyn ProviderFactory> {
    Arc::new(ConfigFactory {
        config: config.provider_config(),
        make: Arc::new(|config, info| marv_provider::create_provider(config, info, Some(marv_provider::errors::SDK_HINTS))),
        // Marv's model lists need no key (OpenRouter's is public, Ollama's local, Z.ai's plan models Marv's own table).
        load: Arc::new(|kind, base_url| Box::pin(async move { marv_provider::models::list_models(kind, &base_url, None).await })),
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
        assert!(c.checkpoints && c.suggestions);
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
    fn debug_raw_only_from_its_env_var() {
        // #90: the flag is off unless MARV_DEBUG_RAW names it on; "off" and "0" turn it off.
        assert!(!debug_raw_from(None));
        assert!(!debug_raw_from(Some("")));
        assert!(!debug_raw_from(Some("0")));
        assert!(!debug_raw_from(Some("off")));
        assert!(debug_raw_from(Some("1")));
        assert!(debug_raw_from(Some("on")));
        assert!(debug_raw_from(Some(" any ")));
    }

    #[test]
    fn the_formatter_setting_is_off_auto_or_a_command_per_extension() {
        let parsed = |value: Value| FileConfig::from_value(&json!({"formatter": value}));
        assert_eq!(parsed(json!(false)).unwrap().formatter, Some(FormatterSetting::Off));
        assert_eq!(parsed(json!(true)).unwrap().formatter, Some(FormatterSetting::Auto));
        let custom = parsed(json!({"rs": "rustfmt --edition 2024", "py": "ruff format"})).unwrap().formatter;
        match custom {
            Some(FormatterSetting::Custom(commands)) => {
                assert_eq!(commands.get("rs").map(String::as_str), Some("rustfmt --edition 2024"));
                assert_eq!(commands.get("py").map(String::as_str), Some("ruff format"));
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(parsed(json!("off")).unwrap_err(), "formatter: expected false, true, or an object of extension to command");
        assert_eq!(parsed(json!({"py": ""})).unwrap_err(), "formatter.py: expected a non-empty command string");
        // Absent: on (detect). Saved: round-trips, `Off` as false and `Auto` as true.
        let c = resolve_config(Some(&file()), &Env::new());
        assert_eq!(c.formatter, FormatterSetting::Auto);
        let mut off = file();
        off.formatter = Some(FormatterSetting::Off);
        let mut custom = file();
        custom.formatter = Some(FormatterSetting::Custom([("rs".to_string(), "rustfmt".to_string())].into_iter().collect()));
        let custom_setting = custom.formatter.clone();
        let round = |f: FileConfig| FileConfig::from_value(&f.to_value()).unwrap().formatter;
        assert_eq!(round(off), Some(FormatterSetting::Off));
        assert_eq!(round(custom), custom_setting);
        assert_eq!(round(file()), None);
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

    // #98: the config's pre-approved scopes and extra writable folders.
    #[test]
    fn approve_and_writable_lists_are_read_and_saved() {
        let f = FileConfig::from_value(&json!({
            "provider": "openrouter",
            "approve": ["web", "network"],
            "writable": ["~/Projects/marv/.beads"],
        }))
        .unwrap();
        assert_eq!(f.approve.clone().unwrap(), ["web", "network"]);
        let c = resolve_config(Some(&f), &Env::new());
        assert_eq!(c.approve, ["web", "network"]);
        assert_eq!(c.writable, ["~/Projects/marv/.beads"]);
        // Round-trips as saved (the setup screen writes the file back).
        let saved = FileConfig::from_value(&f.to_value()).unwrap();
        assert_eq!(saved, f);
        // Nothing set: empty, not an error.
        let none = resolve_config(Some(&file()), &Env::new());
        assert!(none.approve.is_empty() && none.writable.is_empty());
    }

    #[test]
    fn bad_approve_and_writable_lists_are_rejected() {
        for bad in [json!("web"), json!(3), json!([""]), json!(["web", 7])] {
            for key in ["approve", "writable"] {
                let mut raw = json!({"provider": "openrouter"});
                raw[key] = bad.clone();
                assert!(FileConfig::from_value(&raw).is_err(), "{bad}");
            }
        }
    }

    #[test]
    fn reasoning_effort_comes_from_the_file_and_reaches_the_provider_config() {
        assert_eq!(resolve_config(None, &Env::new()).reasoning_effort, Some(marv_provider::ReasoningEffort::Medium));
        let file = FileConfig::from_value(&json!({"reasoningEffort": "high"})).unwrap();
        assert_eq!(file.to_value()["reasoningEffort"], json!("high"));
        let config = resolve_config(Some(&file), &Env::new());
        assert_eq!(config.provider_config().reasoning_effort, Some(marv_provider::ReasoningEffort::High));
        // "default": the model picks, as before #143.
        let own = FileConfig::from_value(&json!({"reasoningEffort": "default"})).unwrap();
        assert_eq!(own.to_value()["reasoningEffort"], json!("default"));
        assert_eq!(resolve_config(Some(&own), &Env::new()).reasoning_effort, None);
        for bad in [json!("max"), json!(2), json!(true)] {
            assert!(FileConfig::from_value(&json!({ "reasoningEffort": bad })).unwrap_err().contains("reasoningEffort"), "{bad}");
        }
    }

    #[test]
    fn compact_tokens_defaults_to_100k_and_0_or_false_turns_it_off() {
        assert_eq!(resolve_config(None, &Env::new()).compact_tokens, Some(DEFAULT_COMPACT_TOKENS));
        let file = |v: Value| FileConfig::from_value(&json!({ "compactTokens": v }));
        let set = file(json!(150_000)).unwrap();
        assert_eq!(set.compact_tokens, Some(150_000));
        assert_eq!(set.to_value()["compactTokens"], json!(150_000));
        assert_eq!(resolve_config(Some(&set), &Env::new()).compact_tokens, Some(150_000));
        for off in [json!(0), json!(false)] {
            let off = file(off).unwrap();
            assert_eq!(resolve_config(Some(&off), &Env::new()).compact_tokens, None);
            assert_eq!(off.to_value()["compactTokens"], json!(0));
        }
        assert_eq!(file(json!(MIN_COMPACT_TOKENS)).unwrap().compact_tokens, Some(MIN_COMPACT_TOKENS));
        for bad in [json!(MIN_COMPACT_TOKENS - 1), json!(-5), json!("100k"), json!(true), json!(1.5)] {
            assert!(file(bad.clone()).unwrap_err().contains("compactTokens"), "{bad}");
        }
    }

    /// #74: the Herdr door's pane limit is the user's file's (default 4, at most 16), and `MARV_YOLO=off` only ever
    /// turns yolo off.
    #[test]
    fn herdr_peers_comes_from_the_file_and_marv_yolo_only_turns_yolo_off() {
        assert_eq!(resolve_config(None, &Env::new()).herdr_peers, DEFAULT_HERDR_PEERS);
        let two = FileConfig::from_value(&json!({"herdr": {"peers": 2}})).unwrap();
        assert_eq!(resolve_config(Some(&two), &Env::new()).herdr_peers, 2);
        assert_eq!(two.to_value()["herdr"], json!({"peers": 2}));
        for bad in [json!({"peers": 17}), json!({"peers": -1}), json!({"peers": "4"}), json!({"max": 1}), json!(4)] {
            assert!(FileConfig::from_value(&json!({ "herdr": bad })).is_err(), "{bad}");
        }
        let off: Env = [("MARV_YOLO".to_string(), "off".to_string())].into();
        let on: Env = [("MARV_YOLO".to_string(), "on".to_string())].into();
        let no_yolo = FileConfig { yolo: Some(false), ..FileConfig::default() };
        assert!(!resolve_config(None, &off).yolo);
        assert!(resolve_config(None, &on).yolo);
        assert!(!resolve_config(Some(&no_yolo), &on).yolo, "never turns it on");
    }

    #[test]
    fn the_radio_is_off_unless_the_file_turns_it_on_and_keeps_its_titles_choice() {
        let env: Env = [("MARV_RADIO", "1"), ("MARV_MESH_RADIO", "true")].map(|(k, v)| (k.to_string(), v.to_string())).into();
        assert!(!resolve_config(None, &env).mesh_radio, "no environment variable turns it on");
        assert!(!resolve_config(Some(&file()), &env).mesh_radio);
        let on = FileConfig::from_value(&json!({"provider": "openrouter", "mesh": {"wake": true, "radio": true}})).unwrap();
        let config = resolve_config(Some(&on), &Env::new());
        assert!(config.mesh_radio && config.radio_titles && config.mesh_wake);
        assert_eq!(on.to_value()["mesh"], json!({"wake": true, "radio": true}));
        let quiet = FileConfig::from_value(&json!({"mesh": {"radio": {"titles": false}}})).unwrap();
        let config = resolve_config(Some(&quiet), &Env::new());
        assert!(config.mesh_radio && !config.radio_titles);
        assert_eq!(quiet.to_value()["mesh"], json!({"radio": {"titles": false}}));
        let off = FileConfig { mesh_radio: Some(RadioSetting { on: false, titles: true }), ..quiet.clone() };
        assert!(!resolve_config(Some(&off), &Env::new()).mesh_radio);
        assert_eq!(off.to_value()["mesh"], json!({"radio": false}));
        // Off with titles off keeps that choice: turned on again, it still leaves the title out.
        let quiet_off = FileConfig { mesh_radio: Some(RadioSetting { on: false, titles: false }), ..quiet.clone() };
        assert_eq!(quiet_off.to_value()["mesh"], json!({"radio": {"on": false, "titles": false}}));
        let saved = FileConfig::from_value(&quiet_off.to_value()).unwrap();
        let config = resolve_config(Some(&saved), &Env::new());
        assert!(!config.mesh_radio && !config.radio_titles);
        assert_eq!(saved.mesh_radio, Some(RadioSetting { on: false, titles: false }));
        assert!(FileConfig::from_value(&json!({"mesh": {"radio": {"on": "yes"}}})).is_err());
        assert!(FileConfig::from_value(&json!({"mesh": {"radio": "yes"}})).is_err());
        assert!(FileConfig::from_value(&json!({"mesh": {"radio": {"titles": "no"}}})).is_err());
        assert!(FileConfig::from_value(&json!({"mesh": {"radio": {"everyone": true}}})).is_err());
    }

    #[test]
    fn mesh_wake_is_off_unless_the_file_turns_it_on() {
        assert!(!resolve_config(None, &Env::new()).mesh_wake);
        assert!(!resolve_config(Some(&file()), &Env::new()).mesh_wake);
        let f = FileConfig::from_value(&json!({"provider": "openrouter", "mesh": {"wake": true}})).unwrap();
        assert_eq!(f.mesh_wake, Some(true));
        assert!(resolve_config(Some(&f), &Env::new()).mesh_wake);
        assert_eq!(f.to_value()["mesh"], json!({"wake": true}));
        assert!(file().to_value().get("mesh").is_none(), "unset stays out of the file");
        assert!(FileConfig::from_value(&json!({"mesh": {"wake": "yes"}})).is_err());
        assert!(FileConfig::from_value(&json!({"mesh": true})).is_err());
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

    /// #159: the Coding Plan's key has its own field and variable, and is saved under `zaiApiKey`.
    #[test]
    fn the_zai_plan_has_its_own_key_endpoint_and_default_model() {
        let f = FileConfig::from_value(&json!({"provider": "zai", "zaiApiKey": "zai-file", "apiKey": "sk-or"})).unwrap();
        assert_eq!((f.provider, f.key_for(ProviderKind::Zai)), (Some(ProviderKind::Zai), Some("zai-file")));
        assert_eq!(f.to_value()["zaiApiKey"], json!("zai-file"));
        let c = resolve_config(Some(&f), &env(&[]));
        assert_eq!(
            (c.api_key.as_deref(), c.base_url.as_str(), c.model.as_str()),
            (Some("zai-file"), "https://api.z.ai/api/coding/paas/v4", "glm-5.3")
        );
        let c = resolve_config(Some(&f), &env(&[("ZAI_API_KEY", "zai-env")]));
        assert_eq!((c.api_key.as_deref(), c.api_key_source), (Some("zai-env"), Some(KeySource::Env)));
        let mut g = FileConfig::default();
        g.set_key(ProviderKind::Zai, "z".into());
        g.set_key(ProviderKind::Ollama, "nothing".into());
        assert_eq!((g.api_key, g.zai_api_key.as_deref()), (None, Some("z")));
    }

    /// #159: ChatGPT needs no key but a saved sign-in with plan usage; its token file is in the config folder.
    #[tokio::test]
    async fn chatgpt_needs_a_sign_in_in_the_config_folder() {
        let dir = std::env::temp_dir().join(format!("marv-chatgpt-{}-{}", std::process::id(), marv_fs::random_hex()));
        struct Gone(PathBuf);
        impl Drop for Gone {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _gone = Gone(dir.clone());
        let env = env(&[("MARV_CONFIG_DIR", dir.to_str().unwrap()), ("OPENROUTER_API_KEY", "sk-or")]);
        let f = FileConfig::from_value(&json!({"provider": "chatgpt", "model": "gpt-6.1-sol", "apiKey": "sk-or"})).unwrap();
        let c = resolve_config(Some(&f), &env);
        assert_eq!(
            (c.provider, c.api_key.as_deref(), c.base_url.as_str()),
            (ProviderKind::ChatGpt, None, "https://api.openai.com/v1")
        );
        assert_eq!(c.provider_config().auth_file, Some(dir.join("chatgpt.json")));
        assert!(needs_setup(Some(&f), &c), "not signed in yet");
        let store = marv_provider::chatgpt::AuthStore::new(&dir);
        let signed_in = marv_provider::chatgpt::StoredAuth {
            client_id: Some("oaiapp_1".into()),
            access_token: Some("at".into()),
            refresh_token: Some("rt".into()),
            scopes: vec![marv_provider::chatgpt::PLAN_SCOPE.into()],
            ..Default::default()
        };
        store.save(&signed_in).await.unwrap();
        assert!(!needs_setup(Some(&f), &c));
        // Another provider's config never carries the token file.
        let o = resolve_config(Some(&FileConfig { provider: Some(ProviderKind::OpenRouter), ..f }), &env);
        assert_eq!(o.provider_config().auth_file, None);
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
        let dir = std::env::temp_dir().join(format!("marv-config-{}-{}", std::process::id(), marv_fs::random_hex()));
        let store = ConfigStore::new(&dir.join("nested"));
        assert!(store.load().unwrap().is_none());
        let f = FileConfig {
            model: Some("openai/gpt-5.6-sol".into()),
            max_steps: Some(MaxStepsSetting::Off),
            mesh_wake: Some(true),
            ..file()
        };
        store.save(&f).unwrap();
        assert_eq!(store.load().unwrap(), Some(f));
        assert_eq!(std::fs::metadata(&store.path).unwrap().permissions().mode() & 0o777, 0o600);
        std::fs::write(&store.path, "{not json").unwrap();
        assert!(store.load().is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    // #108: saving the config (/yolo, /setup…) keeps the user's hooks; a bad one names itself.
    #[test]
    fn hooks_survive_a_save_and_a_bad_one_is_rejected() {
        let raw = json!({"provider": "ollama", "hooks": {"sessionStart": ["bd ready", {"command": "x", "timeout": 3}]}});
        let f = FileConfig::from_value(&raw).unwrap();
        assert_eq!(f.hooks.as_ref().unwrap().session_start.len(), 2);
        assert_eq!(f.to_value()["hooks"], raw["hooks"]);
        let bad = FileConfig::from_value(&json!({"hooks": {"sessionStart": [""]}})).unwrap_err();
        assert!(bad.contains("hooks.sessionStart[0]"), "{bad}");
    }
}
