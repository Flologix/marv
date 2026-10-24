//! Deferred MCP tools (Marv #14). Ports `src/mcp/deferred.ts`.
//!
//! Sending every MCP tool's full spec with every request costs thousands of
//! tokens once a few servers are connected, and gives small models dozens of
//! tools to choose from. Past a threshold ([`DEFER_TOKENS`], 8k in marv-rs, #87), the model is offered two constant
//! tools instead, `tool_search` (a tool's full spec, as a tool result) and
//! `mcp_call` (runs one), and learns the MCP tools' names from a note from Marv
//! at the start of the turn's user message.
//!
//! - **Prompt cache:** the tool list and the system prompt never change, so a
//!   server trusted or added mid-session reaches the model only through an
//!   appended note ([`announcement`]). The mode is decided once per
//!   conversation ([`choose_mode`]) and saved with it; a conversation never
//!   switches. A direct (eager) conversation's list only grows when the user
//!   adds a server mid-session (`/mcp add`, `trust`, `login`), the new tools
//!   after the old ones; it never turns deferred, however big it gets.
//! - `mcp_call` never asks by itself: it hands the call to `run_tool` with the
//!   real MCP tool, so its preview, approval, scope, no-`autoSafe` and label
//!   apply unchanged (and the taint rule: Egress once tainted).
//! - A direct call to a listed `mcp__` tool is rewritten to `mcp_call` before it
//!   enters the history ([`as_mcp_call`]); `arguments` sent as a JSON string are
//!   parsed ([`arguments_as_object`]), each with a `Marv:` note.
//! - Subagents get neither MCP tools nor these two.

use std::collections::HashSet;

use marv_core::{ToolCall, ToolSpec};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::text::{printable, truncate};
use crate::tool::McpTool;

/// The config/SDK setting `mcpTools`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum McpToolsSetting {
    #[default]
    Auto,
    Deferred,
    Eager,
}

/// What a conversation decided (saved with it: `SavedSession.mcp.mode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum McpMode {
    Deferred,
    Eager,
}

/// A listed tool's description, at most.
pub const MAX_LINE: usize = 100;
/// Defer when the MCP specs cost more than this many tokens per request…
///
/// **Difference from Marv (marv-rs #87):** Marv deferred past 4000. The eval tracker (25 tools, 4.7k tokens) was
/// deferred there, and each tool group then cost a `tool_search` round trip (2–3 per task, 5–10 s on a fast model)
/// to save under 4k tokens a request, most of it a cached prefix. Up to 8k, the specs go in directly; past it
/// (170 tools) deferral still pays. `mcpTools: "deferred"` / `"eager"` override it either way.
pub const DEFER_TOKENS: f64 = 8000.0;
/// …or more than this share of the context window, whichever is less.
pub const DEFER_SHARE: f64 = 0.1;
/// Specs `tool_search` returns at most.
pub const MAX_RESULTS: usize = 5;

pub const SEARCH_DESCRIPTION: &str = "Loads MCP tools' parameters: by exact names (from Marv's list of MCP tools in the conversation), or by words. Returns each tool's name, description and JSON schema (at most 5). Then run it with mcp_call.";
pub const CALL_DESCRIPTION: &str = "Runs an MCP tool: tool is its name (mcp__<server>__<tool>), arguments an object matching its parameters. Load them with tool_search first. MCP tools run outside the sandbox; the user may be asked first.";
pub const DEFERRED_NAMES: [&str; 2] = ["tool_search", "mcp_call"];

/// Marv's 4-chars-a-token estimate (`estimateTokens` in `src/usage.ts`): `marv_core::estimate_tokens` (Marv #63).
pub fn estimate_tokens(text: &str) -> usize {
    marv_core::estimate_tokens(text) as usize
}

