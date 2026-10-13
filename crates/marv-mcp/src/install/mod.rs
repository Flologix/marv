//! `/mcp add`'s lookup: from what the user typed to a [`Proposal`] (or, for a
//! README naming several servers, the commands to choose from). Ports
//! `src/mcp/install/index.ts`.
//!
//! Every step is deterministic code: **no model is in the loop**, the model gets
//! no tool for it, and nothing here writes or starts anything (that happens
//! only after the user approves the exact entry, see [`proposal`]).
//! Packages are pinned (`npx -y <pkg>@<exact>`, `uvx <pkg>@<exact>`). Known
//! limits: npx dependencies follow their ranges; a remote server is whatever
//! is deployed; no Docker or build-from-source.

pub mod github;
pub mod npm;
pub mod proposal;
pub mod pypi;
pub mod registry;
pub mod source;
mod util;

pub use proposal::{Choices, McpEntry, Proposal, ResolveDeps, ResolveOptions};
pub use source::{AddOptions, Source, parse_add_args, parse_source, server_name};

use crate::McpError;
use util::{config, name_for};

/// A proposal, or the commands to choose from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolved {
    Proposal(Proposal),
    Choices(Choices),
}

/// Resolves the source, then adds a `login` fact for a remote server that asks
/// for an OAuth login (Marv #18), so "added" isn't mistaken for "working".
pub async fn resolve_add(options: &AddOptions, deps: &dyn ResolveDeps) -> crate::Result<Resolved> {
    Ok(match resolve_source(options, deps).await? {
        Resolved::Proposal(p) => Resolved::Proposal(with_login_fact(p, deps).await),
        choices => choices,
    })
}

/// A remote proposal without its own Authorization header that `probe_login`
/// says wants a login gets the fact `login    it asks for an OAuth login: after
/// adding it, /mcp login <name>`. Not one with its own Authorization header:
/// that's how it logs in.
pub async fn with_login_fact(mut proposal: Proposal, deps: &dyn ResolveDeps) -> Proposal {
    let McpEntry::Http { url, headers, .. } = &proposal.entry else { return proposal };
    if headers.as_ref().is_some_and(|h| h.keys().any(|k| k.eq_ignore_ascii_case("authorization"))) {
        return proposal;
    }
    if deps.probe_login(url, &deps.cancel()).await != Some(true) {
        return proposal;
    }
    let text = format!("it asks for an OAuth login: after adding it, /mcp login {}", proposal.name);
    proposal.facts.push(proposal::fact("login", &text));
    proposal
}

async fn resolve_source(options: &AddOptions, deps: &dyn ResolveDeps) -> crate::Result<Resolved> {
    let source = parse_source(&options.source).map_err(McpError::Config)?;
    let opts = ResolveOptions {
        name: options.name.clone(),
        env: options.env.clone(),
        headers: options.headers.clone(),
        args: options.args.clone(),
    };
    if matches!(source, Source::Npm { .. } | Source::Pypi { .. }) && !options.headers.is_empty() {
        return Err(config("--header is for remote (URL) servers; a local server reads its secrets from --env."));
    }
    let fitted = |r: Resolved| fits(r, options).map_err(McpError::Config);
    match &source {
        Source::Npm { name, version } => Ok(Resolved::Proposal(npm::resolve_npm(name, version.as_deref(), &opts, deps).await?.0)),
        Source::Pypi { name, version } => {
            Ok(Resolved::Proposal(pypi::resolve_pypi(name, version.as_deref(), &opts, deps).await?.0))
        }
        Source::Http { url } => {
            if !options.env.is_empty() || !options.args.is_empty() {
                return Err(config("--env and server arguments are for local servers; a remote one takes --header."));
            }
            Ok(Resolved::Proposal(proposal::http_proposal(url, name_for(&opts, &source)?, &options.headers)))
        }
        Source::Registry { name } => fitted(Resolved::Proposal(registry::resolve_registry(name, &opts, deps).await?)),
        Source::Github { owner, repo } => fitted(github::resolve_github(owner, repo, &opts, deps).await?),
    }
}

