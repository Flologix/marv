//! Marv desktop. The user picks a root folder once; the window shows its tree, and a chat runs Marv on any folder
//! in it (Marv reads and changes what's below that folder, and asks before anything else). Chats are driven by
//! `marv_web::host`, the same code marv-web runs for a browser: the page sends the same messages and gets the same
//! events, through Tauri commands and a channel instead of a WebSocket.

mod background;
mod store;
mod tree;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use background::LiveChat;
use marv_web::host::{ChatSpec, run_chat};
use marv_web::protocol::{ClientMessage, is_model_id};
use serde::Serialize;
use serde_json::Value;
use store::{Chat, Store};
use tauri::ipc::Channel;
use tauri::{AppHandle, Manager, State};
use tokio::sync::mpsc;

struct Desktop {
    store: Store,
    /// Running chats, shown or not (#155). Removing one ends it (its session closes).
    live: Mutex<HashMap<String, LiveChat>>,
    /// The folder named on the command line (`marv-desktop <dir>`), until the page takes it.
    launch: Mutex<Option<String>>,
    /// MCP servers looked up for adding, by name: what the person saw is exactly what gets written.
    proposals: Mutex<HashMap<String, Value>>,
}

/// The CLI's own folder (~/.marv): memory, MCP servers, logins and saved sessions shared with `marv`.
fn marv_dir() -> PathBuf {
    let env: marv_session::config::Env = std::env::vars().collect();
    marv_session::config::default_config_dir(&env, &marv_session::paths::home_dir())
}

impl Desktop {
    fn root(&self) -> Result<PathBuf, String> {
        self.store.settings().root.ok_or_else(|| "Pick your Marv folder first.".to_string())
    }

    /// The chats, with any saved relative to the root (before folders were full paths) converted and saved back.
    fn chats(&self) -> Vec<Chat> {
        let mut chats = self.store.chats();
        if let Ok(root) = self.root()
            && chats.iter().any(|c| !c.folder.starts_with('/'))
        {
            for chat in chats.iter_mut().filter(|c| !c.folder.starts_with('/')) {
                chat.folder = root.join(&chat.folder).to_string_lossy().trim_end_matches('/').to_string();
            }
            let _ = self.store.save_chats(&chats);
        }
        chats
    }

    /// `rel` (from the page, relative to the top folder) as a full path.
    fn full(&self, rel: &str) -> Result<String, String> {
        Ok(tree::resolve(&self.root()?, rel)?.to_string_lossy().trim_end_matches('/').to_string())
    }

