//! Drives one chat's session: builds it in the chat's folder, then turns the page's messages into turns,
//! interrupts, approvals and settings, and the session's events into JSON for the page.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use marv_provider::ProviderOption;
use marv_session::sdk::Store;
use marv_session::skills::LoadedSkill;
use marv_session::sources::Source;
use marv_session::{Configure, MarvSession, ProviderChoice, ResumeTarget, SendOptions, SessionBuilder};
use serde_json::{Value, json};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::approver::WebApprover;
use crate::events;
use crate::protocol::{ClientMessage, is_model_id};

/// Where and how a chat runs.
pub struct ChatSpec {
    /// The folder Marv works in: it reads and changes what's below it, and nothing else without asking.
    pub root: PathBuf,
    /// Marv's own files: saved sessions, checkpoints.
    pub config_dir: PathBuf,
    /// The OpenRouter key.
    pub api_key: String,
    /// #159: the Z.ai Coding Plan key, for `zai/…` models (the person's own machine only).
    pub zai_api_key: Option<String>,
    /// #159: the ChatGPT sign-in's token file, for `chatgpt/…` models (the person's own machine only).
    pub chatgpt_auth: Option<PathBuf>,
    pub model: String,
    pub max_steps: u32,
    /// Sandboxed commands and edits below `root` run without asking.
    pub yolo: bool,
    /// The saved session to continue (`Id` from an earlier `ready`), or `None` for a new one.
    pub resume: Option<ResumeTarget>,
    /// Offer the process tool (dev servers, their ports on this machine's 127.0.0.1): only where the person using
    /// the page is on this machine.
    pub processes: bool,
    /// What Marv loads from disk, as the CLI does: `User` (~/.marv's skills, memory, MCP servers, instructions) and
    /// `Project` (the folder's). Only for the person's own machine; marv-web loads none.
    pub sources: Vec<Source>,
    /// Put the chat on this machine's mesh, as the TUI is: it gets a name other Marvs see (`marv mesh ls`, the
    /// `mesh_list` tool), wires need its person's approval, and a message on a wire wakes it. Only on the person's own
    /// machine.
    pub mesh: bool,
    /// MCP servers switched off for this chat, by name.
    pub mcp_off: Vec<String>,
    /// Allow `mcp` login/logout/trust (OAuth opens the browser on this machine): only on the person's own machine.
    pub manage_mcp: bool,
    /// Let bash run a command on this machine outside the sandbox, asked every time (never yolo, never "always"):
    /// only where the person approving is at this machine.
    pub host_commands: bool,
    /// #204: write each turn's trajectory (`config_dir/trajectories/<project>/<session>.jsonl`), as the TUI and
    /// `marv -p` do: the config's `trajectories` ([`trajectories_on`]).
    pub trajectories: bool,
}

/// #204: the config's `trajectories` in `config_dir` (on by default), resolved as `marv -p` resolves it. A config file
/// that can't be read leaves the default: the chat still opens (`marv -p` would stop and say why).
pub fn trajectories_on(config_dir: &std::path::Path) -> bool {
    use marv_session::config::{ConfigStore, Env, resolve_config};
    let env: Env = std::env::vars().collect();
    let file = ConfigStore::new(config_dir).load().ok().flatten();
    resolve_config(file.as_ref(), &env).trajectories
}

/// The commands the page offers in its `/` menu (besides the skills), and what they do here.
const COMMANDS: &[(&str, &str)] = &[
    ("help", "Show the commands"),
    ("skills", "List the skills; run one with /<skill> <request>"),
    ("compact", "Summarize the conversation to free up context (/compact <what to keep>)"),
    ("cost", "Tokens used and cost so far"),
    ("memory", "Show what Marv remembers"),
    ("model", "Switch the model (/model <id>)"),
    ("permissions", "What Marv may do without asking: this chat's approvals, and what's saved"),
];

