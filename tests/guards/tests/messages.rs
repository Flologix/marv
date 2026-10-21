//! A message the person sends while a turn runs ("send now") goes into the history only at a step boundary: never
//! between a tool call and its result. The APIs reject such a history (every later request would fail), and it would
//! read as the person answering in the tool's place. The guard is `marv_agent::send_now::at_step_boundary`, checked
//! right before the message is appended (`LoopState::deliver_sent_now`).
//!
//! And it never makes two user turns in a row (some APIs reject them, others merge them): it joins a user turn no
//! request has carried yet, and waits behind one a request already carried.
//!
//! Proven: with the `at_step_boundary` check in `can_deliver` removed, `a_history_that_ends_mid_call_keeps_the_message_
//! waiting` fails (the message lands right after the call still waiting for its result); with the merge into the last
//! user turn replaced by a push, `a_message_sent_now_never_makes_two_user_turns_in_a_row` fails.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::StreamExt;
use marv_agent::agent::{AgentOptions, SendNow, ToolRun, ToolRunner, run_agent};
use marv_core::{ChatTurn, MarvError, ToolCall, ToolKind, ToolResult};
use marv_guards::{Scripted, say, tool_call, use_tools};
use serde_json::json;
use tokio_util::sync::CancellationToken;

/// Runs every call (all of them together), and the person sends a message while each one runs.
struct TypingWhileToolsRun(SendNow);

#[async_trait]
impl ToolRunner for TypingWhileToolsRun {
    fn kind(&self, _call: &ToolCall) -> ToolKind {
        ToolKind::Read
    }
    fn concurrent(&self, _call: &ToolCall) -> bool {
        true
    }
    async fn run(&self, call: ToolCall, _cancel: CancellationToken) -> Result<ToolRun, MarvError> {
        let _ = self.0.push(format!("while {} ran", call.id));
        tokio::task::yield_now().await;
        let result = ToolResult { output: format!("ran {}", call.id), summary: None, is_error: false };
        Ok(ToolRun::new(result, call.name))
    }
}

/// Where a user message sits between a tool call and its result in `history`, if anywhere.
fn user_message_inside_a_call(history: &[ChatTurn]) -> Option<usize> {
    let mut waiting: HashSet<&str> = HashSet::new();
    for (i, turn) in history.iter().enumerate() {
        match turn {
            ChatTurn::Assistant { tool_calls, .. } => waiting.extend(tool_calls.iter().map(|c| c.id.as_str())),
            ChatTurn::Tool { call_id, .. } => {
                waiting.remove(call_id.as_str());
            }
            ChatTurn::User { .. } if !waiting.is_empty() => return Some(i),
            ChatTurn::User { .. } => {}
        }
    }
    None
}

/// Where two user turns follow each other in `history`, if anywhere.
fn two_user_turns_in_a_row(history: &[ChatTurn]) -> Option<usize> {
    history.windows(2).position(|w| matches!(w, [ChatTurn::User { .. }, ChatTurn::User { .. }]))
}

fn read(id: &str) -> ToolCall {
    tool_call(id, "read_file", json!({ "path": id }))
}

fn options(provider: Arc<Scripted>, history: Vec<ChatTurn>, send_now: &SendNow) -> (AgentOptions, Arc<Mutex<Vec<ChatTurn>>>) {
    let history = Arc::new(Mutex::new(history));
    let runner = Arc::new(TypingWhileToolsRun(send_now.clone()));
    let mut options = AgentOptions::new(provider, history.clone(), "SYSTEM".into(), Vec::new(), runner, CancellationToken::new());
    options.send_now = Some(send_now.clone());
    (options, history)
}

#[tokio::test]
async fn a_message_sent_during_a_turn_never_lands_between_a_tool_call_and_its_result() {
    let provider = Scripted::new(vec![
        use_tools(vec![read("a"), read("b"), read("c")]),
        use_tools(vec![read("d"), read("e")]),
        say("Done."),
    ]);
    let send_now = SendNow::new();
    let (options, history) = options(provider.clone(), vec![ChatTurn::User { text: "go".into() }], &send_now);
    let events: Vec<_> = run_agent(options).collect().await;
    assert!(!events.is_empty());
    let requests = provider.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 3);
    for (n, (sent, _)) in requests.iter().enumerate() {
        assert_eq!(user_message_inside_a_call(sent), None, "request {n}: {sent:?}");
    }
    let history = history.lock().unwrap().clone();
    assert_eq!(user_message_inside_a_call(&history), None, "{history:?}");
    // Every message was delivered: none was dropped to keep the order.
    let delivered: usize =
        history.iter().map(|t| if let ChatTurn::User { text } = t { text.matches("while ").count() } else { 0 }).sum();
    assert_eq!(delivered, 5);
    assert!(send_now.close().is_empty());
}

#[tokio::test]
async fn a_history_that_ends_mid_call_keeps_the_message_waiting() {
    // A conversation cut short with a call still waiting for its result, and a message already sent: it must not go
    // in right after that call.
    let waiting = ChatTurn::Assistant { text: String::new(), tool_calls: vec![read("x")], reasoning: None };
    let provider = Scripted::new(vec![say("Ok."), say("Ok, noted.")]);
    let send_now = SendNow::new();
    send_now.push("and also this".into()).unwrap();
    let (options, _history) = options(provider.clone(), vec![ChatTurn::User { text: "go".into() }, waiting], &send_now);
    let _: Vec<_> = run_agent(options).collect().await;
    let requests = provider.requests.lock().unwrap().clone();
    for (n, (sent, _)) in requests.iter().enumerate() {
        assert_eq!(user_message_inside_a_call(sent), None, "request {n}: {sent:?}");
    }
    // It never found a boundary: it comes back to the person, never dropped.
    assert_eq!(send_now.close(), ["and also this"]);
}

#[tokio::test]
async fn a_message_sent_now_never_makes_two_user_turns_in_a_row() {
    // Sent before the first request (after the turn's own message), during each step's calls (after a check's note),
    // and during a plain answer: every request is free of two user turns in a row.
    let provider = Scripted::new(vec![use_tools(vec![read("a"), read("b")]), say("Done."), say("Done again.")]);
    let send_now = SendNow::new();
    send_now.push("before the first request".into()).unwrap();
    let (mut options, history) = options(provider.clone(), vec![ChatTurn::User { text: "go".into() }], &send_now);
    options.after_step = Some(Arc::new(|_| Box::pin(async { Some("(Automatic note from Marv) a check's note".to_string()) })));
    let typed = send_now.clone();
    let mut stream = std::pin::pin!(run_agent(options));
    while let Some(event) = stream.next().await {
        if matches!(&event, marv_agent::LoopEvent::TextDelta(text) if text == "Done.") {
            typed.push("during the answer".into()).unwrap();
        }
    }
    let requests = provider.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 3);
    for (n, (sent, _)) in requests.iter().enumerate() {
        assert_eq!(two_user_turns_in_a_row(sent), None, "request {n}: {sent:?}");
        assert_eq!(user_message_inside_a_call(sent), None, "request {n}: {sent:?}");
    }
    // Everything arrived.
    let all = format!("{:?}", history.lock().unwrap());
    for text in ["before the first request", "while a ran", "while b ran", "during the answer"] {
        assert!(all.contains(text), "{text}: {all}");
    }
}
