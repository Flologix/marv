//! Compaction: when the conversation gets close to the model's context window, ask the model to
//! summarize it, then continue from the summary. Ports `src/compact.ts`.
//!
//! - Triggered at [`COMPACT_AT`] (85%) of the window, by the last request's prompt size (known for
//!   Ollama, and for OpenRouter via the model list). The session runs it before a turn
//!   (`runTurn` → `summarizeInto`) and mid-turn through the loop's `on_full` hook.
//! - **The summary request reuses the exact prefix** of a normal request (same system prompt,
//!   tools and history) and only appends one user message, so it hits the prompt cache and is
//!   cheap. The tools are offered for that reason, not to be used.
//! - Afterwards the model's history is just [`compacted_history`]: the cache starts over once,
//!   deliberately. **This is the one exception to "never rewrite the history".** The transcript
//!   the user sees is never touched.
//! - **Not a summary** (the history is kept, an error is returned): a reply that calls a tool
//!   ("Let me re-read src/app.tsx first."), one cut off at the output limit, an empty one, a
//!   provider error, or a cancel. Replacing the history with any of those would lose the
//!   conversation for good.
//! - A `Retry` event means the provider is asking again from the start: what the failed attempt
//!   sent isn't part of the summary that will replace the conversation (Marv #36 review).
//! - Mid-turn, [`MID_TURN_NOTE`] follows the compacted history: it ends with the model's own
//!   "Got it", so the next request would otherwise end with an assistant message and the model
//!   would have no instruction to go on.
//! - The summary request counts toward tokens and cost (`on_usage`).
//! - The plan (#15): the summary replaces the plan calls the model made, so the session's own copy is appended to
//!   the summary's message when it has something left to do, as structured text rather than the model's paraphrase.
//!   Marv's `compactedHistory(summary, plan)` takes the items; marv-agent doesn't depend on marv-tools, so here it
//!   takes the plan's text (`marv_tools::plan::plan_text`), and the session passes it only for an open plan.
//!
//! Planned — **compaction v2 (Marv #63, spec first; nothing here is settled):**
//! - a structured template ([`V2_SECTIONS`]), with one nudge if the reply ignores it;
//! - a verbatim tail: the newest ~[`V2_TAIL_TOKENS`] tokens stay as they are, never splitting a
//!   tool call from its result ([`tail_start`]);
//! - a rolling update: the next compaction sends "previous summary + what happened since" and gets
//!   an updated summary back ([`rolling_update_request`]);
//! - overflow recovery: a context-length error ([`marv_provider::errors::is_context_overflow`])
//!   compacts and retries the step once, then shrinks the target ([`V2_OVERFLOW_TARGETS`]), then
//!   drops the oldest exchanges.

use futures::StreamExt;
use marv_core::{ChatTurn, ProviderEvent, ToolSpec, Usage};
use marv_provider::{Provider, StreamOptions};
use tokio_util::sync::CancellationToken;

/// Compact before the next message once this much of the context window is used.
pub const COMPACT_AT: f64 = 0.85;

/// The instruction appended to the history for the summary request.
pub const INSTRUCTIONS: &str = "Summarize our conversation so far, so we can continue it from your summary alone (the earlier messages will be removed).

Include, specifically:
- What the user wants overall, and what they asked for most recently.
- Decisions made, and why.
- Files read or changed (paths), and what changed in them.
- Commands run and their important results (test results, errors).
- What's done, what's left, and the next step.
- Anything the user said to do or avoid.

Keep names, paths, numbers and error messages exact. Write it as notes for yourself. Don't call any tools, and don't do anything else.";

/// Appended after the compacted history when compaction happened mid-turn (between two steps).
/// Not used when compacting before a user message: there the user's message itself follows.
pub const MID_TURN_NOTE: &str = "(Automatic note from Marv, the program running you) Your conversation was just compacted to save space in the middle of this task (everything before this point is summarized above). Continue where you left off.";

/// What the history starts with after compacting.
pub const COMPACTED_PREFIX: &str =
    "This conversation was compacted to save space. Here is a summary of everything before this point:\n\n";
/// The model's acknowledgement of the summary.
pub const COMPACTED_ACK: &str = "Got it. I have the context from the summary and will continue from here.";

pub struct SummarizeOptions<'a> {
    pub provider: &'a dyn Provider,
    pub history: &'a [ChatTurn],
    pub system: &'a str,
    pub tools: &'a [ToolSpec],
    pub cancel: CancellationToken,
    /// What the user wants the summary to keep, from `/compact <focus>`.
    pub focus: Option<&'a str>,
    /// The summary request is a real request: count its tokens and cost too.
    pub on_usage: Option<&'a (dyn Fn(Usage) + Send + Sync)>,
}

/// The summary request's history: the same history plus one instruction message.
pub fn summary_request(history: &[ChatTurn], focus: Option<&str>) -> Vec<ChatTurn> {
    let text = match focus {
        Some(focus) => format!("{INSTRUCTIONS}\n\nFocus especially on: {focus}"),
        None => INSTRUCTIONS.to_string(),
    };
    let mut request = history.to_vec();
    request.push(ChatTurn::User { text });
    request
}