/// A model id that starts with this runs on the local Ollama (`ollama/gemma4:12b`); one with [`ZAI_PREFIX`] or
/// [`CHATGPT_PREFIX`] on that plan (#159); any other is OpenRouter's.
pub const OLLAMA_PREFIX: &str = "ollama/";
/// #159: `zai/glm-5.3`: the Z.ai GLM Coding Plan, with its own key. (OpenRouter's Z.ai models are `z-ai/…`.)
pub const ZAI_PREFIX: &str = "zai/";
/// #159: `chatgpt/<slug>`: the person's ChatGPT plan, through their sign-in.
pub const CHATGPT_PREFIX: &str = "chatgpt/";

pub fn is_local(model: &str) -> bool {
    model.starts_with(OLLAMA_PREFIX)
}

/// The provider a chat's model id names. Each gets only its own credential: a plan without one gets an empty key
/// (its first request says what's missing), never another provider's.
pub fn provider_option(spec: &ChatSpec, model: &str) -> ProviderOption {
    if let Some(local) = model.strip_prefix(OLLAMA_PREFIX) {
        // Ollama on its default port, with the CLI's default context window (Ollama's own is far smaller).
        return ProviderOption::Ollama {
            model: local.to_string(),
            host: None,
            context_length: Some(marv_session::config::DEFAULT_CONTEXT_LENGTH),
        };
    }
    if let Some(plan) = model.strip_prefix(ZAI_PREFIX) {
        let api_key = spec.zai_api_key.clone().unwrap_or_default();
        return ProviderOption::Zai { api_key, model: Some(plan.to_string()), base_url: None };
    }
    if let Some(slug) = model.strip_prefix(CHATGPT_PREFIX) {
        return match spec.chatgpt_auth.clone() {
            Some(auth_file) => ProviderOption::ChatGpt { model: slug.to_string(), auth_file },
            // Never an empty path (it would read a file in the working folder).
            None => ProviderOption::Custom(std::sync::Arc::new(marv_provider::FailingProvider {
                name: format!("chatgpt · {slug}"),
                message: "Sign in with ChatGPT isn't available here.".into(),
            })),
        };
    }
    ProviderOption::OpenRouter { api_key: spec.api_key.clone(), model: Some(model.to_string()), base_url: None }
}

/// Why a chat in `root` has no runtime folder, when it's because `MARV_RUNTIME_DIR` was refused
/// (`marv_sandbox::override_refusal_for`), printable; `None` otherwise (no override: the mesh is quietly off).
fn mesh_refused_notice(env: &std::collections::BTreeMap<String, String>, root: &std::path::Path) -> Option<String> {
    marv_sandbox::override_refusal_for(env, Some(root)).map(|why| format!("Not on the mesh: {}", marv_core::printable(&why)))
}

