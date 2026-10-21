//! Compaction: when the conversation gets close to the model's context window, ask the model to
//! summarize it, then continue from the summary. Ports `src/compact.ts`.
//!
//! - Triggered at [`COMPACT_AT`] (85%) of the window, by the last request's prompt size (known for
//!   Ollama, and for OpenRouter via the model list), or sooner at the user's `compactTokens` (marv-rs #140,
//!   default 100k: on a large window 85% was never reached). The session decides (`nearly_full`) and runs it
//!   before a turn (`runTurn` → `summarizeInto`) and mid-turn through the loop's `on_full` hook.
//! - **The summary request reuses the exact prefix** of a normal request (same system prompt,
//!   tools and history) and only appends one user message, so it hits the prompt cache and is
//!   cheap. The tools are offered for that reason, not to be used.
//! - Afterwards the model's history is just [`compacted_history`]: the cache starts over once,
//!   deliberately. **This is the one exception to "never rewrite the history".** The transcript
//!   the user sees is never touched.
//! - **Not a summary** (the history is kept, an error is returned): a reply that calls a tool
//!   ("Let me re-read src/app.tsx first."), one cut off at the output limit, an empty one, a
//!   provider error, or a cancel. Replacing the history with any of those would lose the
//!   conversation for good. marv-rs #144: a reply that calls a tool is asked again once with
//!   `tool_choice: "none"` (same prefix, tools still sent) before giving up.
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
//! **Compaction v2 (Marv #63, spec `docs/specs/2026-10-14-compaction-v2.md`).** A deliberate behaviour change from
//! Marv, behind the temporary `compaction` key ([`Version`], default v1 until the eval):
//! - a structured template ([`V2_SECTIONS`], [`summary_instructions`]); a good text reply that ignores it gets one
//!   nudge ([`TEMPLATE_NUDGE`]), never on top of #144's re-ask, so a summary is at most 2 requests;
//! - a verbatim tail: the newest turns, about `compactTailTokens`, stay as they are after the summary, never splitting
//!   a tool call from its result ([`tail_start`], [`compacted_history_with_tail`]); the summary covers them too;
//! - a rolling update: when the history already starts with a summary (the session says so, never by sniffing the
//!   text), the instruction asks to update it ([`ROLLING_INSTRUCTIONS`]). Still `history + one user message`, so the
//!   cache hits as before.
//!
//! **Overflow recovery (#63, both versions):** a context-length error ([`marv_provider::errors::is_context_overflow`])
//! reaches the session through the loop's `on_overflow`. The session summarizes a cut-down history
//! ([`fit_for_summary`], never a cache hit: error path only) at [`V2_OVERFLOW_TARGETS`] of the window, then drops
//! the oldest exchanges with no request ([`drop_to_fit`], [`DROPPED_NOTE`]).

use futures::StreamExt;
use marv_core::{ChatTurn, ProviderEvent, ToolChoice, ToolSpec, Usage};
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

/// #63: which compaction runs: v1 (one free-form summary of everything) or v2 (the template, a verbatim tail and
/// rolling updates). The `compaction` config key; temporary, for the A/B.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Version {
    #[default]
    V1,
    V2,
}

impl Version {
    pub fn parse(text: &str) -> Option<Version> {
        match text {
            "v1" => Some(Version::V1),
            "v2" => Some(Version::V2),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Version::V1 => "v1",
            Version::V2 => "v2",
        }
    }
}

/// #63: what the summary request asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SummaryKind {
    /// v1's free-form [`INSTRUCTIONS`].
    V1,
    /// v2's template, from scratch.
    Full,
    /// v2's template, updating the summary the history starts with.
    Rolling,
}

/// #63: how a full v2 summary starts.
pub const FULL_INSTRUCTIONS: &str = "Summarize our conversation so far, so we can continue it from your summary alone. The earlier messages will be removed; the most recent ones stay as they are after your summary.";

/// #63: how a rolling update starts, when the history already starts with a summary.
pub const ROLLING_INSTRUCTIONS: &str = "Update the summary at the start of this conversation with everything that happened since it, so we can continue from your updated summary alone. Keep what is still true; change what changed; drop what no longer matters (finished details, superseded decisions). Use exactly the same sections. Return the whole updated summary, not just the changes.";

