//! Marv desktop. The user picks a root folder once; the window shows its tree, and a chat runs Marv on any folder
//! in it (Marv reads and changes what's below that folder, and asks before anything else). Chats are driven by
//! `marv_web::host`, the same code marv-web runs for a browser: the page sends the same messages and gets the same
//! events, through Tauri commands and a channel instead of a WebSocket.

mod store;
mod tree;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

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
    /// Open chats: where their page messages go. Dropping one ends the chat (its session closes).
    live: Mutex<HashMap<String, mpsc::UnboundedSender<ClientMessage>>>,
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

    fn live(&self) -> std::sync::MutexGuard<'_, HashMap<String, mpsc::UnboundedSender<ClientMessage>>> {
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
}

fn info(desktop: &Desktop) -> AppInfo {
    let root = desktop.store.settings().root;
    AppInfo {
        root_name: root.as_ref().and_then(|r| r.file_name()).map(|n| n.to_string_lossy().into_owned()),
        root: root.map(|r| r.to_string_lossy().into_owned()),
        has_key: desktop.api_key().is_some(),
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
    settings.root = Some(root);
    desktop.store.save_settings(&settings)?;
    Ok(info(desktop))
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
    };
    let (to_chat, from_page) = mpsc::unbounded_channel();
    let (out, mut events) = mpsc::unbounded_channel::<Value>();
    // Replaces (and so ends) an earlier connection to the same chat.
    desktop.live().insert(chat.id.clone(), to_chat);
    tauri::async_runtime::spawn(run_chat(spec, from_page, out));
    let chat_id = chat.id.clone();
    tauri::async_runtime::spawn(async move {
        while let Some(event) = events.recv().await {
            // Remember the session, so reopening the chat continues it.
            if event["type"] == "ready"
                && let Some(session) = event["sessionId"].as_str()
            {
                let session = session.to_string();
                let _ = app.state::<Desktop>().store.update_chat(&chat_id, |c| c.session_id = Some(session));
            }
            if on_event.send(event).is_err() {
                break;
            }
        }
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
async fn mcp_lookup(desktop: State<'_, Desktop>, source: String, name: Option<String>) -> Result<Value, String> {
    use marv_mcp::install::{AddOptions, Resolved, resolve_add};
    let env: marv_session::config::Env = std::env::vars().collect();
    let options =
        AddOptions { source: source.trim().to_string(), name: name.filter(|n| !n.trim().is_empty()), ..AddOptions::default() };
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

/// Writes a looked-up server to ~/.marv/mcp.json (every folder's): exactly the entry `mcp_lookup` showed.
#[tauri::command]
async fn mcp_install(desktop: State<'_, Desktop>, name: String) -> Result<(), String> {
    let entry = desktop.proposals.lock().unwrap_or_else(|e| e.into_inner()).remove(&name).ok_or("Look the server up first.")?;
    marv_mcp::personal_file::add_personal_server(&marv_dir(), &name, entry).await.map_err(|e| e.to_string())
}

/// Takes one of your servers out of ~/.marv/mcp.json (a folder's are in its .mcp.json).
#[tauri::command]
async fn mcp_remove(name: String) -> Result<(), String> {
    if !marv_mcp::personal_file::personal_server_names(&marv_dir()).contains(&name) {
        return Err(format!("\"{name}\" isn't in ~/.marv/mcp.json: a folder's server is removed from its .mcp.json."));
    }
    marv_mcp::personal_file::remove_personal_server(&marv_dir(), &name).await.map_err(|e| e.to_string())
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
    chat.send(message).map_err(|_| "This chat has closed.".to_string())
}

#[tauri::command]
fn close_chat(desktop: State<'_, Desktop>, id: String) {
    desktop.live().remove(&id);
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
            mcp_servers,
            mcp_lookup,
            mcp_install,
            mcp_remove,
            dictate,
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
        ])
        .run(tauri::generate_context!())
        .expect("error while running Marv desktop");
}
