//! A subagent's own transcript. Ports `src/ui/AgentView.tsx` and the log it shows, `src/agent-log.ts`
//! (tests: `tests/agent-log.test.ts`).
//!
//! A click on a subagent's entry opens its view: its task, what it said and thought, its tool calls,
//! live while it runs (the same `MessageView`s, a streaming reply, the waiting line, a live draft).
//! [`AgentViewHeader`] is pinned above it: `● agent <title> · running|finished` and "esc to go back".
//! The log is built from the same events as the main transcript (`UiEvent::Subagent`) but kept
//! apart: only the subagent's final report reaches the parent's conversation, and none of this
//! reaches any model. Reasoning collapses to `✻ Thought for Ns` as in the main transcript; what
//! streamed before an error or stop isn't lost. The title goes through `printable()`.
//!
//! Marv kept the log's bookkeeping (next id, open tool calls, when thinking began) in a `WeakMap`
//! beside it; here they are private fields. [`AgentView`] holds what Marv's mounted components
//! held: the reply's stream lexer, the waiting line's clock, and each entry's lines (`memo`).
//!
//! Gap: `UiEvent::ToolStart` carries no arguments, so the App can't give [`AgentLog::new`] the
//! task (Marv's first user message) until the engine sends it; an empty prompt adds no message.

use std::collections::HashMap;
use std::time::Instant;

use marv_core::{StopReason, printable};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use super::markdown::StreamLexer;
use super::message_view;
use super::select::truncate_end;
use super::theme;
use super::thinking_view::{DraftView, ThinkingView};
use crate::view::{Message, Role, ToolState, ToolStatus, UiEvent};

/// The header's rows: its line and the blank line under it (Marv's `marginBottom`).
pub const HEADER_HEIGHT: u16 = 2;

/// A subagent's log (Marv `AgentLog`).
#[derive(Debug, Clone, Default)]
pub struct AgentLog {
    /// "general-purpose · Read notes"
    pub title: String,
    /// Finished entries.
    pub messages: Vec<Message>,
    /// The reply streaming in, and any reasoning before it.
    pub streaming: String,
    pub thinking: String,
    /// A tool call still streaming in (the latest), shown as it's written: (index, name, args).
    pub draft: Option<(usize, String, String)>,
    pub running: bool,
    /// Bumped on every change so the view knows to draw again (Marv's `version`).
    pub version: u64,
    next_id: u64,
    /// Each running call's entry (index into `messages`).
    tools: HashMap<String, usize>,
    thinking_since: Option<Instant>,
}

fn message(role: Role, text: String) -> Message {
    Message { id: 0, role, text, is_error: false, markdown: false, notice: None, tool: None }
}

/// Events after which a streaming tool call is done streaming (as in the App).
fn is_step_end(event: &UiEvent) -> bool {
    crate::app::is_step_end(event)
}

/// What a subagent's run ending says, if anything (Marv `STOPPED`).
fn stopped(reason: StopReason) -> Option<&'static str> {
    match reason {
        StopReason::Aborted => Some("Interrupted."),
        StopReason::Declined => Some("Stopped: the user declined."),
        StopReason::Length => Some("The reply was cut off at the model's output limit, so nothing in it ran."),
        StopReason::MaxSteps => Some("It hit its step limit."),
        _ => None,
    }
}

impl AgentLog {
    /// A running subagent's log, its task as the first (user) message (Marv `createAgentLog`).
    pub fn new(title: String, prompt: String) -> Self {
        let mut log = Self { title, running: true, ..Self::default() };
        if !prompt.is_empty() {
            log.add(message(Role::User, prompt));
        }
        log
    }

    fn add(&mut self, message: Message) {
        self.messages.push(Message { id: self.next_id, ..message });
        self.next_id += 1;
    }

    fn system(&mut self, text: impl Into<String>, is_error: bool) {
        self.add(Message { is_error, ..message(Role::System, text.into()) });
    }

    /// Like the main transcript: reasoning collapses to one line once the step moves on.
    fn note_thought(&mut self, now: Instant) {
        if !self.thinking.is_empty() {
            let since = self.thinking_since.unwrap_or(now);
            // Marv's `Math.max(1, Math.round(ms / 1000))`.
            let seconds = ((now.saturating_duration_since(since).as_millis() as u64 + 500) / 1000).max(1);
            self.system(format!("✻ Thought for {seconds}s"), false);
        }
        self.thinking.clear();
        self.thinking_since = None;
    }