/// #63: the template's sections, as asked, and the closing lines.
pub const TEMPLATE: &str = "Use exactly these sections, as Markdown headings, in this order:

## Objective
What the user wants overall, and what they asked for most recently, in their words where it matters.
## Requirements
Constraints and preferences the user stated: what to do, what to avoid.
## Decisions
What was decided, and why. Approaches tried and rejected, and why.
## Work State
What's done, what's in progress, what's left. Commands run and their important results (tests, errors), exactly.
## Next Move
The very next step you were about to take.
## Relevant Files
At most 15 paths, one per line, each with what it is and what changed in it.
## Important Context
Anything else needed to continue: names, numbers, error messages, URLs, exactly.

Write it as notes for yourself. Keep names, paths, numbers and error messages exact. Don't call any tools, and don't do anything else.";

/// #63: asked once after a good text reply that ignored the template.
pub const TEMPLATE_NUDGE: &str = "Rewrite that summary using exactly the sections asked for, as Markdown headings in this order: ## Objective, ## Requirements, ## Decisions, ## Work State, ## Next Move, ## Relevant Files, ## Important Context. Return only the summary.";

/// The instruction message for a summary of `kind`, with `/compact <focus>` appended.
pub fn summary_instructions(kind: SummaryKind, focus: Option<&str>) -> String {
    let text = match kind {
        SummaryKind::V1 => INSTRUCTIONS.to_string(),
        SummaryKind::Full => format!("{FULL_INSTRUCTIONS}\n\n{TEMPLATE}"),
        SummaryKind::Rolling => format!("{ROLLING_INSTRUCTIONS}\n\n{TEMPLATE}"),
    };
    match focus {
        Some(focus) => format!("{text}\n\nFocus especially on: {focus}"),
        None => text,
    }
}

/// #63: whether a reply follows the template: at least 5 of the 7 `## <section>` headings, at line start, any case.
pub fn is_structured(summary: &str) -> bool {
    let headings: Vec<String> = summary
        .lines()
        .filter_map(|line| line.strip_prefix("##"))
        .filter(|rest| !rest.starts_with('#'))
        .map(|rest| rest.trim().trim_end_matches(':').to_lowercase())
        .collect();
    V2_SECTIONS.iter().filter(|section| headings.iter().any(|h| *h == section.to_lowercase())).count() >= 5
}

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
    /// #63: v1, or v2 from scratch or rolling.
    pub kind: SummaryKind,
    /// #63: a v2 reply that ignores the template is asked once to follow it (off during overflow recovery).
    pub nudge: bool,
}

/// A summary, and whether it followed the v2 template (#63; always checked, so a v1 summary is `false`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Summary {
    pub text: String,
    pub structured: bool,
}

/// The summary request's history: the same history plus one instruction message (#63: the prefix is the history, so
/// the prompt cache hits).
pub fn summary_request(history: &[ChatTurn], focus: Option<&str>, kind: SummaryKind) -> Vec<ChatTurn> {
    let mut request = history.to_vec();
    request.push(ChatTurn::User { text: summary_instructions(kind, focus) });
    request
}

/// The error when the model called a tool instead of summarizing (after the retry with `tool_choice: "none"`).
pub const TOOL_CALL_ERROR: &str = "The model tried to use a tool instead of summarizing.";

