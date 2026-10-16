//! Drives one chat's session: builds it in the chat's folder, then turns the page's messages into turns,
//! interrupts, approvals and settings, and the session's events into JSON for the page.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use marv_provider::ProviderOption;
use marv_session::sdk::Store;
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
}

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
    let _ = out.send(json!({"type": "ready", "sessionId": session.id(), "model": spec.model}));

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

async fn open(spec: &ChatSpec, approver: Arc<WebApprover>, out: UnboundedSender<Value>) -> Result<MarvSession, String> {
    let build = |resume: Option<ResumeTarget>| {
        let mut builder = SessionBuilder::new(&spec.root, openrouter(&spec.api_key, &spec.model))
            .approve(approver.clone())
            .max_steps(spec.max_steps)
            .yolo(spec.yolo)
            .persist(Store::Default)
            .processes(spec.processes);
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
