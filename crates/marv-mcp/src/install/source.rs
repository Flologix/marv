//! What the user typed after `/mcp add`: which kind of source it is (npm, PyPI,
//! a URL, a GitHub repository, a name in the MCP Registry), the server's name,
//! and the options. Ports `src/mcp/install/source.ts`. Pure: nothing here
//! fetches anything.
//!
//! - Secrets only as `${VAR}`: `--env NAME=value` and literal header values are
//!   refused, so a secret never sits in the file.
//! - Plain http only to loopback (a token in a header would travel in clear).

use std::collections::BTreeMap;

use url::Url;

use crate::config::valid_server_name;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Source {
    Npm { name: String, version: Option<String> },
    Pypi { name: String, version: Option<String> },
    Http { url: String },
    Github { owner: String, repo: String },
    Registry { name: String },
}

/// The options of `/mcp add`, checked.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AddOptions {
    pub source: String,
    pub name: Option<String>,
    /// Variable names: each is written as `"NAME": "${NAME}"`.
    pub env: Vec<String>,
    /// Header values, each a `${VAR}` reference (with an optional scheme like "Bearer").
    pub headers: BTreeMap<String, String>,
    /// Passed to the server, after the package (what came after `--`).
    pub args: Vec<String>,
}

pub const ADD_USAGE: &str = "/mcp add <npm package | pypi:<package> | https URL | GitHub repo | registry name> [--name <name>] [--env VAR]... [--header \"Name: Bearer ${VAR}\"]... [-- <server args>...]";

fn not_a_source(input: &str) -> String {
    format!("\"{input}\" isn't an npm package, a URL, a GitHub repository or an MCP Registry name. Usage: {ADD_USAGE}")
}

/// npm's rules for one name segment: lowercase, `[a-z0-9-~][a-z0-9-._~]*`.
fn npm_segment(s: &str) -> bool {
    let mut b = s.bytes();
    b.next().is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-' || c == b'~')
        && b.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || b"-._~".contains(&c))
}

fn npm_name(name: &str) -> bool {
    match name.strip_prefix('@') {
        Some(scoped) => scoped.split_once('/').is_some_and(|(scope, pkg)| npm_segment(scope) && npm_segment(pkg)),
        None => npm_segment(name),
    }
}

fn pypi_name(name: &str) -> bool {
    let b = name.as_bytes();
    !b.is_empty()
        && b[0].is_ascii_alphanumeric()
        && b[b.len() - 1].is_ascii_alphanumeric()
        && b.iter().all(|c| c.is_ascii_alphanumeric() || b"._-".contains(c))
}

fn version_ok(v: &str) -> bool {
    let b = v.as_bytes();
    !b.is_empty() && b[0].is_ascii_alphanumeric() && b.iter().all(|c| c.is_ascii_alphanumeric() || b".+!_-".contains(c))
}

/// io.github.acme/weather: a reverse-DNS namespace, a slash, and a name.
fn registry_name(text: &str) -> bool {
    let Some((ns, name)) = text.split_once('/') else { return false };
    let ok = |s: &str, extra: &[u8]| {
        s.as_bytes().first().is_some_and(u8::is_ascii_alphanumeric)
            && s.bytes().all(|c| c.is_ascii_alphanumeric() || extra.contains(&c))
    };
    ok(ns, b".-") && ok(name, b"._-")
}

pub(crate) fn var_name(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty() && (b[0].is_ascii_alphabetic() || b[0] == b'_') && b.iter().all(|c| c.is_ascii_alphanumeric() || *c == b'_')
}

/// An optional scheme word, then one `${VAR}`. Anything else could be a secret typed in.
fn header_value_ok(v: &str) -> bool {
    let reference = match v.split_once(' ') {
        Some((scheme, rest)) => {
            let sb = scheme.as_bytes();
            if !(sb.first().is_some_and(u8::is_ascii_alphabetic) && sb.iter().all(|c| c.is_ascii_alphanumeric() || *c == b'-')) {
                return false;
            }
            rest
        }
        None => v,
    };
    reference.strip_prefix("${").and_then(|r| r.strip_suffix('}')).is_some_and(var_name)
}