/// Asks the model for a summary. `Ok(summary)` (trimmed), or `Err(message)` with the history to be kept.
///
/// marv-rs #144 (Marv asks once): a reply that calls a tool is asked again once with `tool_choice: "none"`. The tools
/// are still sent, so the retry keeps the same prefix and hits the prompt cache (`tool_choice` is outside the prefix
/// rule). Only a tool call is retried: a cut-off, empty or failed summary would most likely fail the same way.
///
/// #63: a v2 summary that came back as good text but without the template gets one nudge (the same request, the reply
/// as an assistant turn, then [`TEMPLATE_NUDGE`]: the cache still hits up to the reply). Never after the re-ask, so a
/// summary is at most 2 requests. The nudge can't fail a compaction: whatever it returns other than a structured
/// summary, the first reply is kept (`structured: false`). Only a stop during it stops.
pub async fn summarize(options: SummarizeOptions<'_>) -> Result<Summary, String> {
    let request = summary_request(options.history, options.focus, options.kind);
    let first = match ask(&options, &request, ToolChoice::Auto).await {
        Err(Failure::ToolCall) => {
            // #144's re-ask; its reply is accepted as it is, template or not.
            return ask(&options, &request, ToolChoice::None).await.map(summary_of).map_err(failure_text);
        }
        other => other.map_err(failure_text)?,
    };
    if options.kind == SummaryKind::V1 || !options.nudge || is_structured(&first) {
        return Ok(summary_of(first));
    }
    let mut nudged = request;
    nudged.push(ChatTurn::Assistant { text: first.clone(), tool_calls: vec![], reasoning: None });
    nudged.push(ChatTurn::User { text: TEMPLATE_NUDGE.into() });
    match ask(&options, &nudged, ToolChoice::Auto).await {
        Ok(second) if is_structured(&second) => Ok(summary_of(second)),
        _ if options.cancel.is_cancelled() => Err("Stopped.".into()),
        _ => Ok(Summary { text: first, structured: false }),
    }
}

fn summary_of(text: String) -> Summary {
    let structured = is_structured(&text);
    Summary { text, structured }
}

fn failure_text(failure: Failure) -> String {
    match failure {
        Failure::ToolCall => TOOL_CALL_ERROR.into(),
        Failure::Other(message) => message,
    }
}

/// Why one summary request gave no summary.
enum Failure {
    ToolCall,
    Other(String),
}

/// One summary request.
async fn ask(options: &SummarizeOptions<'_>, request: &[ChatTurn], tool_choice: ToolChoice) -> Result<String, Failure> {
    let stream_options = StreamOptions {
        system: Some(options.system.to_string()),
        tools: options.tools.to_vec(),
        tool_choice,
        cancel: options.cancel.clone(),
        ..Default::default()
    };
    let mut text = String::new();
    let mut tool_call = false;
    let mut reason: Option<String> = None;
    let mut events = options.provider.stream(request, stream_options);
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
            ProviderEvent::Error(message) => return Err(Failure::Other(message)),
            ProviderEvent::ThinkingDelta(_) | ProviderEvent::ToolCallDelta { .. } | ProviderEvent::Reasoning(_) => {}
        }
    }
    if options.cancel.is_cancelled() {
        return Err(Failure::Other("Stopped.".into()));
    }
    if tool_call {
        return Err(Failure::ToolCall);
    }
    if reason.as_deref() == Some("length") {
        return Err(Failure::Other("The summary was cut off by the model's output limit.".into()));
    }
    let summary = text.trim();
    if summary.is_empty() { Err(Failure::Other("The model returned an empty summary.".into())) } else { Ok(summary.to_string()) }
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

/// #63: the history after a v2 compaction: the summary (and `kept`: the plan's text, the saved pages' list), the ACK
/// only when the tail is empty or starts with a user turn (never two user or two assistant turns in a row), then the
/// tail, the same `ChatTurn` values moved (arguments, reasoning and fenced results keep their bytes).
pub fn compacted_history_with_tail(summary: &str, kept: Option<&str>, tail: Vec<ChatTurn>) -> Vec<ChatTurn> {
    let mut history = compacted_history(summary, kept);
    if matches!(tail.first(), Some(ChatTurn::Assistant { .. })) {
        history.pop();
    }
    history.extend(tail);
    history
}

/// Whether the last request filled the window past [`COMPACT_AT`].
pub fn should_compact(prompt_tokens: u64, context_length: Option<u64>) -> bool {
    context_length.is_some_and(|window| window > 0 && prompt_tokens as f64 >= COMPACT_AT * window as f64)
}

// ---- Compaction v2 (Marv #63) ----

/// #63: the structured summary's sections, in order (opencode's template). Relevant Files: ≤ 15.
pub const V2_SECTIONS: [&str; 7] =
    ["Objective", "Requirements", "Decisions", "Work State", "Next Move", "Relevant Files", "Important Context"];
