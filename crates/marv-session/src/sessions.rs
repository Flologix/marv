//! Saved sessions: every conversation is kept so it can be picked up again. Ports Marv's `src/sessions.ts`.
//!
//! `~/.marv/sessions/<project>/<id>.json`, 0600 (it holds your code and prompts), written atomically through
//! [`write_private`]. A session holds both histories, kept separate as in the App: the transcript the user saw and the
//! conversation the model saw, plus the model and what it cost. Files are shape-checked on load: an old format or a
//! hand edit is skipped like a damaged file, never a crash. Sessions under the old project key are still found, but
//! only if their recorded `root` matches (the old key collided), and move on their next save.

use std::path::{Path, PathBuf};

use marv_core::ChatTurn;
use marv_fs::{random_hex, write_private};
use marv_mcp::deferred::McpMode;
use serde::{Deserialize, Serialize};

use crate::paths::{legacy_project_key, project_key};
use crate::types::{Message, Role};
use crate::usage::Totals;

pub const VERSION: u32 = 1;
const TITLE_CHARS: usize = 56;

/// A deferred-MCP conversation's listing (#14), saved with it; checked on resume.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpListing {
    pub mode: McpMode,
    pub announced: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SavedSession {
    pub version: u32,
    pub id: String,
    pub root: String,
    pub created_at: u64,
    pub updated_at: u64,
    pub provider: String,
    pub model: String,
    /// What the model sees (with every tool call answered: `answer_all_calls`).
    pub conversation: Vec<ChatTurn>,
    /// What the user saw.
    pub transcript: Vec<Message>,
    pub totals: Totals,
    /// Absent before a turn had MCP tools. A malformed value is dropped on load (`mcp_listing_of`), not fatal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp: Option<serde_json::Value>,
    /// The plan tool's list (#15), when one is set. Kept as JSON and checked on resume (`plan_items`): a bad one is
    /// dropped, not the session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<serde_json::Value>,
    /// The conversation's checkpoint chain (#16): its id (the session's own, or the one a resume started when the old
    /// chain couldn't be read) and, per turn number, the paths its tools reported changing (an undo preview lists
    /// those it can't restore). Absent in older files; checked on resume (`saved_checkpoints`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoints: Option<serde_json::Value>,
    /// A note for the model not yet sent (an undo's, #16): it leads the next user message. Checked on resume.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_note: Option<serde_json::Value>,
    /// marv-rs: what tainted the conversation (web, MCP, mesh, image), restored on resume: the outside text is still
    /// in it. Absent in older files: `taint_sources` then infers it from the conversation's tool calls.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub taint: Option<Vec<marv_core::Provenance>>,
    /// marv-rs #124 (phase 3): radio requests a turn read and hasn't reported on yet; the next turn that reads the
    /// radio reports on them. Absent when there are none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub radio_requests: Option<Vec<marv_mesh::radio::OpenRequest>>,
    /// #63: the last compaction's summary, which the conversation starts with: the next v2 compaction updates it
    /// (rolling) rather than starting over. Absent in older files: the next summary is a full one (nothing is guessed
    /// from the conversation's text).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// #63 review: the summary follows the v2 template, so a v2 compaction may update it (rolling). A v1 summary (or one
    /// from an older file) is summarized from scratch when v2 is switched on.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub summary_v2: bool,
}

/// A saved session's checkpoint chain and its turns' reported paths (#16).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SavedCheckpoints {
    pub chain: String,
    pub reported: Vec<(u32, Vec<String>)>,
}

