//! Shared types for every Marv crate: the conversation the model sees, tool calls,
//! the events a provider streams, and token usage. No I/O lives here.
//!
//! Ported from `src/provider/types.ts` and `src/types.ts` in Marv (TypeScript).
//!
//! **Prompt-cache rule (applies to every crate):** the conversation is append-only.
//! Every request must start with the exact bytes of the previous one, so a
//! [`ToolCall::arguments`] is kept as the raw JSON text the model produced and
//! never re-serialized. Compaction is the only deliberate exception.

use serde::{Deserialize, Serialize};

pub mod command;
pub mod printable;
pub use printable::printable;

/// A tool the model asked Marv to run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    /// Pairs the call with its result in the next request. De-duplicated by the
    /// agent loop (Ollama reuses `call_0`).
    pub id: String,
    pub name: String,
    /// The arguments as the raw JSON text the model produced. Kept verbatim so
    /// every later request re-sends exactly the same bytes (prompt cache).
    pub arguments: String,
}

/// Provider-specific reasoning kept on an assistant turn so it can be sent back
/// on later steps (OpenRouter `reasoning_details`, Anthropic thinking blocks).
/// Opaque: stored and re-sent byte for byte. (Marv issue #57.)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Reasoning(pub serde_json::Value);

/// One entry of the conversation the model sees.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "lowercase")]
pub enum ChatTurn {
    User {
        text: String,
    },
    Assistant {
        text: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty", rename = "toolCalls")]
        tool_calls: Vec<ToolCall>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reasoning: Option<Reasoning>,
    },
    Tool {
        #[serde(rename = "callId")]
        call_id: String,
        name: String,
        text: String,
    },
}

/// How a tool is described to the model: what it does and the JSON schema of its input.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

/// Token counts and cost for one request.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    /// Tokens in the prompt we sent (system + tools + conversation).
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    /// Prompt tokens served from the provider's cache, when reported.
    pub cached_tokens: Option<u64>,
    /// USD, when the provider reports it (OpenRouter does).
    pub cost: Option<f64>,
}

impl Usage {
    /// Adds another request's usage to a running total.
    pub fn add(&mut self, other: &Usage) {
        self.prompt_tokens += other.prompt_tokens;
        self.completion_tokens += other.completion_tokens;
        self.cached_tokens = match (self.cached_tokens, other.cached_tokens) {
            (None, None) => None,
            (a, b) => Some(a.unwrap_or(0) + b.unwrap_or(0)),
        };
        self.cost = match (self.cost, other.cost) {
            (None, None) => None,
            (a, b) => Some(a.unwrap_or(0.0) + b.unwrap_or(0.0)),
        };
    }
}

/// What a provider streams. The agent loop and the UI only ever see these, so
/// swapping vendors never touches them.
#[derive(Debug, Clone, PartialEq)]
pub enum ProviderEvent {
    TextDelta(String),
    /// The model's private reasoning (thinking models only).
    ThinkingDelta(String),
    /// A piece of a tool call as it streams, for display only (a long write_file
    /// being written). The call itself still arrives whole as [`ProviderEvent::ToolCall`].
    ToolCallDelta {
        index: usize,
        name: String,
        text: String,
    },
    ToolCall(ToolCall),
    /// Opaque reasoning payload to keep on the assistant turn.
    Reasoning(Reasoning),
    /// One request's usage and how it ended (#90): the vendor's finish reason ("stop",
    /// "tool_calls", "length"…), and, when debug recording was on, the raw response.
    Usage(Usage, ReplyEnd),
    /// `reason` is the vendor's stop reason: "stop", "tool_calls", "length"…
    Done {
        reason: Option<String>,
    },
    Error(String),
    /// The provider is retrying after a dropped connection. `continued`: the partial
    /// reply was kept and the next attempt continues it, instead of starting over (Marv #64).
    /// `cause`: why (trajectories count stalls and degenerate replies, Marv #88, #96).
    Retry {
        attempt: u32,
        max: u32,
        message: String,
        continued: bool,
        cause: RetryCause,
    },
    /// Throw away everything this reply has streamed so far (text, reasoning, calls), with no new attempt after it:
    /// an `Error` follows. Marv #96: the second degenerate reply, which must not end up in the history.
    Discard {
        cause: RetryCause,
    },
}

