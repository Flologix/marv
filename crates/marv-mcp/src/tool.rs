//! An MCP server's tool as one of Marv's tools. Ports `src/mcp/tool.ts`.
//!
//! - The model sees it as `mcp__<server>__<tool>` (sanitized, at most 64 chars:
//!   OpenAI-compatible APIs reject longer names; a hash keeps a shortened name
//!   unique), with the server's own description (prefixed with the server's
//!   name) and JSON schema ([`acceptable_schema`]: one tool a provider rejects
//!   fails every request of the session, since the tool list can't change).
//! - MCP servers run outside the sandbox, with the user's permissions and
//!   network, so a call asks first, scoped per tool, and **yolo never skips
//!   it** (no `autoSafe`: nothing contains what it does).
//! - Results: text blocks joined, images/audio *named but not sent* (Marv passes
//!   text only; images are a taint and exfiltration risk, docs/SECURITY.md),
//!   structured content as JSON, capped at 30k chars (head and tail kept).
//! - A server error or a dead connection becomes a tool error; a
//!   [`crate::oauth::LoginNeeded`] passes Marv's own sentence through as is, so
//!   the model tells the user to log in rather than retrying.
//!
//! **Difference from Marv (security fix, 2026-10-12 audit):** a `readOnlyHint`
//! tool is [`ToolKind::Read`] only while the session is untainted. Once web,
//! MCP, mesh or image text has entered the context, *every* MCP call is
//! [`ToolKind::Egress`] and asks even in yolo: a "read-only" tool still carries
//! its arguments to a server outside the sandbox, so an injected model could
//! leak data through them ([`McpTool::kind`]).
//!
//! This crate doesn't see `marv-tools`' `Tool` trait; `marv-session` wraps an
//! [`McpTool`] in it (name, spec, kind, scope, preview, run).

use std::sync::Arc;

use async_trait::async_trait;
use marv_core::{ToolKind, ToolResult, ToolSpec};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tokio_util::sync::CancellationToken;

use crate::config::{McpServerConfig, ServerSource};
use crate::text::{sha256_hex, truncate};
use crate::McpError;

/// Tool names longer than this are rejected by OpenAI-compatible APIs.
pub const MAX_NAME: usize = 64;
pub const MAX_DESCRIPTION: usize = 2000;
/// A result is capped at this many chars: the first third and last two thirds are kept.
pub const MAX_OUTPUT: usize = 30_000;
/// The approval shows at most this much of the arguments.
pub const PREVIEW_CHARS: usize = 1500;

/// What a server says about one of its tools (`tools/list`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpToolInfo {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub input_schema: Map<String, Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotations: Option<McpToolAnnotations>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpToolAnnotations {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_only_hint: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

/// One content block of a `tools/call` result (loose: unknown types are named).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContentBlock {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    /// Base64 (image/audio).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uri: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource: Option<EmbeddedResource>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct EmbeddedResource {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uri: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

/// A `tools/call` result.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpCallResult {
    #[serde(default)]
    pub content: Vec<ContentBlock>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub structured_content: Option<Value>,
    #[serde(default)]
    pub is_error: bool,
}

/// How a tool reaches its server: the manager's connection, by the server's own tool name.
#[async_trait]
pub trait McpCaller: Send + Sync {
    async fn call(&self, tool: &str, args: Map<String, Value>, cancel: CancellationToken) -> crate::Result<McpCallResult>;
}

/// Only characters every provider accepts.
fn clean(text: &str) -> String {
    text.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' }).collect()
}

/// `mcp__<server>__<tool>`, only characters every provider accepts, at most 64
/// long (a hash of the original keeps a shortened or cleaned name unique).
pub fn mcp_tool_name(server: &str, tool: &str) -> String {
    let cleaned = clean(tool);
    let name = format!("mcp__{server}__{cleaned}");
    if name.len() <= MAX_NAME && cleaned == tool {
        return name;
    }
    let hash = &sha256_hex(tool.as_bytes())[..8];
    format!("{}_{hash}", &name[..name.len().min(MAX_NAME - 9)])
}