impl SavedSession {
    /// The taint to restore on resume. Saved files say it; an older file (or one from TypeScript Marv) is judged by
    /// its conversation, erring on the tainted side: a web or github call counts as web content, an MCP, mesh or herdr
    /// call as theirs, and a subagent's report (which could carry either) as web content.
    pub fn taint_sources(&self) -> Vec<marv_core::Provenance> {
        use marv_core::Provenance;
        if let Some(saved) = &self.taint {
            return saved.clone();
        }
        let mut sources = Vec::new();
        for turn in &self.conversation {
            let marv_core::ChatTurn::Tool { name, .. } = turn else { continue };
            let source = match name.as_str() {
                "web_fetch" | "github" | "agent" => Provenance::Web,
                n if n.starts_with("mcp") => Provenance::Mcp,
                n if n.starts_with("mesh") => Provenance::Mesh,
                "herdr" => Provenance::Herdr,
                _ => continue,
            };
            if !sources.contains(&source) {
                sources.push(source);
            }
        }
        sources
    }

    /// The saved plan, or `None` for none, an old file or a hand edit (the plan is dropped, the session still loads).
    pub fn plan_items(&self) -> Option<Vec<marv_tools::PlanItem>> {
        let items: Vec<marv_tools::PlanItem> = serde_json::from_value(self.plan.clone()?).ok()?;
        (!items.is_empty()).then_some(items)
    }

    /// The conversation's MCP listing, or `None` for an old file or a hand edit (then nothing counts as announced).
    pub fn mcp_listing(&self) -> Option<McpListing> {
        self.mcp.as_ref().and_then(|v| serde_json::from_value(v.clone()).ok())
    }

    /// The checkpoint chain and reported paths, or `None` for an old file or a hand edit (a chain id that isn't
    /// `[\w.-]+`, a turn that isn't a positive whole number).
    pub fn saved_checkpoints(&self) -> Option<SavedCheckpoints> {
        let saved: SavedCheckpoints = serde_json::from_value(self.checkpoints.clone()?).ok()?;
        let id_ok =
            !saved.chain.is_empty() && saved.chain.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-');
        (id_ok && saved.reported.iter().all(|(turn, _)| *turn > 0)).then_some(saved)
    }

