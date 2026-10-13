//! Which MCP servers to connect to. Ports `src/mcp/config.ts`.
//!
//! The format is Claude Code's, so the same files work in both:
//!
//! ```text
//! .mcp.json (the project)  and  ~/.marv/mcp.json (yours, every project)
//! { "mcpServers": {
//!     "files": { "command": "npx", "args": ["-y", "some-server"], "env": { … } },
//!     "docs":  { "type": "http", "url": "https://…/mcp", "headers": { "Authorization": "Bearer ${TOKEN}" } } } }
//! ```
//!
//! - `${VAR}` and `${VAR:-default}` are filled in from the environment, so tokens
//!   stay out of the file. An unset variable is a problem for that server only.
//! - The old `sse` transport is reported, not supported.
//! - A project server can't take the name of one of yours: it would look like
//!   yours when you're asked to trust it.
//! - Server names become part of tool names (`mcp__<server>__<tool>`), so they
//!   can't contain `__` or end in `_` (the tool name must read one way only).
//! - **Trust key** ([`McpServerConfig::key`]): a hash of the raw entry plus the
//!   content of the project files its command line names (`node mcp/server.js`:
//!   an agent could edit that file without asking, and Marv would run the new
//!   code on the next start). Files *those* load aren't covered; the trust
//!   notice says it runs project files.
//! - What the user is shown ([`McpServerConfig::display`]) is the raw config,
//!   `${VAR}`s unexpanded, so secrets never reach the screen.
//!
//! **Differences from Marv:**
//! - The key hashes the entry as canonical JSON (keys sorted, compact), not
//!   `JSON.stringify` in file order, so Marv's `mcp-trust.json` entries don't
//!   carry over: each project server is trusted once more. Reordering keys
//!   no longer drops trust; any real change still does.
//! - `--flag=path` words are also checked as project files (Marv only looked
//!   at whole words), so `node --import=./hook.js server.js` covers `hook.js`.
//! - `env`/`headers` are shown in key order, not file order.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use serde::Deserialize;
use serde::de::{Deserializer, MapAccess, Visitor};
use serde_json::Value;

use crate::Env;
use crate::text::sha256_hex;

/// The project's file, at its root.
pub const PROJECT_FILE: &str = ".mcp.json";
/// Yours, in the config folder (`~/.marv/mcp.json`).
pub const PERSONAL_FILE: &str = "mcp.json";
/// Seconds a tool call may take when the entry doesn't say (Marv: `DEFAULT_CALL_TIMEOUT_S`).
pub const DEFAULT_TIMEOUT_S: f64 = 120.0;
/// Marv's own variable: the project folder, for servers of yours that work on it.
pub const PROJECT_DIR_VAR: &str = "MARV_PROJECT_DIR";

const MAX_NAME: usize = 32;

/// Whose config a server comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ServerSource {
    /// The project's `.mcp.json`: starts only once trusted, in the project folder
    /// (but `npx`/`uvx` never there, see `manager::cwd_for`).
    Project,
    /// `~/.marv/mcp.json` or the SDK's `mcpServers`: yours, never asks for trust.
    Personal,
}

/// How Marv logs in to a server that wants OAuth (Marv #18), all optional. A
/// server without dynamic client registration needs a client registered by
/// hand: its id, its secret (a `${VAR}`, like headers) and the port its redirect
/// URI names. `scope` overrides the scope the server asks for.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct McpOAuthConfig {
    pub client_id: Option<String>,
    /// Expanded. Never stored, never displayed.
    pub client_secret: Option<String>,
    /// 1024–65535: below that needs root to listen on.
    pub callback_port: Option<u16>,
    pub scope: Option<String>,
}

/// How to reach a server: a local process over stdio, or Streamable HTTP.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpTransport {
    Stdio { command: String, args: Vec<String>, env: BTreeMap<String, String> },
    Http { url: String, headers: BTreeMap<String, String>, oauth: Option<McpOAuthConfig> },
}

