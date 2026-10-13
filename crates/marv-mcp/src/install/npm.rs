//! An npm package as an MCP server: `npx -y <pkg>@<exact version>`. Ports
//! `src/mcp/install/npm.ts`.
//!
//! The version is resolved here, before the approval, and written into args:
//! npm never lets a published version's files change, so it fixes the server's
//! own code (its dependencies still follow their ranges: npx has no lockfile).
//! A requested version must exist. Facts: version, publish date, publisher,
//! maintainers, repository; warnings: deprecated, published in the last
//! [`YOUNG_DAYS`] days.

use super::proposal::{Proposal, ResolveDeps, ResolveOptions};

/// A packument has every version's metadata: popular packages run to megabytes.
pub const MAX_BYTES: usize = 32 * 1024 * 1024;
pub const YOUNG_DAYS: u64 = 7;

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
    _name: &str,
    _version: Option<&str>,
    _opts: &ResolveOptions,
    _deps: &dyn ResolveDeps,
) -> crate::Result<(Proposal, Option<String>)> {
    todo!(
        "port: resolveNpm (packument, dist-tags.latest or requested version, facts/warnings, npx -y pkg@ver + args, env_entries)"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scoped_names_encode_the_slash() {
        assert_eq!(packument_url("https://r", "@scope/name"), "https://r/@scope%2Fname");
        assert_eq!(packument_url("https://r", "plain"), "https://r/plain");
    }
}
