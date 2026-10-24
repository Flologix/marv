//! A session's events as the page reads them: `marv -p --output stream-json`'s shapes, plus call ids (to pair a
//! tool's start and end), edit diffs, and subagents' tool calls.

use marv_agent::agent::LoopEvent;
use marv_core::printable;
use marv_session::{SessionEvent, TurnStatus};
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

/// The most of a checkpoint failure's own text the page gets.
const MAX_DETAIL: usize = 160;

/// `text`'s first non-empty line, printable, at most `max` bytes (cut on a character, with "…").
fn first_line(text: &str, max: usize) -> String {
    let line = printable(text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("no reason given"));
    if line.len() <= max {
        return line;
    }
    let mut end = max;
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", line[..end].trim_end())
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
        // A running call's status line (#201: a mesh send waiting for another Marv's person; a subagent's progress),
        // shown on its card until it ends.
        SessionEvent::SubagentProgress { call_id, progress } => {
            Some(json!({"type": "tool_progress", "id": call_id, "line": printable(&progress.line)}))
        }
        SessionEvent::Status(status) => Some(json!({"type": "status", "status": match status {
            TurnStatus::WaitingForMcp => "waiting_for_mcp",
            TurnStatus::Compacting => "compacting",
            TurnStatus::Checking => "checking",
            TurnStatus::Checkpointing => "checkpointing",
            TurnStatus::Running => "running",
        }})),
        SessionEvent::Compaction(result) => Some(marv_session::compaction_json(result)),
        SessionEvent::Tainted { source } => Some(json!({"type": "tainted", "source": source})),
        // #203: the TUI's sentence, as a warning; git's own text cut to its first line (it can run to a page of hints,
        // and as a plain notice above the turn's tool cards it read like a command's output).
        SessionEvent::CheckpointFailed { message } => Some(json!({
            "type": "checkpoint_failed",
            "text": format!("This turn can't be undone: its checkpoint failed ({}).", first_line(message, MAX_DETAIL)),
        })),
        // "Send now" messages the turn never read: the page puts them back in the composer.
        SessionEvent::NotSent { texts } => Some(json!({"type": "not_sent", "texts": texts})),
        SessionEvent::Notice { text } => Some(json!({"type": "notice", "text": text})),
        SessionEvent::TurnEnd { reason, .. } => Some(json!({"type": "turn_end", "reason": reason.as_str()})),
        _ => None,
    }
}

fn loop_json(event: &LoopEvent, agent: Option<&String>) -> Option<Value> {
    let mut line = match event {
        LoopEvent::TextDelta(text) => json!({"type": "text_delta", "text": text}),
        LoopEvent::ThinkingDelta(text) => json!({"type": "thinking_delta", "text": text}),
        LoopEvent::Assistant { text } => json!({"type": "assistant", "text": text}),
        // "Send now": the person's message went in here, after the step's tool results.
        LoopEvent::SentNow { text } => json!({"type": "sent_now", "text": text}),
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

#[cfg(test)]
mod tests {
    use super::*;

    /// #201: a running call's status line (a mesh send waiting for another Marv's person) reaches the page, printable.
    #[test]
    fn a_calls_status_line_reaches_the_page() {
        let event = SessionEvent::SubagentProgress {
            call_id: "c1".into(),
            progress: marv_tools::AgentProgress { line: "waiting for beta to accept the wire\u{1b}[2J".into(), steps: vec![] },
        };
        let shown = to_json(&event).unwrap();
        assert_eq!(shown["type"], "tool_progress");
        assert_eq!(shown["id"], "c1");
        assert!(shown["line"].as_str().unwrap().starts_with("waiting for beta to accept the wire"));
        assert!(!shown["line"].as_str().unwrap().contains('\u{1b}'));
    }

    // #203: a warning in the TUI's words, with git's text cut to its first line and bounded.
    #[test]
    fn a_failed_checkpoint_is_a_typed_warning_with_gits_first_line() {
        let message = "error: 'kernel-lab-out/' does not have a commit checked out\nerror: unable to index file\nfatal: adding files failed";
        let line = to_json(&SessionEvent::CheckpointFailed { message: message.into() }).unwrap();
        assert_eq!(
            line,
            json!({"type": "checkpoint_failed", "text": "This turn can't be undone: its checkpoint failed (error: 'kernel-lab-out/' does not have a commit checked out)."})
        );
        let long = format!("\n\n  {}\u{1b}[31m\nmore", "é".repeat(200));
        let text = to_json(&SessionEvent::CheckpointFailed { message: long }).unwrap()["text"].as_str().unwrap().to_string();
        assert!(!text.contains('\u{1b}') && !text.contains("more") && text.contains('…'), "{text}");
        assert!(text.len() < 260, "{}", text.len());
        assert_eq!(first_line(" \n", 40), "no reason given");
    }
}