/// One server, ready to connect: `${VAR}`s expanded.
#[derive(Debug, Clone, PartialEq)]
pub struct McpServerConfig {
    pub name: String,
    pub source: ServerSource,
    pub transport: McpTransport,
    /// Seconds a tool call may take (default [`DEFAULT_TIMEOUT_S`]).
    pub timeout: Option<f64>,
    /// Identifies this exact config (before `${VAR}`s are filled in), plus the
    /// content of the project files its command line names: trust is given to
    /// it, and lost when either changes.
    pub key: String,
    /// The raw config on one line, `${VAR}`s unexpanded.
    pub display: String,
    /// Variables it reads through `${VAR}` (a project server reading
    /// `$OPENROUTER_API_KEY` should stand out).
    pub reads: Vec<String>,
    /// Project files its command line names (a project server's).
    pub runs_project_files: Vec<String>,
}

/// What [`load_mcp_config`] found: the servers (yours first), and what couldn't be read and why.
#[derive(Debug, Clone, Default)]
pub struct LoadedConfig {
    pub servers: Vec<McpServerConfig>,
    pub problems: Vec<String>,
}

/// Which files [`load_mcp_config`] reads.
#[derive(Debug, Clone, Copy)]
pub struct Include {
    pub personal: bool,
    pub project: bool,
}

impl Default for Include {
    fn default() -> Self {
        Self { personal: true, project: true }
    }
}

/// A name [`parse_server`] accepts: letters, digits, `-`, `_`, at most 32, no
/// `__`, no trailing `_`.
pub fn valid_server_name(name: &str) -> bool {
    name_chars_ok(name) && !name.contains("__") && !name.ends_with('_')
}

fn name_chars_ok(name: &str) -> bool {
    !name.is_empty() && name.len() <= MAX_NAME && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn is_var_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_'
}
fn is_var_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// One `${NAME}` or `${NAME:-default}` at the start of `text`: (name, default, length).
fn variable_at(text: &str) -> Option<(&str, Option<&str>, usize)> {
    let rest = text.strip_prefix("${")?;
    let bytes = rest.as_bytes();
    if !bytes.first().is_some_and(|&b| is_var_start(b)) {
        return None;
    }
    let end = bytes.iter().position(|&b| !is_var_char(b)).unwrap_or(bytes.len());
    let name = &rest[..end];
    let after = &rest[end..];
    if let Some(after) = after.strip_prefix('}') {
        let _ = after;
        return Some((name, None, 2 + end + 1));
    }
    let fallback = after.strip_prefix(":-")?;
    let close = fallback.find('}')?;
    Some((name, Some(&fallback[..close]), 2 + end + 2 + close + 1))
}

/// Fills in `${VAR}` and `${VAR:-default}`; errors naming the first variable that isn't set.
pub fn expand(text: &str, lookup: &dyn Fn(&str) -> Option<String>) -> Result<String, String> {
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < text.len() {
        if let Some((name, fallback, len)) = variable_at(&text[i..]) {
            match lookup(name).or_else(|| fallback.map(str::to_string)) {
                Some(value) => out.push_str(&value),
                None => return Err(format!("${{{name}}} isn't set")),
            }
            i += len;
        } else {
            let ch = text[i..].chars().next().expect("in bounds");
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    Ok(out)
}

/// The variables a config reads from the environment, in order, without
/// duplicates (not `MARV_PROJECT_DIR`: that's Marv's own).
pub fn reads_of(raw: &Value) -> Vec<String> {
    let text = raw.to_string();
    let mut names: Vec<String> = Vec::new();
    let mut i = 0;
    while i < text.len() {
        if let Some((name, _, len)) = variable_at(&text[i..]) {
            if name != PROJECT_DIR_VAR && !names.iter().any(|n| n == name) {
                names.push(name.to_string());
            }
            i += len;
        } else {
            i += text[i..].chars().next().map_or(1, char::len_utf8);
        }
    }
    names
}

fn str_map(value: Option<&Value>, what: &str) -> Result<BTreeMap<String, String>, String> {
    match value {
        None => Ok(BTreeMap::new()),
        Some(Value::Object(map)) => map
            .iter()
            .map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())).ok_or_else(|| format!("{what}.{k} must be a string")))
            .collect(),
        Some(_) => Err(format!("{what} must be an object of strings")),
    }
}