/// Runs the chat until `incoming` closes (the page went away). Sends `ready` (with the session's id, for a later
/// resume) or `fatal` first.
pub async fn run_chat(spec: ChatSpec, mut incoming: UnboundedReceiver<ClientMessage>, out: UnboundedSender<Value>) {
    let approver = Arc::new(WebApprover::new(out.clone()));
    // The mesh first: the session's tools include it from the start (the prompt cache).
    let env: std::collections::BTreeMap<String, String> = std::env::vars().collect();
    let mut mesh = None;
    if spec.mesh {
        match marv_session::mesh_process::start(&env, &spec.root, std::slice::from_ref(&spec.root)) {
            Ok(Some(started)) => mesh = Some(started),
            // A MARV_RUNTIME_DIR that was refused says why (a symlink on the way, inside the project), as the TUI
            // does, rather than the chat just being off the mesh (#168).
            Ok(None) => {
                if let Some(text) = mesh_refused_notice(&env, &spec.root) {
                    let _ = out.send(json!({"type": "notice", "text": text}));
                }
            }
            Err(e) => {
                let _ = out.send(json!({"type": "notice", "text": format!("Not on the mesh: {}", marv_core::printable(&e))}));
            }
        }
    }
    let session = match open(&spec, approver.clone(), out.clone(), mesh.as_ref().map(|m| m.handle.clone())).await {
        Ok(session) => session,
        Err(message) => {
            let _ = out.send(json!({"type": "fatal", "message": message}));
            return;
        }
    };
    // #202: the chat's last turn was cut off by Marv quitting (a force-quit during an approval): the window's own
    // transcript still shows the turn as it was, so it gets the session's notice.
    if let Some(text) = session.take_cut_off_notice() {
        let _ = out.send(json!({"type": "notice", "text": text}));
    }
    // Its door: other Marvs find it and ask for wires, which its person answers here.
    let as_approver: Arc<dyn marv_tools::types::Approver> = approver.clone();
    if let Some(mesh) = mesh.as_mut()
        && let Some(wires) = marv_session::mesh::wire_approver(None, Some(&as_approver))
        && let Err(e) = mesh.open_door(wires)
    {
        let _ = out.send(json!({"type": "notice", "text": format!("Not on the mesh: {}", marv_core::printable(&e))}));
    }
    let watching = CancellationToken::new();
    if let Some(handle) = session.mesh() {
        tokio::spawn(watch_mesh(handle, session.clone(), approver.clone(), out.clone(), watching.clone()));
    }
    let home = marv_session::paths::home_dir();
    let skills: Vec<LoadedSkill> = if spec.sources.is_empty() {
        Vec::new()
    } else {
        let (project, user) = (spec.sources.contains(&Source::Project), spec.sources.contains(&Source::User));
        marv_session::skills::load_skills(project.then_some(spec.root.as_path()), user.then_some(home.as_path())).0
    };
    let menu: Vec<Value> = COMMANDS
        .iter()
        .map(|(name, description)| json!({"name": name, "description": description}))
        .chain(skills.iter().map(|s| json!({"name": s.skill.name, "description": s.skill.description, "skill": true})))
        .collect();
    let mesh_name = mesh.as_ref().map(|m| m.name.clone());
    let _ = out.send(json!({
        "type": "ready", "sessionId": session.id(), "model": spec.model, "commands": menu, "mesh": mesh_name,
    }));

    let mut turn: Option<(CancellationToken, JoinHandle<()>)> = None;
    while let Some(message) = incoming.recv().await {
        match message {
            ClientMessage::Send { text } => {
                let text = text.trim();
                if text.is_empty() {
                    continue;
                }
                let cancel = CancellationToken::new();
                let before = session.usage().totals;
                match session.send(text, SendOptions { for_model: None, cancel: Some(cancel.clone()) }) {
                    Ok(events) => {
                        turn = Some((cancel, tokio::spawn(forward(events, before, session.clone(), out.clone()))));
                    }
                    Err(e) => {
                        let _ = out.send(json!({"type": "error", "message": e.to_string()}));
                    }
                }
            }
            ClientMessage::Command { line } => {
                let (name, args) =
                    line.trim().trim_start_matches('/').split_once(' ').unwrap_or((line.trim().trim_start_matches('/'), ""));
                let (name, args) = (name.to_lowercase(), args.trim());
                if let Some(skill) = skills.iter().find(|s| s.skill.name == name) {
                    // As typed in the transcript; the skill's instructions for the model.
                    let for_model = marv_session::skills::skill_message(&skill.skill, args);
                    let cancel = CancellationToken::new();
                    let before = session.usage().totals;
                    match session.send(line.trim(), SendOptions { for_model: Some(for_model), cancel: Some(cancel.clone()) }) {
                        Ok(events) => {
                            turn = Some((cancel, tokio::spawn(forward(events, before, session.clone(), out.clone()))));
                        }
                        Err(e) => {
                            let _ = out.send(json!({"type": "error", "message": e.to_string()}));
                        }
                    }
                    continue;
                }
                let print = |text: String| {
                    let _ = out.send(json!({"type": "command_output", "text": text}));
                };
                match name.as_str() {
                    "help" | "" => print(help(&skills)),
                    "skills" => print(skills_text(&skills)),
                    "cost" => {
                        let usage = session.usage();
                        print(marv_session::usage::usage_report(&usage.totals, usage.last.as_ref(), usage.context_length));
                    }
                    // The same view as the TUI's (#82), shown here only: never sent to the model. The desktop has no
                    // config file of its own to show.
                    "permissions" if !args.is_empty() => print(marv_session::permissions::PERMISSIONS_USAGE.into()),
                    "permissions" => {
                        let servers = session.mcp().map(|m| m.status());
                        print(marv_session::permissions::permissions_text(&session.permissions(), None, servers.as_deref()))
                    }
                    "memory" => print(match session.memory() {
                        Some(m) if !m.personal.is_empty() || !m.project.is_empty() => {
                            let list = |items: &[String]| items.iter().map(|i| format!("- {i}")).collect::<Vec<_>>().join("\n");
                            format!("**Personal**\n{}\n\n**This project**\n{}", list(&m.personal), list(&m.project))
                        }
                        _ => "Marv doesn't remember anything yet.".into(),
                    }),
                    "model" if is_model_id(args) => {
                        let changes = Configure {
                            provider: Some(ProviderChoice::Option(provider_option(&spec, args))),
                            ..Configure::default()
                        };
                        match session.configure(changes) {
                            Ok(()) => {
                                let _ = out.send(json!({"type": "configured", "model": args}));
                                print(format!("Model: `{args}` from the next message."));
                            }
                            Err(e) => print(e.to_string()),
                        }
                    }
                    "model" => {
                        print("Use /model <OpenRouter id>, e.g. /model z-ai/glm-5.3-flash, or the model menu up top.".into())
                    }
                    "compact" => {
                        let (session, out) = (session.clone(), out.clone());
                        let focus = (!args.is_empty()).then(|| args.to_string());
                        tokio::spawn(async move {
                            let text = match session.compact(focus.as_deref()).await {
                                Ok(marv_session::CompactResult::Compacted { .. }) => "Conversation compacted.".to_string(),
                                Ok(marv_session::CompactResult::NotCompacted { reason, .. }) => {
                                    format!("Not compacted ({}).", reason.as_str())
                                }
                                Err(e) => e.to_string(),
                            };
                            let _ = out.send(json!({"type": "command_output", "text": text}));
                        });
                    }
                    other => print(format!("/{other} isn't in Marv desktop yet. Type /help for what is.")),
                }
            }
            ClientMessage::Interrupt => {
                if let Some((cancel, _)) = &turn {
                    cancel.cancel();
                }
                // A turn a mesh message woke, too.
                session.interrupt();
                approver.cancel_all();
            }
            ClientMessage::Approval { id, decision } => approver.answer(id, decision),
            ClientMessage::Configure { model, max_steps, yolo } => {
                let model = model.filter(|m| is_model_id(m));
                let changes = Configure {
                    provider: model.as_deref().map(|m| ProviderChoice::Option(provider_option(&spec, m))),
                    max_steps: max_steps.map(|s| s.min(500)),
                    yolo,
                    ..Configure::default()
                };
                let reply = match session.configure(changes) {
                    Ok(()) => json!({"type": "configured", "model": model, "maxSteps": max_steps, "yolo": yolo}),
                    Err(e) => json!({"type": "error", "message": e.to_string()}),
                };
                let _ = out.send(reply);
            }
            ClientMessage::Mcp { action, name } => {
                let Some(mcp) = session.mcp() else {
                    let _ = out.send(json!({"type": "mcp", "servers": []}));
                    continue;
                };
                if action != "status" && !spec.manage_mcp {
                    continue;
                }
                let (out, name) = (out.clone(), name.unwrap_or_default());
                tokio::spawn(async move {
                    let say = |text: String| {
                        let _ = out.send(json!({"type": "command_output", "text": text}));
                    };
                    match action.as_str() {
                        "login" => {
                            let tell = out.clone();
                            let server = name.clone();
                            let authorize: marv_mcp::oauth::Authorize = Arc::new(move |url| {
                                let _ = tell.send(json!({"type": "command_output", "text": format!(
                                    "Log in to **{server}**: your browser is opening. If it doesn't, open [this link]({url})."
                                )}));
                                // The person's own browser, on their machine.
                                let _ = std::process::Command::new("xdg-open").arg(url.as_str()).spawn();
                            });
                            match mcp.login(&name, authorize) {
                                Ok(login) => match login.done.await {
                                    Ok(Ok(status)) => say(format!("{name}: logged in · {} tools", status.tools.len())),
                                    Ok(Err(e)) => say(format!("{name}: not logged in: {e}")),
                                    Err(_) => say(format!("{name}: not logged in: the login stopped")),
                                },
                                Err(e) => say(e.to_string()),
                            }
                        }
                        "logout" => match mcp.logout(&name).await {
                            Ok(_) => say(format!("{name}: logged out.")),
                            Err(e) => say(e.to_string()),
                        },
                        "trust" => match mcp.trust_all().await {
                            Ok(started) => say(started
                                .iter()
                                .map(|s| format!("{}: {}", s.name, state_name(s.state)))
                                .collect::<Vec<_>>()
                                .join("\n")),
                            Err(e) => say(format!("Couldn't trust the servers: {e}")),
                        },
                        _ => {}
                    }
                    let _ = out.send(mcp_status(&mcp));
                });
            }
            ClientMessage::Hello { .. } => {}
        }
    }
    // The page went away: stop the turn, wait for its cleanup, then close the session.
    if let Some((cancel, work)) = turn {
        cancel.cancel();
        approver.cancel_all();
        let _ = tokio::time::timeout(Duration::from_secs(5), work).await;
    }
    watching.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(3), session.close()).await;
    drop(mesh);
}

