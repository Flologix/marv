//! The prompt-cache rules on a run's own requests (AGENTS.md, "Prompt cache"): history is
//! append-only, the system prompt is byte-stable, tool specs keep a stable order.
//!
//! New in marv-rs. Marv states the rule in its `CLAUDE.md` and keeps it by construction
//! (`toMessages` in `src/provider/openai-compat.ts` is deterministic); this checks it on the wire.
//! The cache doesn't need marv-rs's bytes to equal TypeScript Marv's, only each run's requests to
//! extend one another, so the check reads one run's requests and nothing else:
//!
//! - every message of the previous request is in this one, at the same index, with the same bytes;
//! - every other field (`model`, `tools`, `stream_options`, …) has the same bytes, except
//!   `tool_choice`: the wrap-up reply at the step limit sends `"none"` with the tools still there
//!   (#59), which leaves the prefix alone, and `max_tokens`: it follows the room left in the window
//!   (#116), and an output cap isn't part of the cached prompt;
//! - `cache_control` breakpoints (#56) don't count. They mark the last message, so they move each
//!   request by design: a marked message (`[{"type":"text","text":…,"cache_control":…}]`) counts as
//!   its plain text, and a marker elsewhere is cut out.
//!
//! A compaction rewrites the history on purpose ("the cache starts over once, deliberately",
//! `src/compact.ts`): a scenario names the request after it in `run.json` `prefix_resets`.

use std::collections::BTreeMap;

use serde_json::value::RawValue;

/// A request that doesn't extend the one before it.
#[derive(Debug, Clone, PartialEq)]
pub struct Break {
    /// 1-based, as in `requests.jsonl`.
    pub request: usize,
    pub detail: String,
}

/// Fields that may change between requests without touching the cached prefix.
const FREE_FIELDS: [&str; 3] = ["messages", "tool_choice", "max_tokens"];

/// Checks each body against the one before it. `resets` are 1-based request numbers allowed to
/// start over (after a compaction).
pub fn prefix_breaks(bodies: &[String], resets: &[usize]) -> Vec<Break> {
    let mut breaks = Vec::new();
    for i in 1..bodies.len() {
        let request = i + 1;
        if resets.contains(&request) {
            continue;
        }
        if let Some(detail) = extends(&bodies[i - 1], &bodies[i]) {
            breaks.push(Break { request, detail });
        }
    }
    breaks
}

/// `None` when `next` extends `prev`; otherwise what broke.
fn extends(prev: &str, next: &str) -> Option<String> {
    fn parse(body: &str) -> serde_json::Result<BTreeMap<String, &RawValue>> {
        serde_json::from_str(body)
    }
    let (prev, next) = match (parse(prev), parse(next)) {
        (Ok(prev), Ok(next)) => (prev, next),
        (Err(err), _) | (_, Err(err)) => return Some(format!("not a JSON object: {err}")),
    };
    for (key, value) in &prev {
        if FREE_FIELDS.contains(&key.as_str()) {
            continue;
        }
        match next.get(key) {
            None => return Some(format!("`{key}` is gone")),
            Some(other) if other.get() != value.get() => {
                return Some(format!("`{key}` changed: {}", first_difference(value.get(), other.get())));
            }
            Some(_) => {}
        }
    }
    if let Some(key) = next.keys().find(|k| !prev.contains_key(*k) && !FREE_FIELDS.contains(&k.as_str())) {
        return Some(format!("`{key}` is new"));
    }
    let messages = |body: &BTreeMap<String, &RawValue>| -> Result<Vec<String>, String> {
        let Some(raw) = body.get("messages") else { return Ok(Vec::new()) };
        let list: Vec<&RawValue> = serde_json::from_str(raw.get()).map_err(|err| format!("messages: {err}"))?;
        Ok(list.into_iter().map(|m| unmarked(m.get())).collect())
    };
    let (before, after) = match (messages(&prev), messages(&next)) {
        (Ok(before), Ok(after)) => (before, after),
        (Err(err), _) | (_, Err(err)) => return Some(err),
    };
    if after.len() < before.len() {
        return Some(format!("{} messages, fewer than the previous request's {}", after.len(), before.len()));
    }
    for (index, (a, b)) in before.iter().zip(&after).enumerate() {
        if a != b {
            return Some(format!("message {index} changed: {}", first_difference(a, b)));
        }
    }
    None
}

