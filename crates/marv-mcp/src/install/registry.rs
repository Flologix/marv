//! The official MCP Registry (registry.modelcontextprotocol.io, no key).
//! Ports `src/mcp/install/registry.ts`.
//!
//! `/mcp search` lists it, `/mcp add <name>` installs an entry. The registry
//! isn't curated: a publisher proves only its namespace (`io.github.<user>` by
//! signing in to GitHub as that user, a domain by DNS), so that and the
//! repository are shown for the user to judge. A package then goes through the
//! npm/PyPI resolver at the version the entry names, so npm's facts are shown
//! too and the registry can't point at a version the package registry doesn't
//! have.
//!
//! - Required env vars become `${NAME}` (a default `${NAME:-x}`); optional ones
//!   are only named.
//! - Fixed argument values starting with `-` are refused (they could be an
//!   npx/uvx option); a required one without a value comes from after `--`.
//! - A `streamable-http` remote's headers become `${VAR}` references; a
//!   non-secret fixed value is refused (an entry can't plant a header).
//! - OCI is unsupported; a status other than `active` is refused.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value};

use super::proposal::{McpEntry, Proposal, ResolveDeps, ResolveOptions, fact, http_proposal, repo_of};
use super::source::{Source, parse_source, var_name};
use super::util::{config, encode_component, name_for};
use super::{npm, pypi};
use crate::text::printable;

/// `/mcp search` shows at most this many.
pub const MAX_RESULTS: usize = 10;
const OFFICIAL: &str = "io.modelcontextprotocol.registry/official";
/// Marv's `get` default: the registry's answers are small.
const MAX_BYTES: usize = 5 * 1024 * 1024;

