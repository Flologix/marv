//! A session's events as the page reads them: `marv -p --output stream-json`'s shapes, plus call ids (to pair a
//! tool's start and end), edit diffs, and subagents' tool calls.

use marv_agent::agent::LoopEvent;
use marv_core::printable;
use marv_session::{CompactResult, SessionEvent, TurnStatus};
use marv_tools::types::DiffShown;
use serde_json::{Value, json};

/// The most of a tool's output the page gets: the model may read more, the page only shows it.
const MAX_RESULT: usize = 20_000;

fn clip(text: &str) -> String {
    let text = printable(text);
    if text.len() <= MAX_RESULT {
        return text;
    }
    let mut end = MAX_RESULT;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n… ({} more bytes)", &text[..end], text.len() - end)
}

pub fn to_json(event: &SessionEvent) -> Option<Value> {
    match event {
        SessionEvent::TurnStart { .. } => Some(json!({"type": "turn_start"})),
        SessionEvent::Loop(e) => loop_json(e, None),
        // A subagent's tool calls and errors, not its text: its report comes back as the agent tool's result.
        SessionEvent::Subagent { call_id, event } => match event {
            LoopEvent::ToolStart { .. } | LoopEvent::ToolEnd { .. } | LoopEvent::ToolSkipped { .. } | LoopEvent::Error(_) => {
                loop_json(event, Some(call_id))
            }
            _ => None,
        },
        SessionEvent::Status(status) => Some(json!({"type": "status", "status": match status {
            TurnStatus::WaitingForMcp => "waiting_for_mcp",
            TurnStatus::Compacting => "compacting",
            TurnStatus::Checking => "checking",
            TurnStatus::Checkpointing => "checkpointing",
            TurnStatus::Running => "running",
        }})),
        SessionEvent::Compaction(result) => Some(match result {
            CompactResult::Compacted { .. } => json!({"type": "compaction", "compacted": true}),
            CompactResult::NotCompacted { reason, error } => {
                json!({"type": "compaction", "compacted": false, "reason": reason.as_str(), "error": error})
            }
        }),
        SessionEvent::Tainted { source } => Some(json!({"type": "tainted", "source": source})),
        SessionEvent::CheckpointFailed { message } => Some(json!({"type": "notice", "text": message})),
        SessionEvent::TurnEnd { reason, .. } => Some(json!({"type": "turn_end", "reason": reason.as_str()})),
        _ => None,
    }
}

fn loop_json(event: &LoopEvent, agent: Option<&String>) -> Option<Value> {
    let mut line = match event {
        LoopEvent::TextDelta(text) => json!({"type": "text_delta", "text": text}),
        LoopEvent::ThinkingDelta(text) => json!({"type": "thinking_delta", "text": text}),
        LoopEvent::Assistant { text } => json!({"type": "assistant", "text": text}),
        LoopEvent::ToolStart { call, label } => {
            json!({"type": "tool_start", "id": call.id, "tool": call.name, "label": printable(label)})
        }
        LoopEvent::ToolEnd { call, run } => {
            let mut line = json!({
                "type": "tool_end",
                "id": call.id,
                "tool": call.name,
                "label": printable(&run.label),
                "result": clip(&run.result.output),
                "summary": run.result.summary,
                "error": run.result.is_error,
                "approval": run.approval,
            });
            if let Some(diff) = run.display.as_ref().and_then(|d| d.downcast_ref::<DiffShown>()) {
                line["diff"] = json!(diff);
            }
            line
        }
        LoopEvent::ToolSkipped { call, output } => {
            json!({"type": "tool_skipped", "id": call.id, "tool": call.name, "reason": printable(output)})
        }
        LoopEvent::Error(message) => json!({"type": "error", "message": printable(message)}),
        LoopEvent::StepLimit { steps, continued } => json!({"type": "step_limit", "steps": steps, "continued": continued}),
        LoopEvent::CutOff { continued } => json!({"type": "cut_off", "continued": continued}),
        LoopEvent::TextLoop { continued } => json!({"type": "text_loop", "continued": continued}),
        LoopEvent::ToolLoop { continued } => json!({"type": "tool_loop", "continued": continued}),
        _ => return None,
    };
    if let Some(agent) = agent {
        line["agent"] = json!(agent);
    }
    Some(line)
}