/// What a deferred conversation is offered in place of the MCP specs: the same bytes all session.
pub fn deferred_specs() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            name: "tool_search".into(),
            description: SEARCH_DESCRIPTION.into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "names": {"type": "array", "items": {"type": "string"}, "description": "Exact tool names (mcp__<server>__<tool>) to load."},
                    "query": {"type": "string", "description": "Words to look for in the tools' names and descriptions."}
                },
                "additionalProperties": false
            }),
        },
        ToolSpec {
            name: "mcp_call".into(),
            description: CALL_DESCRIPTION.into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "tool": {"type": "string", "description": "The MCP tool's name, mcp__<server>__<tool>."},
                    "arguments": {"type": "object", "propertyNames": {"type": "string"}, "additionalProperties": {}, "description": "Its arguments, as an object matching its parameters (see tool_search)."}
                },
                "required": ["tool"],
                "additionalProperties": false
            }),
        },
    ]
}

/// The description `make_mcp_tool` wrote, without its `(MCP server "x")`
/// prefix, as its first sentence on one line, at most [`MAX_LINE`] chars. A
/// sentence ends at `.`, `!` or `?` followed by a space or the end ("v2.1" doesn't end one).
pub fn one_line(description: &str) -> String {
    let text = printable(description);
    let mut about = text.as_str();
    if let Some(rest) = about.strip_prefix("(MCP server \"")
        && let Some(close) = rest.find("\")")
    {
        about = rest[close + 2..].trim_start();
    }
    let about = about.split_whitespace().collect::<Vec<_>>().join(" ");
    let chars: Vec<char> = about.chars().collect();
    let end =
        (0..chars.len()).find(|&i| matches!(chars[i], '.' | '!' | '?') && chars.get(i + 1).is_none_or(|c| c.is_whitespace()));
    let sentence: String = match end {
        Some(i) => chars[..=i].iter().collect(),
        None => about,
    };
    truncate(&sentence, MAX_LINE)
}

/// What Marv tells the model about MCP tools it hasn't heard of, and ones that are gone; `None` when there's nothing to say.
pub fn announcement(added: &[ToolSpec], gone: &[String]) -> Option<String> {
    let mut parts = Vec::new();
    if !added.is_empty() {
        let lines: Vec<String> = added
            .iter()
            .map(|s| {
                let about = one_line(&s.description);
                if about.is_empty() { format!("- {}", s.name) } else { format!("- {}: {about}", s.name) }
            })
            .collect();
        parts.push(format!(
            "Marv: MCP tools you can use (not loaded: call tool_search with their names to see their parameters, then run one with mcp_call):\n{}",
            lines.join("\n")
        ));
    }
    if !gone.is_empty() {
        parts.push(format!("Marv: these MCP tools are no longer available: {}.", gone.join(", ")));
    }
    (!parts.is_empty()).then(|| parts.join("\n\n"))
}

/// The decision, with its numbers (logged as `turn_start.mcpTools`).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModeChoice {
    pub mode: McpMode,
    /// What the MCP specs would add to every request.
    pub spec_tokens: usize,
    /// What deferral costs instead: the two specs plus the note.
    pub deferred_tokens: usize,
    pub count: usize,
}

/// Whether a conversation defers its MCP tools. Tokens, not a tool count:
/// schemas vary 20× in size, and tokens are what costs money and context.
/// Deferral costs a `tool_search` step per tool first used, so `auto` defers
/// only when it saves at least half.
pub fn choose_mode(setting: McpToolsSetting, specs: &[ToolSpec], context_length: Option<u64>) -> ModeChoice {
    let spec_tokens = estimate_tokens(&serde_json::to_string(specs).unwrap_or_default());
    let deferred_tokens = estimate_tokens(&serde_json::to_string(&deferred_specs()).unwrap_or_default())
        + estimate_tokens(&announcement(specs, &[]).unwrap_or_default());
    let limit = context_length.map_or(DEFER_TOKENS, |c| DEFER_TOKENS.min(c as f64 * DEFER_SHARE));
    let defer = match setting {
        McpToolsSetting::Deferred => true,
        McpToolsSetting::Eager => false,
        McpToolsSetting::Auto => spec_tokens as f64 > limit && deferred_tokens * 2 <= spec_tokens,
    };
    ModeChoice { mode: if defer { McpMode::Deferred } else { McpMode::Eager }, spec_tokens, deferred_tokens, count: specs.len() }
}