/// Asks the model for a summary. `Ok(summary)` (trimmed), or `Err(message)` with the history to be kept.
pub async fn summarize(options: SummarizeOptions<'_>) -> Result<String, String> {
    let request = summary_request(options.history, options.focus);
    let stream_options = StreamOptions {
        system: Some(options.system.to_string()),
        tools: options.tools.to_vec(),
        cancel: options.cancel.clone(),
        ..Default::default()
    };
    let mut text = String::new();
    let mut tool_call = false;
    let mut reason: Option<String> = None;
    let mut events = options.provider.stream(&request, stream_options);
    while let Some(event) = events.next().await {
        match event {
            ProviderEvent::TextDelta(t) => text.push_str(&t),
            ProviderEvent::Retry { .. } | ProviderEvent::Discard { .. } => {
                // The provider is asking again from the start: drop what the failed attempt sent.
                text.clear();
                tool_call = false;
                reason = None;
            }
            ProviderEvent::ToolCall(_) => tool_call = true,
            ProviderEvent::Done { reason: r } => reason = r,
            ProviderEvent::Usage(usage, _) => {
                if let Some(on_usage) = options.on_usage {
                    on_usage(usage);
                }
            }
            ProviderEvent::Error(message) => return Err(message),
            ProviderEvent::ThinkingDelta(_) | ProviderEvent::ToolCallDelta { .. } | ProviderEvent::Reasoning(_) => {}
        }
    }
    if options.cancel.is_cancelled() {
        return Err("Stopped.".into());
    }
    if tool_call {
        return Err("The model tried to use a tool instead of summarizing.".into());
    }
    if reason.as_deref() == Some("length") {
        return Err("The summary was cut off by the model's output limit.".into());
    }
    let summary = text.trim();
    if summary.is_empty() { Err("The model returned an empty summary.".into()) } else { Ok(summary.to_string()) }
}

/// The model's history after compacting: the summary, acknowledged, with the plan's text when one has something left
/// to do (`plan`: `marv_tools::plan::plan_text`).
pub fn compacted_history(summary: &str, plan: Option<&str>) -> Vec<ChatTurn> {
    let kept = plan.map(|plan| format!("\n\n{plan}")).unwrap_or_default();
    vec![
        ChatTurn::User { text: format!("{COMPACTED_PREFIX}{summary}{kept}") },
        ChatTurn::Assistant { text: COMPACTED_ACK.into(), tool_calls: vec![], reasoning: None },
    ]
}

/// Whether the last request filled the window past [`COMPACT_AT`].
pub fn should_compact(prompt_tokens: u64, context_length: Option<u64>) -> bool {
    context_length.is_some_and(|window| window > 0 && prompt_tokens as f64 >= COMPACT_AT * window as f64)
}

// ---- Compaction v2 (Marv #63, planned; spec first) ----

/// #63: the structured summary's sections, in order (opencode's template). Relevant Files: ≤ 15.
pub const V2_SECTIONS: [&str; 7] =
    ["Objective", "Requirements", "Decisions", "Work State", "Next Move", "Relevant Files", "Important Context"];
/// #63: tokens of the newest history kept verbatim after the summary.
pub const V2_TAIL_TOKENS: u64 = 15_000;
/// #63: after an overflow, the window shares to compact down to, in turn.
pub const V2_OVERFLOW_TARGETS: [f64; 3] = [0.70, 0.50, 0.35];

/// #63: the index where the verbatim tail starts: the newest turns worth about `tail_tokens`
/// (estimated at 4 characters a token, like Marv's `estimateTokens`), moved earlier so a tool
/// result is never separated from the assistant turn that called it.
pub fn tail_start(_history: &[ChatTurn], _tail_tokens: u64) -> usize {
    todo!("port: #63 verbatim tail split (never between a call and its result)")
}

