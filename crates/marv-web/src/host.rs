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
    pub api_key: String,
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
}

/// The commands the page offers in its `/` menu (besides the skills), and what they do here.
const COMMANDS: &[(&str, &str)] = &[
    ("help", "Show the commands"),
    ("skills", "List the skills; run one with /<skill> <request>"),
    ("compact", "Summarize the conversation to free up context (/compact <what to keep>)"),
    ("cost", "Tokens used and cost so far"),
    ("memory", "Show what Marv remembers"),
    ("model", "Switch the model (/model <id>)"),
];

fn openrouter(api_key: &str, model: &str) -> ProviderOption {
    ProviderOption::OpenRouter { api_key: api_key.to_string(), model: Some(model.to_string()), base_url: None }
}

/// Runs the chat until `incoming` closes (the page went away). Sends `ready` (with the session's id, for a later
/// resume) or `fatal` first.
pub async fn run_chat(spec: ChatSpec, mut incoming: UnboundedReceiver<ClientMessage>, out: UnboundedSender<Value>) {
    let approver = Arc::new(WebApprover::new(out.clone()));
    let session = match open(&spec, approver.clone(), out.clone()).await {
        Ok(session) => session,
        Err(message) => {
            let _ = out.send(json!({"type": "fatal", "message": message}));
            return;
        }
    };
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
    let _ = out.send(json!({"type": "ready", "sessionId": session.id(), "model": spec.model, "commands": menu}));

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
                let (name, args) = line.trim().trim_start_matches('/').split_once(' ').unwrap_or((line.trim().trim_start_matches('/'), ""));
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
                    "memory" => print(match session.memory() {
                        Some(m) if !m.personal.is_empty() || !m.project.is_empty() => {
                            let list = |items: &[String]| items.iter().map(|i| format!("- {i}")).collect::<Vec<_>>().join("\n");
                            format!("**Personal**\n{}\n\n**This project**\n{}", list(&m.personal), list(&m.project))
                        }
                        _ => "Marv doesn't remember anything yet.".into(),
                    }),
                    "model" if is_model_id(args) => {
                        let changes = Configure {
                            provider: Some(ProviderChoice::Option(openrouter(&spec.api_key, args))),
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
                    "model" => print("Use /model <OpenRouter id>, e.g. /model z-ai/glm-5.3-flash, or the model menu up top.".into()),
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
                approver.cancel_all();
            }
            ClientMessage::Approval { id, decision } => approver.answer(id, decision),
            ClientMessage::Configure { model, max_steps, yolo } => {
                let model = model.filter(|m| is_model_id(m));
                let changes = Configure {
                    provider: model.as_deref().map(|m| ProviderChoice::Option(openrouter(&spec.api_key, m))),
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
            ClientMessage::Hello { .. } => {}
        }
    }
    // The page went away: stop the turn, wait for its cleanup, then close the session.
    if let Some((cancel, work)) = turn {
        cancel.cancel();
        approver.cancel_all();
        let _ = tokio::time::timeout(Duration::from_secs(5), work).await;
    }
    let _ = tokio::time::timeout(Duration::from_secs(3), session.close()).await;
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

async fn open(spec: &ChatSpec, approver: Arc<WebApprover>, out: UnboundedSender<Value>) -> Result<MarvSession, String> {
    let build = |resume: Option<ResumeTarget>| {
        let mut builder = SessionBuilder::new(&spec.root, openrouter(&spec.api_key, &spec.model))
            .approve(approver.clone())
            .max_steps(spec.max_steps)
            .yolo(spec.yolo)
            .persist(Store::Default)
            .processes(spec.processes)
            .sources(&spec.sources);
        if let Some(resume) = resume {
            builder = builder.resume(resume);
        }
        // No `sources`: none of the user's skills, memory, MCP servers or instructions from ~/.marv.
        builder.options.config_dir = Some(spec.config_dir.clone());
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