fn listed(values: &BTreeMap<String, String>, sep: &str) -> String {
    values.iter().map(|(k, v)| format!("{k}{sep}{v}")).collect::<Vec<_>>().join(", ")
}

/// "oauth client c, port 33418, scope repo": never the secret (it should be a
/// `${VAR}`, but a literal one mustn't show either).
fn oauth_shown(o: &McpOAuthConfig) -> String {
    let parts: Vec<String> = [
        o.client_id.as_ref().map(|c| format!("client {c}")),
        o.callback_port.map(|p| format!("port {p}")),
        o.scope.as_ref().map(|s| format!("scope {s}")),
    ]
    .into_iter()
    .flatten()
    .collect();
    if parts.is_empty() { "oauth".into() } else { format!("oauth {}", parts.join(", ")) }
}

/// The raw config, as the user wrote it, on one line: `npx -y pkg · env K=${V}`
/// or `https://x/mcp · headers Authorization: Bearer ${T} · oauth client c`.
/// `raw` must be an entry [`parse_server`] accepts (it isn't re-validated).
pub fn display_of(raw: &Value) -> String {
    let get = |k: &str| raw.get(k);
    if let Some(url) = get("url").and_then(Value::as_str) {
        let mut parts = vec![url.to_string()];
        let headers = str_map(get("headers"), "headers").unwrap_or_default();
        if !headers.is_empty() {
            parts.push(format!("headers {}", listed(&headers, ": ")));
        }
        if let Some(oauth) = get("oauth") {
            let o = parse_oauth(oauth, &|_| Some(String::new())).unwrap_or_default();
            parts.push(oauth_shown(&o));
        }
        return parts.join(" · ");
    }
    let mut line = vec![get("command").and_then(Value::as_str).unwrap_or_default().to_string()];
    if let Some(Value::Array(args)) = get("args") {
        line.extend(args.iter().filter_map(Value::as_str).map(str::to_string));
    }
    let mut parts = vec![line.join(" ")];
    let env = str_map(get("env"), "env").unwrap_or_default();
    if !env.is_empty() {
        parts.push(format!("env {}", listed(&env, "=")));
    }
    parts.join(" · ")
}

fn parse_timeout(raw: &Value) -> Result<Option<f64>, String> {
    match raw.get("timeout") {
        None => Ok(None),
        Some(v) => match v.as_f64() {
            Some(t) if t > 0.0 => Ok(Some(t)),
            _ => Err("timeout must be a positive number of seconds".into()),
        },
    }
}

/// The `oauth` block, strict (unknown keys refused), secret expanded.
fn parse_oauth(raw: &Value, lookup: &dyn Fn(&str) -> Option<String>) -> Result<McpOAuthConfig, String> {
    let Value::Object(map) = raw else { return Err("oauth must be an object".into()) };
    let mut o = McpOAuthConfig::default();
    for (key, value) in map {
        let text = || {
            value
                .as_str()
                .filter(|s| !s.trim().is_empty())
                .map(str::to_string)
                .ok_or_else(|| format!("oauth.{key} must be a non-empty string"))
        };
        match key.as_str() {
            "clientId" => o.client_id = Some(text()?),
            "clientSecret" => o.client_secret = Some(expand(&text()?, lookup)?),
            "scope" => o.scope = Some(text()?.trim().to_string()),
            "callbackPort" => {
                let port = value
                    .as_u64()
                    .filter(|p| (1024..=65535).contains(p))
                    .ok_or("oauth.callbackPort must be a whole number from 1024 to 65535")?;
                o.callback_port = Some(port as u16);
            }
            other => return Err(format!("oauth: unknown key \"{other}\"")),
        }
    }
    Ok(o)
}

/// Canonical JSON (sorted keys, compact): serde_json's default map is ordered.
fn canonical(raw: &Value) -> String {
    raw.to_string()
}

