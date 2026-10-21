//! `recall` (marv-rs, after Marv #63): the model reads back what a compaction summarized away, from this session's
//! saved pages (`crate::swap`).
//!
//! - It reads only the current session's pages (`ToolContext::swap`, set for the main agent only; a subagent has none
//!   of its own and never its parent's). It takes no session id. `page` (+ `from`/`to`) returns those turns; `query`
//!   searches every page. Read-only, never asks.
//! - **Provenance comes back with the page.** A recalled web or MCP result is untrusted text again: the output's
//!   provenance is the first tainting source among the returned turns (the session fences it), and each tainting
//!   source is marked on the session's taint. Recalling can't launder outside text into trusted text.
//! - **Always offered**, last in the built-in list: offering it only after the first compaction would change the tool
//!   specs mid-session and break the prompt cache. Before then it answers that nothing was saved yet.
//! - Nothing is retrieved automatically: a page comes back only as the result of a `recall` call, appended like any
//!   other result.

use async_trait::async_trait;
use marv_core::{ChatTurn, Provenance};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::swap::{PagedTurn, read_page, read_table};
use crate::types::{Tool, ToolContext, ToolError, ToolOutput};

/// The most characters one `recall` returns; past it, the model is told to narrow the range.
pub const MAX_RECALL_CHARS: usize = 40_000;
/// The most matches a `query` lists.
pub const MAX_MATCHES: usize = 20;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecallInput {
    pub page: Option<u32>,
    pub from: Option<usize>,
    pub to: Option<usize>,
    pub query: Option<String>,
}

pub const RECALL_DESCRIPTION: &str = "Reads back messages that were summarized away when this conversation was compacted. The summary ends with a list of saved pages: give page (and optionally from and to, the message numbers within it) to read those messages exactly as they were, or query to search all pages for text. Read-only. Files in the project are better read again with read_file.";

pub struct Recall;

#[async_trait]
impl Tool for Recall {
    type Input = RecallInput;
    fn name(&self) -> &str {
        "recall"
    }
    fn description(&self) -> &str {
        RECALL_DESCRIPTION
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "page": {"type": "integer", "description": "A saved page's number, from the list after the summary."},
                "from": {"type": "integer", "description": "The first message of the page to read (from 0). Default: the start."},
                "to": {"type": "integer", "description": "The last message of the page to read. Default: the end."},
                "query": {"type": "string", "description": "Text to look for in every saved page (case-insensitive)."}
            },
            "additionalProperties": false
        })
    }
    fn check(&self, input: &RecallInput) -> Result<(), String> {
        match (&input.page, &input.query) {
            (None, None) => Err("give page or query".into()),
            (Some(_), Some(_)) => Err("give page or query, not both".into()),
            (None, Some(_)) if input.from.is_some() || input.to.is_some() => Err("from and to go with page".into()),
            (None, Some(q)) if q.trim().is_empty() => Err("query is empty".into()),
            _ => Ok(()),
        }
    }
    fn label(&self, input: &RecallInput) -> String {
        match (&input.page, &input.query) {
            (Some(page), _) => format!("page {page}"),
            (_, Some(query)) => format!("\"{query}\""),
            _ => String::new(),
        }
    }
    async fn run(&self, input: &RecallInput, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let Some(dir) = ctx.swap.as_deref() else {
            return Ok(ToolOutput::ok("Nothing has been saved from this conversation.", "nothing saved"));
        };
        let table = read_table(dir);
        if table.is_empty() {
            return Ok(ToolOutput::ok(
                "Nothing has been summarized away in this conversation yet: everything is still in it.",
                "nothing saved yet",
            ));
        }
        let (turns, header) = match (input.page, &input.query) {
            (Some(page), _) => {
                let Some(turns) = read_page(dir, page) else {
                    let pages: Vec<String> = table.iter().map(|p| p.page.to_string()).collect();
                    return Err(ToolError(format!("There's no page {page}. Saved pages: {}.", pages.join(", "))));
                };
                let (from, to) = (input.from.unwrap_or(0), input.to.unwrap_or(usize::MAX));
                let picked: Vec<(u32, usize, PagedTurn)> =
                    turns.into_iter().enumerate().filter(|(i, _)| *i >= from && *i <= to).map(|(i, t)| (page, i, t)).collect();
                (picked, None)
            }
            (None, Some(query)) => {
                let needle = query.to_lowercase();
                let mut hits = Vec::new();
                for entry in &table {
                    for (i, turn) in read_page(dir, entry.page).unwrap_or_default().into_iter().enumerate() {
                        if turn_text(&turn.turn).to_lowercase().contains(&needle) {
                            hits.push((entry.page, i, turn));
                        }
                    }
                }
                let total = hits.len();
                hits.truncate(MAX_MATCHES);
                let header = (total > MAX_MATCHES)
                    .then(|| format!("({total} messages match; the first {MAX_MATCHES} are shown: narrow the query.)"));
                (hits, header)
            }
            _ => unreachable!("checked"),
        };
        if turns.is_empty() {
            return Ok(ToolOutput::ok("No saved messages match.", "0 messages"));
        }
        let mut text = String::new();
        if let Some(header) = header {
            text.push_str(&header);
            text.push_str("\n\n");
        }
        let mut shown = 0;
        let mut provenance = Provenance::Project;
        for (page, i, paged) in &turns {
            let block = format!("[page {page}, message {i}, {}]\n{}\n\n", role(&paged.turn), turn_text(&paged.turn));
            if text.chars().count() + block.chars().count() > MAX_RECALL_CHARS && shown > 0 {
                text.push_str(&format!(
                    "({} more messages not shown: ask for a smaller range, from message {i} of page {page}.)",
                    turns.len() - shown
                ));
                break;
            }
            text.push_str(&block);
            shown += 1;
            // Outside text comes back as it came: fenced (the first source) and tainting (every source).
            if paged.provenance.taints() {
                ctx.taint.mark(paged.provenance);
                if provenance == Provenance::Project {
                    provenance = paged.provenance;
                }
            }
        }
        let mut out =
            ToolOutput::ok(text.trim_end().to_string(), format!("{shown} message{}", if shown == 1 { "" } else { "s" }));
        out.provenance = provenance;
        Ok(out)
    }
}