/// #63: the most tokens of the newest history kept verbatim after the summary (the default is the smaller of this and
/// [`V2_TAIL_SHARE`] of the trigger).
pub const V2_TAIL_TOKENS: u64 = 15_000;
/// #63: the tail's default share of the compaction threshold (`min(85% × window, compactTokens)`).
pub const V2_TAIL_SHARE: f64 = 0.15;
/// #63: after an overflow, the window shares to compact down to, in turn (levels 0, 1, 2; level 3 drops at the last).
pub const V2_OVERFLOW_TARGETS: [f64; 3] = [0.70, 0.50, 0.35];
/// #63: a recovery level whose history budget is below this is skipped rather than run on an empty history.
pub const MIN_HISTORY_BUDGET: u64 = 1_024;

/// #63: the default tail for a compaction threshold (tokens): `min(15k, 15% of it)`.
pub fn default_tail_tokens(threshold: Option<u64>) -> u64 {
    threshold.map_or(V2_TAIL_TOKENS, |t| V2_TAIL_TOKENS.min((t as f64 * V2_TAIL_SHARE) as u64))
}

/// #63: the index where the verbatim tail starts: the newest turns worth about `tail_tokens`
/// ([`marv_core::estimate_turn_tokens`]), moved earlier so a tool result is never separated from the assistant turn
/// that called it. In this order:
/// 1. the whole history fits (`tail_tokens > 0`; an empty history too) → `0`: nothing worth compacting;
/// 2. even the newest turn is over budget → `len` (no tail), without snapping;
/// 3. snapping moved the start back to 0 → `len`;
/// 4. the snapped tail is over `2 × tail_tokens` (a giant tool result) → `len`, so it goes into the summary;
/// 5. otherwise the snapped index, `> 0` and `< len`.
///
/// A budget of `0` on a non-empty history is always `len`.
pub fn tail_start(history: &[ChatTurn], tail_tokens: u64) -> usize {
    let len = history.len();
    if len == 0 || (tail_tokens > 0 && marv_core::estimate_history_tokens(history) <= tail_tokens) {
        return 0;
    }
    let mut start = len;
    let mut sum = 0;
    while start > 0 {
        let next = sum + marv_core::estimate_turn_tokens(&history[start - 1]);
        if next > tail_tokens {
            break;
        }
        sum = next;
        start -= 1;
    }
    if start == len {
        return len;
    }
    while start > 0 && matches!(history[start], ChatTurn::Tool { .. }) {
        start -= 1;
    }
    if start == 0 || marv_core::estimate_history_tokens(&history[start..]) > 2 * tail_tokens {
        return len;
    }
    start
}

/// The first index at or after `from` whose turn isn't a tool result (`len` if none): a history may start there.
fn next_safe(history: &[ChatTurn], from: usize) -> usize {
    (from..history.len()).find(|&i| !matches!(history[i], ChatTurn::Tool { .. })).unwrap_or(history.len())
}

/// The steps of a history from `from`: each a user turn, or an assistant turn with the results that follow it.
fn steps(history: &[ChatTurn], from: usize) -> Vec<std::ops::Range<usize>> {
    let mut out: Vec<std::ops::Range<usize>> = Vec::new();
    for (i, turn) in history.iter().enumerate().skip(from) {
        match (turn, out.last_mut()) {
            (ChatTurn::Tool { .. }, Some(last)) => last.end = i + 1,
            _ => out.push(i..i + 1),
        }
    }
    out
}

/// #63: `text` cut to about `tokens` tokens, its head and tail kept (the saved tool outputs' shape).
fn head_and_tail(text: &str, tokens: u64) -> String {
    let keep = (tokens * marv_core::CHARS_PER_TOKEN / 2) as usize;
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= 2 * keep {
        return text.to_string();
    }
    let head: String = chars[..keep].iter().collect();
    let tail: String = chars[chars.len() - keep..].iter().collect();
    format!("{head}\n[… cut for the summary …]\n{tail}")
}