    fn live(&self) -> std::sync::MutexGuard<'_, HashMap<String, LiveChat>> {
        self.live.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The pasted key, else `OPENROUTER_API_KEY`, else the one `marv`'s setup saved.
    fn api_key(&self) -> Option<String> {
        if let Some(key) = self.store.settings().api_key.filter(|k| !k.trim().is_empty()) {
            return Some(key);
        }
        if let Ok(key) = std::env::var("OPENROUTER_API_KEY")
            && !key.trim().is_empty()
        {
            return Some(key.trim().to_string());
        }
        let home = marv_session::paths::home_dir();
        let saved: Value = serde_json::from_str(&std::fs::read_to_string(home.join(".marv/config.json")).ok()?).ok()?;
        saved.get("apiKey")?.as_str().map(str::to_string)
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AppInfo {
    root: Option<String>,
    root_name: Option<String>,
    has_key: bool,
    /// "openrouter" or "ollama", once chosen.
    provider: Option<String>,
    spaces: Vec<store::Space>,
    active: Option<String>,
}

fn info(desktop: &Desktop) -> AppInfo {
    let settings = desktop.store.settings();
    let root = settings.root.clone();
    AppInfo {
        root_name: root.as_ref().and_then(|r| r.file_name()).map(|n| n.to_string_lossy().into_owned()),
        root: root.map(|r| r.to_string_lossy().into_owned()),
        has_key: desktop.api_key().is_some(),
        provider: settings.provider.clone(),
        active: settings.active_space().map(|s| s.id.clone()),
        spaces: settings.spaces,
    }
}

#[tauri::command]
fn app_info(desktop: State<'_, Desktop>) -> AppInfo {
    info(&desktop)
}

/// A root folder: any folder inside the home folder. Not the home folder or above it, since Marv changes files
/// below its folder without asking (dotfiles included, there).
fn check_root(path: &str) -> Result<PathBuf, String> {
    let root = std::fs::canonicalize(path).map_err(|e| format!("Couldn't open {path}: {e}"))?;
    if !root.is_dir() {
        return Err(format!("{path} isn't a folder."));
    }
    if marv_session::paths::home_or_above(&root, &marv_session::paths::home_dir()) {
        return Err(format!(
            "{} is your home folder or above it: Marv changes files in its folder without asking, so pick a folder inside your home folder (like ~/Projects).",
            root.display()
        ));
    }
    Ok(root)
}

fn save_root(desktop: &Desktop, root: PathBuf) -> Result<AppInfo, String> {
    let mut settings = desktop.store.settings();
    settings.set_root(root);
    desktop.store.save_settings(&settings)?;
    Ok(info(desktop))
}

/// A new space on `path` (named `name`), and switch to it.
#[tauri::command]
fn create_space(desktop: State<'_, Desktop>, name: String, path: String) -> Result<AppInfo, String> {
    let root = check_root(&path)?;
    let name: String = name.trim().chars().take(40).collect();
    let name =
        if name.is_empty() { root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default() } else { name };
    let mut settings = desktop.store.settings();
    let id = uuid::Uuid::new_v4().to_string();
    settings.spaces.push(store::Space { id: id.clone(), name, root: root.clone(), apps_off: Vec::new() });
    settings.active = Some(id);
    settings.root = Some(root);
    desktop.store.save_settings(&settings)?;
    Ok(info(&desktop))
}

#[tauri::command]
fn switch_space(desktop: State<'_, Desktop>, id: String) -> Result<AppInfo, String> {
    let mut settings = desktop.store.settings();
    let root = settings.spaces.iter().find(|s| s.id == id).ok_or("That space is gone.")?.root.clone();
    settings.active = Some(id);
    settings.root = Some(root);
    desktop.store.save_settings(&settings)?;
    Ok(info(&desktop))
}

#[tauri::command]
fn rename_space(desktop: State<'_, Desktop>, id: String, name: String) -> Result<AppInfo, String> {
    let mut settings = desktop.store.settings();
    let space = settings.spaces.iter_mut().find(|s| s.id == id).ok_or("That space is gone.")?;
    let name: String = name.trim().chars().take(40).collect();
    if !name.is_empty() {
        space.name = name;
    }
    desktop.store.save_settings(&settings)?;
    Ok(info(&desktop))
}

/// Removes a space (its folder, files and chats stay); not the last one.
#[tauri::command]
fn remove_space(desktop: State<'_, Desktop>, id: String) -> Result<AppInfo, String> {
    let mut settings = desktop.store.settings();
    if settings.spaces.len() <= 1 {
        return Err("That's your only space.".into());
    }
    settings.spaces.retain(|s| s.id != id);
    if settings.active.as_ref() == Some(&id) {
        settings.active = settings.spaces.first().map(|s| s.id.clone());
    }
    desktop.store.save_settings(&settings)?;
    Ok(info(&desktop))
}

/// The apps (MCP servers) switched off in the active space.
#[tauri::command]
fn set_apps_off(desktop: State<'_, Desktop>, off: Vec<String>) -> Result<AppInfo, String> {
    let mut settings = desktop.store.settings();
    settings.active_space_mut().ok_or("Pick a folder first.")?.apps_off = off;
    desktop.store.save_settings(&settings)?;
    Ok(info(&desktop))
}

/// "openrouter" or "ollama": how Marv thinks, chosen at onboarding.
#[tauri::command]
fn set_provider(desktop: State<'_, Desktop>, provider: String) -> Result<AppInfo, String> {
    if provider != "openrouter" && provider != "ollama" {
        return Err(format!("Unknown provider {provider}."));
    }
    let mut settings = desktop.store.settings();
    settings.provider = Some(provider);
    desktop.store.save_settings(&settings)?;
    Ok(info(&desktop))
}

#[derive(Serialize)]
struct LocalModel {
    /// `ollama/<name>`, as a chat's model.
    id: String,
    /// Ollama says it can call tools (Marv needs them).
    tools: bool,
}

/// The models pulled into the local Ollama; an error when it isn't running.
#[tauri::command]
async fn local_models() -> Result<Vec<LocalModel>, String> {
    let models = marv_provider::models::list_models(marv_provider::ProviderKind::Ollama, "http://localhost:11434", None)
        .await
        .map_err(|_| "Ollama isn't running (sudo systemctl enable --now ollama).".to_string())?;
    Ok(models.into_iter().map(|m| LocalModel { id: format!("ollama/{}", m.id), tools: m.tools.unwrap_or(false) }).collect())
}

/// `ollama pull <name>`, its progress lines to `on_progress`.
#[tauri::command]
async fn pull_model(name: String, on_progress: Channel<String>) -> Result<(), String> {
    use tokio::io::AsyncReadExt;
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || "-_.:/".contains(c)) {
        return Err(format!("\"{name}\" isn't a model name."));
    }
    let mut child = tokio::process::Command::new("ollama")
        .args(["pull", &name])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|_| "Ollama isn't installed.".to_string())?;
    // Progress comes on stderr, redrawn with \r.
    let mut err = child.stderr.take().ok_or("no output")?;
    let mut buf = [0u8; 4096];
    let mut line = String::new();
    while let Ok(n) = err.read(&mut buf).await {
        if n == 0 {
            break;
        }
        for c in String::from_utf8_lossy(&buf[..n]).chars() {
            if c == '\r' || c == '\n' {
                let text = marv_core::printable(line.trim());
                if !text.is_empty() {
                    let _ = on_progress.send(text);
                }
                line.clear();
            } else {
                line.push(c);
            }
        }
    }
    let status = child.wait().await.map_err(|e| e.to_string())?;
    if status.success() { Ok(()) } else { Err(format!("ollama pull {name} failed.")) }
}

