//! GitHub links, read through GitHub's API instead of its HTML (which is mostly menus): a repository becomes its
//! description, README (cut at 12k) and file list (at most 500; a tree over 16 MB or truncated falls back to the top
//! level; an empty repo says so), `tree/<ref>/<dir>` that folder's entries, `blob/` the file numbered like `read_file`.
//!
//! Ports Marv's `src/tools/web/github.ts`.
//!
//! - Segments are decoded once and re-encoded per segment, and owner/repo/ref/path are validated (no `..`, no encoded
//!   `/` in names, owner `[A-Za-z0-9_-]{1,39}`, repo `[A-Za-z0-9._-]{1,100}`), so a crafted link can't steer the token
//!   to another API endpoint.
//! - `GITHUB_TOKEN`/`GH_TOKEN` go to the API host only (raw.githubusercontent.com without a token, the contents API
//!   with one). Rate-limit errors say when to retry; others include GitHub's message.
//! - github.com/<about|login|settings|…>/… are GitHub's own pages ([`NOT_OWNERS`]), read as web pages.

use crate::types::ToolError;
use crate::web::fetch::Page;

pub const API: &str = "https://api.github.com";
pub const RAW: &str = "https://raw.githubusercontent.com";
pub const MAX_FILES: usize = 500;
pub const MAX_README: usize = 12_000;
/// The whole-repository tree can be big (a monorepo's is megabytes of JSON); past this the top level is listed.
pub const TREE_BYTES: usize = 16 * 1024 * 1024;
pub const SLASH_HINT: &str = " If the branch name has a slash in it, the link is ambiguous: use the commit hash instead.";
/// github.com/<these>/… are GitHub's own pages, not repositories.
pub const NOT_OWNERS: &[&str] = &[
    "about", "account", "apps", "codespaces", "collections", "copilot", "customer-stories", "dashboard", "enterprise", "events", "explore",
    "features", "issues", "join", "login", "marketplace", "new", "notifications", "organizations", "orgs", "pricing", "pulls", "readme",
    "resources", "search", "security", "settings", "signup", "site", "solutions", "sponsors", "stars", "team", "topics", "trending", "users", "watching",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GithubLink {
    Repo { owner: String, repo: String },
    Tree { owner: String, repo: String, git_ref: String, dir: String },
    Blob { owner: String, repo: String, git_ref: String, path: String },
}

/// A decoded path or folder segment we're willing to put in a URL.
pub fn safe_segment(seg: &str) -> bool {
    !seg.is_empty() && seg != "." && seg != ".." && !seg.contains('/') && !seg.chars().any(|c| (c as u32) < 0x20 || c as u32 == 0x7f)
}

/// The repository, folder or file a github.com link points to; None for anything else (issues, PRs…).
pub fn parse_github(_url: &url::Url) -> Option<GithubLink> {
    todo!("port: parseGithub (decode once, validate OWNER/REPO/safe_ref/safe_segment, NOT_OWNERS)")
}

/// A repository's README text, for `/mcp add` (GitHub resolver) too.
pub async fn fetch_readme(_owner: &str, _repo: &str) -> Result<Option<String>, ToolError> {
    todo!("port: fetchReadme")
}

pub async fn fetch_github(_link: &GithubLink) -> Result<Page, ToolError> {
    todo!("port: fetchGithub (repo summary, tree listing, blob numbered like read_file, rate-limit messages)")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsafe_segments_are_refused() {
        assert!(safe_segment("src"));
        for bad in ["", ".", "..", "a/b", "x\ny"] {
            assert!(!safe_segment(bad), "{bad:?}");
        }
    }
}