    /// What streamed before an error or a stop isn't lost.
    fn keep_partial(&mut self) {
        if !self.streaming.is_empty() {
            let text = std::mem::take(&mut self.streaming);
            self.add(message(Role::Assistant, text));
        }
    }

    /// Folds one event into the log (Marv `applyEvent`).
    pub fn apply(&mut self, event: &UiEvent, now: Instant) {
        if is_step_end(event) {
            self.draft = None;
        }
        self.version += 1;
        match event {
            UiEvent::ToolCallDelta { index, name, text } => {
                let draft = match &mut self.draft {
                    Some(d) if d.0 == *index => d,
                    _ => self.draft.insert((*index, name.clone(), String::new())),
                };
                if !name.is_empty() {
                    draft.1.clone_from(name);
                }
                draft.2.push_str(text);
            }
            UiEvent::ThinkingDelta(text) => {
                self.thinking_since.get_or_insert(now);
                self.thinking.push_str(text);
            }
            UiEvent::TextDelta(text) => self.streaming.push_str(text),
            UiEvent::TextLoop { continued } => {
                self.streaming.clear();
                self.system(
                    if *continued {
                        "✻ Stuck repeating itself: the reply was stopped and dropped, and the model was told."
                    } else {
                        "Stopped: still stuck repeating itself."
                    },
                    false,
                );
            }
            UiEvent::ToolLoop { continued } => self.system(
                if *continued {
                    "✻ The same tool call kept getting the same result: answered with a note instead."
                } else {
                    "Stopped: still repeating the same tool call."
                },
                false,
            ),
            UiEvent::Retry { message: text, .. } => {
                // The provider asks again from the start: drop what the failed attempt drew, and say why.
                self.streaming.clear();
                self.thinking.clear();
                self.thinking_since = None;
                self.draft = None;
                self.system(text.clone(), false);
            }
            UiEvent::Assistant { text } => {
                self.note_thought(now);
                self.add(message(Role::Assistant, text.clone()));
                self.streaming.clear();
            }
            UiEvent::ToolStart { call_id, name, label } => {
                self.note_thought(now);
                self.keep_partial();
                self.tools.insert(call_id.clone(), self.messages.len());
                let status =
                    ToolStatus { label: label.clone(), status: ToolState::Running, summary: None, steps: None, diff: None };
                self.add(Message { tool: Some(status), ..message(Role::Tool, name.clone()) });
            }
            UiEvent::ToolEnd { call_id, status, .. } => {
                // The engine already made a plain failure's summary its message, as for the main transcript.
                if let Some(index) = self.tools.remove(call_id)
                    && let Some(m) = self.messages.get_mut(index)
                {
                    m.tool = Some(status.clone());
                }
            }
            UiEvent::Error(text) => {
                self.note_thought(now);
                self.keep_partial();
                self.system(text.clone(), true);
            }
            UiEvent::Done(reason) => {
                self.note_thought(now);
                self.keep_partial();
                if let Some(text) = stopped(*reason) {
                    self.system(text, false);
                }
                self.running = false;
            }
            _ => {}
        }
    }

    /// The last entry is a tool call still running (its own line says so: no waiting line).
    fn tool_running(&self) -> bool {
        self.messages.last().and_then(|m| m.tool.as_ref()).is_some_and(|t| t.status == ToolState::Running)
    }
}

/// Pinned above a subagent's view: whose it is, and the way back.
pub struct AgentViewHeader<'a> {
    pub log: &'a AgentLog,
}

impl AgentViewHeader<'_> {
    const BACK: &'static str = "esc to go back";

    pub fn line(&self, width: u16) -> Line<'static> {
        // paddingX 1; the hint keeps its width and 2 columns before it; the title is cut.
        let inner = width.saturating_sub(2);
        let back = Self::BACK.width() as u16;
        let title = truncate_end(
            Line::from(vec![
                Span::styled("● agent ", Style::new().fg(theme::ACCENT)),
                Span::styled(printable(&self.log.title), Style::new().add_modifier(Modifier::BOLD)),
                Span::styled(
                    format!(" · {}", if self.log.running { "running" } else { "finished" }),
                    Style::new().fg(theme::DIM),
                ),
            ]),
            inner.saturating_sub(back + 2),
        );
        let gap = (inner as usize).saturating_sub(title.width() + back as usize);
        let mut spans = vec![Span::raw(" ")];
        spans.extend(title.spans);
        spans.push(Span::raw(" ".repeat(gap)));
        if inner >= back {
            spans.push(Span::styled(Self::BACK, Style::new().fg(theme::DIM)));
        }
        Line::from(spans)
    }

    pub fn render(&self, area: Rect, buf: &mut Buffer) {
        if area.height > 0 {
            buf.set_line(area.x, area.y, &self.line(area.width), area.width);
        }
    }
}