#[tauri::command]
fn set_root(desktop: State<'_, Desktop>, path: String) -> Result<AppInfo, String> {
    save_root(&desktop, check_root(&path)?)
}

/// Going into a folder (double-click in the tree): it becomes the top folder.
#[tauri::command]
fn enter_folder(desktop: State<'_, Desktop>, rel: String) -> Result<AppInfo, String> {
    let dir = tree::resolve(&desktop.root()?, &rel)?;
    save_root(&desktop, check_root(&dir.to_string_lossy())?)
}

/// The Marv folder's parent becomes the Marv folder (the tree's ↑), where that's allowed.
#[tauri::command]
fn root_up(desktop: State<'_, Desktop>) -> Result<AppInfo, String> {
    let root = desktop.root()?;
    let parent = root.parent().ok_or("There's nothing above this folder.")?;
    save_root(&desktop, check_root(&parent.to_string_lossy())?)
}

#[derive(Serialize)]
struct Launch {
    info: AppInfo,
    /// The folder to select, relative to the root.
    folder: String,
}

/// `marv-desktop <dir>`, once: a folder inside the Marv folder is selected; any other becomes the Marv folder.
#[tauri::command]
fn take_launch(desktop: State<'_, Desktop>) -> Result<Option<Launch>, String> {
    let Some(path) = desktop.launch.lock().unwrap_or_else(|e| e.into_inner()).take() else { return Ok(None) };
    let dir = std::fs::canonicalize(&path).map_err(|e| format!("Couldn't open {path}: {e}"))?;
    if !dir.is_dir() {
        return Err(format!("{path} isn't a folder."));
    }
    if let Some(root) = desktop.store.settings().root
        && let Ok(rel) = dir.strip_prefix(&root)
    {
        return Ok(Some(Launch { info: info(&desktop), folder: rel.to_string_lossy().into_owned() }));
    }
    let info = save_root(&desktop, check_root(&path)?)?;
    Ok(Some(Launch { info, folder: String::new() }))
}

#[tauri::command]
fn set_api_key(desktop: State<'_, Desktop>, key: String) -> Result<AppInfo, String> {
    let mut settings = desktop.store.settings();
    settings.api_key = Some(key.trim().to_string()).filter(|k| !k.is_empty());
    desktop.store.save_settings(&settings)?;
    Ok(info(&desktop))
}

#[tauri::command]
fn list_dir(desktop: State<'_, Desktop>, rel: String) -> Result<Vec<tree::Entry>, String> {
    tree::list(&desktop.root()?, &rel)
}

#[tauri::command]
fn create_folder(desktop: State<'_, Desktop>, parent: String, name: String) -> Result<String, String> {
    tree::create_folder(&desktop.root()?, &parent, &name)
}

