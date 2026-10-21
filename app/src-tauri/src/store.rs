//! What Marv desktop remembers: the user's root folder and key (`settings.json`, 0600, in the app's config folder)
//! and the chats (`chats.json`, in its data folder): which folder each one runs in and the Marv session it
//! continues.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    /// The active space's top folder (kept equal to it: what the tree and chats use).
    pub root: Option<PathBuf>,
    /// Spaces: each a top folder, its own chats (by folder) and the apps (MCP servers) switched off in it.
    #[serde(default)]
    pub spaces: Vec<Space>,
    #[serde(default)]
    pub active: Option<String>,
    /// "openrouter", "ollama", "zai" or "chatgpt" (#159), as chosen at onboarding.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// The OpenRouter key, when the user pasted one here (else `OPENROUTER_API_KEY` or `marv`'s own config).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    /// #159: the Z.ai Coding Plan key, when the user pasted one here (else `ZAI_API_KEY` or `marv`'s own config).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub zai_api_key: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Space {
    pub id: String,
    pub name: String,
    pub root: PathBuf,
    /// MCP servers switched off in this space, by name (a new server is on everywhere).
    #[serde(default)]
    pub apps_off: Vec<String>,
}

impl Settings {
    /// Settings from before spaces: the root becomes the first space.
    fn migrate(&mut self) {
        if self.spaces.is_empty()
            && let Some(root) = &self.root
        {
            let name = root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "Projects".into());
            self.spaces.push(Space { id: "main".into(), name, root: root.clone(), apps_off: Vec::new() });
            self.active = Some("main".into());
        }
    }

    pub fn active_space(&self) -> Option<&Space> {
        self.spaces.iter().find(|s| Some(&s.id) == self.active.as_ref()).or(self.spaces.first())
    }

    pub fn active_space_mut(&mut self) -> Option<&mut Space> {
        let id = self.active_space()?.id.clone();
        self.spaces.iter_mut().find(|s| s.id == id)
    }

    /// The active space's top folder, now `root`, in both places.
    pub fn set_root(&mut self, root: PathBuf) {
        match self.active_space_mut() {
            Some(space) => space.root = root.clone(),
            None => {
                let name = root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "Projects".into());
                self.spaces.push(Space { id: "main".into(), name, root: root.clone(), apps_off: Vec::new() });
                self.active = Some("main".into());
            }
        }
        self.root = Some(root);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Chat {
    pub id: String,
    /// The folder Marv works in, as a full path: the tree's top folder can move (↑, going into a folder) without
    /// losing a chat's place. (Chats saved before were relative to the root: [`crate::Desktop::chats`] converts them.)
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

    pub fn settings(&self) -> Settings {
        let mut settings: Settings = read(&self.config.join("settings.json"));
        settings.migrate();
        // The active space wins: `root` is only its mirror.
        if let Some(root) = settings.active_space().map(|s| s.root.clone()) {
            settings.root = Some(root);
        }
        settings
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_root_from_before_spaces_becomes_the_first_space() {
        let mut settings = Settings { root: Some("/home/u/Projects".into()), ..Settings::default() };
        settings.migrate();
        assert_eq!(settings.spaces.len(), 1);
        assert_eq!((settings.spaces[0].name.as_str(), settings.active.as_deref()), ("Projects", Some("main")));
        // Moving the top folder moves the active space's.
        settings.set_root("/home/u/Work".into());
        assert_eq!(settings.active_space().unwrap().root, PathBuf::from("/home/u/Work"));
        assert_eq!(settings.root, Some(PathBuf::from("/home/u/Work")));
    }
}