/// One entry of an `mcpServers` map as a server. `where_` names the file in messages.
pub fn parse_server(
    name: &str,
    raw: &Value,
    source: ServerSource,
    lookup: &dyn Fn(&str) -> Option<String>,
    where_: &str,
) -> Result<McpServerConfig, String> {
    if !name_chars_ok(name) {
        return Err(format!("\"{name}\" in {where_}: use letters, digits, - and _ (at most 32)"));
    }
    // mcp__<server>__<tool> must read one way only: with "a__b" or "a_" allowed, server "a" + tool "b__c" and server
    // "a__b" + tool "c" (or "a" + "_x" and "a_" + "x") would get the same tool name.
    if name.contains("__") || name.ends_with('_') {
        return Err(format!("\"{name}\" in {where_}: a server name can't contain \"__\" or end in \"_\""));
    }
    let fail = |e: String| format!("\"{name}\" in {where_}: {e}");
    let Value::Object(map) = raw else { return Err(fail("an entry must be an object".into())) };
    let kind = map.get("type").and_then(Value::as_str);
    if kind == Some("sse") {
        return Err(fail("the old sse transport isn't supported; most servers also speak \"type\": \"http\"".into()));
    }
    let key = sha256_hex(canonical(raw).as_bytes());
    let timeout = parse_timeout(raw).map_err(fail)?;
    let base = |transport| McpServerConfig {
        name: name.to_string(),
        source,
        transport,
        timeout,
        key: key.clone(),
        display: display_of(raw),
        reads: reads_of(raw),
        runs_project_files: Vec::new(),
    };
    match kind {
        Some("http" | "streamable-http") => {
            let url = map
                .get("url")
                .and_then(Value::as_str)
                .filter(|u| !u.is_empty())
                .ok_or_else(|| fail("url must be a non-empty string".into()))?;
            let headers = str_map(map.get("headers"), "headers").map_err(fail)?;
            let oauth = map.get("oauth").map(|o| parse_oauth(o, lookup)).transpose().map_err(fail)?;
            let url = expand(url, lookup).map_err(fail)?;
            let headers =
                headers.into_iter().map(|(k, v)| Ok((k, expand(&v, lookup)?))).collect::<Result<_, String>>().map_err(fail)?;
            Ok(base(McpTransport::Http { url, headers, oauth }))
        }
        None | Some("stdio") => {
            // Unknown keys of a stdio entry are dropped, but this one would otherwise look like it did something.
            if map.contains_key("oauth") {
                return Err(fail("\"oauth\" is only for http servers (a login to a remote server)".into()));
            }
            let command = map
                .get("command")
                .and_then(Value::as_str)
                .filter(|c| !c.is_empty())
                .ok_or_else(|| fail("command must be a non-empty string".into()))?;
            let args = match map.get("args") {
                None => Vec::new(),
                Some(Value::Array(a)) => a
                    .iter()
                    .map(|v| v.as_str().map(str::to_string).ok_or_else(|| fail("args must be strings".into())))
                    .collect::<Result<_, _>>()?,
                Some(_) => return Err(fail("args must be a list of strings".into())),
            };
            let env = str_map(map.get("env"), "env").map_err(fail)?;
            let command = expand(command, lookup).map_err(fail)?;
            let args = args.iter().map(|a| expand(a, lookup)).collect::<Result<_, _>>().map_err(fail)?;
            let env = env.into_iter().map(|(k, v)| Ok((k, expand(&v, lookup)?))).collect::<Result<_, String>>().map_err(fail)?;
            Ok(base(McpTransport::Stdio { command, args, env }))
        }
        Some(other) => Err(fail(format!("unknown type \"{other}\" (use \"stdio\" or \"http\")"))),
    }
}