/// "name@1.2.3" → name and version; the "@" of a scope doesn't count.
fn split_version(text: &str) -> (&str, Option<&str>) {
    match text.rfind('@') {
        Some(at) if at > 0 => (&text[..at], Some(&text[at + 1..])),
        _ => (text, None),
    }
}

fn npm(text: &str, input: &str) -> Result<Source, String> {
    let (name, version) = split_version(text);
    if !npm_name(name) || name.len() > 214 || version.is_some_and(|v| !version_ok(v)) {
        return Err(not_a_source(input));
    }
    Ok(Source::Npm { name: name.into(), version: version.map(str::to_string) })
}

fn pypi(text: &str, input: &str) -> Result<Source, String> {
    let (name, version) = split_version(text);
    if !pypi_name(name) || version.is_some_and(|v| !version_ok(v)) {
        return Err(not_a_source(input));
    }
    Ok(Source::Pypi { name: name.into(), version: version.map(str::to_string) })
}

/// localhost, 127.0.0.0/8 and [::1]: the only hosts plain http is allowed to.
fn loopback(url: &Url) -> bool {
    match url.host() {
        Some(url::Host::Domain(d)) => d == "localhost",
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    }
}

/// GitHub's owner and repository name rules (Marv: `parseGithub` in `tools/web/github.ts`).
fn github_repo(owner: &str, repo: &str) -> bool {
    let owner_ok = !owner.is_empty()
        && owner.len() <= 39
        && !owner.starts_with('-')
        && owner.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-');
    let repo = repo.strip_suffix(".git").unwrap_or(repo);
    let repo_ok = !repo.is_empty()
        && repo != "."
        && repo != ".."
        && repo.len() <= 100
        && repo.bytes().all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c));
    owner_ok && repo_ok
}

fn has_scheme(text: &str) -> bool {
    let Some((scheme, _)) = text.split_once("://") else { return false };
    let b = scheme.as_bytes();
    b.first().is_some_and(u8::is_ascii_alphabetic) && b.iter().all(|c| c.is_ascii_alphanumeric() || b"+.-".contains(c))
}

/// Which source the text names, in this order: GitHub, other URLs, `npm:`/`pypi:`,
/// a scoped or bare npm name, a registry name.
pub fn parse_source(input: &str) -> Result<Source, String> {
    let text = input.trim();
    if has_scheme(text) {
        let url = Url::parse(text).map_err(|_| not_a_source(input))?;
        if matches!(url.host_str(), Some("github.com" | "www.github.com")) {
            // Any link into a repository names the repository: its README is what's read.
            let mut parts = url.path_segments().into_iter().flatten().filter(|s| !s.is_empty());
            return match (parts.next(), parts.next()) {
                (Some(owner), Some(repo)) if url.scheme() == "https" && github_repo(owner, repo) => {
                    Ok(Source::Github { owner: owner.into(), repo: repo.strip_suffix(".git").unwrap_or(repo).into() })
                }
                _ => Err(format!("{text} isn't a GitHub repository (https://github.com/<owner>/<repo>).")),
            };
        }
        return match url.scheme() {
            "https" => Ok(Source::Http { url: url.to_string() }),
            "http" if loopback(&url) => Ok(Source::Http { url: url.to_string() }),
            "http" => Err(format!(
                "Use https for {}: over plain http, a token in a header would travel in clear (plain http is for localhost only).",
                url.host_str().unwrap_or("")
            )),
            _ => Err(not_a_source(input)),
        };
    }
    if let Some(rest) = text.strip_prefix("npm:") {
        return npm(rest, input);
    }
    if let Some(rest) = text.strip_prefix("pypi:") {
        return pypi(rest, input);
    }
    if text.starts_with('@') {
        return npm(text, input);
    }
    if text.contains('/') {
        return if registry_name(text) { Ok(Source::Registry { name: text.into() }) } else { Err(not_a_source(input)) };
    }
    npm(text, input)
}

const PREFIXES: [&str; 3] = ["mcp-server-", "server-", "mcp-"];
const SUFFIXES: [&str; 2] = ["-mcp-server", "-mcp"];
const GENERIC_LABELS: [&str; 3] = ["mcp", "www", "api"];

