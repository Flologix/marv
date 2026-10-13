//! An npm package as an MCP server: `npx -y <pkg>@<exact version>`. Ports
//! `src/mcp/install/npm.ts`.
//!
//! The version is resolved here, before the approval, and written into args:
//! npm never lets a published version's files change, so it fixes the server's
//! own code (its dependencies still follow their ranges: npx has no lockfile).
//! A requested version must exist. Facts: version, publish date, publisher,
//! maintainers, repository; warnings: deprecated, published in the last
//! [`YOUNG_DAYS`] days.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::Value;

use super::proposal::{McpEntry, Proposal, ResolveDeps, ResolveOptions, env_entries, fact, repo_of};
use super::source::Source;
use super::util::{age_days, age_text, config, day, iso_ms, name_for};

/// A packument has every version's metadata: popular packages run to megabytes.
pub const MAX_BYTES: usize = 32 * 1024 * 1024;
pub const YOUNG_DAYS: u64 = 7;

#[derive(Deserialize)]
struct Person {
    name: String,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Repository {
    Url(String),
    Object {
        #[serde(default)]
        url: Option<String>,
    },
}

#[derive(Deserialize)]
struct Version {
    #[serde(default)]
    deprecated: Option<String>,
    #[serde(default)]
    repository: Option<Repository>,
    #[serde(default)]
    bin: Option<Value>,
    #[serde(default, rename = "_npmUser")]
    npm_user: Option<Person>,
}

#[derive(Deserialize)]
struct Packument {
    #[allow(dead_code)] // part of the shape checked: a packument without a name isn't one
    name: String,
    #[serde(default, rename = "dist-tags")]
    dist_tags: Option<BTreeMap<String, String>>,
    versions: BTreeMap<String, Version>,
    #[serde(default)]
    time: Option<BTreeMap<String, String>>,
    #[serde(default)]
    maintainers: Option<Vec<Person>>,
    #[serde(default)]
    repository: Option<Repository>,
}

fn url_of(repo: Option<&Repository>) -> Option<&str> {
    match repo? {
        Repository::Url(u) => Some(u),
        Repository::Object { url } => url.as_deref(),
    }
}

/// The registry's URL for a name: a scope's slash is encoded (`@scope%2fname`), as npm itself does.
pub fn packument_url(base: &str, name: &str) -> String {
    let enc = |s: &str| s.replace('%', "%25").replace('/', "%2F").replace('@', "%40");
    match name.strip_prefix('@') {
        Some(rest) => format!("{base}/@{}", enc(rest)),
        None => format!("{base}/{}", enc(name)),
    }
}

/// The proposal, plus the package's repository (for the GitHub resolver's check).
pub async fn resolve_npm(
    name: &str,
    version: Option<&str>,
    opts: &ResolveOptions,
    deps: &dyn ResolveDeps,
) -> crate::Result<(Proposal, Option<String>)> {
    let url = packument_url(deps.npm_registry(), name);
    let got = deps.get(&url, "application/json", MAX_BYTES, &deps.cancel()).await?;
    if got.status == 404 {
        return Err(config(format!("There's no npm package named {name}.")));
    }
    if !(200..300).contains(&got.status) {
        return Err(config(format!("npm answered {} for {name}.", got.status)));
    }
    let data: Packument = serde_json::from_str(&got.body)
        .map_err(|_| config(format!("npm's answer for {name} isn't what Marv expected (a proxy?): nothing was added.")))?;
    let latest = data.dist_tags.as_ref().and_then(|t| t.get("latest")).cloned();
    let Some(version) = version.map(str::to_string).or_else(|| latest.clone()) else {
        return Err(config(format!("npm lists no latest version of {name}: give one, like {name}@1.2.3.")));
    };
    // Exact, never a fallback: a version that doesn't exist is the user's typo or a range, and npx would resolve it later.
    let Some(meta) = data.versions.get(&version) else {
        return Err(config(format!(
            "{name} has no version {version} on npm (give an exact version; the latest is {}).",
            latest.as_deref().unwrap_or("unknown")
        )));
    };

    let time = data.time.clone().unwrap_or_default();
    let published = day(time.get(&version).map(String::as_str));
    let by = meta.npm_user.as_ref().map(|p| p.name.clone());
    let maintainers: Vec<&str> = data.maintainers.iter().flatten().map(|m| m.name.as_str()).collect();
    let repository = repo_of(url_of(meta.repository.as_ref()).or(url_of(data.repository.as_ref())));
    let mut facts = vec![fact(
        "npm",
        &format!(
            "{name} {version}{}{}",
            published.map(|p| format!(", published {p}")).unwrap_or_default(),
            by.map(|b| format!(" by {b}")).unwrap_or_default()
        ),
    )];
    if !maintainers.is_empty() {
        let more = if maintainers.len() > 5 { format!(" and {} more", maintainers.len() - 5) } else { String::new() };
        facts.push(fact("owners", &format!("{}{more}", maintainers[..maintainers.len().min(5)].join(", "))));
    }
    facts.push(fact(
        "repo",
        &match &repository {
            Some(r) => format!("{r} (as the package states it: npm doesn't check it)"),
            None => "none stated".into(),
        },
    ));
    let mut warnings = Vec::new();
    if let Some(d) = &meta.deprecated {
        warnings.push(format!("npm says {version} is deprecated: {}", d.chars().take(300).collect::<String>()));
    }
    if let Some(created) = time.get("created").and_then(|c| iso_ms(c)) {
        let age = age_days(deps.now_ms(), created);
        if age < YOUNG_DAYS as f64 {
            warnings.push(format!(
                "{name} was first published {} ago: check that it's the package you meant (lookalike names are a common trick).",
                age_text(age)
            ));
        }
    }
    if meta.bin.is_none() {
        warnings.push(format!("{name} {version} declares no command (bin): npx may not be able to run it."));
    }

    let source = Source::Npm { name: name.into(), version: Some(version.clone()) };
    let mut args = vec!["-y".to_string(), format!("{name}@{version}")];
    args.extend(opts.args.iter().cloned());
    let proposal = Proposal {
        name: name_for(opts, &source)?,
        entry: McpEntry::Stdio { command: "npx".into(), args, env: (!opts.env.is_empty()).then(|| env_entries(&opts.env)) },
        facts,
        warnings,
    };
    Ok((proposal, repository))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::install::util::fake::Fake;

    fn none() -> ResolveOptions {
        ResolveOptions::default()
    }

    fn text(p: &Proposal) -> String {
        [p.facts.clone(), p.warnings.clone()].concat().join("\n")
    }

    fn args(p: &Proposal) -> Vec<String> {
        match &p.entry {
            McpEntry::Stdio { args, .. } => args.clone(),
            McpEntry::Http { .. } => panic!("not a local server"),
        }
    }

    #[test]
    fn scoped_names_encode_the_slash() {
        assert_eq!(packument_url("https://r", "@scope/name"), "https://r/@scope%2Fname");
        assert_eq!(packument_url("https://r", "plain"), "https://r/plain");
    }

    #[tokio::test]
    async fn latest_is_pinned_into_args_with_the_facts_to_judge_it_by() {
        let opts = ResolveOptions { args: vec!["${MARV_PROJECT_DIR}".into()], ..none() };
        let (p, repo) = resolve_npm("@modelcontextprotocol/server-filesystem", None, &opts, &Fake::default()).await.unwrap();
        assert_eq!(p.name, "filesystem");
        assert_eq!(
            p.entry,
            McpEntry::Stdio {
                command: "npx".into(),
                args: vec!["-y".into(), "@modelcontextprotocol/server-filesystem@2026.1.14".into(), "${MARV_PROJECT_DIR}".into()],
                env: None,
            }
        );
        assert!(text(&p).contains("2026.1.14, published 2026-01-14 by jspahrsummers"), "{}", text(&p));
        assert!(text(&p).contains("github.com/modelcontextprotocol/servers (as the package states it"));
        assert!(text(&p).contains("owners   jspahrsummers, pcarleton"));
        assert!(p.warnings.is_empty(), "{:?}", p.warnings);
        assert_eq!(repo.as_deref(), Some("github.com/modelcontextprotocol/servers"));
    }

    #[tokio::test]
    async fn a_requested_version_is_used_as_given() {
        let (p, _) =
            resolve_npm("@modelcontextprotocol/server-filesystem", Some("2025.8.21"), &none(), &Fake::default()).await.unwrap();
        assert_eq!(args(&p), ["-y", "@modelcontextprotocol/server-filesystem@2025.8.21"]);
    }

    #[tokio::test]
    async fn a_version_that_doesnt_exist_is_an_error_never_a_fallback() {
        let deps = Fake::default();
        let e = resolve_npm("@modelcontextprotocol/server-filesystem", Some("9.9.9"), &none(), &deps).await.unwrap_err();
        assert!(e.to_string().contains("9.9.9"));
        let e = resolve_npm("@modelcontextprotocol/server-filesystem", Some("^2026"), &none(), &deps).await.unwrap_err();
        assert!(e.to_string().contains("exact version"));
    }

    #[tokio::test]
    async fn a_package_that_doesnt_exist() {
        let e = resolve_npm("nope", None, &none(), &Fake::default()).await.unwrap_err();
        assert!(e.to_string().contains("no npm package named nope"));
    }

    #[tokio::test]
    async fn deprecated_brand_new_and_commandless_packages_are_flagged_and_facts_are_printable() {
        let (p, _) = resolve_npm("weather-mcp", None, &none(), &Fake::default()).await.unwrap();
        let warnings = p.warnings.join("\n");
        assert!(warnings.contains("deprecated: Use @acme/weather instead"));
        assert!(warnings.contains("first published 2 days ago"));
        assert!(warnings.contains("declares no command"));
        assert!(!p.facts.join("\n").contains('\u{1b}'));
    }

    #[tokio::test]
    async fn env_becomes_a_reference() {
        let opts = ResolveOptions { env: vec!["API_KEY".into()], ..none() };
        let (p, _) = resolve_npm("weather-mcp", None, &opts, &Fake::default()).await.unwrap();
        assert_eq!(p.name, "weather");
        let McpEntry::Stdio { env: Some(env), .. } = &p.entry else { panic!("no env") };
        assert_eq!(env["API_KEY"], "${API_KEY}");
    }

    #[tokio::test]
    async fn an_answer_of_the_wrong_shape_adds_nothing() {
        struct Proxy;
        #[async_trait::async_trait]
        impl ResolveDeps for Proxy {
            async fn get(
                &self,
                _u: &str,
                _a: &str,
                _m: usize,
                _c: &tokio_util::sync::CancellationToken,
            ) -> crate::Result<super::super::proposal::Fetched> {
                Ok(super::super::proposal::Fetched { status: 200, body: "<html>".into() })
            }
            async fn readme(
                &self,
                _o: &str,
                _r: &str,
                _c: &tokio_util::sync::CancellationToken,
            ) -> crate::Result<Option<String>> {
                Ok(None)
            }
            fn now_ms(&self) -> u64 {
                0
            }
            fn cancel(&self) -> tokio_util::sync::CancellationToken {
                Default::default()
            }
        }
        let e = resolve_npm("x", None, &none(), &Proxy).await.unwrap_err();
        assert!(e.to_string().contains("isn't what Marv expected"));
    }
}
