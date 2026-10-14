//! Next-step suggestions (new in marv-rs): after a turn, the message the user would most likely send next, which the
//! TUI shows in the empty prompt (Tab takes it).
//!
//! The request is compaction's shape ([`crate::compact::summarize`]): the conversation's own system prompt, tools and
//! history, plus one instruction at the end, so it reads the prompt cache the turn just wrote and costs little more
//! than its short reply. The instruction is never added to the conversation (history stays append-only): the next turn
//! goes on from the same prefix as if no suggestion had been asked for.
//!
//! What comes back is checked ([`clean`]): one short line, in the user's voice, or nothing. A reply that calls a tool,
//! runs long, or says there's no clear next step is no suggestion; a suggestion is only ever text the user may take.

use futures::StreamExt;
use marv_core::{ChatTurn, ProviderEvent, ToolSpec, Usage};
use marv_provider::{Provider, StreamOptions};
use tokio_util::sync::CancellationToken;

/// The instruction after the conversation. Marv speaks (the model is told who), so it isn't the user's message.
pub const INSTRUCTIONS: &str = "(Automatic note from Marv, the program running you) Don't answer this note and don't use a tool. Write the message the user is most likely to send you next, given where this conversation stands now: a short instruction in their words, at most 12 words, for example \"run the tests\" or \"commit this\". Not a question to the user, not a summary, no quotes. If there's no clear next step, write only NONE.";

/// The longest suggestion shown, in characters: more is a reply, not a next message.
pub const MAX_CHARS: usize = 100;

pub struct SuggestOptions<'a> {
    pub provider: &'a dyn Provider,
    pub history: &'a [ChatTurn],
    pub system: &'a str,
    pub tools: &'a [ToolSpec],
    pub cancel: CancellationToken,
    /// A real request: its tokens and cost count.
    pub on_usage: Option<&'a (dyn Fn(Usage) + Send + Sync)>,
}

/// The suggestion request's history: the same history plus the instruction.
pub fn suggestion_request(history: &[ChatTurn]) -> Vec<ChatTurn> {
    let mut request = history.to_vec();
    request.push(ChatTurn::User { text: INSTRUCTIONS.into() });
    request
}

/// A reply as a suggestion: its one line, without quotes or a leading `>`, or `None` (NONE, empty, several lines,
/// longer than [`MAX_CHARS`]).
pub fn clean(reply: &str) -> Option<String> {
    let reply = reply.trim();
    if reply.lines().count() != 1 {
        return None;
    }
    let text = reply.trim_start_matches('>').trim().trim_matches(|c| matches!(c, '"' | '\'' | '`' | '“' | '”')).trim();
    let none = text.trim_end_matches('.').eq_ignore_ascii_case("none");
    (!text.is_empty() && !none && text.chars().count() <= MAX_CHARS).then(|| text.to_string())
}

/// Asks the model for the user's next message. `None` when there's nothing to suggest, the model tried a tool, the
/// request failed or was stopped: a missing suggestion is never an error the user sees.
pub async fn suggest(options: SuggestOptions<'_>) -> Option<String> {
    if !matches!(options.history.last(), Some(ChatTurn::Assistant { tool_calls, .. }) if tool_calls.is_empty()) {
        return None; // only after a finished reply, never mid-turn
    }
    let request = suggestion_request(options.history);
    let stream_options = StreamOptions {
        system: Some(options.system.to_string()),
        tools: options.tools.to_vec(),
        cancel: options.cancel.clone(),
        ..Default::default()
    };
    let mut text = String::new();
    let mut events = options.provider.stream(&request, stream_options);
    while let Some(event) = events.next().await {
        match event {
            ProviderEvent::TextDelta(t) => text.push_str(&t),
            ProviderEvent::Retry { .. } => text.clear(),
            ProviderEvent::ToolCall(_) | ProviderEvent::Error(_) => return None,
            ProviderEvent::Done { reason } if reason.as_deref() == Some("length") => return None,
            ProviderEvent::Usage(usage) => {
                if let Some(on_usage) = options.on_usage {
                    on_usage(usage);
                }
            }
            _ => {}
        }
    }
    if options.cancel.is_cancelled() {
        return None;
    }
    clean(&text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::ScriptedProvider;

    fn history() -> Vec<ChatTurn> {
        vec![
            ChatTurn::User { text: "fix the failing test".into() },
            ChatTurn::Assistant { text: "Fixed: the discount was applied twice.".into(), tool_calls: vec![], reasoning: None },
        ]
    }

    fn say(text: &str) -> Vec<ProviderEvent> {
        vec![ProviderEvent::TextDelta(text.into()), ProviderEvent::Done { reason: None }]
    }

    async fn run(provider: &ScriptedProvider, history: &[ChatTurn], tools: &[ToolSpec]) -> Option<String> {
        suggest(SuggestOptions { provider, history, system: "SYSTEM", tools, cancel: CancellationToken::new(), on_usage: None })
            .await
    }

    #[tokio::test]
    async fn asks_on_top_of_the_same_prefix_and_leaves_the_history_alone() {
        let model = ScriptedProvider::new(vec![say("run the tests")]);
        let tools = vec![ToolSpec { name: "bash".into(), description: "d".into(), parameters: serde_json::json!({}) }];
        let h = history();
        assert_eq!(run(&model, &h, &tools).await.as_deref(), Some("run the tests"));
        let (sent, options) = &model.requests()[0];
        assert_eq!(options.system.as_deref(), Some("SYSTEM"));
        assert_eq!(options.tools, tools, "the same tools: the cached prefix holds");
        assert_eq!(&sent[..h.len()], &h[..]);
        assert!(matches!(sent.last(), Some(ChatTurn::User { text }) if text == INSTRUCTIONS));
        assert_eq!(h.len(), 2);
    }

    #[tokio::test]
    async fn nothing_mid_turn_or_after_a_tool_call_or_a_failure() {
        let model = ScriptedProvider::new(vec![]);
        let mid = vec![
            ChatTurn::User { text: "x".into() },
            ChatTurn::Assistant {
                text: String::new(),
                tool_calls: vec![marv_core::ToolCall { id: "c".into(), name: "bash".into(), arguments: "{}".into() }],
                reasoning: None,
            },
        ];
        assert_eq!(run(&model, &mid, &[]).await, None);
        assert_eq!(run(&model, &[], &[]).await, None);
        assert!(model.requests().is_empty(), "not even asked");

        let call = marv_core::ToolCall { id: "c".into(), name: "bash".into(), arguments: "{}".into() };
        let model = ScriptedProvider::new(vec![
            vec![ProviderEvent::ToolCall(call), ProviderEvent::Done { reason: None }],
            vec![ProviderEvent::Error("rate limited".into())],
            vec![ProviderEvent::TextDelta("run the".into()), ProviderEvent::Done { reason: Some("length".into()) }],
        ]);
        for _ in 0..3 {
            assert_eq!(run(&model, &history(), &[]).await, None);
        }
    }

    #[test]
    fn only_one_short_line_is_a_suggestion() {
        assert_eq!(clean("  \"commit this\"  ").as_deref(), Some("commit this"));
        assert_eq!(clean("> run the tests").as_deref(), Some("run the tests"));
        assert_eq!(clean("`cargo test`").as_deref(), Some("cargo test"));
        for no in ["NONE", "none.", "", "  ", "do this\nthen that", &"word ".repeat(30)] {
            assert_eq!(clean(no), None, "{no:?}");
        }
    }
}