fn role(turn: &ChatTurn) -> String {
    match turn {
        ChatTurn::User { .. } => "user".into(),
        ChatTurn::Assistant { .. } => "assistant".into(),
        ChatTurn::Tool { name, .. } => format!("{name} result"),
    }
}

/// A turn's text as recall shows it: an assistant turn's calls with their arguments.
fn turn_text(turn: &ChatTurn) -> String {
    match turn {
        ChatTurn::User { text } | ChatTurn::Tool { text, .. } => text.clone(),
        ChatTurn::Assistant { text, tool_calls, .. } => {
            let mut out = text.clone();
            for call in tool_calls {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(&format!("(called {} {})", call.name, call.arguments));
            }
            out
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{builtin_tools, run_tool};
    use crate::swap::tests::page;
    use crate::swap::{page_entry, write_page};
    use crate::test_support::TempDir;
    use marv_core::ToolCall;

    async fn recall(ctx: &ToolContext, args: Value) -> ToolOutput {
        let call = ToolCall { id: "r".into(), name: "recall".into(), arguments: args.to_string() };
        run_tool(&call, ctx, builtin_tools()).await.result
    }

    #[tokio::test]
    async fn recall_reads_pages_back_byte_for_byte_and_a_web_page_taints_again() {
        let dir = TempDir::new("recall-read");
        let swap = dir.path().join("s.swap");
        let turns = page(dir.path());
        write_page(&swap, &page_entry(1, &turns, "auto", None), &turns).await.unwrap();
        assert_eq!(read_page(&swap, 1).unwrap(), turns);
        let mut ctx = ToolContext::new(dir.path());
        ctx.swap = Some(swap.clone());
        let out = recall(&ctx, json!({"page": 1, "from": 0, "to": 0})).await;
        assert_eq!(out.output, "[page 1, message 0, user]\nfix the parser");
        assert_eq!(out.provenance, Provenance::Project);
        assert!(ctx.taint.sources().is_empty());
        let out = recall(&ctx, json!({"query": "IGNORE the"})).await;
        assert!(out.output.contains("[page 1, message 3, web_fetch result]"), "{}", out.output);
        assert_eq!(out.provenance, Provenance::Web, "fenced again by the session");
        assert_eq!(ctx.taint.sources(), [Provenance::Web]);
        assert!(recall(&ctx, json!({"page": 7})).await.is_error);
        assert!(recall(&ctx, json!({})).await.is_error);
    }

    #[tokio::test]
    async fn recall_without_a_swap_or_before_any_page_says_so() {
        let dir = TempDir::new("recall-none");
        let mut ctx = ToolContext::new(dir.path());
        assert!(recall(&ctx, json!({"page": 1})).await.output.starts_with("Nothing has been saved"));
        ctx.swap = Some(dir.path().join("x.swap"));
        assert!(recall(&ctx, json!({"query": "x"})).await.output.starts_with("Nothing has been summarized away"));
    }
}