fn from_package(name: &str) -> String {
    let mut base = match name.strip_prefix('@') {
        Some(scoped) => scoped.split_once('/').map_or(name, |(_, pkg)| pkg),
        None => name,
    };
    if let Some(p) = PREFIXES.iter().find(|p| base.starts_with(**p) && base.len() > p.len()) {
        base = &base[p.len()..];
    }
    if let Some(s) = SUFFIXES.iter().find(|s| base.ends_with(**s) && base.len() > s.len()) {
        base = &base[..base.len() - s.len()];
    }
    base.to_string()
}

/// What a source would be called in `mcp.json` (`--name` overrides it).
pub fn server_name(source: &Source) -> Result<String, String> {
    let raw = match source {
        Source::Npm { name, .. } | Source::Pypi { name, .. } => from_package(name),
        Source::Github { repo, .. } => from_package(repo),
        Source::Registry { name } => from_package(name.rsplit('/').next().unwrap_or(name)),
        Source::Http { url } => {
            let host = Url::parse(url).ok().and_then(|u| u.host_str().map(str::to_string)).unwrap_or_default();
            let labels: Vec<&str> = host.split('.').collect();
            labels.iter().find(|l| !GENERIC_LABELS.contains(*l)).unwrap_or(&labels[0]).to_string()
        }
    };
    let mut name = String::new();
    for c in raw.chars() {
        let c = if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' };
        if !(c == '_' && name.ends_with('_')) {
            name.push(c);
        }
    }
    let name: String = name.trim_matches('_').chars().take(32).collect();
    let name = name.trim_end_matches('_').to_string();
    if !valid_server_name(&name) {
        return Err("Couldn't make a server name from it: pass --name <name> (letters, digits, - and _).".into());
    }
    Ok(name)
}

/// Splits like a shell, with quotes but no expansion: `${VAR}` stays as typed.
pub fn split_words(text: &str) -> Result<Vec<String>, String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut quote: Option<char> = None;
    for ch in text.chars() {
        if let Some(q) = quote {
            if ch == q { quote = None } else { word.push(ch) }
        } else if ch == '"' || ch == '\'' {
            quote = Some(ch);
            in_word = true;
        } else if ch.is_whitespace() {
            if in_word {
                words.push(std::mem::take(&mut word));
            }
            in_word = false;
        } else {
            word.push(ch);
            in_word = true;
        }
    }
    if let Some(q) = quote {
        return Err(format!("A {q} isn't closed."));
    }
    if in_word {
        words.push(word);
    }
    Ok(words)
}