/// A message on a wired inbox wakes the chat, as `mesh.wake` does in the TUI: only while nothing runs and no approval
/// waits (otherwise it waits in the inbox for the next turn, and a later check picks it up), and within
/// `WAKES_PER_MINUTE` per wire. The woken turn reads it fenced as data from another agent, tainting the session.
async fn watch_mesh(
    mesh: marv_session::mesh::MeshHandle,
    session: MarvSession,
    approver: Arc<WebApprover>,
    out: UnboundedSender<Value>,
    stop: CancellationToken,
) {
    let mut limit = marv_session::mesh::WakeLimit::default();
    loop {
        tokio::select! {
            _ = mesh.mail() => {}
            // Mail that came during a turn is still unread after it: look again now and then.
            _ = tokio::time::sleep(Duration::from_secs(2)) => {}
            _ = stop.cancelled() => return,
        }
        if session.busy() || approver.waiting() {
            continue;
        }
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64);
        let check = marv_session::mesh::wake_check(&mesh, &mut limit, now);
        for wire in &check.limited {
            let _ = out.send(json!({"type": "notice", "text": format!(
                "{} woke Marv {} times a minute already: its messages on {} wait for your next message.",
                marv_core::printable(wire.sender.as_str()),
                marv_session::mesh::WAKES_PER_MINUTE,
                marv_core::printable(wire.receptacle.as_str()),
            )}));
        }
        let Some(by) = check.wake else { continue };
        let before = session.usage().totals;
        if let Ok(Some(turn)) = session.wake(&by) {
            let _ = out.send(json!({"type": "notice", "text": by.line()}));
            forward(turn, before, session.clone(), out.clone()).await;
        }
    }
}

