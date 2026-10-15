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
//! The servers start in the background (`McpManager::start`); the first turn waits for them.
//! Env vars (`OPENROUTER_API_KEY`, `MARV_MODEL`) are ignored: a library uses what it's given.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use marv_mcp::deferred::McpToolsSetting;
use marv_mcp::manager::{CONNECT_TIMEOUT, LOGIN_TIMEOUT, ManagerOptions, McpManager, McpState};
use marv_mcp::oauth_store::McpTokenStore;
use marv_mcp::trust::McpTrust;
use marv_provider::ProviderOption;
use marv_tools::{Approver, ToolRef};
use serde_json::Value;

use crate::config::{Env, default_config_dir};
use crate::paths::{home_dir, home_or_above, project_key};
use crate::session::{MarvSession, ProviderChoice, ResumeTarget, SessionError, SessionInit, SystemPrompt, TranscriptFn, Warn};
use crate::sessions::SessionStore;
use crate::sources::{Source, load_sources};
use crate::trajectory::TrajectoryStore;
use marv_tools::formatter::FormatterSetting;

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
    /// Offer the plan tool: a checklist the model keeps for a long task (`session.plan()` reads it, each change also
    /// arrives as a tool end's `result.plan`). Default true. Fixed for the session, since it changes the tool list.
    pub plan: bool,
    pub diagnostics: bool,
    pub checks: bool,
    /// The formatter the edit tools run after a change (#67). Default on (detect from the project's config).
    pub formatter: FormatterSetting,
    /// Before a turn's first change, snapshot the project (git commits under a hidden ref,
    /// `refs/marv/checkpoints/<session>`, or a git dir of Marv's own in `config_dir` outside a repository), and again
    /// when it ends, so `session.undo_plan()` and `session.undo(plan)` can put that turn's files back. Default true.
    pub checkpoints: bool,
    pub mcp_tools: McpToolsSetting,
    pub sandbox: bool,
    /// Let bash run a command on the host, outside the sandbox (`sandbox: false`), when your approver says yes. Asked
    /// every time (the request has `door: Host` and `once`; an "always" counts as one yes). Default false: such calls
    /// are refused before your approver is asked (Marv #52).
    pub host_commands: bool,
    /// Offer the github tool (gh with the user's login, outside the sandbox) when gh is on PATH and logged in,
    /// checked once here. Reads run without asking; writes and sensitive API reads go to your approver (`door:
    /// Github`, scope `github:<command>`); what can never run (auth, config, extensions, --web, another host…) is
    /// refused. Default false (Marv #47).
    pub github: bool,
    /// Whether gh is there and logged in, when the caller already asked (`marv -p` asks before it joins the mesh);
    /// None: asked here, when `github` is set.
    pub github_known: Option<bool>,
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
    /// The process's switchboard (marv-rs mesh; `crate::mesh::start`), and who grants its wires.
    pub mesh: Option<crate::mesh::MeshHandle>,
    pub wire_approver: Option<Arc<dyn crate::mesh::WireApprover>>,
    /// Offer the process tool (Marv #51): a dev server or watcher that keeps running in its own sandbox, its ports
    /// forwarded to 127.0.0.1, each start through your approver (`door: process`; scope `process:<command>`). Default
    /// true, where bubblewrap works. `close()` stops them all.
    pub processes: bool,
    /// Pre-approved scopes (#98): `--approve`-style names ("web", "network"…) that seed the session's "always"
    /// scopes, and are listed in the prompt. A subagent inherits the egress ones never. Default: none.
    pub pre_approved: Vec<String>,
    /// Extra writable sandbox folders (#98), e.g. bd's database folder: canonical paths, already checked
    /// (`marv_sandbox::resolve_writable`), bound read-write into every command's sandbox. Default: none.
    pub writable: Vec<PathBuf>,
    /// Session-start hooks (#108): commands run as each conversation begins, outside the sandbox, their output sent
    /// once at the start of its first message. Pass only commands the user wrote into their own config
    /// (`crate::hooks::session_start_hooks`), never a repository's. Default none.
    pub hooks: Vec<crate::hooks::HookCommand>,
    /// A one-shot run, like `marv -p`: the process tool tells the model its processes end when the run does (#86).
    /// Default false.
    pub one_shot: bool,
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
                plan: true,
                diagnostics: true,
                checks: true,
                formatter: FormatterSetting::Auto,
                checkpoints: true,
                mcp_tools: McpToolsSetting::Auto,
                sandbox: true,
                host_commands: false,
                github: false,
                github_known: None,
                yolo: true,
                thinking: false,
                persist: Store::Off,
                trajectories: Store::Off,
                transcript: None,
                resume: None,
                config_dir: None,
                on_warning: None,
                mesh: None,
                wire_approver: None,
                processes: true,
                pre_approved: Vec::new(),
                writable: Vec::new(),
                hooks: Vec::new(),
                one_shot: false,
            },
        }
    }

    pub fn sources(mut self, sources: &[Source]) -> Self {
        self.options.sources = sources.to_vec();
        self
    }
    /// The client's approver. A request with `must_ask` (tainted egress, a protected-path write) is one a person
    /// must answer every time: an approver that answers from a policy instead (as `marv -p --approve` does) should
    /// refuse it unless its user opted in (`--approve-tainted`, `--approve-protected`).
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
    pub fn processes(mut self, on: bool) -> Self {
        self.options.processes = on;
        self
    }
    /// Pre-approved scopes (#98): `--approve` names ("web", "network"…) seeded as "always" for the session.
    pub fn pre_approve(mut self, names: &[String]) -> Self {
        self.options.pre_approved = names.to_vec();
        self
    }
    /// Extra writable sandbox folders (#98), already checked (`marv_sandbox::resolve_writable`).
    pub fn writable(mut self, folders: Vec<PathBuf>) -> Self {
        self.options.writable = folders;
        self
    }
    pub fn one_shot(mut self, on: bool) -> Self {
        self.options.one_shot = on;
        self
    }
    pub fn max_steps(mut self, steps: u32) -> Self {
        self.options.max_steps = Some(steps);
        self
    }
    /// `false` leaves the plan tool out (#15; the eval's "off" arm).
    pub fn plan(mut self, on: bool) -> Self {
        self.options.plan = on;
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
        "cwd {} is {where_}: with yolo on, Marv would change files there without asking, dotfiles included. Use a project folder, or pass yolo: false (then every change goes to your approver, or is refused without one). There, sandboxed commands see the home read-only, with Marv's config, credentials and agent sockets hidden.",
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
    // Logins made with /mcp login in the TUI (same config dir) are used and refreshed; a library never starts one.
    let tokens_file = config_dir.join("mcp-tokens.json");
    let mcp: Option<crate::mcp_tools::McpHandle> = (!servers.is_empty()).then(|| {
        McpManager::new(
            servers,
            ManagerOptions {
                root: root.clone(),
                project_key: project_key(&root),
                home: home.clone(),
                version: env!("CARGO_PKG_VERSION").into(),
                trust: Some(McpTrust::new(config_dir.join("mcp-trust.json"))),
                logins: Some(McpTokenStore::new(&tokens_file)),
                env: env.clone(),
                connect_timeout: CONNECT_TIMEOUT,
                login_timeout: LOGIN_TIMEOUT,
            },
        )
    });

    // Before the session: its tool list is fixed from the start (the prompt cache).
    let github = options.github
        && match options.github_known {
            Some(known) => known,
            None => {
                let pairs: Vec<(String, String)> = env.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
                marv_tools::github::github_available(&pairs, marv_tools::github::AUTH_CHECK).await
            }
        };
    let mut init = SessionInit::new(root.clone(), ProviderChoice::Option(options.provider));
    init.thinking = options.thinking;
    init.instructions = loaded.instructions.map(|i| i.text);
    // Nested AGENTS.md files are the project's too (#62).
    init.nested_instructions = options.sources.contains(&Source::Project);
    init.skills = loaded.skills;
    init.agents = loaded.agents;
    init.memory = loaded.memory;
    init.mcp = mcp;
    init.owns_mcp = true;
    init.tools = options.tools;
    init.system_prompt = options.system_prompt;
    init.approve = options.approve;
    init.mesh = options.mesh;
    init.wire_approver = options.wire_approver;
    init.max_steps = options.max_steps;
    init.plan = options.plan;
    init.diagnostics = options.diagnostics;
    init.checks = options.checks;
    init.formatter = options.formatter;
    init.checkpoints = options.checkpoints;
    init.checkpoints_dir = Some(config_dir.join("checkpoints").join(project_key(&root)));
    init.mcp_tools = options.mcp_tools;
    init.sandbox = options.sandbox;
    init.host_commands = options.host_commands;
    init.github = github;
    init.yolo = options.yolo;
    init.no_yolo = no_yolo;
    init.sessions = store_for(&options.persist, || SessionStore::new(config_dir.join("sessions")));
    init.trajectories = store_for(&options.trajectories, || TrajectoryStore::new(config_dir.join("trajectories")));
    init.worktrees_dir = Some(config_dir.join("worktrees").join(project_key(&root)));
    init.tool_output_dir = Some(config_dir.join(marv_tools::output::TOOL_OUTPUT_DIR));
    init.transcript = options.transcript;
    init.on_warning = options.on_warning.clone();
    init.problems = problems;
    init.processes = options.processes;
    init.pre_approved = options.pre_approved;
    init.writable = options.writable;
    init.hooks = options.hooks;
    init.one_shot = options.one_shot;
    let session = MarvSession::new(init)?;
    if let Some(mcp) = &session.mcp() {
        // A server that needs a login is told once (again only after it connected meanwhile).
        let warned = std::sync::Mutex::new(std::collections::HashSet::new());
        let (weak, warn) = (Arc::downgrade(mcp), options.on_warning.clone());
        mcp.set_on_change(move || {
            let Some(mcp) = weak.upgrade() else { return };
            let mut warned = warned.lock().unwrap_or_else(|e| e.into_inner());
            for s in mcp.status() {
                if s.state != McpState::NeedsLogin {
                    if s.state == McpState::Connected {
                        warned.remove(&s.name);
                    }
                    continue;
                }
                if !warned.insert(s.name.clone()) {
                    continue;
                }
                if let Some(warn) = &warn {
                    let text = format!(
                        "MCP server \"{}\" needs a login: run marv in a terminal and /mcp login {}; logins are stored in {}",
                        s.name,
                        s.name,
                        tokens_file.display()
                    );
                    // The caller's handler must not break the manager's state change.
                    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| warn(text)));
                }
            }
        });
        let mcp = mcp.clone();
        tokio::spawn(async move { mcp.start().await });
    }
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

    #[tokio::test]
    async fn plan_false_leaves_the_plan_tool_out_by_default_its_offered() {
        let provider = || ProviderOption::Ollama { model: "m".into(), host: None, context_length: None };
        let dir = std::env::temp_dir().join(format!("marv-sdk-plan-{}-{}", std::process::id(), marv_fs::random_hex()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut on = SessionBuilder::new(&dir, provider());
        on.options.config_dir = Some(dir.join("config"));
        let off = SessionBuilder { options: SessionOptions { plan: false, ..on.options.clone() } };
        assert!(on.options.plan);
        on = on.plan(true);
        let (on, off) = (on.build().await.unwrap(), off.build().await.unwrap());
        assert!(on.system_prompt().contains("keep a plan with the plan tool"));
        assert!(!off.system_prompt().contains("plan tool"));
        assert_eq!(on.plan(), None);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Writes made.txt, then says it did.
    struct MakesAFile(std::sync::Mutex<u32>);
    impl marv_provider::Provider for MakesAFile {
        fn name(&self) -> &str {
            "scripted"
        }
        fn context_length(&self) -> Option<u64> {
            None
        }
        fn stream(&self, _history: &[marv_core::ChatTurn], _options: marv_provider::StreamOptions) -> marv_provider::EventStream {
            use marv_core::ProviderEvent;
            let first = {
                let mut n = self.0.lock().unwrap();
                *n += 1;
                *n == 1
            };
            let events = if first {
                let arguments = serde_json::json!({"path": "made.txt", "content": "made\n"}).to_string();
                vec![
                    ProviderEvent::ToolCall(marv_core::ToolCall { id: "w".into(), name: "write_file".into(), arguments }),
                    ProviderEvent::Done { reason: Some("tool_calls".into()) },
                ]
            } else {
                vec![ProviderEvent::TextDelta("Made it.".into()), ProviderEvent::Done { reason: Some("stop".into()) }]
            };
            Box::pin(futures::stream::iter(events))
        }
    }

    // Ports tests/sdk.test.ts "checkpoints: a turn's files can be put back with undoPlan() and undo(), outside a
    // repository too (#16)".
    #[tokio::test]
    async fn checkpoints_a_turns_files_can_be_put_back_outside_a_repository_too() {
        use futures::StreamExt;
        let base = std::env::temp_dir().join(format!("marv-sdk-cp-{}-{}", std::process::id(), marv_fs::random_hex()));
        let (root, config_dir) = (base.join("project"), base.join("config"));
        std::fs::create_dir_all(&root).unwrap();
        let session_with = |checkpoints: bool| {
            let mut builder = SessionBuilder::new(&root, ProviderOption::Custom(Arc::new(MakesAFile(std::sync::Mutex::new(0)))));
            builder.options.config_dir = Some(config_dir.clone());
            builder.options.sandbox = false;
            builder.options.checkpoints = checkpoints;
            builder.build()
        };
        let session = session_with(true).await.unwrap();
        session.send("make a file", crate::session::SendOptions::default()).unwrap().collect::<Vec<_>>().await;
        assert!(root.join("made.txt").exists());
        let plan = session.undo_plan().await.unwrap().unwrap_or_else(|r| panic!("{}", r.reason));
        let files: Vec<(&str, crate::checkpoints::undo::UndoAction)> =
            plan.files.iter().map(|f| (f.path.as_str(), f.action)).collect();
        assert_eq!(files, [("made.txt", crate::checkpoints::undo::UndoAction::Delete)]);
        assert_eq!(session.undo(&plan).await.unwrap().deleted, ["made.txt"]);
        assert!(!root.join("made.txt").exists());
        assert!(config_dir.join("checkpoints").exists());
        session.close().await;

        let off = session_with(false).await.unwrap();
        off.send("make a file", crate::session::SendOptions::default()).unwrap().collect::<Vec<_>>().await;
        assert!(off.undo_plan().await.unwrap().unwrap_err().reason.contains("off"));
        off.close().await;
        let _ = std::fs::remove_dir_all(&base);
    }
}
