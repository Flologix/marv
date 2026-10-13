//! The library entry point: a session without a terminal. Ports Marv's `src/sdk.ts` (`createSession`) as a builder:
//!
//! ```ignore
//! let session = SessionBuilder::new("/path/to/project", ProviderOption::OpenRouter { api_key, model: None, base_url: None })
//!     .sources(&[Source::User, Source::Project])
//!     .approve(my_approver)
//!     .build()
//!     .await?;
//! let mut turn = session.send("fix the failing test", SendOptions::default())?;
//! while let Some(event) = turn.next().await { /* … */ }
//! ```
//!
//! It reads nothing from disk unless asked (`sources`). Without an approver only what yolo vouches for runs (reads,
//! edits outside `.git`, sandboxed commands without network); anything else goes back to the model as refused.
//! Checked before anything starts:
//! - `cwd` is a folder;
//! - with yolo on (the default) it isn't the home folder, above it, or `/`: yolo-safe edits run before any approver
//!   is asked, so they'd change dotfiles unasked. With `yolo(false)` it's allowed, and `no_yolo` keeps it off;
//! - `resume` needs `persist`.
//!
//! `mcp_servers` given in code are trusted like your own (no trust prompt, started in your home folder,
//! `${MARV_PROJECT_DIR}` points at the project) and win a name clash with the config files. Never pass config read
//! from a repository you don't control: that's what `Source::Project` (still trust-gated) is for. The session is
//! built before the MCP servers start (its constructor can fail), and everything is closed if anything after fails.
//! Env vars (`OPENROUTER_API_KEY`, `MARV_MODEL`) are ignored: a library uses what it's given.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use marv_mcp::deferred::McpToolsSetting;
use marv_provider::ProviderOption;
use marv_tools::{Approver, ToolRef};
use serde_json::Value;

use crate::config::{Env, default_config_dir};
use crate::paths::{home_dir, home_or_above, project_key};
use crate::session::{MarvSession, ProviderChoice, ResumeTarget, SessionError, SessionInit, SystemPrompt, TranscriptFn, Warn};
use crate::sessions::SessionStore;
use crate::sources::{Source, load_sources};
use crate::trajectory::TrajectoryStore;

/// `true` → the default store; a store → that one; `false`/absent → none.
#[derive(Clone, Default)]
pub enum Store<T> {
    #[default]
    Off,
    Default,
    Given(Arc<T>),
}

/// What `createSession` takes (Marv's `SessionOptions`).
#[derive(Clone)]
pub struct SessionOptions {
    pub cwd: PathBuf,
    pub provider: ProviderOption,
    pub sources: Vec<Source>,
    pub system_prompt: Option<SystemPrompt>,
    /// Offered to the main agent next to the built-in ones; names must be their own.
    pub tools: Vec<ToolRef>,
    /// `.mcp.json`'s format, `{ name: entry }`, in order.
    pub mcp_servers: Vec<(String, Value)>,
    pub approve: Option<Arc<dyn Approver>>,
    pub max_steps: Option<u32>,
    pub diagnostics: bool,
    pub checks: bool,
    pub mcp_tools: McpToolsSetting,
    pub sandbox: bool,
    pub yolo: bool,
    pub thinking: bool,
    pub persist: Store<SessionStore>,
    pub trajectories: Store<TrajectoryStore>,
    pub transcript: Option<TranscriptFn>,
    /// Continue a saved session (needs `persist`). `Latest` with nothing saved starts a new one; an unknown id fails.
    pub resume: Option<ResumeTarget>,
    /// Where Marv keeps its files (default `~/.marv`, or `MARV_CONFIG_DIR` from the real environment).
    pub config_dir: Option<PathBuf>,
    pub on_warning: Option<Warn>,
}

/// Builds a [`MarvSession`] (Marv's `createSession`).
pub struct SessionBuilder {
    pub options: SessionOptions,
}

impl SessionBuilder {
    pub fn new(cwd: impl Into<PathBuf>, provider: ProviderOption) -> Self {
        Self {
            options: SessionOptions {
                cwd: cwd.into(),
                provider,
                sources: Vec::new(),
                system_prompt: None,
                tools: Vec::new(),
                mcp_servers: Vec::new(),
                approve: None,
                max_steps: None,
                diagnostics: true,
                checks: true,
                mcp_tools: McpToolsSetting::Auto,
                sandbox: true,
                yolo: true,
                thinking: false,
                persist: Store::Off,
                trajectories: Store::Off,
                transcript: None,
                resume: None,
                config_dir: None,
                on_warning: None,
            },
        }
    }

    pub fn sources(mut self, sources: &[Source]) -> Self {
        self.options.sources = sources.to_vec();
        self
    }
    pub fn approve(mut self, approver: Arc<dyn Approver>) -> Self {
        self.options.approve = Some(approver);
        self
    }
    pub fn yolo(mut self, on: bool) -> Self {
        self.options.yolo = on;
        self
    }
    pub fn persist(mut self, store: Store<SessionStore>) -> Self {
        self.options.persist = store;
        self
    }
    pub fn trajectories(mut self, store: Store<TrajectoryStore>) -> Self {
        self.options.trajectories = store;
        self
    }
    pub fn resume(mut self, target: ResumeTarget) -> Self {
        self.options.resume = Some(target);
        self
    }
    pub fn max_steps(mut self, steps: u32) -> Self {
        self.options.max_steps = Some(steps);
        self
    }

    /// Checks the options, loads the sources, makes the session, starts its MCP servers, resumes.
    pub async fn build(self) -> Result<MarvSession, SessionError> {
        create_session(self.options).await
    }
}

