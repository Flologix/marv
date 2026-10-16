//! marv-web: Marv chats for a browser, over a WebSocket. One socket is one chat, in its own workspace
//! (`<data>/workspaces/<chat id>`, a fresh git repo, continued on reconnect); [`marv_web::host`] drives it.
//!
//! Local only for now: there's no login, so it binds to 127.0.0.1 and only takes WebSockets from the origins in
//! `MARV_WEB_ORIGINS`. Everything runs as the server's user, with the server's OpenRouter key.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::ws::{Message, WebSocket};
use axum::extract::{State, WebSocketUpgrade};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use futures::{SinkExt, StreamExt};
use marv_web::host::{ChatSpec, run_chat};
use marv_web::protocol::{ClientMessage, is_chat_id, is_model_id};
use serde_json::{Value, json};
use tokio::sync::mpsc;

struct App {
    data: PathBuf,
    api_key: String,
    default_model: String,
    origins: Vec<String>,
    /// Chats with a connection now: one at a time each, as a session has one turn at a time.
    live: Mutex<HashSet<String>>,
}

/// The OpenRouter key: `OPENROUTER_API_KEY`, else (for local use) the one saved by `marv`'s setup.
fn api_key() -> Option<String> {
    if let Ok(key) = std::env::var("OPENROUTER_API_KEY")
        && !key.trim().is_empty()
    {
        return Some(key.trim().to_string());
    }
    let home = marv_session::paths::home_dir();
    let saved: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(home.join(".marv/config.json")).ok()?).ok()?;
    saved.get("apiKey")?.as_str().map(str::to_string)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let env = |name: &str, default: &str| std::env::var(name).unwrap_or_else(|_| default.to_string());
    let Some(api_key) = api_key() else {
        anyhow::bail!("No OpenRouter key: set OPENROUTER_API_KEY (or run marv once to save one).");
    };
    let data = PathBuf::from(env("MARV_WEB_DATA", ".marv-web"));
    std::fs::create_dir_all(data.join("workspaces"))?;
    std::fs::create_dir_all(data.join("marv"))?;
    let app = Arc::new(App {
        data: std::fs::canonicalize(&data)?,
        api_key,
        default_model: env("MARV_WEB_MODEL", "z-ai/glm-5.3-flash"),
        origins: env("MARV_WEB_ORIGINS", "http://localhost:3000,http://127.0.0.1:3000")
            .split(',')
            .map(|o| o.trim().trim_end_matches('/').to_string())
            .filter(|o| !o.is_empty())
            .collect(),
        live: Mutex::new(HashSet::new()),
    });
    let addr = env("MARV_WEB_ADDR", "127.0.0.1:8787");
    let router = Router::new()
        .route("/ws", get(websocket))
        .route("/health", get(|| async { "ok" }))
        .with_state(app.clone());
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    eprintln!("marv-web on ws://{addr}/ws (data in {}, origins: {})", app.data.display(), app.origins.join(", "));
    axum::serve(listener, router).await?;
    Ok(())
}

async fn websocket(State(app): State<Arc<App>>, headers: HeaderMap, upgrade: WebSocketUpgrade) -> Response {
    // A browser always sends Origin: without this check any page the user opens could drive their agent.
    let origin = headers.get(header::ORIGIN).and_then(|o| o.to_str().ok()).unwrap_or("");
    if !app.origins.iter().any(|allowed| allowed == origin) {
        return (StatusCode::FORBIDDEN, "origin not allowed").into_response();
    }
    upgrade.on_upgrade(move |socket| chat(app, socket))
}

fn parse(message: Message) -> Option<ClientMessage> {
    let Message::Text(text) = message else { return None };
    serde_json::from_str(&text).ok()
}

async fn chat(app: Arc<App>, socket: WebSocket) {
    let (mut sink, mut incoming) = socket.split();
    let (out, mut outgoing) = mpsc::unbounded_channel::<Value>();
    let writer = tokio::spawn(async move {
        while let Some(value) = outgoing.recv().await {
            if sink.send(Message::Text(value.to_string().into())).await.is_err() {
                break;
            }
        }
    });
    let finish = |out: mpsc::UnboundedSender<Value>| async move {
        drop(out);
        let _ = writer.await;
    };

    let hello = loop {
        match incoming.next().await {
            Some(Ok(message)) => {
                if let Some(message) = parse(message) {
                    break message;
                }
            }
            _ => return finish(out).await,
        }
    };
    let ClientMessage::Hello { chat_id, model, max_steps, yolo } = hello else {
        let _ = out.send(json!({"type": "fatal", "message": "Expected hello with a chat id and a model."}));
        return finish(out).await;
    };
    let model = model.unwrap_or_else(|| app.default_model.clone());
    if !is_chat_id(&chat_id) || !is_model_id(&model) {
        let _ = out.send(json!({"type": "fatal", "message": "Expected hello with a chat id and a model."}));
        return finish(out).await;
    }
    if !app.live.lock().unwrap_or_else(|e| e.into_inner()).insert(chat_id.clone()) {
        // Often this tab's own last connection, still closing: the page tries again.
        let _ = out.send(json!({"type": "fatal", "message": "This chat is open in another tab.", "retry": true}));
        return finish(out).await;
    }

    let root = app.data.join("workspaces").join(&chat_id);
    let resume = root.exists().then_some(marv_session::ResumeTarget::Latest);
    if resume.is_none()
        && let Err(e) = new_workspace(&root)
    {
        let _ = out.send(json!({"type": "fatal", "message": format!("Couldn't make the workspace: {e}")}));
    } else {
        let spec = ChatSpec {
            root,
            config_dir: app.data.join("marv"),
            api_key: app.api_key.clone(),
            model,
            max_steps: max_steps.unwrap_or(50).min(500),
            yolo: yolo.unwrap_or(true),
            resume,
            // Its ports would be forwarded to the server's 127.0.0.1, not the person's.
            processes: false,
        };
        let (to_chat, from_page) = mpsc::unbounded_channel();
        let reader = async move {
            while let Some(Ok(message)) = incoming.next().await {
                if let Some(message) = parse(message) {
                    let _ = to_chat.send(message);
                }
            }
        };
        tokio::join!(run_chat(spec, from_page, out.clone()), reader);
    }
    app.live.lock().unwrap_or_else(|e| e.into_inner()).remove(&chat_id);
    finish(out).await;
}

fn new_workspace(root: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(root)?;
    let status = std::process::Command::new("git").args(["init", "-q"]).current_dir(root).status()?;
    if !status.success() {
        return Err(std::io::Error::other("git init failed"));
    }
    Ok(())
}