/// Why a provider retries a request.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum RetryCause {
    /// The connection dropped or failed, the server answered a temporary error, or the stream broke off.
    #[default]
    Dropped,
    /// Marv #88: the stream went silent (no bytes at all) for longer than the idle timeout.
    Stalled,
    /// Marv #96: the reply degenerated (whitespace, or one short piece over and over) and was thrown away;
    /// `discarded` is how many bytes of text and reasoning that attempt had streamed.
    Degenerate { discarded: u64 },
}

/// Whether the model may call tools on a request. `None` is used for the
/// wrap-up reply at the step limit; tools are still sent so the cache holds (Marv #59).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ToolChoice {
    #[default]
    Auto,
    None,
}

/// Why a run of the agent loop ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    End,
    Length,
    Aborted,
    Declined,
    MaxSteps,
    Error,
    Loop,
}

/// How much a tool hands back to the loop.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ToolResult {
    /// What the model reads.
    pub output: String,
    /// One line for the transcript ("42 lines", "exit 0").
    pub summary: Option<String>,
    pub is_error: bool,
}

/// How a request ended (#90), sent with the usage on [`ProviderEvent::Usage`]: the vendor's
/// finish reason always (a reply that "finished" while Marv parsed no tool call out of it is
/// what a diagnostic looks for), and the raw response only when the provider was asked to
/// collect it (`StreamOptions::debug_raw`, `MARV_DEBUG_RAW`): the last attempt's payloads.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ReplyEnd {
    pub finish_reason: Option<String>,
    /// The raw response payloads (debug recording only; the prompt cache and the loop never read it).
    pub raw_response: Option<String>,
}

impl ReplyEnd {
    /// The end every non-debug caller records: just the vendor's finish reason.
    pub fn reason(reason: Option<String>) -> Self {
        Self { finish_reason: reason, raw_response: None }
    }
}

/// How much a tool call can do, which decides approval, yolo and parallelism.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolKind {
    /// Reads only. May run in parallel with other reads (Marv #58).
    Read,
    /// Changes files in the project.
    Write,
    /// Runs something (bash, MCP, a subagent).
    Execute,
    /// Reaches the network or leaves the sandbox: always asks once the session is tainted.
    Egress,
}

/// The narrow, named ways out of the sandbox (Marv Sprint 4), each with its own rule for what it can never do:
/// `Host` is bash with `sandbox: false` (one command on the user's machine, Marv #52); `Github` runs gh with the
/// user's login (#47); `Process` keeps a server running in its own sandbox (#51); `Herdr` drives the Herdr terminal
/// multiplexer Marv runs in from Marv's own process (#74). A call through one carries it on its approval (the prompt
/// shows the door's warning line) and on its result (trajectories name it).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Door {
    Github,
    Process,
    Host,
    Herdr,
}

/// Where a piece of text came from. Text from anything but the user or Marv
/// itself is data, never instructions (see `docs/SECURITY.md`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provenance {
    User,
    Marv,
    Project,
    Web,
    Mcp,
    Mesh,
    Subagent,
    Image,
    /// A session-start hook's output (#108): the user chose the command, not what it prints (`bd ready` lists issue
    /// titles anyone can file). A hook marked `trusted` in the user's config doesn't taint.
    Hook,
    /// What the Herdr door read back (#74): other panes' screens, names and titles, which another agent or program
    /// wrote. Fenced as data and tainting, like a mesh message.
    Herdr,
}

impl Provenance {
    /// Whether text from this source taints the session (exfiltration-capable
    /// calls ask from then on, even in yolo).
    pub fn taints(self) -> bool {
        matches!(self, Self::Web | Self::Mcp | Self::Mesh | Self::Image | Self::Hook | Self::Herdr)
    }
}

/// The most characters a reason for declining a call keeps (#185). The engine cuts there (marv-tools'
/// `declined_because`, the web approver); the TUI's input stops there.
pub const MAX_DECLINE_REASON: usize = 500;

/// A reason's characters on one line (#185): newlines, tabs and other whitespace as spaces, control characters
/// dropped. The TUI's input and the engine read a reason the same way.
pub fn reason_chars(text: &str) -> impl Iterator<Item = char> + '_ {
    text.chars().map(|c| if c.is_whitespace() { ' ' } else { c }).filter(|c| !c.is_control())
}

/// A reason for declining a call as the model reads it: [`reason_chars`], trimmed, at most [`MAX_DECLINE_REASON`].
pub fn decline_reason(text: &str) -> String {
    let flat: String = reason_chars(text).collect();
    flat.trim().chars().take(MAX_DECLINE_REASON).collect()
}