/// `tool_search`'s input.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SearchInput {
    #[serde(default)]
    pub names: Vec<String>,
    #[serde(default)]
    pub query: Option<String>,
}

/// Exact names first, then the best word matches (no embeddings: names and
/// descriptions are short). Returns (found, at most [`MAX_RESULTS`]; unknown names).
pub fn search_tools<'a>(specs: &'a [ToolSpec], input: &SearchInput) -> (Vec<&'a ToolSpec>, Vec<String>) {
    let mut found: Vec<&ToolSpec> = Vec::new();
    let mut unknown = Vec::new();
    for name in &input.names {
        match specs.iter().find(|s| &s.name == name) {
            None => unknown.push(name.clone()),
            Some(m) if !found.iter().any(|f| std::ptr::eq(*f, m)) => found.push(m),
            Some(_) => {}
        }
    }
    let query = input.query.as_deref().unwrap_or("").to_lowercase();
    let words: Vec<&str> =
        query.split(|c: char| !c.is_ascii_lowercase() && !c.is_ascii_digit()).filter(|w| !w.is_empty()).collect();
    if !words.is_empty() {
        let mut scored: Vec<(usize, usize, &ToolSpec)> = specs
            .iter()
            .enumerate()
            .filter(|(_, s)| !found.iter().any(|f| std::ptr::eq(*f, *s)))
            .map(|(i, s)| {
                let hay = format!("{} {}", s.name, s.description).to_lowercase();
                (words.iter().filter(|w| hay.contains(**w)).count(), i, s)
            })
            .filter(|(score, _, _)| *score > 0)
            .collect();
        scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        found.extend(scored.into_iter().map(|(_, _, s)| s));
    }
    found.truncate(MAX_RESULTS);
    (found, unknown)
}

/// `tool_search`'s output: one compact JSON line per spec (`readOnly` marked),
/// then notes. Marks the found names as `loaded`. `read_only` says whether a
/// tool is `readOnlyHint`.
/// `tool_search`'s answer over the turn's MCP tools: each found tool's name, description and schema (in the
/// server's key order, as Marv printed it), one compact JSON object per line.
pub fn tool_search_output(tools: &[McpTool], input: &SearchInput, loaded: &mut HashSet<String>) -> (String, String) {
    let specs: Vec<ToolSpec> = tools.iter().map(|t| t.spec.clone()).collect();
    let (found, unknown) = search_tools(&specs, input);
    let mut lines: Vec<String> = Vec::new();
    for s in &found {
        loaded.insert(s.name.clone());
        let tool = tools.iter().find(|t| t.name == s.name);
        // Written by hand: Marv's `JSON.stringify` order (name, description, parameters, readOnly).
        let text = |t: &str| Value::from(t).to_string();
        let read_only = if tool.is_some_and(McpTool::read_only) { ",\"readOnly\":true" } else { "" };
        let parameters = tool.map_or_else(|| s.parameters.to_string(), |t| t.parameters_text.clone());
        lines.push(format!(
            "{{\"name\":{},\"description\":{},\"parameters\":{}{read_only}}}",
            text(&s.name),
            text(&s.description),
            parameters
        ));
    }
    if !unknown.is_empty() {
        lines.push(format!("Marv: no MCP tool is named {}.", unknown.join(", ")));
    }
    if found.is_empty() && unknown.is_empty() {
        lines.push("No MCP tool matches. Marv's note in this conversation lists them all.".into());
    }
    let n = found.len();
    (lines.join("\n"), format!("{n} tool{}", if n == 1 { "" } else { "s" }))
}