/// `root` joined with `word`, `.`/`..` resolved lexically (as Node's `resolve`).
fn lexical_join(root: &Path, word: &str) -> PathBuf {
    let mut out = PathBuf::new();
    for c in root.join(word).components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

/// The project files a stdio command line names, and the digest of their
/// names and contents (empty when there are none). The trust key input.
///
/// Each word, and the part after `=` of a `--flag=value` word, is resolved
/// against `root`; one that stays inside the project and is a file counts. A
/// symlink's target text is hashed too, so repointing it drops trust.
pub fn project_files(root: &Path, transport: &McpTransport) -> (Vec<String>, String) {
    let McpTransport::Stdio { command, args, .. } = transport else { return (Vec::new(), String::new()) };
    let mut files: Vec<String> = Vec::new();
    let mut input: Vec<u8> = Vec::new();
    let words = std::iter::once(command).chain(args).flat_map(|w| {
        let after_eq = w.starts_with('-').then(|| w.split_once('=').map(|(_, v)| v)).flatten();
        std::iter::once(w.as_str()).chain(after_eq)
    });
    for word in words {
        let path = lexical_join(root, word);
        let Ok(rel) = path.strip_prefix(root) else { continue };
        if rel.as_os_str().is_empty() {
            continue;
        }
        let rel_text = rel.components().map(|c| c.as_os_str().to_string_lossy()).collect::<Vec<_>>().join("/");
        if files.contains(&rel_text) || !path.is_file() {
            continue;
        }
        let Ok(content) = std::fs::read(&path) else { continue };
        input.extend_from_slice(rel_text.as_bytes());
        input.push(0);
        if let Ok(target) = std::fs::read_link(&path) {
            input.extend_from_slice(target.to_string_lossy().as_bytes());
            input.push(0);
        }
        input.extend_from_slice(&content);
        input.push(0);
        files.push(rel_text);
    }
    let digest = if files.is_empty() { String::new() } else { sha256_hex(&input) };
    (files, digest)
}

/// `mcpServers` in file order (serde_json's own map would sort it, and the
/// order of servers is the order of tools).
#[derive(Debug, Default)]
pub struct OrderedEntries(pub Vec<(String, Value)>);

impl<'de> Deserialize<'de> for OrderedEntries {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = OrderedEntries;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("an object of servers")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<OrderedEntries, A::Error> {
                let mut out = Vec::new();
                while let Some((k, v)) = map.next_entry::<String, Value>()? {
                    out.push((k, v));
                }
                Ok(OrderedEntries(out))
            }
        }
        d.deserialize_map(V)
    }
}

#[derive(Deserialize)]
struct FileShape {
    #[serde(rename = "mcpServers")]
    mcp_servers: Option<OrderedEntries>,
}

fn read_file(
    path: &Path,
    source: ServerSource,
    lookup: &dyn Fn(&str) -> Option<String>,
    problems: &mut Vec<String>,
) -> Vec<McpServerConfig> {
    let shown = path.display();
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(e) => {
            problems.push(format!("{shown}: {e}"));
            return Vec::new();
        }
    };
    let entries = match serde_json::from_str::<FileShape>(&text) {
        Ok(f) => f.mcp_servers.unwrap_or_default().0,
        Err(e) => {
            problems.push(format!("{shown}: {e}"));
            return Vec::new();
        }
    };
    let root = path.parent().unwrap_or(Path::new("."));
    let mut servers = Vec::new();
    for (name, raw) in entries {
        match parse_server(&name, &raw, source, lookup, &shown.to_string()) {
            Ok(mut server) => {
                if source == ServerSource::Project {
                    let (files, digest) = project_files(root, &server.transport);
                    if !files.is_empty() {
                        server.key = sha256_hex(format!("{}\0{digest}", server.key).as_bytes());
                        server.runs_project_files = files;
                    }
                }
                servers.push(server);
            }
            Err(problem) => problems.push(problem),
        }
    }
    servers
}

/// The servers to connect to (yours first, then the project's), and what
/// couldn't be read, and why. Never fails: a broken file is a problem line.
pub fn load_mcp_config(root: &Path, config_dir: &Path, env: &Env, include: Include) -> LoadedConfig {
    let mut problems = Vec::new();
    // Servers of yours start in your home folder (see manager::cwd_for); one that works on the project gets it this way.
    let root_text = root.to_string_lossy().into_owned();
    let lookup = |name: &str| if name == PROJECT_DIR_VAR { Some(root_text.clone()) } else { env.get(name).cloned() };
    let project_path = root.join(PROJECT_FILE);
    let project =
        if include.project { read_file(&project_path, ServerSource::Project, &lookup, &mut problems) } else { Vec::new() };
    let personal = if include.personal {
        read_file(&config_dir.join(PERSONAL_FILE), ServerSource::Personal, &lookup, &mut problems)
    } else {
        Vec::new()
    };
    let mut servers = personal;
    for server in project {
        if servers.iter().any(|p| p.name == server.name) {
            problems.push(format!(
                "\"{}\" in {} was ignored: you have a server of your own with that name.",
                server.name,
                project_path.display()
            ));
        } else {
            servers.push(server);
        }
    }
    LoadedConfig { servers, problems }
}