/// The model reads text: images and other binary blocks are named, not sent.
pub fn format_mcp_result(result: &McpCallResult) -> String {
    let parts: Vec<String> = result
        .content
        .iter()
        .map(|block| match block.kind.as_str() {
            "text" => block.text.clone().unwrap_or_default(),
            "image" | "audio" => {
                let kb = (block.data.as_ref().map_or(0, String::len) as f64 * 3.0 / 4.0 / 1024.0).round();
                format!("[{} ({}), {kb} KB, not shown: Marv passes text only]", block.kind, block.mime_type.as_deref().unwrap_or("unknown type"))
            }
            "resource" => {
                let r = block.resource.clone().unwrap_or_default();
                r.text.unwrap_or_else(|| format!("[resource {}]", r.uri.unwrap_or_default()))
            }
            "resource_link" => {
                let named = block.name.as_ref().map(|n| format!(" ({n})")).unwrap_or_default();
                format!("[link: {}{named}]", block.uri.as_deref().unwrap_or_default())
            }
            other => format!("[{other} content, not shown]"),
        })
        .collect();
    let mut text = parts.join("\n");
    if text.is_empty()
        && let Some(s) = &result.structured_content {
            text = serde_json::to_string_pretty(s).unwrap_or_default();
        }
    if text.is_empty() {
        text = "(no output)".into();
    }
    let len = text.chars().count();
    if len > MAX_OUTPUT {
        let head: String = text.chars().take(MAX_OUTPUT / 3).collect();
        let tail: String = text.chars().skip(len - 2 * MAX_OUTPUT / 3).collect();
        text = format!("{head}\n\n… ({} characters omitted) …\n\n{tail}", len - MAX_OUTPUT);
    }
    text
}

/// For the transcript: the first non-empty string argument, or the arguments as JSON, at most 60 chars.
pub fn label_of(args: &Map<String, Value>) -> String {
    let first = args.values().find_map(|v| v.as_str().filter(|s| !s.is_empty()));
    let text = match first {
        Some(s) => s.to_string(),
        None if args.is_empty() => String::new(),
        None => Value::Object(args.clone()).to_string(),
    };
    truncate(&text, 60)
}

/// The server's schema, made acceptable to every provider: `$schema` dropped; a
/// root that isn't a plain object (`anyOf`/`oneOf`/`allOf`, or another type)
/// becomes an open object; there's always a `properties`. The server validates
/// the arguments anyway.
pub fn acceptable_schema(schema: &Map<String, Value>) -> Value {
    let mut rest = schema.clone();
    rest.remove("$schema");
    let ty = rest.get("type");
    let plain = (ty.is_none() || ty == Some(&Value::from("object"))) && !["anyOf", "oneOf", "allOf"].iter().any(|k| rest.contains_key(*k));
    if !plain {
        return serde_json::json!({"type": "object", "properties": {}});
    }
    rest.insert("type".into(), "object".into());
    rest.entry("properties").or_insert_with(|| Value::Object(Map::new()));
    Value::Object(rest)
}

/// What the approval shows for a call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpPreview {
    /// `<server>: <title or tool>`.
    pub title: String,
    /// The arguments as pretty JSON, cut at [`PREVIEW_CHARS`]; none for `{}`.
    pub text: Option<String>,
    /// Where it runs: `MCP server "x" (this project's .mcp.json) · runs outside the sandbox`.
    pub note: String,
}

/// One MCP tool, ready to be wrapped as a Marv `Tool`.
#[derive(Clone)]
pub struct McpTool {
    /// Model-facing: `mcp__<server>__<tool>`.
    pub name: String,
    pub description: String,
    pub spec: ToolSpec,
    pub server: String,
    pub source: ServerSource,
    pub info: McpToolInfo,
    caller: Arc<dyn McpCaller>,
}

impl std::fmt::Debug for McpTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpTool").field("name", &self.name).field("server", &self.server).finish_non_exhaustive()
    }
}

impl McpTool {
    /// The server's own claim, `readOnlyHint`.
    pub fn read_only(&self) -> bool {
        self.info.annotations.as_ref().and_then(|a| a.read_only_hint) == Some(true)
    }

    /// How much this call can do, given whether the session is tainted.
    ///
    /// Untainted: `readOnlyHint` tools are [`ToolKind::Read`] (you chose to run
    /// that server, so its read-only tools run freely), the rest
    /// [`ToolKind::Execute`] (asks). Tainted: always [`ToolKind::Egress`], so it
    /// asks even in yolo: the arguments leave the sandbox.
    pub fn kind(&self, tainted: bool) -> ToolKind {
        match (tainted, self.read_only()) {
            (true, _) => ToolKind::Egress,
            (false, true) => ToolKind::Read,
            (false, false) => ToolKind::Execute,
        }
    }

