//! What the page sends. The events it gets back are [`crate::events`] plus `ready`, `fatal`, `usage` and the
//! approver's `approval` / `approval_done`.

use marv_tools::types::Decision;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", rename_all_fields = "camelCase")]
pub enum ClientMessage {
    /// marv-web's first message: which chat, with what settings. (Marv desktop opens a chat with a command instead.)
    Hello {
        chat_id: String,
        model: Option<String>,
        max_steps: Option<u32>,
        yolo: Option<bool>,
    },
    Send {
        text: String,
    },
    /// A `/command` line, run by Marv (not sent to the model as text): `/help`, `/skills`, `/<skill> request`…
    Command {
        line: String,
    },
    Interrupt,
    /// `decision`: `"yes"`, `"always"`, `"no"`, or `{"declined": "<reason>"}` (#185: not run, the reason goes to the
    /// model and the turn goes on).
    Approval {
        id: u64,
        decision: Decision,
    },
    Configure {
        model: Option<String>,
        max_steps: Option<u32>,
        yolo: Option<bool>,
    },
    /// The chat's MCP servers: `status` (sends `mcp`), `login` / `logout` a remote one (OAuth), `trust` the folder's.
    Mcp {
        action: String,
        name: Option<String>,
    },
}

/// A chat id is a UUID: it names a folder, so nothing else gets through.
pub fn is_chat_id(id: &str) -> bool {
    id.len() == 36 && id.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
}

/// OpenRouter ids: "vendor/model", with an optional ":variant".
pub fn is_model_id(id: &str) -> bool {
    id.len() < 200
        && id.contains('/')
        && id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '-' | '_' | '.' | ':'))
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
        // #185: declined with a reason.
        let reason: ClientMessage =
            serde_json::from_str(r#"{"type":"approval","id":4,"decision":{"declined":"use pnpm"}}"#).unwrap();
        assert!(matches!(reason, ClientMessage::Approval { id: 4, decision: Decision::Declined(r) } if r == "use pnpm"));
        assert!(matches!(serde_json::from_str(r#"{"type":"interrupt"}"#).unwrap(), ClientMessage::Interrupt));
    }
}