#[tauri::command]
fn rename_entry(desktop: State<'_, Desktop>, rel: String, name: String) -> Result<String, String> {
    let (from, mut chats) = (desktop.full(&rel)?, desktop.chats());
    let renamed = tree::rename(&desktop.root()?, &rel, &name)?;
    let to = desktop.full(&renamed)?;
    // Chats in the folder (or below it) follow it.
    for chat in &mut chats {
        if chat.folder == from {
            chat.folder = to.clone();
        } else if let Some(below) = chat.folder.strip_prefix(&format!("{from}/")) {
            chat.folder = format!("{to}/{below}");
        }
    }
    desktop.store.save_chats(&chats)?;
    Ok(renamed)
}

#[tauri::command]
fn trash_entry(desktop: State<'_, Desktop>, rel: String) -> Result<(), String> {
    tree::trash(&desktop.root()?, &rel)
}

/// The chats, newest first: all of them, or one folder's.
#[tauri::command]
fn list_chats(desktop: State<'_, Desktop>, folder: Option<String>) -> Vec<Chat> {
    let folder = folder.and_then(|f| desktop.full(&f).ok());
    let mut chats: Vec<Chat> = desktop.chats().into_iter().filter(|c| folder.as_ref().is_none_or(|f| &c.folder == f)).collect();
    chats.sort_by_key(|c| std::cmp::Reverse(c.created));
    chats
}

#[tauri::command]
fn rename_chat(desktop: State<'_, Desktop>, id: String, title: String) -> Result<(), String> {
    let title: String = title.trim().chars().take(80).collect();
    desktop.store.update_chat(&id, |c| c.title = title).map(|_| ())
}

#[tauri::command]
fn delete_chat(desktop: State<'_, Desktop>, id: String) -> Result<(), String> {
    desktop.live().remove(&id);
    let chats: Vec<Chat> = desktop.chats().into_iter().filter(|c| c.id != id).collect();
    desktop.store.save_chats(&chats)
}

/// A new chat in `folder` (relative to the top folder), titled from its first message.
#[tauri::command]
fn create_chat(desktop: State<'_, Desktop>, folder: String, title: String) -> Result<Chat, String> {
    let full = desktop.full(&folder)?;
    if !std::path::Path::new(&full).is_dir() {
        return Err(format!("{folder} isn't a folder."));
    }
    let folder = full;
    let chat = Chat {
        id: uuid::Uuid::new_v4().to_string(),
        folder,
        title: title.trim().chars().take(80).collect(),
        session_id: None,
        created: SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0),
    };
    let mut chats = desktop.chats();
    chats.push(chat.clone());
    desktop.store.save_chats(&chats)?;
    Ok(chat)
}