/// `mcp_call` for an unknown tool: the error, naming the closest.
pub fn unknown_tool_error(specs: &[ToolSpec], tool: &str) -> String {
    let query = SearchInput { names: vec![], query: Some(tool.trim_start_matches("mcp__").to_string()) };
    let close: Vec<&str> = search_tools(specs, &query).0.iter().map(|s| s.name.as_str()).collect();
    let hint = if close.is_empty() { String::new() } else { format!(" Closest: {}.", close.join(", ")) };
    format!("No MCP tool is named \"{tool}\".{hint}")
}

/// What `mcp_call` appends to a rejected call to a tool whose schema the
/// conversation hasn't seen (once: the caller adds the name to `loaded`), so a
/// guess at the arguments costs one step, not two. The call itself goes
/// through `run_tool` with the real tool, which lives in `marv-session`.
pub fn schema_hint(spec: &ToolSpec) -> String {
    format!("\n\nMarv: {} takes: {}", spec.name, spec.parameters)
}

/// Marv #10: `arguments` sent as a JSON string of an object. Certain, so it's
/// read as the object (with a note).
pub fn arguments_as_object(raw: &Value) -> Option<(Value, &'static str)> {
    let text = raw.get("arguments")?.as_str()?;
    let parsed: Value = serde_json::from_str(text).ok()?;
    if !parsed.is_object() {
        return None;
    }
    let mut out = raw.as_object()?.clone();
    out.insert("arguments".into(), parsed);
    Some((
        Value::Object(out),
        "Marv: mcp_call's arguments came as a JSON string; it was read as the object inside. Pass arguments as an object.",
    ))
}

/// Whether `text` is exactly one JSON object. Port note: Marv also accepted
/// what #10's JSON repair (`repairJsonArgs`, `marv-tools`) makes one; pass it in
/// once that crate exposes it.
fn one_object(text: &str) -> bool {
    serde_json::from_str::<Value>(text).is_ok_and(|v| v.is_object())
}