fn known_base(kind: &str) -> Option<&'static str> {
    match kind {
        "npm" => Some("https://registry.npmjs.org"),
        "pypi" => Some("https://pypi.org"),
        _ => None,
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Input {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    value: Option<String>,
    #[serde(default)]
    default: Option<String>,
    #[serde(default)]
    is_required: Option<bool>,
    #[serde(default)]
    is_secret: Option<bool>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    value_hint: Option<String>,
    #[serde(default, rename = "type")]
    kind: Option<String>,
}

#[derive(Deserialize)]
struct Transport {
    #[serde(rename = "type")]
    kind: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Package {
    registry_type: String,
    #[serde(default)]
    registry_base_url: Option<String>,
    identifier: String,
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    transport: Option<Transport>,
    #[serde(default)]
    package_arguments: Option<Vec<Input>>,
    #[serde(default)]
    environment_variables: Option<Vec<Input>>,
}

#[derive(Deserialize)]
struct Remote {
    #[serde(rename = "type")]
    kind: String,
    url: String,
    #[serde(default)]
    headers: Option<Vec<Input>>,
}

#[derive(Deserialize)]
struct Repository {
    #[serde(default)]
    url: Option<String>,
}

#[derive(Deserialize)]
struct Server {
    name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    repository: Option<Repository>,
    #[serde(default)]
    packages: Option<Vec<Package>>,
    #[serde(default)]
    remotes: Option<Vec<Remote>>,
}

#[derive(Deserialize)]
struct Entry {
    server: Server,
    #[serde(default, rename = "_meta")]
    meta: Option<Map<String, Value>>,
}

#[derive(Deserialize)]
struct List {
    servers: Vec<Entry>,
}

/// active, deprecated or deleted, as the registry marks it (`None`: not said).
fn status_of(entry: &Entry) -> Option<&str> {
    entry.meta.as_ref()?.get(OFFICIAL)?.get("status")?.as_str()
}

fn parse<T: DeserializeOwned>(body: &str) -> crate::Result<T> {
    serde_json::from_str(body).map_err(|_| {
        config("The MCP Registry answered with something Marv doesn't understand (a proxy, or a new format): nothing was added.")
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryHit {
    pub name: String,
    pub description: Option<String>,
    pub version: Option<String>,
    pub repository: Option<String>,
}

/// What a namespace proves: `io.github.<user>` a GitHub sign-in, anything else a domain's DNS.
pub fn namespace_proof(name: &str) -> String {
    let namespace = name.split('/').next().unwrap_or(name);
    match namespace.strip_prefix("io.github.") {
        Some(user) if !user.is_empty() => format!("the publisher signed in to GitHub as {user}"),
        _ => format!("the publisher proved it controls {}", namespace.split('.').rev().collect::<Vec<_>>().join(".")),
    }
}

/// At most [`MAX_RESULTS`] latest, active entries matching the words.
pub async fn search_registry(words: &str, deps: &dyn ResolveDeps) -> crate::Result<Vec<RegistryHit>> {
    let url = format!("{}/v0/servers?search={}&version=latest&limit=30", deps.registry(), encode_component(words));
    let got = deps.get(&url, "application/json", MAX_BYTES, &deps.cancel()).await?;
    if !(200..300).contains(&got.status) {
        return Err(config(format!("The MCP Registry answered {}.", got.status)));
    }
    let list: List = parse(&got.body)?;
    Ok(list
        .servers
        .into_iter()
        .filter(|e| status_of(e).unwrap_or("active") == "active")
        .take(MAX_RESULTS)
        .map(|Entry { server, .. }| RegistryHit {
            repository: repo_of(server.repository.as_ref().and_then(|r| r.url.as_deref())),
            name: server.name,
            description: server.description.filter(|d| !d.is_empty()),
            version: server.version.filter(|v| !v.is_empty()),
        })
        .collect())
}

/// The registry's arguments: fixed values and defaults, then what the user gave after `--` fills the required rest.
/// A registry value that starts with "-" is refused: anyone can publish an entry, and a value like `--from` or `--`
/// could be read as an option to npx/uvx rather than the package. A named argument's own name (`--mode`) is the
/// package's.
fn args_of(inputs: &[Input], given: &[String], what: &str) -> crate::Result<Vec<String>> {
    let mut args = Vec::new();
    let mut rest = given.iter();
    let mut missing = Vec::new();
    for input in inputs {
        let value = input.value.as_ref().or(input.default.as_ref());
        let named = match (&input.kind, &input.name) {
            (Some(k), Some(n)) if k == "named" && !n.is_empty() => Some(n.as_str()),
            _ => None,
        };
        if let Some(value) = value.filter(|v| v.starts_with('-')) {
            let which = named.map_or("the argument value".to_string(), |n| format!("the argument {n} the value"));
            return Err(config(format!(
                "{what}'s entry gives {which} {value}, which starts with \"-\": Marv only adds values that can't be read as options. Nothing was added."
            )));
        }
        if let Some(value) = value {
            args.extend(named.map(str::to_string));
            args.push(value.clone());
        } else if input.is_required == Some(true) {
            match rest.next() {
                Some(next) => {
                    args.extend(named.map(str::to_string));
                    args.push(next.clone());
                }
                None => {
                    let hint = input.value_hint.as_deref().or(input.name.as_deref()).unwrap_or("value");
                    let about = input.description.as_ref().map(|d| format!(" ({d})")).unwrap_or_default();
                    missing.push(format!("{}<{hint}>{about}", named.map(|n| format!("{n} ")).unwrap_or_default()));
                }
            }
        }
        // An optional argument without a value is left out: the server's own default applies.
    }
    if !missing.is_empty() {
        let (an, it) = if missing.len() == 1 { ("an argument", "it") } else { ("arguments", "them") };
        return Err(config(format!(
            "{what} needs {an} the registry gives no value for: {}. Give {it} after --, e.g. /mcp add {what} -- <value>.",
            missing.join(", ")
        )));
    }
    args.extend(rest.cloned());
    Ok(args)
}

/// `{api_key}` → `API_KEY`: a variable name made of anything.
fn variable(text: &str) -> String {
    let mut v: String = text
        .to_uppercase()
        .chars()
        .map(|c| if c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_' { c } else { '_' })
        .collect();
    if v.starts_with(|c: char| c.is_ascii_digit()) {
        v.insert(0, '_');
    }
    v
}

/// Each `{name}` in `text` as `${NAME}`; `None` when there's none.
fn placeholders(text: &str) -> Option<String> {
    let mut out = String::new();
    let mut found = false;
    let mut rest = text;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        match after.find('}') {
            Some(close) if close > 0 => {
                out.push_str(&format!("${{{}}}", variable(&after[..close])));
                rest = &after[close + 1..];
                found = true;
            }
            _ => {
                out.push('{');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    found.then_some(out)
}

/// "Bearer {api_key}" → "Bearer ${API_KEY}"; a value-less header → `${HEADER_NAME}`; a secret's fixed value too.
/// Only references reach the file, as with a typed `--header`: a fixed value is refused, so a registry entry can't
/// plant a static Authorization or Cookie header in your config.
fn header_value(input: &Input, what: &str, url: &str) -> crate::Result<Option<String>> {
    let name = input.name.as_deref().unwrap_or("TOKEN");
    let Some(value) = &input.value else {
        return Ok((input.is_required == Some(true)).then(|| format!("${{{}}}", variable(name))));
    };
    if let Some(replaced) = placeholders(value) {
        return Ok(Some(replaced));
    }
    if input.is_secret == Some(true) {
        return Ok(Some(format!("${{{}}}", variable(name))));
    }
    let shown = input.name.as_deref().unwrap_or("undefined");
    Err(config(format!(
        "{what}'s entry gives the header {shown} a fixed value: Marv writes only ${{VAR}} references into your config, so a registry entry can't plant a header. Nothing was added; to add it by its URL with your own: /mcp add {url} --header \"{shown}: Bearer ${{TOKEN}}\""
    )))
}

/// The registry's environment variables: required ones as `${NAME}` (a default as `${NAME:-default}`); optional ones
/// are only named.
fn env_of(inputs: &[Input]) -> (BTreeMap<String, String>, Vec<String>) {
    let mut env = BTreeMap::new();
    let mut optional = Vec::new();
    for input in inputs {
        let Some(name) = input.name.as_deref().filter(|n| var_name(n)) else { continue };
        let fallback = input.default.as_deref().filter(|d| !d.contains(['}', '$']));
        if input.is_required == Some(true) {
            let value = match fallback {
                Some(f) => format!("${{{name}:-{f}}}"),
                None => format!("${{{name}}}"),
            };
            env.insert(name.to_string(), value);
        } else {
            optional.push(name.to_string());
        }
    }
    (env, optional)
}

pub async fn resolve_registry(name: &str, opts: &ResolveOptions, deps: &dyn ResolveDeps) -> crate::Result<Proposal> {
    let url = format!("{}/v0/servers/{}/versions/latest", deps.registry(), encode_component(name));
    let got = deps.get(&url, "application/json", MAX_BYTES, &deps.cancel()).await?;
    if got.status == 404 {
        return Err(config(format!("There's no server named {name} in the MCP Registry (/mcp search <words> finds one).")));
    }
    if !(200..300).contains(&got.status) {
        return Err(config(format!("The MCP Registry answered {} for {name}.", got.status)));
    }
    let entry: Entry = parse(&got.body)?;
    if let Some(status) = status_of(&entry).filter(|s| *s != "active") {
        return Err(config(format!("The MCP Registry marks {} as {status}: not added.", entry.server.name)));
    }
    let server = &entry.server;

    let server_name = name_for(opts, &Source::Registry { name: name.into() })?;
    let stdio = |p: &&Package| p.transport.as_ref().map_or("stdio", |t| t.kind.as_str()) == "stdio";
    let packages = server.packages.as_deref().unwrap_or_default();
    let remotes = server.remotes.as_deref().unwrap_or_default();
    let pkg = packages
        .iter()
        .filter(stdio)
        .find(|p| p.registry_type == "npm")
        .or_else(|| packages.iter().filter(stdio).find(|p| p.registry_type == "pypi"));
    let remote = remotes.iter().find(|r| r.kind == "streamable-http");
    let mut registry_facts = vec![fact(
        "registry",
        &format!(
            "{}{} (namespace verified: {})",
            server.name,
            server.version.as_ref().map(|v| format!(" {v}")).unwrap_or_default(),
            namespace_proof(&server.name)
        ),
    )];
    if let Some(d) = server.description.as_ref().filter(|d| !d.is_empty()) {
        registry_facts.push(fact("about", &d.chars().take(200).collect::<String>()));
    }

    if let Some(pkg) = pkg {
        let base = pkg.registry_base_url.as_deref().map(|b| b.trim_end_matches('/'));
        if let Some(base) = base.filter(|b| Some(*b) != known_base(&pkg.registry_type)) {
            return Err(config(format!(
                "{}'s package is on {base}, not the public {} registry: not supported.",
                server.name, pkg.registry_type
            )));
        }
        let Some(version) = &pkg.version else {
            return Err(config(format!(
                "{}'s entry names no version of {}: add it yourself with an exact version.",
                server.name, pkg.identifier
            )));
        };
        let args = args_of(pkg.package_arguments.as_deref().unwrap_or_default(), &opts.args, name)?;
        let (mut env, optional) = env_of(pkg.environment_variables.as_deref().unwrap_or_default());
        for n in &opts.env {
            env.insert(n.clone(), format!("${{{n}}}"));
        }
        let inner = ResolveOptions { name: Some(server_name), env: vec![], headers: BTreeMap::new(), args };
        // The identifier goes through the same parser as typed input: a registry entry can't smuggle a URL or flags in.
        let parsed = parse_source(&format!("{}:{}@{version}", pkg.registry_type, pkg.identifier)).map_err(config)?;
        let (mut resolved, _) = match &parsed {
            Source::Npm { name, version } => npm::resolve_npm(name, version.as_deref(), &inner, deps).await?,
            Source::Pypi { name, version } => pypi::resolve_pypi(name, version.as_deref(), &inner, deps).await?,
            _ => return Err(config(format!("{}'s package {} isn't one Marv can add.", server.name, pkg.identifier))),
        };
        if let McpEntry::Stdio { env: slot, .. } = &mut resolved.entry {
            *slot = (!env.is_empty()).then_some(env);
        }
        resolved.facts.extend(registry_facts);
        if !optional.is_empty() {
            resolved
                .facts
                .push(fact("optional", &format!("{} (the server can read these too: add with --env NAME)", optional.join(", "))));
        }
        return Ok(resolved);
    }

    if let Some(remote) = remote {
        let Ok(Source::Http { url }) = parse_source(&remote.url) else {
            return Err(config(format!("{}'s remote URL {} isn't one Marv can add.", server.name, remote.url)));
        };
        let mut headers = BTreeMap::new();
        for h in remote.headers.iter().flatten() {
            let Some(n) = h.name.as_ref().filter(|n| !n.is_empty() && n.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-'))
            else {
                continue;
            };
            if let Some(value) = header_value(h, &server.name, &remote.url)? {
                headers.insert(n.clone(), value);
            }
        }
        headers.extend(opts.headers.iter().map(|(k, v)| (k.clone(), v.clone())));
        let mut proposal = http_proposal(&url, server_name, &headers);
        proposal.facts.extend(registry_facts);
        return Ok(proposal);
    }

    if let Some(oci) = packages.iter().find(|p| p.registry_type == "oci") {
        return Err(config(format!("Docker images aren't supported yet; the entry says: docker run {}", oci.identifier)));
    }
    let what = match (packages.first(), remotes.first()) {
        (Some(p), _) => format!("{} {}", p.registry_type, p.identifier),
        (None, Some(r)) => format!("a {} remote", r.kind),
        (None, None) => "nothing".into(),
    };
    Err(config(format!(
        "{} offers {what}, which Marv can't add yet (it adds npm and PyPI packages and https remotes).",
        server.name
    )))
}

/// `/mcp search`'s answer, as Markdown. Descriptions are the publishers' own words, made printable and kept short.
pub fn search_text(words: &str, hits: &[RegistryHit]) -> String {
    if hits.is_empty() {
        return format!("Nothing in the MCP Registry matches \"{}\".", printable(words));
    }
    let clean = |text: &str, max: usize| {
        let one = printable(text).split_whitespace().collect::<Vec<_>>().join(" ").replace('`', "'");
        crate::text::truncate(&one, max)
    };
    let mut parts = vec![format!(
        "**MCP Registry** matches for \"{}\". The registry isn't curated: check the publisher and the repository before adding one.",
        printable(words)
    )];
    for h in hits {
        let about: Vec<String> = [
            h.version.clone(),
            h.description.as_deref().map(|d| clean(d, 160)).filter(|d| !d.is_empty()),
            h.repository.as_deref().map(|r| clean(r, 100)).filter(|r| !r.is_empty()),
        ]
        .into_iter()
        .flatten()
        .collect();
        let about = if about.is_empty() { String::new() } else { format!(": {}", about.join(" · ")) };
        parts.push(format!("- `{}`{about}\n  `/mcp add {}`", h.name, h.name));
    }
    parts.join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::install::util::fake::Fake;

    fn text(p: &Proposal) -> String {
        [p.facts.clone(), p.warnings.clone()].concat().join("\n")
    }

    fn none() -> ResolveOptions {
        ResolveOptions::default()
    }

    fn vars(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    async fn refusal(name: &str) -> String {
        resolve_registry(name, &none(), &Fake::default()).await.unwrap_err().to_string()
    }

    #[test]
    fn namespaces_say_what_they_prove() {
        assert_eq!(namespace_proof("io.github.acme/x"), "the publisher signed in to GitHub as acme");
        assert_eq!(namespace_proof("com.example/x"), "the publisher proved it controls example.com");
    }

    #[tokio::test]
    async fn search_latest_active_entries_only_at_most_10() {
        let deps = Fake::default();
        let hits = search_registry("files", &deps).await.unwrap();
        let names: Vec<&str> = hits.iter().map(|h| h.name.as_str()).collect();
        assert_eq!(names, ["io.modelcontextprotocol/filesystem", "com.pulsemcp/remote-filesystem"]);
        assert_eq!(
            hits[0],
            RegistryHit {
                name: "io.modelcontextprotocol/filesystem".into(),
                description: Some("Secure file operations with configurable access controls.".into()),
                version: Some("2026.1.14".into()),
                repository: Some("github.com/modelcontextprotocol/servers".into()),
            }
        );
        assert_eq!(deps.requests.lock().unwrap().last().unwrap(), "/registry/v0/servers?search=files&version=latest&limit=30");
        let many: Vec<Value> =
            (0..14).map(|i| serde_json::json!({"server": {"name": format!("com.example/s{i}"), "version": "1.0.0"}})).collect();
        *deps.search_body.lock().unwrap() = Some(serde_json::json!({ "servers": many }).to_string());
        assert_eq!(search_registry("s", &deps).await.unwrap().len(), 10);
        *deps.search_body.lock().unwrap() = Some("<html>proxy</html>".into());
        assert!(search_registry("s", &deps).await.unwrap_err().to_string().contains("doesn't understand"));
    }

    #[tokio::test]
    async fn an_npm_package_goes_through_the_npm_resolver_at_the_registrys_version() {
        let opts = ResolveOptions { args: vec!["/data".into()], ..none() };
        let p = resolve_registry("io.modelcontextprotocol/filesystem", &opts, &Fake::default()).await.unwrap();
        assert_eq!(p.name, "filesystem");
        assert_eq!(
            p.entry,
            McpEntry::Stdio {
                command: "npx".into(),
                args: ["-y", "@modelcontextprotocol/server-filesystem@2025.8.21", "stdio", "--mode", "safe", "/data"]
                    .map(String::from)
                    .to_vec(),
                env: Some(vars(&[("FS_TOKEN", "${FS_TOKEN}"), ("FS_LEVEL", "${FS_LEVEL:-info}")])),
            }
        );
        let t = text(&p);
        assert!(t.contains("2025.8.21, published 2025-08-21"), "{t}"); // npm's facts
        assert!(t.contains(
            "registry io.modelcontextprotocol/filesystem 2025.8.21 (namespace verified: the publisher proved it controls modelcontextprotocol.io)"
        ));
        assert!(t.contains("optional FS_DEBUG"));
    }

    #[tokio::test]
    async fn a_required_argument_with_no_value_must_come_after_the_dashes() {
        let e = refusal("io.modelcontextprotocol/filesystem").await;
        assert!(e.contains("<root_dir> (The folder it may read)") && e.contains("after --"), "{e}");
    }

    #[tokio::test]
    async fn a_pypi_package_with_an_oci_one_listed_first_skipped() {
        let opts = ResolveOptions { env: vec!["EXTRA".into()], ..none() };
        let p = resolve_registry("io.github.acme/fetch", &opts, &Fake::default()).await.unwrap();
        assert_eq!(
            p.entry,
            McpEntry::Stdio {
                command: "uvx".into(),
                args: vec!["mcp-server-fetch@2025.4.7".into()],
                env: Some(vars(&[("EXTRA", "${EXTRA}")])),
            }
        );
        assert!(text(&p).contains("signed in to GitHub as acme"));
    }

    #[tokio::test]
    async fn a_remote_streamable_http_with_its_headers_as_references() {
        let p = resolve_registry("ai.smithery/github", &none(), &Fake::default()).await.unwrap();
        assert_eq!(
            p.entry,
            McpEntry::Http {
                kind: super::super::proposal::HttpKind::Http,
                url: "https://server.smithery.ai/github/mcp".into(),
                headers: Some(vars(&[("Authorization", "Bearer ${SMITHERY_API_KEY}"), ("X-Api-Key", "${X_API_KEY}")])),
            }
        );
    }

    #[tokio::test]
    async fn an_argument_value_that_looks_like_a_flag_is_refused_naming_it() {
        assert_eq!(
            refusal("com.example/flag-arg").await,
            "com.example/flag-arg's entry gives the argument value --prefer-offline, which starts with \"-\": Marv only adds values that can't be read as options. Nothing was added."
        );
        assert!(
            refusal("com.example/flag-default").await.contains("argument --mode the value --from=evil, which starts with \"-\"")
        );
    }

    #[tokio::test]
    async fn a_header_with_a_fixed_value_is_refused() {
        let e = refusal("com.example/fixed-header").await;
        assert!(e.starts_with("com.example/fixed-header's entry gives the header Authorization a fixed value"), "{e}");
        assert!(e.contains("only ${VAR} references"));
        assert!(e.contains("--header \"Authorization: Bearer ${TOKEN}\""));
    }

    #[tokio::test]
    async fn docker_only_deprecated_unknown() {
        assert_eq!(
            refusal("io.github.acme/docker-only").await,
            "Docker images aren't supported yet; the entry says: docker run ghcr.io/acme/docker-only:1.0.0"
        );
        assert!(refusal("com.example/deprecated").await.contains("deprecated"));
        assert!(refusal("com.example/nope").await.contains("/mcp search"));
    }

    #[test]
    fn header_placeholders_become_references() {
        assert_eq!(placeholders("Bearer {api_key}").as_deref(), Some("Bearer ${API_KEY}"));
        assert_eq!(placeholders("{a}-{9b}").as_deref(), Some("${A}-${_9B}"));
        assert_eq!(placeholders("{} plain"), None);
    }

    #[test]
    fn search_text_is_printable_and_short() {
        assert_eq!(search_text("x\u{1b}[2J", &[]), "Nothing in the MCP Registry matches \"x\".");
        let hit = RegistryHit {
            name: "io.github.a/b".into(),
            description: Some(format!("Does `things`\n\u{1b}]52;c;aGk=\u{7}{}", "x".repeat(300))),
            version: Some("1.0.0".into()),
            repository: Some("github.com/a/b".into()),
        };
        let text = search_text("b", &[hit]);
        assert!(text.starts_with("**MCP Registry** matches for \"b\"."));
        assert!(text.contains("- `io.github.a/b`: 1.0.0 · Does 'things' xxx"));
        assert!(text.contains("… · github.com/a/b\n  `/mcp add io.github.a/b`"));
        assert!(!text.contains('\u{1b}'));
    }
}