/// Opens a chat and starts its session (continuing the saved one); its events go to `on_event`.
#[tauri::command]
#[allow(clippy::too_many_arguments)] // A Tauri command's arguments are the page's named fields.
fn open_chat(
    app: AppHandle,
    desktop: State<'_, Desktop>,
    id: String,
    model: String,
    max_steps: u32,
    yolo: bool,
    mcp_off: Vec<String>,
    generation: Option<u32>,
    after: Option<u64>,
    on_event: Channel<Value>,
) -> Result<Chat, String> {
    // A local (Ollama) model needs no key.
    let api_key = match desktop.api_key() {
        Some(key) => key,
        None if marv_web::host::is_local(&model) => String::new(),
        None => return Err("Add an OpenRouter key first, or pick a local model (ollama/…).".into()),
    };
    if !is_model_id(&model) {
        return Err(format!("\"{model}\" isn't an OpenRouter model id."));
    }
    let chat = desktop.chats().into_iter().find(|c| c.id == id).ok_or("That chat is gone.")?;
    let sink: background::Sink = Box::new(move |event| on_event.send(event).is_ok());
    let mut off = mcp_off.clone();
    off.sort();
    let key = format!("{}|{}", off.join(","), generation.unwrap_or(0));
    // Still running (in the background, or another view of it): show it again, with the settings picked now.
    {
        let mut live = desktop.live();
        if let Some(running) = live.get_mut(&chat.id) {
            if running.key == key && running.running() {
                running.seen = std::time::Instant::now();
                let configure =
                    ClientMessage::Configure { model: Some(model), max_steps: Some(max_steps.min(500)), yolo: Some(yolo) };
                let _ = running.to_chat.send(configure);
                running.shared().attach(sink, after.unwrap_or(0));
                return Ok(chat);
            }
            // Its servers changed (or it ended): started again below, continuing the conversation.
            live.remove(&chat.id);
        }
        if let Some(oldest) = background::to_evict(&live, &chat.id) {
            live.remove(&oldest);
        }
    }
    // Wherever the top folder is now: the chat runs in its own folder (still never home or above).
    let dir = check_root(&chat.folder).map_err(|e| format!("This chat's folder: {e}"))?;
    let spec = ChatSpec {
        root: dir,
        // The CLI's own folder (~/.marv): the same memory, MCP servers and saved sessions as `marv`.
        config_dir: marv_dir(),
        api_key,
        model,
        max_steps: max_steps.min(500),
        yolo,
        resume: chat.session_id.clone().map(marv_session::ResumeTarget::Id),
        // The person is at this machine: a dev server's port opens on their own 127.0.0.1.
        processes: true,
        // As the CLI: your skills, memory, MCP servers and instructions, and the folder's own.
        sources: vec![marv_session::sources::Source::User, marv_session::sources::Source::Project],
        // On the mesh like a terminal Marv: other Marvs (desktop chats, Herdr panes) see it and can be wired to it.
        mesh: true,
        mcp_off,
        manage_mcp: true,
        host_commands: true,
    };
    let (to_chat, from_page) = mpsc::unbounded_channel();
    let (out, mut events) = mpsc::unbounded_channel::<Value>();
    let shared = std::sync::Arc::new(std::sync::Mutex::new(background::Shared::default()));
    background::lock(&shared).attach(sink, 0);
    let running = LiveChat { to_chat, key, shared: shared.clone(), seen: std::time::Instant::now() };
    desktop.live().insert(chat.id.clone(), running);
    tauri::async_runtime::spawn(run_chat(spec, from_page, out));
    let chat_id = chat.id.clone();
    // Every event, shown or kept for later, until the session ends (delete, stop, a newer start, the app quitting).
    tauri::async_runtime::spawn(async move {
        while let Some(event) = events.recv().await {
            // Remember the session, so reopening the chat continues it.
            if event["type"] == "ready"
                && let Some(session) = event["sessionId"].as_str()
            {
                let session = session.to_string();
                let _ = app.state::<Desktop>().store.update_chat(&chat_id, |c| c.session_id = Some(session));
            }
            background::lock(&shared).deliver(event);
        }
        background::lock(&shared).end();
    });
    Ok(chat)
}

#[derive(Serialize)]
struct ServerEntry {
    name: String,
    /// "global" (~/.marv/mcp.json, every folder) or "folder" (the folder's .mcp.json, once trusted).
    scope: &'static str,
    /// The config as written (`${VAR}`s unexpanded, so no secrets).
    target: String,
}

/// The MCP servers a chat in `folder` gets, from the config files (whether or not a chat runs them).
#[tauri::command]
fn mcp_servers(desktop: State<'_, Desktop>, folder: String) -> Result<Vec<ServerEntry>, String> {
    let dir = tree::resolve(&desktop.root()?, &folder)?;
    let env: marv_mcp::Env = std::env::vars().collect();
    let include = marv_mcp::config::Include { personal: true, project: true };
    let loaded = marv_mcp::config::load_mcp_config(&dir, &marv_dir(), &env, include);
    Ok(loaded
        .servers
        .into_iter()
        .map(|s| ServerEntry {
            scope: if s.source == marv_mcp::config::ServerSource::Project { "folder" } else { "global" },
            name: s.name,
            target: marv_core::printable(&s.display),
        })
        .collect())
}