/// #63, overflow recovery: the history a recovery summary is asked over, at most about `budget` tokens. The first
/// `keep` turns (the previous summary and its ACK) stay; the task (the first user turn after them) stays while
/// anything else can go; then the oldest steps go first, whole (a call is never split from its result). A tool result
/// still larger than `budget / 4` is cut to head and tail. Never a cache hit; that's accepted on the error path.
pub fn fit_for_summary(history: &[ChatTurn], budget: u64, keep: usize) -> Vec<ChatTurn> {
    let keep = keep.min(history.len());
    let mut turns: Vec<ChatTurn> = history.to_vec();
    for turn in &mut turns[keep..] {
        if let ChatTurn::Tool { text, .. } = turn
            && marv_core::estimate_tokens(text) > budget / 4
        {
            *text = head_and_tail(text, budget / 4);
        }
    }
    let units = steps(&turns, keep);
    let task = units.first().filter(|u| matches!(turns[u.start], ChatTurn::User { .. })).cloned();
    let size = |r: &std::ops::Range<usize>| marv_core::estimate_history_tokens(&turns[r.clone()]);
    let mut total = marv_core::estimate_history_tokens(&turns);
    let mut dropped = vec![false; units.len()];
    // The oldest steps after the task first, then the task itself.
    let order = (usize::from(task.is_some())..units.len()).chain(task.iter().map(|_| 0));
    for i in order {
        if total <= budget {
            break;
        }
        total -= size(&units[i]);
        dropped[i] = true;
    }
    let mut out = turns[..keep].to_vec();
    for (unit, gone) in units.iter().zip(dropped) {
        if !gone {
            out.extend_from_slice(&turns[unit.clone()]);
        }
    }
    out
}

/// #63, level 3: the note that replaces the history dropped to fit the window. No asking: a headless run has no one.
pub const DROPPED_NOTE: &str = "(Automatic note from Marv) Older messages were dropped to fit the context window; ask the user if you need something from them.";

