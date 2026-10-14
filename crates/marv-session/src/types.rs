//! The transcript the user sees (not what the model sees). Ports Marv's `src/types.ts`.
//!
//! Two separate histories exist: the session's conversation (`ChatTurn`s, what the model sees) and this transcript
//! (what the user saw: welcome, help text, errors, tool entries). Marv's own notices must never leak into the model's
//! context. The transcript lives here, not in marv-tui, because saved sessions hold it (`SavedSession::transcript`)
//! and `-c`/`-r` bring it back; field names are Marv's, so its session files stay readable.

use marv_tools::DiffShown;
use serde::{Deserialize, Serialize};

/// `system` is Marv's own notices (help text, errors), not the model's system prompt; `tool` a tool call entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
    System,
    Tool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolState {
    Running,
    Done,
    Error,
    Declined,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolStatus {
    /// What it was called on, e.g. the file path.
    pub label: String,
    pub status: ToolState,
    /// "42 lines", or the error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// A subagent's finished tool calls (ctrl+o).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub steps: Option<Vec<String>>,
    /// What a file change did, drawn under the entry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff: Option<DiffShown>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NoticeTone {
    Info,
    Warning,
    Error,
}

/// One bullet of a notice: a short headline, then (optionally) indented detail lines.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NoticeItem {
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<Vec<String>>,
}

/// A distinct, colored block of startup and other issues: transcript only, never sent to the model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Notice {
    pub title: String,
    pub tone: NoticeTone,
    pub items: Vec<NoticeItem>,
}

/// One transcript entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Message {
    pub id: u64,
    pub role: Role,
    /// For `tool`: the tool name.
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub markdown: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notice: Option<Notice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<ToolStatus>,
}

impl Message {
    pub fn new(id: u64, role: Role, text: impl Into<String>) -> Self {
        Self { id, role, text: text.into(), is_error: None, markdown: None, notice: None, tool: None }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_keep_marvs_field_names() {
        let mut m = Message::new(1, Role::System, "x");
        m.is_error = Some(true);
        assert_eq!(serde_json::to_string(&m).unwrap(), r#"{"id":1,"role":"system","text":"x","isError":true}"#);
        let back: Message = serde_json::from_str(r#"{"id":2,"role":"user","text":"hi"}"#).unwrap();
        assert_eq!(back, Message::new(2, Role::User, "hi"));
    }
}
