//! marv-web: Marv sessions for a browser. One WebSocket is one chat: the server builds a [`marv_session`] session in
//! that chat's own workspace, streams its events as JSON, and sends each approval to the browser, waiting for the
//! person's answer (yes, always, no) instead of refusing as `marv -p` does.
//!
//! Local only for now: there's no login, so it binds to 127.0.0.1 and only takes WebSockets from the origins in
//! `MARV_WEB_ORIGINS`. Everything runs as the server's user, with the server's OpenRouter key.

mod approver;
mod chat;
mod events;

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::{State, WebSocketUpgrade};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;

pub struct App {
    /// Workspaces (`workspaces/<chat id>`) and Marv's own files (`marv/`: sessions, checkpoints, trajectories).
    pub data: PathBuf,
    pub api_key: String,
    pub default_model: String,
    origins: Vec<String>,
    /// Chats with a connection now: one at a time each, as a session has one turn at a time.
    pub live: Mutex<HashSet<String>>,
}

impl App {
    pub fn workspaces(&self) -> PathBuf {
        self.data.join("workspaces")
    }
    pub fn config_dir(&self) -> PathBuf {
        self.data.join("marv")
    }
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
    upgrade.on_upgrade(move |socket| chat::run(app, socket))
}