fn state_name(state: marv_mcp::manager::McpState) -> &'static str {
    use marv_mcp::manager::McpState as S;
    match state {
        S::Untrusted => "untrusted",
        S::Connecting => "connecting",
        S::Connected => "connected",
        S::Failed => "failed",
        S::Removed => "removed",
        S::NeedsLogin => "needs_login",
    }
}

/// The chat's servers as the page shows them (never a token).
fn mcp_status(mcp: &marv_session::mcp_tools::McpHandle) -> Value {
    let servers: Vec<Value> = mcp
        .status()
        .iter()
        .map(|s| {
            json!({
                "name": s.name,
                "scope": if s.source == marv_mcp::config::ServerSource::Project { "folder" } else { "global" },
                "state": state_name(s.state),
                "tools": s.tools.len(),
                "error": s.error.as_deref().map(marv_core::printable),
                "canLogin": s.login.is_some(),
                "loggedIn": s.login.as_ref().is_some_and(|l| l.logged_in),
            })
        })
        .collect();
    json!({"type": "mcp", "servers": servers})
}

fn help(skills: &[LoadedSkill]) -> String {
    let mut text: String = COMMANDS.iter().map(|(name, d)| format!("- `/{name}`: {d}\n")).collect();
    if !skills.is_empty() {
        text.push_str(&format!("\n{} skills: `/{}` and more (see `/skills`).", skills.len(), skills[0].skill.name));
    }
    text
}