/// A registry entry or a README can resolve to either kind of server: options
/// that don't apply to the one it resolved to are refused, never silently dropped.
pub fn fits(resolved: Resolved, options: &AddOptions) -> Result<Resolved, String> {
    let Resolved::Proposal(p) = &resolved else { return Ok(resolved) };
    if p.entry.is_remote() && (!options.env.is_empty() || !options.args.is_empty()) {
        return Err(format!(
            "{} is a remote server: --env and server arguments are for local servers; a remote one takes --header.",
            options.source
        ));
    }
    if !p.entry.is_remote() && !options.headers.is_empty() {
        return Err(format!(
            "{} is a local server: --header is for remote (URL) servers; a local server reads its secrets from --env.",
            options.source
        ));
    }
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_must_fit_what_the_source_resolved_to() {
        let remote = Resolved::Proposal(proposal::http_proposal("https://x/", "x".into(), &Default::default()));
        let opts = AddOptions { source: "io.x/y".into(), env: vec!["A".into()], ..Default::default() };
        assert!(fits(remote.clone(), &opts).unwrap_err().contains("is a remote server"));
        assert!(fits(remote, &AddOptions::default()).is_ok());
    }

    use util::fake::Fake;

    async fn add(text: &str, deps: &Fake) -> crate::Result<Proposal> {
        match resolve_add(&parse_add_args(text).unwrap(), deps).await? {
            Resolved::Proposal(p) => Ok(p),
            Resolved::Choices(c) => panic!("choices: {c:?}"),
        }
    }

    fn text(p: &Proposal) -> String {
        [p.facts.clone(), p.warnings.clone()].concat().join("\n")
    }

    #[tokio::test]
    async fn a_url_through_resolve_add_fetches_nothing() {
        let deps = Fake::default();
        let p = add("https://mcp.linear.app/mcp", &deps).await.unwrap();
        assert_eq!(p.name, "linear");
        assert_eq!(p.entry.to_value(), serde_json::json!({"type": "http", "url": "https://mcp.linear.app/mcp"}));
        assert!(deps.requests.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_remote_server_that_asks_for_a_login_says_so_unless_it_has_its_own_header() {
        let deps = Fake { probe: Some(true), ..Fake::default() };
        let p = add("https://mcp.linear.app/mcp", &deps).await.unwrap();
        assert_eq!(*deps.probed.lock().unwrap(), ["https://mcp.linear.app/mcp"]);
        assert!(text(&p).contains("login    it asks for an OAuth login: after adding it, /mcp login linear"));
        let headed = add(r#"https://mcp.linear.app/mcp --header "Authorization: Bearer ${T}""#, &deps).await.unwrap();
        assert_eq!(deps.probed.lock().unwrap().len(), 1);
        assert!(!text(&headed).contains("OAuth"));
        let open = add("https://mcp.linear.app/mcp", &Fake { probe: Some(false), ..Fake::default() }).await.unwrap();
        assert!(!text(&open).contains("OAuth"));
        // No prober (the SDK, tests): nothing is said.
        assert!(!text(&add("https://mcp.linear.app/mcp", &Fake::default()).await.unwrap()).contains("OAuth"));
    }

    #[tokio::test]
    async fn options_that_dont_fit_the_source_are_refused() {
        let deps = Fake::default();
        assert!(add("https://mcp.linear.app/mcp --env X", &deps).await.unwrap_err().to_string().contains("--header"));
        let e = add(r#"weather-mcp --header "Authorization: Bearer ${X}""#, &deps).await.unwrap_err();
        assert!(e.to_string().contains("--env"));
    }

    #[tokio::test]
    async fn options_that_dont_fit_what_a_registry_entry_resolves_to_are_refused_never_dropped() {
        let deps = Fake::default();
        // ai.smithery/github resolves to a remote server: --env and server arguments can't apply to it.
        assert!(add("ai.smithery/github --env X", &deps).await.unwrap_err().to_string().contains("--header"));
        assert!(add("ai.smithery/github -- --verbose", &deps).await.unwrap_err().to_string().contains("--header"));
        // io.modelcontextprotocol/filesystem resolves to a package: a --header can't apply to it.
        let e = add(r#"io.modelcontextprotocol/filesystem --header "X-Key: ${K}" -- /tmp"#, &deps).await.unwrap_err();
        assert!(e.to_string().contains("--env"));
    }

    #[test]
    fn a_server_that_needs_a_login_ends_with_mcp_login_its_name() {
        let status = crate::manager::McpServerStatus {
            name: "linear".into(),
            source: crate::config::ServerSource::Personal,
            target: "https://mcp.linear.app/mcp".into(),
            reads: vec![],
            runs_project_files: vec![],
            state: crate::manager::McpState::NeedsLogin,
            tools: vec![],
            error: None,
            stderr: None,
            skipped: vec![],
            login: None,
        };
        assert_eq!(
            proposal::added_notice(&status),
            (
                "Added linear to ~/.marv/mcp.json. It needs a login: /mcp login linear (Marv can use its tools from your next message after that).".into(),
                false
            )
        );
    }
}