    /// The undo note not yet sent, or `None` (absent, empty, or not a string).
    pub fn pending_note(&self) -> Option<String> {
        self.pending_note.as_ref().and_then(|v| v.as_str()).filter(|s| !s.is_empty()).map(String::from)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSummary {
    pub id: String,
    pub title: String,
    pub updated_at: u64,
    pub messages: usize,
    pub model: String,
}

/// `2026-10-13T08-05-09-042Z-abc123`: sorts by time, and is a safe file name.
pub fn new_session(root: &Path, provider: &str, model: &str, now_ms: u64) -> SavedSession {
    let id = format!("{}-{}", crate::time::iso(now_ms).replace([':', '.'], "-"), &random_hex()[..6]);
    SavedSession {
        version: VERSION,
        id,
        root: root.to_string_lossy().into_owned(),
        created_at: now_ms,
        updated_at: now_ms,
        provider: provider.into(),
        model: model.into(),
        conversation: Vec::new(),
        transcript: Vec::new(),
        totals: Totals::default(),
        mcp: None,
        plan: None,
        checkpoints: None,
        pending_note: None,
        taint: None,
        radio_requests: None,
        summary: None,
        summary_v2: false,
    }
}

/// The first user message, on one line, at most 56 characters.
pub fn title_of(session: &SavedSession) -> String {
    let first = session.transcript.iter().find(|m| m.role == Role::User).map_or("(empty)", |m| m.text.as_str());
    let line = first.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.chars().count() > TITLE_CHARS {
        format!("{}…", line.chars().take(TITLE_CHARS - 1).collect::<String>().trim_end())
    } else {
        line
    }
}

/// "just now", "5 minutes ago", "yesterday", "3 days ago".
pub fn time_ago(then: u64, now: u64) -> String {
    let minutes = now.saturating_sub(then) / 60_000;
    if minutes < 1 {
        return "just now".into();
    }
    if minutes < 60 {
        return format!("{minutes} minute{} ago", if minutes == 1 { "" } else { "s" });
    }
    let hours = (minutes as f64 / 60.0).round() as u64;
    if hours < 24 {
        return format!("{hours} hour{} ago", if hours == 1 { "" } else { "s" });
    }
    let days = (hours as f64 / 24.0).round() as u64;
    if days == 1 { "yesterday".into() } else { format!("{days} days ago") }
}

#[derive(Debug, Clone)]
pub struct SessionStore {
    pub dir: PathBuf,
}

impl SessionStore {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    fn folder(&self, root: &Path) -> PathBuf {
        self.dir.join(project_key(root))
    }

    /// Shared by colliding projects, so each session's root is checked.
    fn legacy_folder(&self, root: &Path) -> PathBuf {
        self.dir.join(legacy_project_key(root))
    }

    /// Where a session is (or will be, once it has a user message) saved.
    pub fn file(&self, root: &Path, id: &str) -> PathBuf {
        self.folder(root).join(format!("{id}.json"))
    }

    /// Saves (or overwrites) a session. Sessions without a user message aren't saved.
    pub async fn save(&self, session: &SavedSession) -> std::io::Result<()> {
        if !session.transcript.iter().any(|m| m.role == Role::User) {
            return Ok(());
        }
        let root = PathBuf::from(&session.root);
        let stamped = SavedSession { updated_at: crate::time::now_ms(), ..session.clone() };
        write_private(&self.file(&root, &session.id), &serde_json::to_string(&stamped).map_err(std::io::Error::other)?).await?;
        // Saved under the old key before: it lives in the new folder from now on.
        let _ = tokio::fs::remove_file(self.legacy_folder(&root).join(format!("{}.json", session.id))).await;
        Ok(())
    }

    /// Deletes a saved session and its saved pages (#63: a swap folder never outlives its session).
    pub async fn remove(&self, root: &Path, id: &str) -> std::io::Result<()> {
        let file = self.file(root, id);
        let swap = marv_tools::swap::swap_dir(&file);
        if std::fs::symlink_metadata(&swap).is_ok_and(|m| m.is_dir()) {
            tokio::fs::remove_dir_all(&swap).await?;
        }
        match tokio::fs::remove_file(&file).await {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }

    pub async fn load(&self, root: &Path, id: &str) -> Option<SavedSession> {
        match Self::read(&self.file(root, id), root).await {
            Some(s) => Some(s),
            None => Self::read(&self.legacy_folder(root).join(format!("{id}.json")), root).await,
        }
    }

    /// A session file of this project: `None` when missing, damaged, an old format, or another project's.
    async fn read(file: &Path, root: &Path) -> Option<SavedSession> {
        let text = tokio::fs::read_to_string(file).await.ok()?;
        let session: SavedSession = serde_json::from_str(&text).ok()?;
        (session.version == VERSION && Path::new(&session.root) == root).then_some(session)
    }

    /// This project's sessions, newest first (a tie in the same millisecond: the one started later first, #42).
    pub async fn list(&self, root: &Path) -> Vec<SessionSummary> {
        let mut ids = std::collections::BTreeSet::new();
        for folder in [self.folder(root), self.legacy_folder(root)] {
            let Ok(mut entries) = tokio::fs::read_dir(&folder).await else {
                continue;
            };
            while let Ok(Some(entry)) = entries.next_entry().await {
                if let Some(id) = entry.file_name().to_string_lossy().strip_suffix(".json") {
                    ids.insert(id.to_string());
                }
            }
        }
        let mut sessions = Vec::new();
        for id in ids {
            if let Some(s) = self.load(root, &id).await {
                sessions.push(s);
            }
        }
        sessions.sort_by(|a, b| b.updated_at.cmp(&a.updated_at).then(b.created_at.cmp(&a.created_at)));
        sessions
            .iter()
            .map(|s| SessionSummary {
                id: s.id.clone(),
                title: title_of(s),
                updated_at: s.updated_at,
                messages: s.transcript.iter().filter(|m| matches!(m.role, Role::User | Role::Assistant)).count(),
                model: s.model.clone(),
            })
            .collect()
    }

    pub async fn latest(&self, root: &Path) -> Option<SavedSession> {
        let newest = self.list(root).await.into_iter().next()?;
        self.load(root, &newest.id).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_user(root: &Path, text: &str, at: u64) -> SavedSession {
        let mut s = new_session(root, "openrouter", "m", at);
        s.transcript.push(Message::new(1, Role::User, text));
        s
    }

    #[tokio::test]
    async fn saves_lists_and_loads_privately_and_skips_damaged_files() {
        let dir = std::env::temp_dir().join(format!("marv-sessions-{}-{}", std::process::id(), random_hex()));
        let store = SessionStore::new(&dir);
        let root = Path::new("/home/me/proj");
        let s = with_user(root, "fix   the\nbug", 1);
        store.save(&s).await.unwrap();
        store.save(&new_session(root, "openrouter", "m", 2)).await.unwrap(); // nothing in it: not saved
        tokio::fs::write(store.file(root, "broken"), "{").await.unwrap();
        let list = store.list(root).await;
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].title, "fix the bug");
        assert_eq!(store.latest(root).await.unwrap().id, s.id);
        assert!(store.latest(Path::new("/nowhere")).await.is_none());
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(store.file(root, &s.id)).unwrap().permissions().mode() & 0o777, 0o600);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn removing_a_session_removes_its_saved_pages() {
        let dir = std::env::temp_dir().join(format!("marv-sessions-rm-{}-{}", std::process::id(), random_hex()));
        let store = SessionStore::new(&dir);
        let root = Path::new("/tmp/project-rm");
        let session = with_user(root, "hi", 1);
        store.save(&session).await.unwrap();
        let swap = marv_tools::swap::swap_dir(&store.file(root, &session.id));
        std::fs::create_dir_all(&swap).unwrap();
        std::fs::write(swap.join("1.jsonl"), "{}").unwrap();
        store.remove(root, &session.id).await.unwrap();
        assert!(!swap.exists() && !store.file(root, &session.id).exists());
        store.remove(root, &session.id).await.unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn titles_and_times() {
        let s = with_user(Path::new("/p"), &"word ".repeat(30), 0);
        let t = title_of(&s);
        assert!(t.ends_with('…') && t.chars().count() <= 56);
        assert_eq!(time_ago(0, 30_000), "just now");
        assert_eq!(time_ago(0, 60_000), "1 minute ago");
        assert_eq!(time_ago(0, 3 * 3_600_000), "3 hours ago");
        assert_eq!(time_ago(0, 30 * 3_600_000), "yesterday");
        assert_eq!(time_ago(0, 72 * 3_600_000), "3 days ago");
    }

    #[test]
    fn a_saved_plan_is_read_back_and_a_bad_or_empty_one_is_dropped() {
        let mut s = new_session(Path::new("/p"), "x", "y", 1);
        assert_eq!(s.plan_items(), None);
        s.plan = Some(serde_json::json!([{ "text": "a", "status": "in_progress" }]));
        let items = s.plan_items().unwrap();
        assert_eq!(items[0].status, marv_tools::PlanStatus::InProgress);
        s.plan = Some(serde_json::json!([{ "text": "x", "status": "started" }]));
        assert_eq!(s.plan_items(), None);
        s.plan = Some(serde_json::json!([]));
        assert_eq!(s.plan_items(), None);
        // Written only when there is one: older readers never see the key.
        s.plan = None;
        assert!(!serde_json::to_string(&s).unwrap().contains("\"plan\""));
    }

    #[test]
    fn ids_are_file_safe_and_sortable() {
        let s = new_session(Path::new("/p"), "x", "y", 1_791_878_709_042);
        assert!(s.id.starts_with("2026-10-13T08-05-09-042Z-"));
        assert!(!s.id.contains(':'));
    }
}