#[derive(Debug, thiserror::Error)]
pub enum MarvError {
    #[error("{0}")]
    Tool(String),
    #[error("provider: {0}")]
    Provider(String),
    #[error("aborted")]
    Aborted,
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

pub type Result<T, E = MarvError> = std::result::Result<T, E>;

/// The tag that fences text quoted as data for a model (`<untrusted-data source=… from=…>`). `marv_session::provenance`
/// builds it for tool results; MCP tool descriptions in Marv's notes use it too ([`untrusted_block`]).
pub const UNTRUSTED_TAG: &str = "untrusted-data";

/// `text` fenced as data: `<untrusted-data source="<source>" from="<from>">`, the text, the closing tag. The text
/// can't close the fence (its closing tags are defused, [`defuse_closing_tag`]) and `source` and `from` can't end
/// their attribute or the tag ([`escape_attribute`]).
pub fn untrusted_block(source: &str, from: &str, text: &str) -> String {
    let (source, from) = (escape_attribute(source), escape_attribute(from));
    let body = defuse_closing_tag(text, UNTRUSTED_TAG);
    format!("<{UNTRUSTED_TAG} source=\"{source}\" from=\"{from}\">\n{body}\n</{UNTRUSTED_TAG}>")
}

/// `value` as it can sit inside a double-quoted attribute: `&`, `"`, `<` and `>` as entities, and line breaks and
/// other control characters as spaces, so a fence's opening tag stays one line that `value` can't close.
pub fn escape_attribute(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            c if c.is_control() => out.push(' '),
            c => out.push(c),
        }
    }
    out
}

/// `text` with every closing tag for `tag` (`</tag`, in any case, spaces allowed after `</`) turned into `<\/…`, so
/// text quoted inside a `<tag>` fence can't close it. `tag` is ASCII.
pub fn defuse_closing_tag(text: &str, tag: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find("</") {
        let after = &rest[i + 2..];
        let name = after.trim_start_matches(|c: char| c.is_ascii_whitespace());
        let closes = name.get(..tag.len()).is_some_and(|n| n.eq_ignore_ascii_case(tag));
        out.push_str(&rest[..i]);
        out.push_str(if closes { "<\\/" } else { "</" });
        rest = after;
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_decline_reason_is_one_trimmed_line_without_controls_and_cut() {
        assert_eq!(super::decline_reason("  use pnpm\n\tnot bun\x1b[2J \r\n"), "use pnpm  not bun[2J");
        assert_eq!(super::decline_reason(&"é".repeat(super::MAX_DECLINE_REASON + 5)).chars().count(), super::MAX_DECLINE_REASON);
    }

    use super::*;

    #[test]
    fn closing_tags_are_defused_in_any_case() {
        let tag = "untrusted-data";
        assert_eq!(defuse_closing_tag("a</untrusted-data>b", tag), "a<\\/untrusted-data>b");
        assert_eq!(defuse_closing_tag("</UNTRUSTED-DATA></Untrusted-Data >", tag), "<\\/UNTRUSTED-DATA><\\/Untrusted-Data >");
        assert_eq!(defuse_closing_tag("</ untrusted-data>", tag), "<\\/ untrusted-data>");
        assert_eq!(defuse_closing_tag("</b> </untrusted <é </", tag), "</b> </untrusted <é </");
        assert_eq!(defuse_closing_tag("</untrusted-datä", tag), "</untrusted-datä");
    }

    #[test]
    fn chat_turn_round_trips_with_marv_ts_field_names() {
        let turn = ChatTurn::Assistant {
            text: "hi".into(),
            tool_calls: vec![ToolCall { id: "c1".into(), name: "read_file".into(), arguments: "{\"path\":\"a\"}".into() }],
            reasoning: None,
        };
        let json = serde_json::to_string(&turn).unwrap();
        assert_eq!(
            json,
            r#"{"role":"assistant","text":"hi","toolCalls":[{"id":"c1","name":"read_file","arguments":"{\"path\":\"a\"}"}]}"#
        );
        assert_eq!(serde_json::from_str::<ChatTurn>(&json).unwrap(), turn);
    }

    #[test]
    fn usage_adds_optional_fields() {
        let mut total = Usage::default();
        total.add(&Usage { prompt_tokens: 10, completion_tokens: 2, cached_tokens: Some(8), cost: None });
        total.add(&Usage { prompt_tokens: 5, completion_tokens: 1, cached_tokens: None, cost: Some(0.01) });
        assert_eq!(total.prompt_tokens, 15);
        assert_eq!(total.cached_tokens, Some(8));
        assert_eq!(total.cost, Some(0.01));
    }
}
