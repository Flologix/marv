//! One WebSocket, one chat. The page says `hello` first (which chat, which model); then `send`, `interrupt`,
//! `approval` and `configure` while the server streams the session's events back.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket};
use futures::{SinkExt, StreamExt};
use marv_provider::ProviderOption;
use marv_session::sdk::Store;
use marv_session::{Configure, MarvSession, ProviderChoice, ResumeTarget, SendOptions, SessionBuilder};
use marv_tools::types::Decision;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::App;
use crate::approver::WebApprover;
use crate::events;

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", rename_all_fields = "camelCase")]
enum ClientMessage {
    Hello { chat_id: String, model: Option<String>, max_steps: Option<u32>, yolo: Option<bool> },
    Send { text: String },
    Interrupt,
    Approval { id: u64, decision: Decision },
    Configure { model: Option<String>, max_steps: Option<u32>, yolo: Option<bool> },
}

fn is_chat_id(id: &str) -> bool {
    id.len() == 36 && id.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
}

/// OpenRouter ids: "vendor/model", with an optional ":variant".
fn is_model_id(id: &str) -> bool {
    id.len() < 200
        && id.contains('/')
        && id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '-' | '_' | '.' | ':'))
}

fn openrouter(app: &App, model: &str) -> ProviderOption {
    ProviderOption::OpenRouter { api_key: app.api_key.clone(), model: Some(model.to_string()), base_url: None }
}

pub async fn run(app: Arc<App>, socket: WebSocket) {
    let (mut sink, mut incoming) = socket.split();
    let (out, mut outgoing) = mpsc::unbounded_channel::<Value>();
    let writer = tokio::spawn(async move {
        while let Some(value) = outgoing.recv().await {
            if sink.send(Message::Text(value.to_string().into())).await.is_err() {
                break;
            }
        }
    });

    let Some((chat_id, model, max_steps, yolo)) = hello(&mut incoming, &out).await else {
        drop(out);
        let _ = writer.await;
        return;
    };
    if !app.live.lock().unwrap_or_else(|e| e.into_inner()).insert(chat_id.clone()) {
        let _ = out.send(json!({"type": "fatal", "message": "This chat is open in another tab.", "retry": true}));
        drop(out);
        let _ = writer.await;
        return;
    }

    let approver = Arc::new(WebApprover::new(out.clone()));
    match open(&app, &chat_id, &model, max_steps, yolo, approver.clone(), out.clone()).await {
        Ok((session, resumed)) => {
            let _ = out.send(json!({"type": "ready", "chatId": chat_id, "model": model, "resumed": resumed}));
            serve(&app, &session, &approver, &mut incoming, &out).await;
            let _ = tokio::time::timeout(Duration::from_secs(3), session.close()).await;
        }
        Err(message) => {
            let _ = out.send(json!({"type": "fatal", "message": message}));
        }
    }
    app.live.lock().unwrap_or_else(|e| e.into_inner()).remove(&chat_id);
    drop(out);
    let _ = writer.await;
}

/// Waits for the page's `hello`.
async fn hello(
    incoming: &mut futures::stream::SplitStream<WebSocket>,
    out: &mpsc::UnboundedSender<Value>,
) -> Option<(String, String, u32, bool)> {
    while let Some(Ok(message)) = incoming.next().await {
        let Message::Text(text) = message else { continue };
        return match serde_json::from_str::<ClientMessage>(&text) {
            Ok(ClientMessage::Hello { chat_id, model, max_steps, yolo }) if is_chat_id(&chat_id) => {
                let model = model.filter(|m| is_model_id(m))?;
                Some((chat_id, model, max_steps.unwrap_or(50).min(500), yolo.unwrap_or(true)))
            }
            _ => {
                let _ = out.send(json!({"type": "fatal", "message": "Expected hello with a chat id and a model."}));
                None
            }
        };
    }
    None
}

