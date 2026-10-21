//! `/mcp add`'s real network and its approval: the engine half of Marv's `liveDeps` (`src/mcp/install/index.ts`) and
//! of `approvalFor`'s request (`src/mcp/install/proposal.ts`). The lookup itself is marv-mcp's `install`; the UI's
//! flow (look up, ask once, write, start) is the TUI's (`marv-cli`'s `tui.rs`, Marv's `app.tsx`).
//!
//! - [`LiveDeps`]: Marv's own `get` (address checks, time and size limits) for npm, PyPI and the MCP Registry;
//!   GitHub's API for a README with the user's `GITHUB_TOKEN` (or `GH_TOKEN`), sent to the API host only; and an
//!   unauthenticated read of a remote server's own metadata (5 s, no registration), only for a URL the user typed.
//! - [`add_request`]: yes or no only (`once`), never answered from a remembered scope (`must_ask`): what's written
//!   is exactly the JSON it shows.

use async_trait::async_trait;
use marv_mcp::install::proposal::{AddApproval, Fetched, ResolveDeps};
use marv_tools::web::fetch::{GetOptions, get};
use marv_tools::web::github::{GithubOptions, fetch_readme};
use marv_tools::{ApprovalRequest, Preview, Scope};
use tokio_util::sync::CancellationToken;

use crate::config::Env;

/// The real network for `/mcp add` and `/mcp search`. `cancel` is the lookup's Esc.
pub struct LiveDeps {
    pub env: Env,
    pub cancel: CancellationToken,
}

impl LiveDeps {
    pub fn new(env: Env, cancel: CancellationToken) -> Self {
        Self { env, cancel }
    }

    fn token(&self) -> Option<String> {
        self.env.get("GITHUB_TOKEN").or_else(|| self.env.get("GH_TOKEN")).filter(|t| !t.is_empty()).cloned()
    }
}

fn server(e: marv_tools::ToolError) -> marv_mcp::McpError {
    marv_mcp::McpError::Server(e.0)
}

#[async_trait]
impl ResolveDeps for LiveDeps {
    async fn get(&self, url: &str, accept: &str, max_bytes: usize, cancel: &CancellationToken) -> marv_mcp::Result<Fetched> {
        let options = GetOptions { headers: vec![("accept".into(), accept.into())], max_bytes, ..GetOptions::default() };
        let got = get(url, &options, cancel).await.map_err(server)?;
        Ok(Fetched { status: got.status, body: got.body })
    }

    async fn readme(&self, owner: &str, repo: &str, cancel: &CancellationToken) -> marv_mcp::Result<Option<String>> {
        let options = GithubOptions { token: self.token(), ..GithubOptions::default() };
        fetch_readme(owner, repo, &options, cancel).await.map_err(server)
    }

    async fn probe_login(&self, url: &str, cancel: &CancellationToken) -> Option<bool> {
        if cancel.is_cancelled() {
            return Some(false);
        }
        Some(marv_mcp::oauth::probe_login(url, cancel.clone()).await)
    }

    fn now_ms(&self) -> u64 {
        crate::time::now_ms()
    }

    fn cancel(&self) -> CancellationToken {
        self.cancel.clone()
    }
}

/// `/mcp add`'s question, as the approval queue asks it.
pub fn add_request(approval: &AddApproval) -> ApprovalRequest {
    ApprovalRequest {
        reason: false,
        tool: approval.tool.into(),
        label: approval.label.clone(),
        preview: Preview {
            title: approval.title.clone(),
            command: approval.command.clone(),
            text: Some(approval.text.clone()),
            note: Some(approval.note.into()),
            warning: approval.warning.clone(),
            ..Preview::default()
        },
        scope: Scope { key: approval.scope_key.clone(), description: approval.scope_description.clone() },
        agent: None,
        network: false,
        once: true,
        must_ask: true,
        protected: None,
        door: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use marv_mcp::install::proposal::{McpEntry, Proposal, approval_for};

    #[test]
    fn the_add_request_is_yes_or_no_and_shows_the_exact_json() {
        let p = Proposal {
            name: "w".into(),
            entry: McpEntry::Stdio { command: "npx".into(), args: vec!["-y".into(), "w@1.0.0".into()], env: None },
            facts: vec![],
            warnings: vec![],
        };
        let (approval, json) = approval_for(&p, &|_| false).unwrap();
        let r = add_request(&approval);
        assert!(r.once && r.must_ask);
        assert_eq!(r.preview.title, "Add MCP server \"w\" to ~/.marv/mcp.json (yours, every project)");
        assert_eq!(r.preview.command.as_deref(), Some("npx -y w@1.0.0"));
        assert!(r.preview.text.unwrap().ends_with(&format!("writes   \"w\": {json}")));
        assert!(r.preview.note.unwrap().starts_with("runs outside the sandbox"));
        // Unique: an "always" for one add never covers another.
        assert_ne!(r.scope.key, add_request(&approval_for(&p, &|_| false).unwrap().0).scope.key);
    }

    #[test]
    fn the_github_token_is_githubs_or_ghs() {
        let deps = |pairs: &[(&str, &str)]| {
            LiveDeps::new(pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(), CancellationToken::new())
        };
        assert_eq!(deps(&[("GH_TOKEN", "b")]).token().as_deref(), Some("b"));
        assert_eq!(deps(&[("GITHUB_TOKEN", "a"), ("GH_TOKEN", "b")]).token().as_deref(), Some("a"));
        assert_eq!(deps(&[]).token(), None);
    }
}