/// `/mcp add`'s lookup: an npm package, a URL or a GitHub repo becomes the exact entry to write, shown to the person
/// first (with what was found about it). Several servers in one README: the choices.
#[tauri::command]
async fn mcp_lookup(
    desktop: State<'_, Desktop>,
    source: String,
    name: Option<String>,
    // Credentials the server reads: written as `${VAR}`, filled in by `mcp_install`.
    vars: Option<Vec<String>>,
    headers: Option<std::collections::BTreeMap<String, String>>,
) -> Result<Value, String> {
    use marv_mcp::install::{AddOptions, Resolved, resolve_add};
    let env: marv_session::config::Env = std::env::vars().collect();
    let options = AddOptions {
        source: source.trim().to_string(),
        name: name.filter(|n| !n.trim().is_empty()),
        env: vars.unwrap_or_default(),
        headers: headers.unwrap_or_default(),
        ..AddOptions::default()
    };
    let deps = marv_session::mcp_install::LiveDeps::new(env.clone(), tokio_util::sync::CancellationToken::new());
    let proposal = match resolve_add(&options, &deps).await.map_err(|e| e.to_string())? {
        Resolved::Choices(c) => return Ok(serde_json::json!({ "choices": c.choices })),
        Resolved::Proposal(p) => p,
    };
    if marv_mcp::personal_file::personal_server_names(&marv_dir()).contains(&proposal.name) {
        return Err(format!("You already have a server named \"{}\": give this one another name.", proposal.name));
    }
    let (approval, json) = marv_mcp::install::proposal::approval_for(&proposal, &|v| env.contains_key(v))?;
    let entry: Value = serde_json::from_str(&json).map_err(|e| e.to_string())?;
    desktop.proposals.lock().unwrap_or_else(|e| e.into_inner()).insert(proposal.name.clone(), entry);
    Ok(serde_json::json!({
        "name": proposal.name,
        "title": approval.title,
        "command": approval.command,
        "text": approval.text,
        "warning": approval.warning,
    }))
}

/// Writes a looked-up server to ~/.marv/mcp.json (every folder's): exactly the entry `mcp_lookup` showed, with its
/// `${VAR}`s filled from `values` (the credentials typed at setup). The file is the user's alone (0600) and hidden
/// from Marv's sandbox.
#[tauri::command]
async fn mcp_install(
    desktop: State<'_, Desktop>,
    name: String,
    values: Option<std::collections::HashMap<String, String>>,
) -> Result<(), String> {
    let mut entry =
        desktop.proposals.lock().unwrap_or_else(|e| e.into_inner()).remove(&name).ok_or("Look the server up first.")?;
    fill(&mut entry, &values.unwrap_or_default());
    marv_mcp::personal_file::add_personal_server(&marv_dir(), &name, entry).await.map_err(|e| e.to_string())
}

/// `${VAR}` → its value, in every string of an entry (only the names given; anything else stays a reference).
fn fill(value: &mut Value, values: &std::collections::HashMap<String, String>) {
    match value {
        Value::String(text) => {
            for (name, v) in values {
                *text = text.replace(&format!("${{{name}}}"), v);
            }
        }
        Value::Array(items) => items.iter_mut().for_each(|i| fill(i, values)),
        Value::Object(map) => map.values_mut().for_each(|i| fill(i, values)),
        _ => {}
    }
}

/// Takes one of your servers out of ~/.marv/mcp.json (a folder's are in its .mcp.json).
#[tauri::command]
async fn mcp_remove(name: String) -> Result<(), String> {
    if !marv_mcp::personal_file::personal_server_names(&marv_dir()).contains(&name) {
        return Err(format!("\"{name}\" isn't in ~/.marv/mcp.json: a folder's server is removed from its .mcp.json."));
    }
    marv_mcp::personal_file::remove_personal_server(&marv_dir(), &name).await.map_err(|e| e.to_string())
}

/// An http(s) link in the person's own browser (a setup page for an app's keys).
#[tauri::command]
fn open_url(url: String) -> Result<(), String> {
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return Err("Only web links open here.".into());
    }
    std::process::Command::new("xdg-open").arg(&url).spawn().map(|_| ()).map_err(|e| e.to_string())
}

/// Starts or stops dictation with Voxtype (Omarchy's; Super+Ctrl+X does the same): it types into the focused field.
#[tauri::command]
fn dictate() -> Result<(), String> {
    std::process::Command::new("voxtype")
        .args(["record", "toggle"])
        .spawn()
        .map(|_| ())
        .map_err(|_| "Dictation needs Voxtype (on Omarchy: Super+Ctrl+X).".to_string())
}

#[tauri::command]
fn chat_message(desktop: State<'_, Desktop>, id: String, message: Value) -> Result<(), String> {
    let message: ClientMessage = serde_json::from_value(message).map_err(|e| e.to_string())?;
    let live = desktop.live();
    let chat = live.get(&id).ok_or("This chat isn't open.")?;
    chat.to_chat.send(message).map_err(|_| "This chat has closed.".to_string())
}

