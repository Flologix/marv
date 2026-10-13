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

pub use proposal::{Choices, McpEntry, Proposal, ResolveDeps, ResolveOptions};
pub use source::{parse_add_args, parse_source, server_name, AddOptions, Source};

/// A proposal, or the commands to choose from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolved {
    Proposal(Proposal),
    Choices(Choices),
}

/// Resolves the source, then adds a `login` fact for a remote server that asks
/// for an OAuth login (Marv #18), so "added" isn't mistaken for "working".
pub async fn resolve_add(_options: &AddOptions, _deps: &dyn ResolveDeps) -> crate::Result<Resolved> {
    todo!("port: resolveAdd (resolveSource by kind; header/env/args checks per kind; withLoginFact)")
}

/// A remote proposal without its own Authorization header that `probe_login`
/// says wants a login gets the fact `login    it asks for an OAuth login: after
/// adding it, /mcp login <name>`.
pub async fn with_login_fact(_proposal: Proposal, _deps: &dyn ResolveDeps) -> Proposal {
    todo!("port: withLoginFact")
}

/// A registry entry or a README can resolve to either kind of server: options
/// that don't apply to the one it resolved to are refused, never silently dropped.
pub fn fits(resolved: Resolved, options: &AddOptions) -> Result<Resolved, String> {
    let Resolved::Proposal(p) = &resolved else { return Ok(resolved) };
    if p.entry.is_remote() && (!options.env.is_empty() || !options.args.is_empty()) {
        return Err(format!("{} is a remote server: --env and server arguments are for local servers; a remote one takes --header.", options.source));
    }
    if !p.entry.is_remote() && !options.headers.is_empty() {
        return Err(format!("{} is a local server: --header is for remote (URL) servers; a local server reads its secrets from --env.", options.source));
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
}
