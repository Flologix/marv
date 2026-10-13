//! A PyPI package as an MCP server: `uvx <pkg>@<exact version>`. Ports
//! `src/mcp/install/pypi.ts`.
//!
//! PyPI refuses to re-upload a file name, so a pinned version's files can't
//! change. Known limit: a package whose command has another name than the
//! package needs the registry's entry or a hand edit. `uvx` runs in your home
//! folder, never the project (see `manager::cwd_for`).

use super::proposal::{Proposal, ResolveDeps, ResolveOptions};

/// The JSON API's URL for a project (and version).
pub fn project_url(base: &str, name: &str, version: Option<&str>) -> String {
    match version {
        Some(v) => format!("{base}/pypi/{name}/{v}/json"),
        None => format!("{base}/pypi/{name}/json"),
    }
}

/// The proposal, plus the repository among `project_urls`, if one looks like it.
pub async fn resolve_pypi(_name: &str, _version: Option<&str>, _opts: &ResolveOptions, _deps: &dyn ResolveDeps) -> crate::Result<(Proposal, Option<String>)> {
    todo!("port: resolvePypi (JSON API, yanked check, facts, uvx pkg@ver + args)")
}