    /// Always: the server is outside the sandbox.
    pub fn uses_network(&self) -> bool {
        true
    }

    /// Never: yolo doesn't skip an MCP call.
    pub fn auto_safe(&self) -> bool {
        false
    }

    /// The approval scope: per tool (`mcp:<server>:<tool>`), and its description.
    pub fn scope(&self) -> (String, String) {
        (format!("mcp:{}:{}", self.server, self.info.name), format!("{}'s {}", self.server, self.info.name))
    }

    pub fn label(&self, args: &Map<String, Value>) -> String {
        label_of(args)
    }

    pub fn preview(&self, args: &Map<String, Value>) -> McpPreview {
        let json = serde_json::to_string_pretty(&Value::Object(args.clone())).unwrap_or_default();
        let text = if args.is_empty() {
            None
        } else if json.chars().count() > PREVIEW_CHARS {
            Some(format!("{}…", json.chars().take(PREVIEW_CHARS).collect::<String>()))
        } else {
            Some(json)
        };
        let title = self.info.annotations.as_ref().and_then(|a| a.title.clone()).unwrap_or_else(|| self.info.name.clone());
        let whose = if self.source == ServerSource::Project { "this project's .mcp.json" } else { "yours" };
        McpPreview { title: format!("{}: {title}", self.server), text, note: format!("MCP server \"{}\" ({whose}) · runs outside the sandbox", self.server) }
    }

    /// Runs the call. `Err` is the tool error text the model reads.
    pub async fn run(&self, args: Map<String, Value>, cancel: CancellationToken) -> Result<ToolResult, String> {
        let result = match self.caller.call(&self.info.name, args, cancel).await {
            Ok(r) => r,
            // Marv's own sentence, as it is: the model should tell the user to log in, not retry.
            Err(McpError::LoginNeeded(e)) => return Err(e.to_string()),
            Err(e) => return Err(format!("{}'s {} failed: {e}", self.server, self.info.name)),
        };
        let output = format_mcp_result(&result);
        if result.is_error {
            return Ok(ToolResult { output, summary: Some("error".into()), is_error: true });
        }
        let lines = output.split('\n').count();
        Ok(ToolResult { output, summary: Some(format!("{lines} line{}", if lines == 1 { "" } else { "s" })), is_error: false })
    }
}

