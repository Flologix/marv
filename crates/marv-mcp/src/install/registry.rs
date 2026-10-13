//! The official MCP Registry (registry.modelcontextprotocol.io, no key).
//! Ports `src/mcp/install/registry.ts`.
//!
//! `/mcp search` lists it, `/mcp add <name>` installs an entry. The registry
//! isn't curated: a publisher proves only its namespace (`io.github.<user>` by
//! signing in to GitHub as that user, a domain by DNS), so that and the
//! repository are shown for the user to judge. A package then goes through the
//! npm/PyPI resolver at the version the entry names.
//!
//! - Required env vars become `${NAME}` (a default `${NAME:-x}`); optional ones
//!   are only named.
//! - Fixed argument values starting with `-` are refused (they could be an
//!   npx/uvx option); a required one without a value comes from after `--`.
//! - A `streamable-http` remote's headers become `${VAR}` references; a
//!   non-secret fixed value is refused (an entry can't plant a header).
//! - OCI is unsupported; a status other than `active` is refused.

use super::proposal::{Proposal, ResolveDeps, ResolveOptions};

/// `/mcp search` shows at most this many.
pub const MAX_RESULTS: usize = 10;

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
pub async fn search_registry(_words: &str, _deps: &dyn ResolveDeps) -> crate::Result<Vec<RegistryHit>> {
    todo!("port: searchRegistry")
}

pub async fn resolve_registry(_name: &str, _opts: &ResolveOptions, _deps: &dyn ResolveDeps) -> crate::Result<Proposal> {
    todo!("port: resolveRegistry (argsOf, headerValue, envOf, npm/pypi at the entry's version)")
}

/// `/mcp search`'s answer, as Markdown. Descriptions are the publishers' own words, made printable and kept short.
pub fn search_text(_words: &str, _hits: &[RegistryHit]) -> String {
    todo!("port: searchText")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn namespaces_say_what_they_prove() {
        assert_eq!(namespace_proof("io.github.acme/x"), "the publisher signed in to GitHub as acme");
        assert_eq!(namespace_proof("com.example/x"), "the publisher proved it controls example.com");
    }
}