/// A message's bytes without its `cache_control` breakpoints.
pub fn unmarked(message: &str) -> String {
    if !message.contains("\"cache_control\"") {
        return message.to_string();
    }
    let Ok(fields) = serde_json::from_str::<BTreeMap<String, &RawValue>>(message) else { return message.to_string() };
    let Some(content) = fields.get("content") else { return cut_markers(message) };
    let start = offset(message, content.get());
    // The breakpoint's shape: the text moved into one marked part. Unmarked, it's the plain string.
    if let Ok(parts) = serde_json::from_str::<Vec<BTreeMap<String, &RawValue>>>(content.get())
        && let [part] = parts.as_slice()
        && part.get("type").map(|t| t.get()) == Some("\"text\"")
        && part.contains_key("cache_control")
        && part.len() == 3
        && let Some(text) = part.get("text")
    {
        return format!("{}{}{}", &message[..start], text.get(), &message[start + content.get().len()..]);
    }
    cut_markers(message)
}

/// Cuts every `"cache_control": …` member (with the comma that joins it) out of `json`.
fn cut_markers(json: &str) -> String {
    let mut out = json.to_string();
    while let Some(key) = out.find("\"cache_control\"") {
        // The value's extent: parse from after the colon as one JSON value.
        let colon = key + out[key..].find(':').unwrap_or(0) + 1;
        let rest = &out[colon..];
        let mut stream = serde_json::Deserializer::from_str(rest).into_iter::<&RawValue>();
        let Some(Ok(_)) = stream.next() else { break };
        let end = colon + stream.byte_offset();
        // Take the comma before (a later member) or after (the first member).
        let before = out[..key].trim_end();
        let (from, to) = if before.ends_with(',') {
            (before.len() - 1, end)
        } else {
            let after = out[end..].trim_start();
            match after.strip_prefix(',') {
                Some(rest) => (key, out.len() - rest.trim_start().len()),
                None => (key, end),
            }
        };
        out.replace_range(from..to, "");
    }
    out
}

/// Where `part` (a slice of `whole`) starts in it.
fn offset(whole: &str, part: &str) -> usize {
    (part.as_ptr() as usize).saturating_sub(whole.as_ptr() as usize)
}