/// The chat's session, in its own workspace: a new git repo the first time, the saved conversation after that.
async fn open(
    app: &App,
    chat_id: &str,
    model: &str,
    max_steps: u32,
    yolo: bool,
    approver: Arc<WebApprover>,
    out: mpsc::UnboundedSender<Value>,
) -> Result<(MarvSession, bool), String> {
    let root = app.workspaces().join(chat_id);
    let resumed = root.exists();
    if !resumed {
        new_workspace(&root).map_err(|e| format!("Couldn't make the workspace: {e}"))?;
    }
    let mut builder = SessionBuilder::new(&root, openrouter(app, model))
        .approve(approver)
        .max_steps(max_steps)
        .yolo(yolo)
        .persist(Store::Default)
        // Its ports would be forwarded to the server's 127.0.0.1, not the person's.
        .processes(false);
    if resumed {
        builder = builder.resume(ResumeTarget::Latest);
    }
    // Marv's files (sessions, checkpoints) in the server's data folder; no `sources`, so none of the server user's
    // skills, memory, MCP servers or instructions.
    builder.options.config_dir = Some(app.config_dir());
    builder.options.on_warning = Some(Arc::new(move |text: String| {
        let _ = out.send(json!({"type": "notice", "text": marv_core::printable(&text)}));
    }));
    let session = builder.build().await.map_err(|e| e.to_string())?;
    Ok((session, resumed))
}

fn new_workspace(root: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(root)?;
    let status = std::process::Command::new("git").args(["init", "-q"]).current_dir(root).status()?;
    if !status.success() {
        return Err(std::io::Error::other("git init failed"));
    }
    Ok(())
}

async fn serve(
    app: &App,
    session: &MarvSession,
    approver: &Arc<WebApprover>,
    incoming: &mut futures::stream::SplitStream<WebSocket>,
    out: &mpsc::UnboundedSender<Value>,
) {
    let mut turn: Option<(CancellationToken, JoinHandle<()>)> = None;
    while let Some(Ok(message)) = incoming.next().await {
        let Message::Text(text) = message else { continue };
        let Ok(message) = serde_json::from_str::<ClientMessage>(&text) else {
            let _ = out.send(json!({"type": "error", "message": "Unknown message."}));
            continue;
        };
        match message {
            ClientMessage::Send { text } => {
                let text = text.trim();
                if text.is_empty() {
                    continue;
                }
                let cancel = CancellationToken::new();
                let before = session.usage().totals;
                match session.send(text, SendOptions { for_model: None, cancel: Some(cancel.clone()) }) {
                    Ok(mut events) => {
                        let (session, out) = (session.clone(), out.clone());
                        let work = tokio::spawn(async move {
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
                        });
                        turn = Some((cancel, work));
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
                    provider: model.as_deref().map(|m| ProviderChoice::Option(openrouter(app, m))),
                    max_steps: max_steps.map(|s| s.min(500)),
                    yolo,
                    ..Configure::default()
                };
                match session.configure(changes) {
                    Ok(()) => {
                        let _ = out.send(json!({"type": "configured", "model": model, "maxSteps": max_steps, "yolo": yolo}));
                    }
                    Err(e) => {
                        let _ = out.send(json!({"type": "error", "message": e.to_string()}));
                    }
                }
            }
            ClientMessage::Hello { .. } => {}
        }
    }
    // The page went away: stop the turn and wait for its cleanup before the session closes.
    if let Some((cancel, work)) = turn {
        cancel.cancel();
        approver.cancel_all();
        let _ = tokio::time::timeout(Duration::from_secs(5), work).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_checked() {
        assert!(is_chat_id("554963ee-fd8e-4bec-a7dc-49051f090acc"));
        assert!(!is_chat_id("../../etc/passwd-aaaaaaaaaaaaaaaaaaaaaaa"));
        assert!(!is_chat_id("short"));
        assert!(is_model_id("z-ai/glm-5.3-flash"));
        assert!(is_model_id("openai/gpt-5.6:free"));
        assert!(!is_model_id("no-vendor"));
        assert!(!is_model_id("a/b c"));
    }

    #[test]
    fn page_messages_parse() {
        let hello: ClientMessage =
            serde_json::from_str(r#"{"type":"hello","chatId":"x","model":"a/b","maxSteps":5,"yolo":false}"#).unwrap();
        assert!(matches!(hello, ClientMessage::Hello { max_steps: Some(5), yolo: Some(false), .. }));
        let answer: ClientMessage = serde_json::from_str(r#"{"type":"approval","id":3,"decision":"always"}"#).unwrap();
        assert!(matches!(answer, ClientMessage::Approval { id: 3, decision: Decision::Always }));
        assert!(serde_json::from_str::<ClientMessage>(r#"{"type":"approval","id":3,"decision":"maybe"}"#).is_err());
        assert!(matches!(serde_json::from_str(r#"{"type":"interrupt"}"#).unwrap(), ClientMessage::Interrupt));
    }
}