fn skills_text(skills: &[LoadedSkill]) -> String {
    if skills.is_empty() {
        return "No skills. Put a SKILL.md in ~/.marv/skills/<name>/ (yours) or .marv/skills/<name>/ (this folder's).".into();
    }
    skills.iter().map(|s| format!("- `/{}`: {}\n", s.skill.name, s.skill.description)).collect()
}

async fn open(
    spec: &ChatSpec,
    approver: Arc<WebApprover>,
    out: UnboundedSender<Value>,
    mesh: Option<marv_session::mesh::MeshHandle>,
) -> Result<MarvSession, String> {
    let build = |resume: Option<ResumeTarget>| {
        let mut builder = session_builder(spec, approver.clone());
        if let Some(resume) = resume {
            builder = builder.resume(resume);
        }
        builder.options.mesh = mesh.clone();
        let out = out.clone();
        builder.options.on_warning = Some(Arc::new(move |text: String| {
            let _ = out.send(json!({"type": "notice", "text": marv_core::printable(&text)}));
        }));
        builder.build()
    };
    match build(spec.resume.clone()).await {
        Ok(session) => Ok(session),
        // A session that was never saved (no message sent) or was deleted: start a new one in the same folder.
        Err(_) if matches!(spec.resume, Some(ResumeTarget::Id(_))) => build(None).await.map_err(|e| e.to_string()),
        Err(e) => Err(e.to_string()),
    }
}

/// The chat's session as `spec` says: saved, and its trajectory written where the config has them on (#204).
fn session_builder(spec: &ChatSpec, approver: Arc<WebApprover>) -> SessionBuilder {
    let mut builder = SessionBuilder::new(&spec.root, provider_option(spec, &spec.model))
        .approve(approver)
        .max_steps(spec.max_steps)
        .yolo(spec.yolo)
        .persist(Store::Default)
        .trajectories(if spec.trajectories { Store::Default } else { Store::Off })
        .processes(spec.processes)
        .sources(&spec.sources);
    // No `sources`: none of the user's skills, memory, MCP servers or instructions from ~/.marv.
    builder.options.config_dir = Some(spec.config_dir.clone());
    builder.options.mcp_skip = spec.mcp_off.clone();
    builder.options.host_commands = spec.host_commands;
    builder
}