/// The first byte where `a` and `b` differ, with some context around it.
pub fn first_difference(a: &str, b: &str) -> String {
    let at = a.bytes().zip(b.bytes()).position(|(x, y)| x != y).unwrap_or(a.len().min(b.len()));
    let around = |s: &str| {
        let mut from = at.saturating_sub(40);
        while !s.is_char_boundary(from) {
            from -= 1;
        }
        let mut to = (at + 40).min(s.len());
        while !s.is_char_boundary(to) {
            to += 1;
        }
        s[from..to].to_string()
    };
    format!("from byte {at}: …{}… became …{}…", around(a), around(b))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(messages: &[&str], tools: &str) -> String {
        format!(r#"{{"model":"m","stream":true,"messages":[{}],"tools":{tools}}}"#, messages.join(","))
    }

    const SYS: &str = r#"{"role":"system","content":"S"}"#;
    const USER: &str = r#"{"role":"user","content":"hi"}"#;
    const REPLY: &str = r#"{"role":"assistant","content":"hello"}"#;

    #[test]
    fn appending_keeps_the_prefix() {
        let bodies = [body(&[SYS, USER], "[]"), body(&[SYS, USER, REPLY, USER], "[]")];
        assert_eq!(prefix_breaks(&bodies, &[]), []);
    }

    #[test]
    fn the_same_request_again_keeps_it() {
        let bodies = [body(&[SYS, USER], "[]"), body(&[SYS, USER], "[]")];
        assert_eq!(prefix_breaks(&bodies, &[]), []);
    }

    #[test]
    fn a_changed_message_breaks_it() {
        let changed = r#"{"role":"system","content":"S2"}"#;
        let breaks = prefix_breaks(&[body(&[SYS, USER], "[]"), body(&[changed, USER, REPLY], "[]")], &[]);
        assert_eq!(breaks.len(), 1);
        assert_eq!(breaks[0].request, 2);
        assert!(breaks[0].detail.starts_with("message 0 changed"), "{}", breaks[0].detail);
    }

    #[test]
    fn reordered_keys_in_a_message_break_it() {
        let reordered = r#"{"content":"hi","role":"user"}"#;
        assert_eq!(prefix_breaks(&[body(&[SYS, USER], "[]"), body(&[SYS, reordered], "[]")], &[]).len(), 1);
    }

    #[test]
    fn dropped_messages_and_changed_tools_break_it() {
        assert!(prefix_breaks(&[body(&[SYS, USER], "[]"), body(&[SYS], "[]")], &[])[0].detail.contains("fewer"));
        let tools = prefix_breaks(&[body(&[SYS], r#"[{"a":1},{"b":2}]"#), body(&[SYS], r#"[{"b":2},{"a":1}]"#)], &[]);
        assert!(tools[0].detail.starts_with("`tools` changed"), "{tools:?}");
    }

    #[test]
    fn tool_choice_may_change() {
        let wrap_up =
            r#"{"model":"m","stream":true,"messages":[{"role":"system","content":"S"}],"tools":[],"tool_choice":"none"}"#;
        assert_eq!(prefix_breaks(&[body(&[SYS], "[]"), wrap_up.to_string()], &[]), []);
    }

    #[test]
    fn max_tokens_may_change_or_go() {
        // #116: the cap follows the room left in the window, and goes when there's none.
        let capped = |cap: &str| {
            format!(r#"{{"model":"m","stream":true,"messages":[{{"role":"system","content":"S"}}],"tools":[]{cap}}}"#)
        };
        let bodies = [capped(r#","max_tokens":32000"#), capped(r#","max_tokens":17672"#), capped("")];
        assert_eq!(prefix_breaks(&bodies, &[]), []);
    }

    #[test]
    fn a_reset_is_allowed_to_start_over() {
        let bodies = [body(&[SYS, USER, REPLY], "[]"), body(&[SYS, USER], "[]"), body(&[SYS], "[]")];
        assert_eq!(prefix_breaks(&bodies, &[2]).iter().map(|b| b.request).collect::<Vec<_>>(), [3]);
    }

    #[test]
    fn moving_cache_breakpoints_dont_count() {
        // #56: the system prompt and the last message carry the marker, so the last message of one
        // request is marked and plain in the next.
        let sys = r#"{"role":"system","content":[{"type":"text","text":"S","cache_control":{"type":"ephemeral"}}]}"#;
        let user_marked = r#"{"role":"user","content":[{"type":"text","text":"hi","cache_control":{"type":"ephemeral"}}]}"#;
        let reply_marked =
            r#"{"role":"assistant","content":[{"type":"text","text":"hello","cache_control":{"type":"ephemeral"}}]}"#;
        let bodies = [body(&[sys, user_marked], "[]"), body(&[sys, USER, reply_marked], "[]")];
        assert_eq!(prefix_breaks(&bodies, &[]), []);
    }

    #[test]
    fn unmarked_shapes() {
        let marked = r#"{"role":"tool","tool_call_id":"c1","content":[{"type":"text","text":"a \"q\"","cache_control":{"type":"ephemeral"}}]}"#;
        assert_eq!(unmarked(marked), r#"{"role":"tool","tool_call_id":"c1","content":"a \"q\""}"#);
        // Other shapes: the marker member is cut, wherever it sits.
        assert_eq!(unmarked(r#"{"a":1,"cache_control":{"type":"ephemeral"}}"#), r#"{"a":1}"#);
        assert_eq!(unmarked(r#"{"cache_control":{"type":"ephemeral"}, "a":1}"#), r#"{"a":1}"#);
        assert_eq!(unmarked(USER), USER);
    }

    #[test]
    fn differences_never_split_a_character() {
        assert!(first_difference("é".repeat(50).as_str(), &format!("{}x", "é".repeat(49))).contains("became"));
    }
}