/// #63, level 3: the first `keep` turns (the previous summary and its ACK, if any), then the newest turns that fit in
/// `budget` tokens, starting where a history may start (never at a tool result), then [`DROPPED_NOTE`] (`note`, with
/// any MCP announcement). No request is sent for it.
pub fn drop_to_fit(history: &[ChatTurn], budget: u64, keep: usize, note: &str) -> Vec<ChatTurn> {
    let keep = keep.min(history.len());
    let mut room = budget.saturating_sub(marv_core::estimate_history_tokens(&history[..keep]));
    room = room.saturating_sub(marv_core::estimate_tokens(note));
    let mut start = history.len();
    while start > keep {
        let size = marv_core::estimate_turn_tokens(&history[start - 1]);
        if size > room {
            break;
        }
        room -= size;
        start -= 1;
    }
    let start = next_safe(history, start.max(keep));
    let mut out = history[..keep].to_vec();
    out.extend_from_slice(&history[start..]);
    out.push(ChatTurn::User { text: note.to_string() });
    out
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
            kind: SummaryKind::V1,
            nudge: false,
        })
        .await
        .map(|s| s.text)
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

    fn calls_a_tool() -> Vec<ProviderEvent> {
        vec![
            ProviderEvent::TextDelta("Let me re-read src/app.tsx first.".into()),
            ProviderEvent::ToolCall(marv_core::ToolCall { id: "c1".into(), name: "read_file".into(), arguments: "{}".into() }),
            ProviderEvent::Done { reason: Some("tool_calls".into()) },
        ]
    }

    #[tokio::test]
    async fn a_reply_that_calls_a_tool_or_is_cut_off_is_not_a_summary() {
        // #144: a tool call is asked again once; a second one is still not a summary.
        let wants_tool = ScriptedProvider::new(vec![calls_a_tool(), calls_a_tool()]);
        let cut_off = ScriptedProvider::new(vec![vec![
            ProviderEvent::TextDelta("## Goal\nThe user wants to refac".into()),
            ProviderEvent::Done { reason: Some("length".into()) },
        ]]);
        for provider in [wants_tool, cut_off] {
            let error = run(&provider, &[ChatTurn::User { text: "hi".into() }], &[], None).await.unwrap_err();
            assert!(error.contains("tool") || error.contains("cut off"), "{error}");
        }
    }

    #[tokio::test]
    async fn a_tool_call_instead_of_a_summary_is_asked_again_once_with_tool_choice_none() {
        // marv-rs #144: the retry keeps the same prefix (system, tools, history) and only sets tool_choice.
        let model = ScriptedProvider::new(vec![calls_a_tool(), say("Summary: scroll glide added.")]);
        let tools = vec![ToolSpec { name: "read_file".into(), description: "d".into(), parameters: serde_json::json!({}) }];
        let h = history();
        assert_eq!(run(&model, &h, &tools, None).await, Ok("Summary: scroll glide added.".into()));
        let requests = model.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].1.tool_choice, ToolChoice::Auto);
        assert_eq!(requests[1].1.tool_choice, ToolChoice::None);
        assert_eq!(requests[1].1.tools, tools);
        assert_eq!(requests[1].1.system, requests[0].1.system);
        assert_eq!(requests[1].0, requests[0].0);

        let stubborn = ScriptedProvider::new(vec![calls_a_tool(), calls_a_tool(), say("never asked")]);
        assert_eq!(run(&stubborn, &h, &tools, None).await, Err(TOOL_CALL_ERROR.into()));
        assert_eq!(stubborn.requests().len(), 2);
    }

    #[tokio::test]
    async fn only_a_tool_call_is_asked_again() {
        for reply in [
            vec![ProviderEvent::Error("Rate limited".into())],
            vec![ProviderEvent::TextDelta("half".into()), ProviderEvent::Done { reason: Some("length".into()) }],
            say("  "),
        ] {
            let model = ScriptedProvider::new(vec![reply, say("a summary")]);
            assert!(run(&model, &history(), &[], None).await.is_err());
            assert_eq!(model.requests().len(), 1);
        }
        // Stopped during the first request: no retry either.
        let model = ScriptedProvider::new(vec![calls_a_tool(), say("a summary")]);
        let cancel = CancellationToken::new();
        cancel.cancel();
        let result = summarize(SummarizeOptions {
            provider: &model,
            history: &history(),
            system: "SYSTEM",
            tools: &[],
            cancel,
            focus: None,
            on_usage: None,
            kind: SummaryKind::V1,
            nudge: false,
        })
        .await;
        assert_eq!(result, Err("Stopped.".into()));
        assert_eq!(model.requests().len(), 1);
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

    // ---- #63 v2 ----

    fn user(text: &str) -> ChatTurn {
        ChatTurn::User { text: text.into() }
    }
    fn calls(id: &str, args_len: usize) -> ChatTurn {
        ChatTurn::Assistant {
            text: String::new(),
            tool_calls: vec![marv_core::ToolCall { id: id.into(), name: "r".into(), arguments: "x".repeat(args_len) }],
            reasoning: None,
        }
    }
    fn result(id: &str, len: usize) -> ChatTurn {
        ChatTurn::Tool { call_id: id.into(), name: "r".into(), text: "y".repeat(len) }
    }
    fn answer(len: usize) -> ChatTurn {
        ChatTurn::Assistant { text: "z".repeat(len), tool_calls: vec![], reasoning: None }
    }

    #[test]
    fn tail_start_already_fits_or_empty_is_zero() {
        let h = vec![user("hi"), answer(8)];
        assert_eq!(tail_start(&h, 100), 0);
        assert_eq!(tail_start(&[], 100), 0);
        assert_eq!(tail_start(&[], 0), 0);
    }

    #[test]
    fn tail_start_newest_turn_over_budget_is_len_without_snapping() {
        // r = 1 token each for the name; a 400-char result is 100 tokens.
        let h = vec![user(&"a".repeat(400)), calls("c1", 4), result("c1", 400)];
        assert_eq!(tail_start(&h, 50), 3);
    }

    #[test]
    fn tail_start_snapped_back_to_zero_or_over_the_cap_is_len() {
        // The tail would start at the result; snapping goes back to the call at index 0.
        let h = vec![calls("c1", 4), result("c1", 40), answer(4)];
        assert_eq!(tail_start(&h, 13), 3);
        // Over 2×: the step's results together are far bigger than the budget.
        let h = vec![user("go"), calls("c1", 4), result("c1", 400), result("c1", 40), answer(4)];
        assert_eq!(tail_start(&h, 13), 5);
    }

    #[test]
    fn tail_start_zero_budget_is_len_and_otherwise_never_at_a_tool_turn() {
        let h = vec![user(&"a".repeat(400)), calls("c1", 4), result("c1", 40), result("c2", 40), answer(4)];
        assert_eq!(tail_start(&h, 0), 5);
        // 23 tokens reach back to c1's result (index 2); the start moves earlier to its call.
        let start = tail_start(&h, 23);
        assert_eq!(start, 1, "snapped earlier to the call");
        assert!(!matches!(h[start], ChatTurn::Tool { .. }));
        let h = vec![user(&"a".repeat(400)), answer(4), user("next"), answer(4)];
        assert_eq!(tail_start(&h, 2), 2, "within budget, no snapping needed");
    }

    #[test]
    fn the_ack_is_kept_iff_the_tail_is_empty_or_starts_with_a_user_turn() {
        let tail = vec![calls("c1", 4), result("c1", 4)];
        let h = compacted_history_with_tail("S", Some("plan"), tail.clone());
        assert!(matches!(&h[0], ChatTurn::User { text } if text.ends_with("S\n\nplan")));
        assert_eq!(&h[1..], &tail[..], "the tail turns are the originals, no ACK before an assistant turn");
        let h = compacted_history_with_tail("S", None, vec![user("go"), answer(4)]);
        assert!(matches!(&h[1], ChatTurn::Assistant { text, .. } if text == COMPACTED_ACK));
        assert_eq!(h.len(), 4);
        assert_eq!(compacted_history_with_tail("S", None, vec![]), compacted_history("S", None));
    }

    #[test]
    fn the_summary_request_is_the_history_plus_the_full_or_rolling_instruction() {
        let h = history();
        for kind in [SummaryKind::Full, SummaryKind::Rolling] {
            let request = summary_request(&h, Some("tests"), kind);
            assert_eq!(&request[..h.len()], &h[..], "the prefix is the history: the cache hits");
            let ChatTurn::User { text } = request.last().unwrap() else { panic!() };
            assert!(text.contains("## Important Context") && text.ends_with("Focus especially on: tests"));
            assert_eq!(text.starts_with("Update the summary"), kind == SummaryKind::Rolling);
        }
    }

    fn sections(n: usize) -> String {
        V2_SECTIONS[..n].iter().map(|s| format!("## {s}\nnotes\n")).collect()
    }

    #[test]
    fn five_of_seven_headings_at_line_start_any_case_is_structured() {
        assert!(is_structured(&sections(5)));
        assert!(is_structured(&sections(7).to_uppercase()));
        assert!(!is_structured(&sections(4)));
        assert!(!is_structured(&sections(7).replace("## ", "text ## ")));
        assert!(!is_structured(&sections(7).replace("## ", "### ")));
    }

    async fn run_v2(provider: &ScriptedProvider, nudge: bool) -> Result<Summary, String> {
        summarize(SummarizeOptions {
            provider,
            history: &history(),
            system: "SYSTEM",
            tools: &[],
            cancel: CancellationToken::new(),
            focus: None,
            on_usage: None,
            kind: SummaryKind::Full,
            nudge,
        })
        .await
    }

    #[tokio::test]
    async fn an_unstructured_reply_gets_one_nudge_on_the_same_prefix() {
        let model = ScriptedProvider::new(vec![say("free prose"), say(&sections(7))]);
        assert_eq!(run_v2(&model, true).await, Ok(Summary { text: sections(7).trim().into(), structured: true }));
        let requests = model.requests();
        assert_eq!(requests.len(), 2);
        let (first, second) = (&requests[0].0, &requests[1].0);
        assert_eq!(&second[..first.len()], &first[..], "the nudge extends the first request");
        assert_eq!(second[first.len()], ChatTurn::Assistant { text: "free prose".into(), tool_calls: vec![], reasoning: None });
        assert!(matches!(second.last(), Some(ChatTurn::User { text }) if text == TEMPLATE_NUDGE));
    }

    #[tokio::test]
    async fn a_failed_or_unstructured_nudge_keeps_the_first_reply() {
        for nudge_reply in [
            say("still prose"),
            vec![ProviderEvent::Error("Rate limited".into())],
            vec![ProviderEvent::TextDelta("## Obj".into()), ProviderEvent::Done { reason: Some("length".into()) }],
            calls_a_tool(),
        ] {
            let model = ScriptedProvider::new(vec![say("free prose"), nudge_reply]);
            assert_eq!(run_v2(&model, true).await, Ok(Summary { text: "free prose".into(), structured: false }));
            assert_eq!(model.requests().len(), 2);
        }
    }

    #[tokio::test]
    async fn at_most_two_requests_a_summary_and_no_nudge_after_the_re_ask_or_when_off() {
        let model = ScriptedProvider::new(vec![calls_a_tool(), say("prose after the re-ask"), say(&sections(7))]);
        assert_eq!(run_v2(&model, true).await, Ok(Summary { text: "prose after the re-ask".into(), structured: false }));
        assert_eq!(model.requests().len(), 2);
        let model = ScriptedProvider::new(vec![say("prose"), say(&sections(7))]);
        assert!(!run_v2(&model, false).await.unwrap().structured);
        assert_eq!(model.requests().len(), 1, "no nudge during overflow recovery");
        let model = ScriptedProvider::new(vec![say(&sections(6))]);
        assert!(run_v2(&model, true).await.unwrap().structured);
        assert_eq!(model.requests().len(), 1);
    }

    #[test]
    fn the_default_tail_is_15k_or_15_percent_of_the_threshold() {
        assert_eq!(default_tail_tokens(Some(100_000)), 15_000);
        assert_eq!(default_tail_tokens(Some(27_852)), 4_177);
        assert_eq!(default_tail_tokens(None), 15_000);
    }

    #[test]
    fn fit_for_summary_drops_the_oldest_steps_keeps_the_summary_and_task_and_cuts_giant_results() {
        let summary = vec![user("SUMMARY"), answer(4)];
        let mut h = summary.clone();
        h.push(user("the task"));
        for i in 0..10 {
            h.push(calls(&format!("c{i}"), 4));
            h.push(result(&format!("c{i}"), 400));
        }
        let fitted = fit_for_summary(&h, 400, 2);
        assert_eq!(&fitted[..3], &h[..3], "the summary and the task stay");
        assert!(marv_core::estimate_history_tokens(&fitted) <= 400);
        assert_eq!(fitted.last(), h.last(), "the newest steps stay");
        // Never a result without its call.
        for (i, turn) in fitted.iter().enumerate() {
            if let ChatTurn::Tool { call_id, .. } = turn {
                assert!(matches!(&fitted[i - 1], ChatTurn::Assistant { tool_calls, .. } if tool_calls[0].id == *call_id));
            }
        }
        // One giant result is cut to head and tail.
        let h = vec![user("go"), calls("c1", 4), result("c1", 40_000)];
        let fitted = fit_for_summary(&h, 2_000, 0);
        let ChatTurn::Tool { text, .. } = &fitted[2] else { panic!("{fitted:?}") };
        assert!(text.contains("[… cut for the summary …]") && marv_core::estimate_tokens(text) <= 520);
    }

    #[test]
    fn drop_to_fit_keeps_the_summary_and_the_newest_turns_then_the_note() {
        let mut h = vec![user("SUMMARY"), answer(4)];
        for i in 0..10 {
            h.push(calls(&format!("c{i}"), 4));
            h.push(result(&format!("c{i}"), 400));
        }
        let dropped = drop_to_fit(&h, 400, 2, DROPPED_NOTE);
        assert_eq!(&dropped[..2], &h[..2]);
        assert!(!matches!(dropped[2], ChatTurn::Tool { .. }), "never starts at a result");
        assert!(marv_core::estimate_history_tokens(&dropped) <= 400);
        assert!(matches!(dropped.last(), Some(ChatTurn::User { text }) if text == DROPPED_NOTE));
        assert_eq!(dropped[dropped.len() - 2], h[h.len() - 1]);
    }

    #[test]
    fn compacts_at_85_percent_of_a_known_window() {
        assert!(should_compact(85, Some(100)));
        assert!(!should_compact(84, Some(100)));
        assert!(!should_compact(1_000_000, None));
    }
}