/// `/mcp add`'s arguments. Secret values are refused: only `${VAR}` references are ever written.
pub fn parse_add_args(text: &str) -> Result<AddOptions, String> {
    let words = split_words(text)?;
    let mut options = AddOptions::default();
    let mut source: Option<String> = None;
    let mut i = 0;
    while i < words.len() {
        let word = words[i].as_str();
        let mut value = || {
            i += 1;
            words.get(i).cloned().ok_or_else(|| format!("{word} needs a value. Usage: {ADD_USAGE}"))
        };
        match word {
            "--" => {
                options.args = words[i + 1..].to_vec();
                break;
            }
            "--name" => {
                let name = value()?;
                if !valid_server_name(&name) {
                    return Err(format!(
                        "\"{name}\" can't be a server name: use letters, digits, - and _ (at most 32), without \"__\" or a trailing \"_\"."
                    ));
                }
                options.name = Some(name);
            }
            "--env" => {
                let name = value()?;
                if let Some((variable, _)) = name.split_once('=') {
                    return Err(format!(
                        "--env takes a variable's name, not its value: Marv writes \"{variable}\": \"${{{variable}}}\" and the server gets the value from your environment, so it never sits in the file. Set {variable} in your shell and pass --env {variable}."
                    ));
                }
                if !var_name(&name) {
                    return Err(format!("--env {name}: not a variable name."));
                }
                if !options.env.contains(&name) {
                    options.env.push(name);
                }
            }
            "--header" => {
                let header = value()?;
                let (key, val) = match header.find(':') {
                    Some(c) if c > 0 => (header[..c].trim().to_string(), header[c + 1..].trim().to_string()),
                    _ => (String::new(), String::new()),
                };
                if key.is_empty() || !key.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-') {
                    return Err(format!("--header \"{header}\": write it as \"Name: value\"."));
                }
                if !header_value_ok(&val) {
                    return Err(format!(
                        "--header {key}: the value must be a ${{VAR}} reference (like \"Bearer ${{TOKEN}}\"), so the secret stays in your environment and never sits in the file."
                    ));
                }
                options.headers.insert(key, val);
            }
            w if w.starts_with("--") => return Err(format!("Unknown option {w}. Usage: {ADD_USAGE}")),
            w => match &source {
                None => source = Some(w.to_string()),
                Some(first) => {
                    return Err(format!(
                        "Only one source at a time (\"{first}\", then \"{w}\"); server arguments go after --. Usage: {ADD_USAGE}"
                    ));
                }
            },
        }
        i += 1;
    }
    options.source = source.ok_or_else(|| format!("Usage: {ADD_USAGE}"))?;
    Ok(options)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn npm_src(name: &str, version: Option<&str>) -> Source {
        Source::Npm { name: name.into(), version: version.map(str::to_string) }
    }

    #[test]
    fn sources_in_order() {
        assert_eq!(parse_source("@scope/pkg@1.2.3").unwrap(), npm_src("@scope/pkg", Some("1.2.3")));
        assert_eq!(parse_source("npm:pkg").unwrap(), npm_src("pkg", None));
        assert_eq!(
            parse_source("pypi:mcp-server-git@0.6.2").unwrap(),
            Source::Pypi { name: "mcp-server-git".into(), version: Some("0.6.2".into()) }
        );
        assert_eq!(parse_source("io.github.acme/weather").unwrap(), Source::Registry { name: "io.github.acme/weather".into() });
        assert_eq!(
            parse_source("https://github.com/acme/tool/tree/main").unwrap(),
            Source::Github { owner: "acme".into(), repo: "tool".into() }
        );
        assert_eq!(
            parse_source("https://mcp.example.com/mcp").unwrap(),
            Source::Http { url: "https://mcp.example.com/mcp".into() }
        );
        assert!(parse_source("http://127.0.0.2:3000/mcp").is_ok());
        assert!(parse_source("http://example.com/mcp").unwrap_err().starts_with("Use https"));
        for bad in ["UPPER", "a b/c d", "ftp://x/y", "http://github.com/a/b"] {
            assert!(parse_source(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn server_names_from_sources() {
        assert_eq!(server_name(&npm_src("@modelcontextprotocol/server-filesystem", None)).unwrap(), "filesystem");
        assert_eq!(server_name(&npm_src("weather-mcp-server", None)).unwrap(), "weather");
        assert_eq!(server_name(&Source::Http { url: "https://mcp.linear.app/sse".into() }).unwrap(), "linear");
        assert_eq!(server_name(&Source::Registry { name: "io.github.a/mcp-x.y".into() }).unwrap(), "x_y");
    }

    #[test]
    fn add_args_refuse_secret_values() {
        let o = parse_add_args(r#"pkg --name x --env TOKEN --header "Authorization: Bearer ${TOKEN}" -- --flag a"#).unwrap();
        assert_eq!(o.source, "pkg");
        assert_eq!(o.name.as_deref(), Some("x"));
        assert_eq!(o.env, vec!["TOKEN"]);
        assert_eq!(o.headers["Authorization"], "Bearer ${TOKEN}");
        assert_eq!(o.args, vec!["--flag", "a"]);
        assert!(parse_add_args("pkg --env TOKEN=abc").unwrap_err().contains("not its value"));
        assert!(parse_add_args("pkg --header 'Authorization: Bearer sk-123'").unwrap_err().contains("${VAR} reference"));
        assert!(parse_add_args("a b").unwrap_err().starts_with("Only one source"));
        assert!(parse_add_args("pkg --what").unwrap_err().starts_with("Unknown option"));
        assert!(parse_add_args("'open").unwrap_err().contains("isn't closed"));
        assert!(parse_add_args("").is_err());
    }
}
