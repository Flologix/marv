//! A PyPI package as an MCP server: `uvx <pkg>@<exact version>`. Ports
//! `src/mcp/install/pypi.ts`.
//!
//! PyPI refuses to re-upload a file name, so a pinned version's files can't
//! change. Known limit: a package whose command has another name than the
//! package needs the registry's entry or a hand edit. `uvx` runs in your home
//! folder, never the project (see `manager::cwd_for`).

use std::collections::BTreeMap;

use serde::Deserialize;

use super::proposal::{McpEntry, Proposal, ResolveDeps, ResolveOptions, env_entries, fact, repo_of};
use super::source::Source;
use super::util::{Ordered, age_days, age_text, config, encode_component, iso_ms, name_for};

pub const MAX_BYTES: usize = 32 * 1024 * 1024;
pub const YOUNG_DAYS: u64 = 7;

#[derive(Deserialize)]
struct File {
    #[serde(default)]
    upload_time_iso_8601: Option<String>,
    #[serde(default)]
    yanked: Option<bool>,
    #[serde(default)]
    yanked_reason: Option<String>,
}

#[derive(Deserialize)]
struct Info {
    name: String,
    version: String,
    #[serde(default)]
    author: Option<String>,
    /// In the file's order: the first link that looks like the repository wins.
    #[serde(default)]
    project_urls: Option<Ordered>,
    #[serde(default)]
    home_page: Option<String>,
}

#[derive(Deserialize)]
struct Ownership {
    #[serde(default)]
    organization: Option<String>,
}

#[derive(Deserialize)]
struct Project {
    info: Info,
    #[serde(default)]
    releases: Option<BTreeMap<String, Vec<File>>>,
    #[serde(default)]
    ownership: Option<Ownership>,
}

/// The JSON API's URL for a project (and version).
pub fn project_url(base: &str, name: &str, version: Option<&str>) -> String {
    let name = encode_component(name);
    match version {
        Some(v) => format!("{base}/pypi/{name}/{}/json", encode_component(v)),
        None => format!("{base}/pypi/{name}/json"),
    }
}

/// The repository among `project_urls`, if one looks like it.
fn repository_of(urls: Option<&Ordered>, home: Option<&str>) -> Option<String> {
    let entries: Vec<(&str, &str)> = urls
        .and_then(Ordered::as_object)
        .unwrap_or_default()
        .iter()
        .filter_map(|(k, v)| Some((k.as_str(), v.as_str()?)))
        .collect();
    let named = entries
        .iter()
        .find(|(k, _)| matches!(k.trim().to_lowercase().as_str(), "repository" | "source" | "source code" | "code" | "github"));
    let hosted = entries.iter().find(|(_, v)| ["github.com", "gitlab.com", "codeberg.org"].iter().any(|h| v.contains(h)));
    repo_of(named.or(hosted).map(|(_, v)| *v).or(home))
}