/// #63: the rolling-update request: the previous summary plus what happened since, asking for
/// an updated summary in the [`V2_SECTIONS`] template.
pub fn rolling_update_request(_previous_summary: &str, _since: &[ChatTurn], _focus: Option<&str>) -> Vec<ChatTurn> {
    todo!("port: #63 rolling summary update request")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::ScriptedProvider;

    // Ported from tests/compact.test.ts.

    fn history() -> Vec<ChatTurn> {
        vec![
            ChatTurn::User { text: "make the scroll smoother".into() },
            ChatTurn::Assistant {
                text: "".into(),
                tool_calls: vec![marv_core::ToolCall {
                    id: "c1".into(),
                    name: "read_file".into(),
                    arguments: r#"{"path":"src/ui/ScrollView.tsx"}"#.into(),
                }],
                reasoning: None,
            },
            ChatTurn::Tool { call_id: "c1".into(), name: "read_file".into(), text: "…file contents…".into() },
            ChatTurn::Assistant { text: "Done: it glides now.".into(), tool_calls: vec![], reasoning: None },
        ]
    }

    fn say(text: &str) -> Vec<ProviderEvent> {
        vec![ProviderEvent::TextDelta(text.into()), ProviderEvent::Done { reason: None }]
    }

    async fn run(
        provider: &ScriptedProvider,
        history: &[ChatTurn],
        tools: &[ToolSpec],
        focus: Option<&str>,
    ) -> Result<String, String> {
        summarize(SummarizeOptions {
            provider,
            history,
            system: "SYSTEM",
            tools,
            cancel: CancellationToken::new(),
            focus,
            on_usage: None,
        })
        .await
    }

    #[tokio::test]
    async fn asks_for_a_summary_on_top_of_the_same_prefix() {
        let model = ScriptedProvider::new(vec![say("Summary: scroll glide added in src/ui/ScrollView.tsx.")]);
        let tools = vec![ToolSpec { name: "read_file".into(), description: "d".into(), parameters: serde_json::json!({}) }];
        let h = history();
        assert_eq!(run(&model, &h, &tools, None).await, Ok("Summary: scroll glide added in src/ui/ScrollView.tsx.".into()));
        let requests = model.requests();
        let (sent, options) = &requests[0];
        assert_eq!(options.system.as_deref(), Some("SYSTEM"));
        assert_eq!(options.tools, tools);
        assert_eq!(&sent[..h.len()], &h[..]);
        assert!(matches!(sent.last(), Some(ChatTurn::User { text }) if text.contains("Summarize")));
    }

    #[tokio::test]
    async fn a_dropped_connection_mid_summary_keeps_only_the_retried_text() {
        let model = ScriptedProvider::new(vec![vec![
            ProviderEvent::TextDelta("Summary: half of a fir".into()),
            ProviderEvent::Retry {
                attempt: 1,
                max: 2,
                message: "connection lost, retrying (1/2)…".into(),
                continued: false,
                cause: Default::default(),
            },
            ProviderEvent::TextDelta("Summary: scroll glide added.".into()),
            ProviderEvent::Done { reason: None },
        ]]);
        assert_eq!(run(&model, &history(), &[], None).await, Ok("Summary: scroll glide added.".into()));
    }

    #[tokio::test]
    async fn passes_along_what_to_focus_on() {
        let model = ScriptedProvider::new(vec![say("ok")]);
        run(&model, &history(), &[], Some("the sandbox")).await.unwrap();
        assert!(
            matches!(model.requests()[0].0.last(), Some(ChatTurn::User { text }) if text.contains("Focus especially on: the sandbox"))
        );
    }

    #[tokio::test]
    async fn reports_errors_and_empty_summaries_instead_of_compacting() {
        let failing = ScriptedProvider::new(vec![vec![ProviderEvent::Error("Rate limited".into())]]);
        assert_eq!(run(&failing, &history(), &[], None).await, Err("Rate limited".into()));
        let empty = ScriptedProvider::new(vec![say("   ")]);
        assert!(run(&empty, &history(), &[], None).await.is_err());
    }

    #[tokio::test]
    async fn a_reply_that_calls_a_tool_or_is_cut_off_is_not_a_summary() {
        let wants_tool = ScriptedProvider::new(vec![vec![
            ProviderEvent::TextDelta("Let me re-read src/app.tsx first.".into()),
            ProviderEvent::ToolCall(marv_core::ToolCall { id: "c1".into(), name: "read_file".into(), arguments: "{}".into() }),
            ProviderEvent::Done { reason: Some("tool_calls".into()) },
        ]]);
        let cut_off = ScriptedProvider::new(vec![vec![
            ProviderEvent::TextDelta("## Goal\nThe user wants to refac".into()),
            ProviderEvent::Done { reason: Some("length".into()) },
        ]]);
        for provider in [wants_tool, cut_off] {
            let error = run(&provider, &[ChatTurn::User { text: "hi".into() }], &[], None).await.unwrap_err();
            assert!(error.contains("tool") || error.contains("cut off"), "{error}");
        }
    }

    #[test]
    fn compacted_history_starts_over_from_the_summary() {
        let h = compacted_history("the summary", None);
        assert!(matches!(&h[0], ChatTurn::User { text } if text.contains("the summary")));
        assert!(matches!(&h[1], ChatTurn::Assistant { tool_calls, .. } if tool_calls.is_empty()));
        assert_eq!(h.len(), 2);
    }

    #[test]
    fn compacted_history_appends_the_plans_text_to_the_summary() {
        let plain = compacted_history("S", None);
        let with_plan = compacted_history("S", Some("Your plan…"));
        let (ChatTurn::User { text: a }, ChatTurn::User { text: b }) = (&plain[0], &with_plan[0]) else { panic!() };
        assert_eq!(*b, format!("{a}\n\nYour plan…"));
        assert_eq!(plain[1], with_plan[1]);
    }

    #[test]
    fn compacts_at_85_percent_of_a_known_window() {
        assert!(should_compact(85, Some(100)));
        assert!(!should_compact(84, Some(100)));
        assert!(!should_compact(1_000_000, None));
    }
}