/// The tool Marv runs, and the spec the model is sent (the server's own schema, made acceptable).
pub fn make_mcp_tool(server: &McpServerConfig, info: McpToolInfo, caller: Arc<dyn McpCaller>) -> McpTool {
    let name = mcp_tool_name(&server.name, &info.name);
    let about = info.description.as_deref().unwrap_or("").trim();
    let about = if about.chars().count() > MAX_DESCRIPTION { format!("{}…", about.chars().take(MAX_DESCRIPTION).collect::<String>()) } else { about.to_string() };
    let description = format!("(MCP server \"{}\") {about}", server.name).trim().to_string();
    let spec = ToolSpec { name: name.clone(), description: description.clone(), parameters: acceptable_schema(&info.input_schema) };
    McpTool { name, description, spec, server: server.name.clone(), source: server.source, info, caller }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn tool_names_are_sanitized_short_and_unique() {
        assert_eq!(mcp_tool_name("gh", "list_issues"), "mcp__gh__list_issues");
        let dotted = mcp_tool_name("gh", "issues.list");
        assert!(dotted.starts_with("mcp__gh__issues_list_") && dotted.len() == "mcp__gh__issues_list_".len() + 8);
        assert_ne!(dotted, mcp_tool_name("gh", "issues/list"));
        let long = mcp_tool_name("server", &"x".repeat(100));
        assert_eq!(long.len(), MAX_NAME);
    }

    #[test]
    fn images_are_named_not_sent_and_output_is_capped() {
        let r = McpCallResult {
            content: vec![
                ContentBlock { kind: "text".into(), text: Some("hi".into()), ..Default::default() },
                ContentBlock { kind: "image".into(), mime_type: Some("image/png".into()), data: Some("A".repeat(4096)), ..Default::default() },
                ContentBlock { kind: "resource_link".into(), uri: Some("file:///a".into()), name: Some("a".into()), ..Default::default() },
                ContentBlock { kind: "weird".into(), ..Default::default() },
            ],
            ..Default::default()
        };
        assert_eq!(format_mcp_result(&r), "hi\n[image (image/png), 3 KB, not shown: Marv passes text only]\n[link: file:///a (a)]\n[weird content, not shown]");
        let big = McpCallResult { content: vec![ContentBlock { kind: "text".into(), text: Some("y".repeat(40_000)), ..Default::default() }], ..Default::default() };
        let out = format_mcp_result(&big);
        assert!(out.contains("(10000 characters omitted)"));
        assert_eq!(out.chars().filter(|&c| c == 'y').count(), MAX_OUTPUT);
        let structured = McpCallResult { structured_content: Some(json!({"a": 1})), ..Default::default() };
        assert_eq!(format_mcp_result(&structured), "{\n  \"a\": 1\n}");
        assert_eq!(format_mcp_result(&McpCallResult::default()), "(no output)");
    }

    #[test]
    fn schemas_become_plain_objects() {
        let s = |v: Value| acceptable_schema(v.as_object().unwrap());
        assert_eq!(s(json!({"$schema": "x", "type": "object"})), json!({"type": "object", "properties": {}}));
        assert_eq!(s(json!({"anyOf": [{}]})), json!({"type": "object", "properties": {}}));
        assert_eq!(s(json!({"properties": {"a": {"type": "string"}}, "required": ["a"]})), json!({"type": "object", "properties": {"a": {"type": "string"}}, "required": ["a"]}));
    }

    struct Never;
    #[async_trait]
    impl McpCaller for Never {
        async fn call(&self, _tool: &str, _args: Map<String, Value>, _cancel: CancellationToken) -> crate::Result<McpCallResult> {
            Err(crate::oauth::LoginNeeded { server: "gh".into() }.into())
        }
    }

    fn server() -> McpServerConfig {
        crate::config::parse_server("gh", &json!({"command": "x"}), ServerSource::Project, &|_| None, "f").unwrap()
    }

    #[test]
    fn read_only_hint_is_egress_once_tainted() {
        let info = McpToolInfo { name: "get".into(), description: Some("Gets.".into()), input_schema: Map::new(), annotations: Some(McpToolAnnotations { read_only_hint: Some(true), title: None }) };
        let tool = make_mcp_tool(&server(), info, Arc::new(Never));
        assert_eq!(tool.kind(false), ToolKind::Read);
        assert_eq!(tool.kind(true), ToolKind::Egress);
        assert!(!tool.auto_safe());
        assert_eq!(tool.description, "(MCP server \"gh\") Gets.");
        let plain = make_mcp_tool(&server(), McpToolInfo { name: "put".into(), description: None, input_schema: Map::new(), annotations: None }, Arc::new(Never));
        assert_eq!(plain.kind(false), ToolKind::Execute);
        assert_eq!(plain.description, "(MCP server \"gh\")");
    }

    #[tokio::test]
    async fn login_needed_reaches_the_model_as_marvs_sentence() {
        let tool = make_mcp_tool(&server(), McpToolInfo { name: "put".into(), description: None, input_schema: Map::new(), annotations: None }, Arc::new(Never));
        let err = tool.run(Map::new(), CancellationToken::new()).await.unwrap_err();
        assert_eq!(err, "gh needs a new login: the user can run /mcp login gh");
    }

    #[test]
    fn labels_and_previews() {
        let args = json!({"n": 3, "q": "x".repeat(80)});
        assert_eq!(label_of(args.as_object().unwrap()).chars().count(), 60);
        assert_eq!(label_of(json!({"n": 3}).as_object().unwrap()), "{\"n\":3}");
        let tool = make_mcp_tool(&server(), McpToolInfo { name: "put".into(), description: None, input_schema: Map::new(), annotations: None }, Arc::new(Never));
        let p = tool.preview(&Map::new());
        assert_eq!(p.text, None);
        assert_eq!(p.note, "MCP server \"gh\" (this project's .mcp.json) · runs outside the sandbox");
    }
}