/// The proposal, plus the repository among `project_urls`, if one looks like it.
pub async fn resolve_pypi(
    name: &str,
    version: Option<&str>,
    opts: &ResolveOptions,
    deps: &dyn ResolveDeps,
) -> crate::Result<(Proposal, Option<String>)> {
    // The project's own document lists every release: a requested version is checked against it.
    let url = project_url(deps.pypi(), name, None);
    let got = deps.get(&url, "application/json", MAX_BYTES, &deps.cancel()).await?;
    if got.status == 404 {
        return Err(config(format!("There's no PyPI package named {name}.")));
    }
    if !(200..300).contains(&got.status) {
        return Err(config(format!("PyPI answered {} for {name}.", got.status)));
    }
    let data: Project = serde_json::from_str(&got.body)
        .map_err(|_| config(format!("PyPI's answer for {name} isn't what Marv expected (a proxy?): nothing was added.")))?;
    let chosen = version.unwrap_or(&data.info.version).to_string();
    let releases = data.releases.unwrap_or_default();
    let files = releases.get(&chosen);
    if version.is_some() && files.is_none_or(Vec::is_empty) {
        return Err(config(format!(
            "{name} has no version {chosen} on PyPI (give an exact version; the latest is {}).",
            data.info.version
        )));
    }
    let shown = &data.info.name;
    let mut times: Vec<&str> = files.into_iter().flatten().filter_map(|f| f.upload_time_iso_8601.as_deref()).collect();
    times.sort();
    let uploaded = times.first().map(|t| t.chars().take(10).collect::<String>());
    let repository = repository_of(data.info.project_urls.as_ref(), data.info.home_page.as_deref());
    let by = data.ownership.and_then(|o| o.organization).or(data.info.author.clone());
    let facts = vec![
        fact(
            "pypi",
            &format!(
                "{shown} {chosen}{}{}",
                uploaded.map(|u| format!(", uploaded {u}")).unwrap_or_default(),
                by.map(|b| format!(" by {b}")).unwrap_or_default()
            ),
        ),
        fact(
            "repo",
            &match &repository {
                Some(r) => format!("{r} (as the package states it: PyPI doesn't check it)"),
                None => "none stated".into(),
            },
        ),
    ];
    let mut warnings = Vec::new();
    if let Some(yanked) = files.into_iter().flatten().find(|f| f.yanked == Some(true)) {
        let why = yanked.yanked_reason.as_ref().map(|r| format!(": {}", r.chars().take(300).collect::<String>()));
        warnings.push(format!("PyPI says {chosen} was yanked{}.", why.unwrap_or_default()));
    }
    let mut all: Vec<&str> = releases.values().flatten().filter_map(|f| f.upload_time_iso_8601.as_deref()).collect();
    all.sort();
    let age = all.first().and_then(|t| iso_ms(t)).map_or(f64::INFINITY, |first| age_days(deps.now_ms(), first));
    if age < YOUNG_DAYS as f64 {
        warnings.push(format!(
            "{shown} was first uploaded {} ago: check that it's the package you meant (lookalike names are a common trick).",
            age_text(age)
        ));
    }

    let source = Source::Pypi { name: name.into(), version: Some(chosen.clone()) };
    let mut args = vec![format!("{name}@{chosen}")];
    args.extend(opts.args.iter().cloned());
    let proposal = Proposal {
        name: name_for(opts, &source)?,
        entry: McpEntry::Stdio { command: "uvx".into(), args, env: (!opts.env.is_empty()).then(|| env_entries(&opts.env)) },
        facts,
        warnings,
    };
    Ok((proposal, repository))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::install::util::fake::Fake;

    fn text(p: &Proposal) -> String {
        [p.facts.clone(), p.warnings.clone()].concat().join("\n")
    }

    #[tokio::test]
    async fn uvx_pkg_at_version() {
        let (p, _) = resolve_pypi("mcp-server-fetch", None, &ResolveOptions::default(), &Fake::default()).await.unwrap();
        assert_eq!(p.name, "fetch");
        assert_eq!(p.entry, McpEntry::Stdio { command: "uvx".into(), args: vec!["mcp-server-fetch@2025.4.7".into()], env: None });
        assert!(text(&p).contains("mcp-server-fetch 2025.4.7, uploaded 2025-04-07"), "{}", text(&p));
        assert!(text(&p).contains("github.com/modelcontextprotocol/servers/tree/main/src/fetch"));
        assert!(p.warnings.is_empty());
    }

    #[tokio::test]
    async fn a_yanked_version_is_flagged_and_a_missing_one_is_an_error() {
        let deps = Fake::default();
        let none = ResolveOptions::default();
        let (p, _) = resolve_pypi("mcp-server-fetch", Some("2025.1.14"), &none, &deps).await.unwrap();
        assert!(p.warnings.join("\n").contains("yanked: broken build"));
        let e = resolve_pypi("mcp-server-fetch", Some("1.2.3"), &none, &deps).await.unwrap_err();
        assert!(e.to_string().contains("no version 1.2.3"));
        let e = resolve_pypi("nope", None, &none, &deps).await.unwrap_err();
        assert!(e.to_string().contains("no PyPI package"));
    }

    #[test]
    fn the_repository_is_the_first_link_that_looks_like_one() {
        let urls: Ordered = serde_json::from_str(
            r#"{"Docs": "https://docs.example", "Tracker": "https://gitlab.com/o/r/issues", "Source": "https://x.example/o/r"}"#,
        )
        .unwrap();
        assert_eq!(repository_of(Some(&urls), None).as_deref(), Some("x.example/o/r"));
        let urls: Ordered =
            serde_json::from_str(r#"{"Docs": "https://docs.example", "Tracker": "https://gitlab.com/o/r"}"#).unwrap();
        assert_eq!(repository_of(Some(&urls), None).as_deref(), Some("gitlab.com/o/r"));
        assert_eq!(repository_of(None, Some("https://home.example/")).as_deref(), Some("home.example"));
    }
}