/// Why yolo isn't allowed in `root`, or `None` (the SDK's `noYolo`).
pub fn no_yolo_reason(root: &Path, home: &Path) -> Option<String> {
    if !home_or_above(root, home) {
        return None;
    }
    let where_ = if root == Path::new("/") { "the root of the file system" } else { "your home folder (or above it)" };
    Some(format!(
        "cwd {} is {where_}: with yolo on, Marv would change files there without asking, dotfiles included. Use a project folder, or pass yolo: false (then every change goes to your approver, or is refused without one).",
        root.display()
    ))
}

fn store_for<T>(option: &Store<T>, make: impl FnOnce() -> T) -> Option<Arc<T>> {
    match option {
        Store::Off => None,
        Store::Default => Some(Arc::new(make())),
        Store::Given(t) => Some(t.clone()),
    }
}

/// Marv's `createSession`. See the module docs for what's checked.
pub async fn create_session(options: SessionOptions) -> Result<MarvSession, SessionError> {
    let root = std::fs::canonicalize(&options.cwd).unwrap_or_else(|_| options.cwd.clone());
    if !root.is_dir() {
        return Err(SessionError::Invalid(format!("cwd {} isn't a folder", root.display())));
    }
    let home = home_dir();
    let no_yolo = no_yolo_reason(&root, &home);
    if let Some(why) = &no_yolo
        && options.yolo
    {
        return Err(SessionError::Invalid(why.clone()));
    }
    if options.resume.is_some() && matches!(options.persist, Store::Off) {
        return Err(SessionError::Invalid("resume needs persist (there's nowhere to resume from)".into()));
    }
    // The real environment only: never a project file (docs/SECURITY.md).
    let env: Env = std::env::vars().collect();
    let config_dir = options.config_dir.clone().unwrap_or_else(|| default_config_dir(&env, &home));
    let loaded = load_sources(&root, &options.sources, &config_dir, &home, &env).await;
    let mut problems = loaded.problems;
    let given = if options.mcp_servers.is_empty() {
        marv_mcp::config::LoadedConfig::default()
    } else {
        let mut env = env.clone();
        env.insert(marv_mcp::config::PROJECT_DIR_VAR.into(), root.to_string_lossy().into_owned());
        marv_mcp::config::parse_mcp_servers(&options.mcp_servers, &env)
    };
    problems.extend(given.problems);
    // Servers given in code first; one from the config files with the same name is skipped.
    let mut servers = given.servers;
    for server in loaded.mcp_servers {
        if servers.iter().any(|s| s.name == server.name) {
            problems.push(format!(
                "MCP server \"{}\" from the config files was ignored: mcpServers has one with that name.",
                server.name
            ));
        } else {
            servers.push(server);
        }
    }
    let mcp: Option<crate::mcp_tools::McpHandle> = if servers.is_empty() {
        None
    } else {
        // Logins made with /mcp login in the TUI (same config dir) are used and refreshed; a library never starts one.
        let _servers = servers;
        todo!(
            "port: McpManager::new(servers, ManagerOptions {{ trust: mcp-trust.json, logins: mcp-tokens.json, … }}), onChange login warnings, start()"
        )
    };

    let mut init = SessionInit::new(root.clone(), ProviderChoice::Option(options.provider));
    init.thinking = options.thinking;
    init.instructions = loaded.instructions.map(|i| i.text);
    init.skills = loaded.skills;
    init.agents = loaded.agents;
    init.memory = loaded.memory;
    init.mcp = mcp;
    init.owns_mcp = true;
    init.tools = options.tools;
    init.system_prompt = options.system_prompt;
    init.approve = options.approve;
    init.max_steps = options.max_steps;
    init.diagnostics = options.diagnostics;
    init.checks = options.checks;
    init.mcp_tools = options.mcp_tools;
    init.sandbox = options.sandbox;
    init.yolo = options.yolo;
    init.no_yolo = no_yolo;
    init.sessions = store_for(&options.persist, || SessionStore::new(config_dir.join("sessions")));
    init.trajectories = store_for(&options.trajectories, || TrajectoryStore::new(config_dir.join("trajectories")));
    init.worktrees_dir = Some(config_dir.join("worktrees").join(project_key(&root)));
    init.transcript = options.transcript;
    init.on_warning = options.on_warning;
    init.problems = problems;
    let session = MarvSession::new(init)?;
    if let Some(target) = options.resume {
        let latest = target == ResumeTarget::Latest;
        match session.resume(target.clone()).await {
            Ok(Some(_)) => {}
            Ok(None) if latest => {}
            Ok(None) => {
                session.close().await;
                let ResumeTarget::Id(id) = target else { unreachable!() };
                return Err(SessionError::Invalid(format!("There's no saved session \"{id}\" for {}.", root.display())));
            }
            Err(err) => {
                session.close().await;
                return Err(err);
            }
        }
    }
    Ok(session)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yolo_is_refused_at_home_and_above() {
        let home = Path::new("/nonexistent/me");
        assert!(no_yolo_reason(home, home).unwrap().contains("your home folder (or above it)"));
        assert!(no_yolo_reason(Path::new("/"), home).unwrap().contains("the root of the file system"));
        assert!(no_yolo_reason(Path::new("/nonexistent/me/project"), home).is_none());
    }

    #[tokio::test]
    async fn resume_needs_persist_and_cwd_must_be_a_folder() {
        let provider = || ProviderOption::Ollama { model: "m".into(), host: None, context_length: None };
        let tmp = std::env::temp_dir();
        let err = SessionBuilder::new(&tmp, provider()).resume(ResumeTarget::Latest).build().await.err().unwrap();
        assert_eq!(err.to_string(), "resume needs persist (there's nowhere to resume from)");
        let err = SessionBuilder::new("/nonexistent/marv/x", provider()).build().await.err().unwrap();
        assert!(err.to_string().ends_with("isn't a folder"));
    }
}
