//! What Marv desktop remembers: the user's root folder and key (`settings.json`, 0600, in the app's config folder)
//! and the chats (`chats.json`, in its data folder): which folder each one runs in and the Marv session it
//! continues.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    pub root: Option<PathBuf>,
    /// The OpenRouter key, when the user pasted one here (else `OPENROUTER_API_KEY` or `marv`'s own config).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Chat {
    pub id: String,
    /// Relative to the root; "" is the root itself.
    pub folder: String,
    pub title: String,
    /// Marv's saved session, once the chat has one (`ready`'s `sessionId`).
    #[serde(default)]
    pub session_id: Option<String>,
    /// Seconds since the epoch.
    pub created: u64,
}

fn read<T: for<'de> Deserialize<'de> + Default>(path: &Path) -> T {
    std::fs::read_to_string(path).ok().and_then(|text| serde_json::from_str(&text).ok()).unwrap_or_default()
}

fn write(path: &Path, value: &impl Serialize, private: bool) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let text = serde_json::to_string_pretty(value).map_err(|e| e.to_string())?;
    // Written whole to a temp file and renamed over, so a crash never leaves half a file.
    let temp = path.with_extension("json.tmp");
    std::fs::write(&temp, text).map_err(|e| e.to_string())?;
    if private {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o600)).map_err(|e| e.to_string())?;
    }
    std::fs::rename(&temp, path).map_err(|e| e.to_string())
}

pub struct Store {
    config: PathBuf,
    data: PathBuf,
}

impl Store {
    pub fn new(config: PathBuf, data: PathBuf) -> Self {
        Self { config, data }
    }

    /// Marv's own files (saved sessions, checkpoints): the app's, not the CLI's `~/.marv`.
    pub fn marv_dir(&self) -> PathBuf {
        self.data.join("marv")
    }

    pub fn settings(&self) -> Settings {
        read(&self.config.join("settings.json"))
    }

    pub fn save_settings(&self, settings: &Settings) -> Result<(), String> {
        write(&self.config.join("settings.json"), settings, true)
    }

    pub fn chats(&self) -> Vec<Chat> {
        read(&self.data.join("chats.json"))
    }

    pub fn save_chats(&self, chats: &[Chat]) -> Result<(), String> {
        write(&self.data.join("chats.json"), &chats, false)
    }

    pub fn update_chat(&self, id: &str, change: impl FnOnce(&mut Chat)) -> Result<Option<Chat>, String> {
        let mut chats = self.chats();
        let Some(chat) = chats.iter_mut().find(|c| c.id == id) else { return Ok(None) };
        change(chat);
        let chat = chat.clone();
        self.save_chats(&chats)?;
        Ok(Some(chat))
    }
}