/// The page stopped showing the chat: it keeps running (on the mesh, its turn going on) and keeps what happens.
#[tauri::command]
fn close_chat(desktop: State<'_, Desktop>, id: String) {
    if let Some(chat) = desktop.live().get_mut(&id) {
        chat.shared().detach();
        chat.seen = std::time::Instant::now();
    }
}

/// Ends a running chat: it leaves the mesh. Opening it again continues the conversation.
#[tauri::command]
fn stop_chat(desktop: State<'_, Desktop>, id: String) {
    desktop.live().remove(&id);
}

/// The running chats, for the sidebar: busy, and how many approvals wait.
#[tauri::command]
fn live_chats(desktop: State<'_, Desktop>) -> Vec<background::LiveState> {
    background::states(&desktop.live())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            let config = app.path().app_config_dir()?;
            let data = app.path().app_data_dir()?;
            // The first argument that isn't a flag: `marv-desktop .`, `marv-desktop ~/Projects/site`.
            let launch = std::env::args().skip(1).find(|a| !a.starts_with('-'));
            app.manage(Desktop {
                store: Store::new(config, data),
                live: Mutex::new(HashMap::new()),
                launch: Mutex::new(launch),
                proposals: Mutex::new(HashMap::new()),
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            app_info,
            set_root,
            root_up,
            enter_folder,
            create_space,
            switch_space,
            rename_space,
            remove_space,
            set_apps_off,
            set_provider,
            local_models,
            pull_model,
            mcp_servers,
            mcp_lookup,
            mcp_install,
            mcp_remove,
            dictate,
            open_url,
            take_launch,
            set_api_key,
            list_dir,
            create_folder,
            rename_entry,
            trash_entry,
            list_chats,
            rename_chat,
            delete_chat,
            create_chat,
            open_chat,
            chat_message,
            close_chat,
            stop_chat,
            live_chats,
        ])
        .run(tauri::generate_context!())
        .expect("error while running Marv desktop");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fill_puts_in_only_the_given_credentials() {
        let mut entry = serde_json::json!({
            "command": "uvx", "args": ["mcparmory-google-gmail==1.0.4"],
            "env": { "OAUTH2_CLIENT_ID": "${OAUTH2_CLIENT_ID}", "OTHER": "${OTHER}" },
            "headers": { "Authorization": "Bearer ${GITHUB_TOKEN}" }
        });
        let values = [("OAUTH2_CLIENT_ID", "id.apps"), ("GITHUB_TOKEN", "pat")].map(|(k, v)| (k.to_string(), v.to_string()));
        fill(&mut entry, &values.into_iter().collect());
        assert_eq!(entry["env"]["OAUTH2_CLIENT_ID"], "id.apps");
        assert_eq!(entry["env"]["OTHER"], "${OTHER}");
        assert_eq!(entry["headers"]["Authorization"], "Bearer pat");
    }
}

#[cfg(test)]
mod live {
    /// The gallery's sources resolve (network: `cargo test -p marv-desktop -- --ignored`).
    #[tokio::test]
    #[ignore]
    async fn gallery_sources_resolve() {
        use marv_mcp::install::{AddOptions, Resolved, resolve_add};
        let env: marv_session::config::Env = std::env::vars().collect();
        let deps = marv_session::mcp_install::LiveDeps::new(env, tokio_util::sync::CancellationToken::new());
        let cases = [
            ("pypi:mcparmory-google-gmail", vec!["OAUTH2_CLIENT_ID".to_string()], vec![]),
            ("slack-workspace-mcp-server", vec!["SLACK_BOT_TOKEN".to_string()], vec![]),
            ("pypi:mcp-server-kicad", vec![], vec![]),
            (
                "https://api.githubcopilot.com/mcp/",
                vec![],
                vec![("Authorization".to_string(), "Bearer ${GITHUB_TOKEN}".to_string())],
            ),
        ];
        for (source, env, headers) in cases {
            let options =
                AddOptions { source: source.into(), env, headers: headers.into_iter().collect(), ..AddOptions::default() };
            match resolve_add(&options, &deps).await {
                Ok(Resolved::Proposal(p)) => println!("OK {source} -> {} {}", p.name, serde_json::to_string(&p.entry).unwrap()),
                Ok(Resolved::Choices(c)) => println!("CHOICES {source}: {:?}", c.choices),
                Err(e) => println!("ERR {source}: {e}"),
            }
        }
    }
}
