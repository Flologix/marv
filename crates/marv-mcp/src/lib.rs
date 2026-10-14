//! marv-mcp: Marv's MCP client. Config in Claude Code's format, project-server
//! trust, the connection manager (stdio and Streamable HTTP), MCP tools as Marv
//! tools, deferred tools (`tool_search`/`mcp_call`), OAuth logins with a shared,
//! lock-protected token store, and the `/mcp add` installers.
//!
//! Ports `src/mcp/*` of Marv (TypeScript). One module per TS file; see
//! `docs/subsystems/mcp.md` for the table and status.
//!
//! **Security rules baked in here (docs/SECURITY.md, "MCP"):**
//! - A project's servers start only once trusted; the trust key covers the raw
//!   entry *and* the project files its command line runs ([`config`], [`trust`]).
//! - `npx`/`uvx` never run with the project as their working directory, even
//!   for a trusted project server ([`manager::cwd_for`]).
//! - A server's `instructions` (from `initialize`) are ignored: never put in the
//!   prompt ([`manager`]).
//! - Every MCP call is [`marv_core::ToolKind::Egress`] once the session is
//!   tainted, `readOnlyHint` tools included: their arguments leave the sandbox
//!   ([`tool::McpTool::kind`]).
//! - Results are capped at 30k chars and images are named, never sent ([`tool`]).
//!
//! This crate doesn't depend on `marv-tools`: [`tool::McpTool`] is plain data
//! plus a call handle, and `marv-session` adapts it to the `Tool` trait.

pub mod client;
pub mod config;
pub mod deferred;
pub mod install;
pub mod jsonrpc;
pub mod manager;
pub mod oauth;
pub mod oauth_store;
pub mod ordered;
pub mod personal_file;
pub mod tool;
pub mod trust;

mod text;

#[cfg(test)]
pub(crate) mod testutil {
    use std::cell::RefCell;
    use std::path::PathBuf;

    thread_local! {
        // The folders this test made, removed when its thread ends. libtest runs each test on its own thread, so that
        // is the end of the test, passed or panicked. Callers get a plain PathBuf (many hand it on by value into a
        // store or a manager), so the guard can't live with them. #89: these used to be "left for the OS to clean",
        // and nothing ever did; test runs filled the /tmp quota.
        static KEPT: RefCell<Vec<tempfile::TempDir>> = const { RefCell::new(Vec::new()) };
    }

    /// A fresh, empty folder under the system temp dir, removed when the test ends.
    pub fn temp_dir(tag: &str) -> PathBuf {
        let dir = tempfile::Builder::new().prefix(&format!("marv-mcp-{tag}-")).tempdir().unwrap();
        let path = dir.path().to_path_buf();
        KEPT.with(|kept| kept.borrow_mut().push(dir));
        path
    }
}

/// What MCP code reports. Messages are Marv's own sentences: the model or the
/// user reads them as they are.
#[derive(Debug, thiserror::Error)]
pub enum McpError {
    /// A config entry, file or `/mcp add` argument Marv can't use, with why.
    #[error("{0}")]
    Config(String),
    /// A server needs the user to log in (again): `<server> needs a new login: ...`.
    #[error(transparent)]
    LoginNeeded(#[from] oauth::LoginNeeded),
    #[error(transparent)]
    UnusableTokenStore(#[from] oauth_store::UnusableTokenStore),
    /// A remote server answered 401 and its login couldn't fix it (none stored,
    /// or the user has to go through the browser).
    #[error("the server wants a login (HTTP 401)")]
    Unauthorized,
    /// A server error, a dead connection, a timeout.
    #[error("{0}")]
    Server(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

pub type Result<T, E = McpError> = std::result::Result<T, E>;

/// The environment variables a config is expanded against (`${VAR}`).
pub type Env = std::collections::BTreeMap<String, String>;