/// Sends a turn's events, then its usage.
async fn forward(
    mut events: marv_session::Turn,
    before: marv_session::usage::Totals,
    session: MarvSession,
    out: UnboundedSender<Value>,
) {
    while let Some(event) = events.next().await {
        if let Some(line) = events::to_json(&event) {
            let _ = out.send(line);
        }
    }
    let after = session.usage();
    let _ = out.send(json!({
        "type": "usage",
        "promptTokens": after.totals.prompt_tokens - before.prompt_tokens,
        "cachedTokens": after.totals.cached_tokens - before.cached_tokens,
        "completionTokens": after.totals.completion_tokens - before.completion_tokens,
        "cost": after.totals.cost.unwrap_or(0.0) - before.cost.unwrap_or(0.0),
        "context": after.last.map(|u| u.prompt_tokens),
        "contextLength": after.context_length,
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> ChatSpec {
        ChatSpec {
            root: PathBuf::from("/tmp"),
            config_dir: PathBuf::from("/tmp/.marv"),
            api_key: "sk-or".into(),
            zai_api_key: Some("zai-key".into()),
            chatgpt_auth: Some(PathBuf::from("/tmp/.marv/chatgpt.json")),
            model: String::new(),
            max_steps: 0,
            yolo: false,
            resume: None,
            processes: false,
            sources: Vec::new(),
            mesh: false,
            mcp_off: Vec::new(),
            manage_mcp: false,
            host_commands: false,
            trajectories: true,
        }
    }

    /// #204: desktop chats write trajectories as the TUI and `marv -p` do, unless the config turns them off.
    #[test]
    fn the_session_writes_trajectories_as_the_config_says() {
        let approver = || Arc::new(WebApprover::new(tokio::sync::mpsc::unbounded_channel().0));
        let on = session_builder(&spec(), approver());
        assert!(matches!(on.options.trajectories, Store::Default));
        assert!(matches!(on.options.persist, Store::Default));
        let off = session_builder(&ChatSpec { trajectories: false, ..spec() }, approver());
        assert!(matches!(off.options.trajectories, Store::Off));

        let dir = PathBuf::from(format!("/tmp/mx-web-traj-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(trajectories_on(&dir), "on by default, with no config file");
        let store = marv_session::config::ConfigStore::new(&dir);
        let config = marv_session::config::FileConfig { trajectories: Some(false), ..Default::default() };
        store.save(&config).unwrap();
        assert!(!trajectories_on(&dir));
        store.save(&marv_session::config::FileConfig { trajectories: Some(true), ..config }).unwrap();
        assert!(trajectories_on(&dir));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// #159: the model id's prefix picks the provider, and each gets only its own credential.
    #[test]
    fn the_prefix_picks_the_provider_and_its_own_credential() {
        let s = spec();
        assert!(matches!(provider_option(&s, "zai/glm-5.3"),
            ProviderOption::Zai { ref api_key, ref model, .. } if api_key == "zai-key" && model.as_deref() == Some("glm-5.3")));
        assert!(matches!(provider_option(&s, "chatgpt/gpt-6.1-sol"),
            ProviderOption::ChatGpt { ref model, ref auth_file } if model == "gpt-6.1-sol" && auth_file.ends_with("chatgpt.json")));
        assert!(
            matches!(provider_option(&s, "z-ai/glm-5.3"),
            ProviderOption::OpenRouter { ref api_key, .. } if api_key == "sk-or"),
            "OpenRouter's z-ai/ models stay OpenRouter's"
        );
        assert!(matches!(provider_option(&s, "ollama/qwen3:14b"), ProviderOption::Ollama { .. }));
        let none = ChatSpec { zai_api_key: None, ..spec() };
        assert!(
            matches!(provider_option(&none, "zai/glm-5.3"), ProviderOption::Zai { ref api_key, .. } if api_key.is_empty()),
            "never the OpenRouter key"
        );
    }

    #[test]
    fn a_refused_runtime_dir_override_is_a_notice_that_says_why() {
        // #168: the desktop said nothing when MARV_RUNTIME_DIR was refused; the CLI, -p and the TUI say why.
        let base = PathBuf::from(format!("/tmp/mx-web-rt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("proj")).unwrap();
        let env = |dir: &std::path::Path| {
            [("MARV_RUNTIME_DIR".to_string(), dir.display().to_string())]
                .into_iter()
                .collect::<std::collections::BTreeMap<_, _>>()
        };
        let text = mesh_refused_notice(&env(&base.join("proj/marv")), &base.join("proj")).unwrap();
        assert!(text.starts_with("Not on the mesh: MARV_RUNTIME_DIR") && text.contains("inside the project"), "{text}");
        let text = mesh_refused_notice(&env(std::path::Path::new("rel/marv")), &base.join("proj")).unwrap();
        assert!(text.contains("isn't an absolute path"), "{text}");
        // Accepted, or not set: nothing to say.
        assert_eq!(mesh_refused_notice(&env(&base.join("marv")), &base.join("proj")), None);
        assert_eq!(mesh_refused_notice(&Default::default(), &base.join("proj")), None);
        let _ = std::fs::remove_dir_all(&base);
    }
}