/// A subagent's own transcript: its task, what it said and thought, and its tool calls, live while
/// it runs. Holds what the view keeps between frames.
#[derive(Debug, Default)]
pub struct AgentView {
    /// The streaming reply's lexer.
    lexer: StreamLexer,
    /// The waiting line (or draft) shown, and whether it's a draft: its clock starts when it appears.
    waiting: Option<(bool, ThinkingView)>,
    /// Each entry's lines: (the message they show, width, show_steps, lines).
    cache: Vec<(Message, u16, bool, Vec<Line<'static>>)>,
}

impl AgentView {
    /// The log as lines `width` wide. ctrl+o (`show_steps`) shows whole diffs.
    pub fn render(&mut self, log: &AgentLog, width: u16, show_steps: bool, now: Instant) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        self.cache.truncate(log.messages.len());
        for (i, message) in log.messages.iter().enumerate() {
            // Only entries with a diff get the flag, so ctrl+o lays out just those.
            let steps = show_steps && message.tool.as_ref().is_some_and(|t| t.diff.is_some());
            let fresh = self.cache.get(i).is_some_and(|(m, w, s, _)| m == message && *w == width && *s == steps);
            if !fresh {
                let entry = (message.clone(), width, steps, message_view::render(message, width, steps, None));
                if i < self.cache.len() {
                    self.cache[i] = entry;
                } else {
                    self.cache.push(entry);
                }
            }
            lines.extend(self.cache[i].3.iter().cloned());
        }
        let waiting = log.running && log.streaming.is_empty() && !log.tool_running();
        let draft = log.draft.as_ref().filter(|_| log.running && !log.tool_running());
        if !log.streaming.is_empty() {
            let reply = message(Role::Assistant, log.streaming.clone());
            lines.extend(message_view::render(&reply, width, false, Some(&mut self.lexer)));
        } else {
            self.lexer = StreamLexer::new();
        }
        let shown = if let Some((_, name, args)) = draft {
            let view = self.waiting_view(true, now);
            lines.extend(DraftView::render(view, name, args, now, width));
            true
        } else if waiting {
            let view = self.waiting_view(false, now);
            lines.extend(view.render(&log.thinking, None, now, width));
            true
        } else {
            false
        };
        if !shown {
            self.waiting = None;
        }
        lines
    }

    fn waiting_view(&mut self, draft: bool, now: Instant) -> &ThinkingView {
        if self.waiting.as_ref().is_none_or(|(d, _)| *d != draft) {
            self.waiting = Some((draft, ThinkingView::new("Thinking…", now)));
        }
        &self.waiting.as_ref().expect("set above").1
    }

    /// The waiting line's next spinner frame, while it's shown.
    pub fn deadline(&self, now: Instant) -> Option<Instant> {
        self.waiting.as_ref().map(|(_, view)| view.deadline(now))
    }