/// Errors with what loading `raw` as server `name` would report, except an
/// unset `${VAR}` (that's the user's environment, not the entry): `/mcp add`
/// checks an entry with it before writing it.
pub fn check_entry(name: &str, raw: &Value) -> Result<(), String> {
    parse_server(name, raw, ServerSource::Personal, &|_| Some("x".into()), "mcp.json").map(|_| ())
}

/// Servers given in code (the SDK's `mcpServers`), in `.mcp.json`'s format.
/// Trusted like your own: whoever wrote the program chose them.
pub fn parse_mcp_servers(entries: &[(String, Value)], env: &Env) -> LoadedConfig {
    let lookup = |name: &str| env.get(name).cloned();
    let mut out = LoadedConfig::default();
    for (name, raw) in entries {
        match parse_server(name, raw, ServerSource::Personal, &lookup, "mcpServers") {
            Ok(s) => out.servers.push(s),
            Err(p) => out.problems.push(p),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn env(pairs: &[(&str, &str)]) -> Env {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("marv-mcp-config-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn expands_var_and_default() {
        let e = env(&[("TOKEN", "t0k")]);
        let lookup = |n: &str| e.get(n).cloned();
        assert_eq!(expand("Bearer ${TOKEN}", &lookup).unwrap(), "Bearer t0k");
        assert_eq!(expand("${HOST:-localhost}:${PORT:-}", &lookup).unwrap(), "localhost:");
        assert_eq!(expand("$TOKEN and ${", &lookup).unwrap(), "$TOKEN and ${");
        assert_eq!(expand("${MISSING}", &lookup).unwrap_err(), "${MISSING} isn't set");
    }

    #[test]
    fn reads_lists_variables_once_without_marvs_own() {
        let raw = json!({"command": "x", "args": ["${A}", "${MARV_PROJECT_DIR}"], "env": {"K": "${B:-d}", "L": "${A}"}});
        assert_eq!(reads_of(&raw), vec!["A", "B"]);
    }

    #[test]
    fn server_names_stay_unambiguous_in_tool_names() {
        assert!(valid_server_name("files-1_x"));
        for bad in ["a__b", "a_", "", "has space", "é", &"x".repeat(33)] {
            assert!(!valid_server_name(bad), "{bad}");
            assert!(parse_server(bad, &json!({"command": "x"}), ServerSource::Personal, &|_| None, "f").is_err());
        }
    }

    #[test]
    fn parses_stdio_and_http_and_refuses_sse() {
        let e = env(&[("T", "secret")]);
        let lookup = |n: &str| e.get(n).cloned();
        let s = parse_server(
            "files",
            &json!({"command": "npx", "args": ["-y", "pkg"], "env": {"K": "${T}"}}),
            ServerSource::Project,
            &lookup,
            "f",
        )
        .unwrap();
        assert_eq!(
            s.transport,
            McpTransport::Stdio { command: "npx".into(), args: vec!["-y".into(), "pkg".into()], env: env(&[("K", "secret")]) }
        );
        assert_eq!(s.display, "npx -y pkg · env K=${T}");
        let h = parse_server(
            "docs",
            &json!({"type": "http", "url": "https://x/mcp", "headers": {"Authorization": "Bearer ${T}"}}),
            ServerSource::Personal,
            &lookup,
            "f",
        )
        .unwrap();
        assert!(matches!(&h.transport, McpTransport::Http { headers, .. } if headers["Authorization"] == "Bearer secret"));
        assert_eq!(h.display, "https://x/mcp · headers Authorization: Bearer ${T}");
        assert!(!h.display.contains("secret"));
        let err =
            parse_server("old", &json!({"type": "sse", "url": "https://x"}), ServerSource::Personal, &lookup, "f").unwrap_err();
        assert!(err.contains("sse transport isn't supported"));
    }

    #[test]
    fn oauth_block_is_strict_and_never_shows_the_secret() {
        let e = env(&[("S", "shh")]);
        let lookup = |n: &str| e.get(n).cloned();
        let raw = json!({"type": "http", "url": "https://x", "oauth": {"clientId": "c", "clientSecret": "${S}", "callbackPort": 33418, "scope": "repo"}});
        let s = parse_server("x", &raw, ServerSource::Personal, &lookup, "f").unwrap();
        let McpTransport::Http { oauth: Some(o), .. } = &s.transport else { panic!() };
        assert_eq!(o.client_secret.as_deref(), Some("shh"));
        assert_eq!(s.display, "https://x · oauth client c, port 33418, scope repo");
        for bad in [
            json!({"command": "x", "oauth": {}}),
            json!({"type": "http", "url": "u", "oauth": {"callbackPort": 80}}),
            json!({"type": "http", "url": "u", "oauth": {"what": 1}}),
        ] {
            assert!(parse_server("x", &bad, ServerSource::Personal, &lookup, "f").is_err(), "{bad}");
        }
    }

    #[test]
    fn trust_key_changes_with_config_and_with_project_files_it_runs() {
        let root = temp("key");
        std::fs::create_dir_all(root.join("mcp")).unwrap();
        std::fs::write(root.join("mcp/server.js"), "v1").unwrap();
        let file = |cmd: &str| {
            std::fs::write(root.join(PROJECT_FILE), json!({"mcpServers": {"s": {"command": "node", "args": [cmd]}}}).to_string())
                .unwrap()
        };
        let load = || load_mcp_config(&root, &root.join("nohome"), &Env::new(), Include::default()).servers.remove(0);
        file("mcp/server.js");
        let first = load();
        assert_eq!(first.runs_project_files, vec!["mcp/server.js"]);
        std::fs::write(root.join("mcp/server.js"), "v2").unwrap();
        let edited = load();
        assert_ne!(first.key, edited.key, "editing the script drops trust");
        file("../outside.js");
        assert!(load().runs_project_files.is_empty());
        let (files, _) = project_files(
            &root,
            &McpTransport::Stdio { command: "node".into(), args: vec!["--import=./mcp/server.js".into()], env: BTreeMap::new() },
        );
        assert_eq!(files, vec!["mcp/server.js"]);
    }

    #[test]
    fn project_server_cannot_take_your_servers_name_and_problems_dont_crash() {
        let root = temp("names");
        let home = root.join("home");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join(PERSONAL_FILE), r#"{"mcpServers": {"b": {"command": "x"}, "a": {"command": "y"}}}"#).unwrap();
        std::fs::write(root.join(PROJECT_FILE), r#"{"mcpServers": {"a": {"command": "evil"}, "c": {"command": "${UNSET}"}}}"#)
            .unwrap();
        let loaded = load_mcp_config(&root, &home, &Env::new(), Include::default());
        let names: Vec<_> = loaded.servers.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["b", "a"], "file order, yours first");
        assert_eq!(loaded.servers[1].source, ServerSource::Personal);
        assert_eq!(loaded.problems.len(), 2);
        std::fs::write(root.join(PROJECT_FILE), "not json").unwrap();
        assert_eq!(load_mcp_config(&root, &home, &Env::new(), Include::default()).problems.len(), 1);
    }

    #[test]
    fn marv_project_dir_is_the_project() {
        let root = temp("pd");
        std::fs::write(root.join(PROJECT_FILE), r#"{"mcpServers": {"p": {"command": "x", "args": ["${MARV_PROJECT_DIR}"]}}}"#)
            .unwrap();
        let s = load_mcp_config(&root, &root, &Env::new(), Include { personal: false, project: true }).servers.remove(0);
        assert!(matches!(s.transport, McpTransport::Stdio { ref args, .. } if args[0] == root.to_string_lossy()));
        assert!(s.reads.is_empty());
    }

    #[test]
    fn check_entry_ignores_unset_variables_only() {
        assert!(check_entry("x", &json!({"command": "${NOPE}"})).is_ok());
        assert!(check_entry("x", &json!({"command": ""})).is_err());
    }
}