/// Marv #10/#14: a direct call to an MCP tool the conversation defers, as the
/// `mcp_call` it meant. Arguments that are one object are spliced in as they
/// came (for `run_tool`'s repair to see); anything else is passed as a string,
/// which `mcp_call` rejects. Spliced, `{},"tool":"…"` would make a valid
/// `mcp_call` whose second `"tool"` key (JSON keeps the last) names another
/// tool than the one called.
pub fn as_mcp_call(call: &ToolCall, mcp_names: &HashSet<String>) -> Option<(ToolCall, String)> {
    if !mcp_names.contains(&call.name) {
        return None;
    }
    let trimmed = call.arguments.trim();
    let raw = if trimmed.is_empty() { "{}" } else { trimmed };
    let args = if one_object(raw) { raw.to_string() } else { Value::from(raw).to_string() };
    let name = Value::from(call.name.as_str()).to_string();
    Some((
        ToolCall { id: call.id.clone(), name: "mcp_call".into(), arguments: format!("{{\"tool\":{name},\"arguments\":{args}}}") },
        format!(
            "Marv: {} isn't offered as a tool of its own: it was run as mcp_call with tool \"{}\". Call MCP tools that way.",
            call.name, call.name
        ),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn spec(name: &str, description: &str) -> ToolSpec {
        ToolSpec { name: name.into(), description: description.into(), parameters: json!({"type": "object", "properties": {}}) }
    }

    #[test]
    fn one_line_takes_the_first_sentence() {
        assert_eq!(one_line("(MCP server \"gh\") Lists issues. Also PRs."), "Lists issues.");
        assert_eq!(one_line("Uses v2.1 of\n the API"), "Uses v2.1 of the API");
        assert_eq!(one_line(&"x".repeat(150)).chars().count(), MAX_LINE);
    }

    #[test]
    fn announcement_lists_new_and_gone() {
        assert_eq!(announcement(&[], &[]), None);
        let a = announcement(&[spec("mcp__a__b", "(MCP server \"a\") Does b.")], &["mcp__x__y".into()]).unwrap();
        assert!(a.contains("\n- mcp__a__b: Does b.\n\nMarv: these MCP tools are no longer available: mcp__x__y."));
    }

    #[test]
    fn auto_defers_only_big_tool_lists_that_halve() {
        let few = vec![spec("mcp__a__b", "short")];
        assert_eq!(choose_mode(McpToolsSetting::Auto, &few, None).mode, McpMode::Eager);
        assert_eq!(choose_mode(McpToolsSetting::Deferred, &few, None).mode, McpMode::Deferred);
        let big: Vec<ToolSpec> = (0..80)
            .map(|i| ToolSpec {
                name: format!("mcp__s__t{i}"),
                description: "Does a thing.".into(),
                parameters: json!({"type": "object", "properties": {"x": {"type": "string", "description": "y".repeat(400)}}}),
            })
            .collect();
        assert_eq!(choose_mode(McpToolsSetting::Auto, &big, None).mode, McpMode::Deferred);
        assert_eq!(choose_mode(McpToolsSetting::Eager, &big, None).mode, McpMode::Eager);
    }

    /// One tool whose spec list costs exactly `tokens` by [`estimate_tokens`] (the description pads it).
    fn costing(tokens: usize) -> Vec<ToolSpec> {
        let mut specs = vec![spec("mcp__s__t", "")];
        let base = serde_json::to_string(&specs).unwrap().chars().count();
        specs[0].description = "x".repeat(tokens * 4 - base);
        assert_eq!(estimate_tokens(&serde_json::to_string(&specs).unwrap()), tokens);
        specs
    }

    /// marv-rs #87: up to 8k tokens of specs are offered directly (the 25-tool tracker, ~4.7k, was deferred under
    /// Marv's 4000), past it deferred; a small context window still defers sooner (10% of it).
    #[test]
    fn auto_offers_up_to_8k_tokens_directly() {
        assert_eq!(DEFER_TOKENS, 8000.0);
        let mode = |tokens, context| choose_mode(McpToolsSetting::Auto, &costing(tokens), context).mode;
        assert_eq!(mode(4700, None), McpMode::Eager);
        assert_eq!(mode(8000, None), McpMode::Eager);
        assert_eq!(mode(8001, None), McpMode::Deferred);
        assert_eq!(mode(8000, Some(1_000_000)), McpMode::Eager);
        assert_eq!(mode(8001, Some(1_000_000)), McpMode::Deferred);
        assert_eq!(mode(4700, Some(32_000)), McpMode::Deferred);
        assert_eq!(mode(3200, Some(32_000)), McpMode::Eager);
    }

    /// `gh`'s tools, each with a schema whose keys the server sent out of alphabetical order.
    fn tools_of(names: &[(&str, bool)]) -> Vec<McpTool> {
        use crate::tool::{McpCallResult, McpCaller, McpToolInfo, make_mcp_tool};
        struct Never;
        #[async_trait::async_trait]
        impl McpCaller for Never {
            async fn call(
                &self,
                _: &str,
                _: serde_json::Map<String, Value>,
                _: tokio_util::sync::CancellationToken,
            ) -> crate::Result<McpCallResult> {
                Err(crate::McpError::Server("never".into()))
            }
        }
        let server = crate::config::parse_server(
            "gh",
            &serde_json::json!({"command": "x"}),
            crate::config::ServerSource::Personal,
            &|_| None,
            "f",
        )
        .unwrap();
        let raw = r#"{"type":"object","properties":{"zeta":{"type":"string"},"alpha":{"type":"number"}},"required":["zeta"]}"#;
        names
            .iter()
            .map(|(name, read_only)| {
                let mut info: McpToolInfo = serde_json::from_value(serde_json::json!({
                    "name": name, "description": "List things", "inputSchema": serde_json::from_str::<Value>(raw).unwrap(),
                }))
                .unwrap();
                if *read_only {
                    info.annotations = Some(crate::tool::McpToolAnnotations { read_only_hint: Some(true), title: None });
                }
                make_mcp_tool(&server, info, std::sync::Arc::new(Never)).with_schema_order(&serde_json::from_str(raw).unwrap())
            })
            .collect()
    }

    #[test]
    fn tool_search_shows_the_schema_in_the_servers_order() {
        let tools = tools_of(&[("prs", false)]);
        let (out, _) =
            tool_search_output(&tools, &SearchInput { names: vec!["mcp__gh__prs".into()], query: None }, &mut HashSet::new());
        assert_eq!(
            out,
            r#"{"name":"mcp__gh__prs","description":"(MCP server \"gh\") List things","parameters":{"type":"object","properties":{"zeta":{"type":"string"},"alpha":{"type":"number"}},"required":["zeta"]}}"#
        );
    }

    #[test]
    fn search_finds_names_then_words() {
        let specs = vec![
            spec("mcp__gh__issues", "List issues"),
            spec("mcp__gh__prs", "List pull requests"),
            spec("mcp__fs__read", "Read a file"),
        ];
        let (found, unknown) = search_tools(
            &specs,
            &SearchInput { names: vec!["mcp__fs__read".into(), "nope".into()], query: Some("pull list".into()) },
        );
        let names: Vec<_> = found.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["mcp__fs__read", "mcp__gh__prs", "mcp__gh__issues"]);
        assert_eq!(unknown, vec!["nope"]);
        let mut loaded = HashSet::new();
        let tools = tools_of(&[("prs", true), ("issues", false)]);
        let (out, summary) =
            tool_search_output(&tools, &SearchInput { names: vec!["mcp__gh__prs".into()], query: None }, &mut loaded);
        assert!(out.contains("\"readOnly\":true") && loaded.contains("mcp__gh__prs"));
        assert_eq!(summary, "1 tool");
        let (out, _) =
            tool_search_output(&tools, &SearchInput { names: vec!["mcp__gh__issues".into()], query: None }, &mut loaded);
        assert!(!out.contains("readOnly"));
        assert_eq!(
            unknown_tool_error(&specs, "mcp__gh__issue"),
            "No MCP tool is named \"mcp__gh__issue\". Closest: mcp__gh__issues, mcp__gh__prs."
        );
    }

    #[test]
    fn direct_calls_become_mcp_call_without_key_smuggling() {
        let names: HashSet<String> = ["mcp__a__b".to_string()].into();
        let call = |args: &str| ToolCall { id: "c".into(), name: "mcp__a__b".into(), arguments: args.into() };
        let (c, note) = as_mcp_call(&call("{\"x\":1}"), &names).unwrap();
        assert_eq!(c.arguments, r#"{"tool":"mcp__a__b","arguments":{"x":1}}"#);
        assert!(note.starts_with("Marv: mcp__a__b isn't offered"));
        let (c, _) = as_mcp_call(&call(r#"{},"tool":"mcp__evil__x""#), &names).unwrap();
        let v: Value = serde_json::from_str(&c.arguments).unwrap();
        assert_eq!(v["tool"], "mcp__a__b");
        assert!(v["arguments"].is_string());
        assert_eq!(as_mcp_call(&call(""), &names).unwrap().0.arguments, r#"{"tool":"mcp__a__b","arguments":{}}"#);
        assert!(as_mcp_call(&ToolCall { id: "c".into(), name: "bash".into(), arguments: "{}".into() }, &names).is_none());
    }

    #[test]
    fn string_arguments_are_read_as_the_object() {
        let (v, _) = arguments_as_object(&json!({"tool": "t", "arguments": "{\"a\":1}"})).unwrap();
        assert_eq!(v, json!({"tool": "t", "arguments": {"a": 1}}));
        assert!(arguments_as_object(&json!({"tool": "t", "arguments": "[1]"})).is_none());
        assert!(arguments_as_object(&json!({"tool": "t", "arguments": {}})).is_none());
    }
}