    /// When the waiting line shown appeared (its spinner's clock), if one is.
    pub fn waiting_since(&self) -> Option<Instant> {
        self.waiting.as_ref().map(|(_, view)| view.since)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::view::{DiffKind, DiffLine, DiffShown};
    use std::time::Duration;

    fn log() -> AgentLog {
        AgentLog::new("t".into(), "p".into())
    }

    fn start(id: &str, name: &str, label: &str) -> UiEvent {
        UiEvent::ToolStart { call_id: id.into(), name: name.into(), label: label.into() }
    }

    fn end(id: &str, name: &str, status: ToolStatus) -> UiEvent {
        UiEvent::ToolEnd { call_id: id.into(), name: name.into(), status }
    }

    fn status(label: &str, state: ToolState, summary: &str) -> ToolStatus {
        ToolStatus { label: label.into(), status: state, summary: Some(summary.into()), steps: None, diff: None }
    }

    fn roles(log: &AgentLog) -> Vec<(Role, String, bool)> {
        log.messages.iter().map(|m| (m.role, m.text.clone(), m.is_error)).collect()
    }

    #[test]
    fn starts_with_its_task_as_the_users_message() {
        let log = AgentLog::new("general-purpose · Read notes".into(), "What's in notes.txt?".into());
        assert_eq!(roles(&log), [(Role::User, "What's in notes.txt?".into(), false)]);
        assert!(log.running);
        assert!(AgentLog::new("t".into(), String::new()).messages.is_empty());
    }

    #[test]
    fn streams_text_and_thinking_then_keeps_each_finished_reply() {
        let t = Instant::now();
        let mut log = log();
        log.apply(&UiEvent::ThinkingDelta("hmm".into()), t);
        assert_eq!(log.thinking, "hmm");
        log.apply(&UiEvent::TextDelta("Look".into()), t);
        log.apply(&UiEvent::TextDelta("ing.".into()), t);
        assert_eq!(log.streaming, "Looking.");
        log.apply(&UiEvent::Assistant { text: "Looking.".into() }, t + Duration::from_millis(2400));
        assert_eq!((log.streaming.as_str(), log.thinking.as_str()), ("", ""));
        assert_eq!(
            roles(&log),
            [
                (Role::User, "p".into(), false),
                (Role::System, "✻ Thought for 2s".into(), false),
                (Role::Assistant, "Looking.".into(), false)
            ]
        );
    }

    #[test]
    fn a_tool_call_is_one_entry_updated_when_it_ends() {
        let t = Instant::now();
        let mut log = log();
        log.apply(&start("r1", "read_file", "a.ts"), t);
        let entry = log.messages.last().unwrap();
        assert_eq!((entry.role, entry.text.as_str()), (Role::Tool, "read_file"));
        assert_eq!(entry.tool.as_ref().unwrap().status, ToolState::Running);
        let version = log.version;
        log.apply(&end("r1", "read_file", status("a.ts", ToolState::Done, "12 lines")), t);
        assert_eq!(log.messages.len(), 2);
        let tool = log.messages[1].tool.as_ref().unwrap();
        assert_eq!((tool.status, tool.summary.as_deref()), (ToolState::Done, Some("12 lines")));
        assert!(log.version > version, "the view knows to draw it again");
    }

    #[test]
    fn a_failed_call_shows_its_message_a_declined_one_says_so() {
        let t = Instant::now();
        let mut log = log();
        log.apply(&start("r1", "read_file", "a.ts"), t);
        log.apply(&end("r1", "read_file", status("a.ts", ToolState::Error, "File not found: a.ts")), t);
        assert_eq!(log.messages[1].tool.as_ref().unwrap().summary.as_deref(), Some("File not found: a.ts"));
        log.apply(&start("w1", "write_file", "b.ts"), t);
        log.apply(&end("w1", "write_file", status("b.ts", ToolState::Declined, "declined")), t);
        assert_eq!(log.messages[2].tool.as_ref().unwrap().status, ToolState::Declined);
    }

    #[test]
    fn a_subagents_edit_keeps_its_diff_for_its_view() {
        let t = Instant::now();
        let mut log = log();
        let diff = DiffShown {
            lines: vec![DiffLine { kind: DiffKind::Add, text: "x".into(), old_line: None, new_line: Some(1) }],
            more: 0,
        };
        log.apply(&start("c1", "edit_file", "a.txt"), t);
        log.apply(
            &end("c1", "edit_file", ToolStatus { diff: Some(diff.clone()), ..status("a.txt", ToolState::Done, "+1 −0") }),
            t,
        );
        assert_eq!(log.messages[1].tool.as_ref().unwrap().diff, Some(diff));
    }

    #[test]
    fn errors_and_how_it_stopped_are_shown() {
        let t = Instant::now();
        let mut log = log();
        log.apply(&UiEvent::TextDelta("half a rep".into()), t);
        log.apply(&UiEvent::Error("connection reset".into()), t);
        log.apply(&UiEvent::Done(StopReason::Aborted), t);
        assert_eq!(
            roles(&log)[1..],
            [
                (Role::Assistant, "half a rep".into(), false), // what had streamed isn't lost
                (Role::System, "connection reset".into(), true),
                (Role::System, "Interrupted.".into(), false),
            ]
        );
        assert!(!log.running);
    }

    #[test]
    fn a_tool_call_still_streaming_is_a_draft_until_the_call_starts() {
        let t = Instant::now();
        let mut log = log();
        let delta = |index, name: &str, text: &str| UiEvent::ToolCallDelta { index, name: name.into(), text: text.into() };
        log.apply(&delta(0, "write_file", r#"{"path":"a.js","#), t);
        log.apply(&delta(0, "write_file", r#""content":"x"#), t);
        assert_eq!(log.draft, Some((0, "write_file".into(), r#"{"path":"a.js","content":"x"#.into())));
        log.apply(&delta(1, "bash", "{"), t);
        assert_eq!(log.draft, Some((1, "bash".into(), "{".into())));
        log.apply(&start("c1", "write_file", "a.js"), t);
        assert_eq!(log.draft, None);
    }

    #[test]
    fn a_retry_drops_what_the_failed_attempt_drew_and_says_why() {
        let t = Instant::now();
        let mut log = log();
        log.apply(&UiEvent::ThinkingDelta("hmm".into()), t);
        log.apply(&UiEvent::TextDelta("Half a rep".into()), t);
        log.apply(&UiEvent::ToolCallDelta { index: 0, name: "read_file".into(), text: r#"{"pa"#.into() }, t);
        let message = "connection lost, retrying (1/2)…".to_string();
        log.apply(&UiEvent::Retry { attempt: 1, max: 2, message: message.clone(), continued: true }, t);
        assert_eq!((log.streaming.as_str(), log.thinking.as_str(), &log.draft), ("", "", &None));
        assert_eq!(roles(&log).last().unwrap(), &(Role::System, message, false));
    }

    #[test]
    fn loop_warnings_and_stops_are_shown() {
        let t = Instant::now();
        let mut log = log();
        log.apply(&UiEvent::TextDelta("Fix it now. Fix it now.".into()), t);
        log.apply(&UiEvent::TextLoop { continued: true }, t);
        assert_eq!(log.streaming, "");
        assert!(log.messages.last().unwrap().text.contains("repeating itself"));
        log.apply(&UiEvent::ToolLoop { continued: false }, t);
        assert!(log.messages.last().unwrap().text.starts_with("Stopped"));
    }

    // The view

    fn text(lines: &[Line]) -> Vec<String> {
        lines.iter().map(|l| l.to_string().trim_end().to_string()).collect()
    }

    #[test]
    fn the_view_shows_entries_the_streaming_reply_and_the_waiting_line() {
        let t = Instant::now();
        let mut log = log();
        let mut view = AgentView::default();
        assert_eq!(text(&view.render(&log, 60, false, t)), ["> p", "", "⠋ Thinking…", ""]);
        assert_eq!(view.deadline(t), Some(t + Duration::from_millis(80)));
        log.apply(&UiEvent::TextDelta("Some **bold**".into()), t);
        assert_eq!(text(&view.render(&log, 60, false, t)), ["> p", "", "● Some bold", ""]);
        assert_eq!(view.deadline(t), None, "no waiting line while the reply streams");
        log.apply(&start("r1", "read_file", "a.ts"), t);
        assert_eq!(text(&view.render(&log, 60, false, t))[2..], ["● Some bold", "", "● read_file a.ts", "  ⎿ running…", ""]);
        log.apply(&end("r1", "read_file", status("a.ts", ToolState::Done, "1 line")), t);
        log.apply(
            &UiEvent::ToolCallDelta { index: 0, name: "write_file".into(), text: r#"{"path":"b.js","content":"x = 1"#.into() },
            t,
        );
        let lines = text(&view.render(&log, 60, false, t + Duration::from_secs(2)));
        // The draft's clock starts when it appears.
        assert_eq!(lines[lines.len() - 3..], ["⠋ Writing b.js… (~8 tokens)", "  x = 1", ""]);
        log.apply(&UiEvent::Done(StopReason::End), t);
        let lines = text(&view.render(&log, 60, false, t));
        assert_eq!(lines.last().unwrap(), "");
        assert!(!lines.iter().any(|l| l.contains("Thinking") || l.contains("Writing")));
    }

    #[test]
    fn the_header_says_whose_view_and_the_way_back() {
        let mut log = AgentLog::new("general-purpose · Read notes\x1b[2J".into(), String::new());
        let line = AgentViewHeader { log: &log }.line(70);
        assert_eq!(line.to_string(), " ● agent general-purpose · Read notes · running        esc to go back");
        assert_eq!(line.width(), 69);
        log.running = false;
        // Narrow: the title is cut, the way back keeps its width and 2 columns before it.
        let narrow = AgentViewHeader { log: &log }.line(36);
        assert_eq!(narrow.to_string(), " ● agent general-p…  esc to go back");
        assert_eq!(narrow.width(), 35);
    }
}
