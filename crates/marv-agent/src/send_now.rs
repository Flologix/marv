//! "Send now" (new in marv-rs): the person's message, typed while a turn runs, delivered to the running agent at
//! the **next step boundary** instead of after the turn.
//!
//! A step boundary is the point between steps: the model's reply is in and every call it made has its result in
//! the history, and the next request hasn't been sent. That's the only place the loop appends one
//! ([`crate::agent`]'s `Top`, and a plain answer that would otherwise end the run), so:
//! - **nothing in flight is stopped**: a running tool call finishes (only a stop preempts it);
//! - **it's an ordinary user message, appended**: the history stays append-only and nothing before it is
//!   re-serialized, so the prompt cache holds;
//! - **it never lands between a tool call and its result**: [`at_step_boundary`] is checked again right before the
//!   append, whatever the loop's phase says, and a history that isn't at one keeps the message waiting.
//!
//! What's still waiting when the turn ends (it was stopped, declined, hit a limit, or ended before the next step)
//! is handed back by [`SendNow::close`], never sent later on its own: the client puts it back in the prompt.

use std::collections::{HashSet, VecDeque};
use std::sync::{Arc, Mutex};

use marv_core::ChatTurn;

/// The running turn's "send now" messages, shared by the client (which pushes) and the loop (which takes them at a
/// step boundary). Cheap to clone.
#[derive(Debug, Clone, Default)]
pub struct SendNow {
    inner: Arc<Mutex<State>>,
}

#[derive(Debug, Default)]
struct State {
    waiting: VecDeque<String>,
    closed: bool,
}

impl SendNow {
    pub fn new() -> Self {
        Self::default()
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Queues `text` for the next step boundary. `Err(text)` once the turn has closed it (it ended): the text is the
    /// caller's again.
    pub fn push(&self, text: String) -> Result<(), String> {
        let mut s = self.state();
        if s.closed {
            return Err(text);
        }
        s.waiting.push_back(text);
        Ok(())
    }

    /// Nothing is waiting.
    pub fn is_empty(&self) -> bool {
        self.state().waiting.is_empty()
    }

    /// Everything waiting, in the order it was sent.
    pub(crate) fn take(&self) -> Vec<String> {
        self.state().waiting.drain(..).collect()
    }

    /// The turn is over: nothing more is taken, and what was never delivered comes back, in order.
    pub fn close(&self) -> Vec<String> {
        let mut s = self.state();
        s.closed = true;
        s.waiting.drain(..).collect()
    }
}

/// Whether a user message may be appended to `history` now: every tool call of the last reply that made calls has
/// its result after it. Appending anywhere else would put the message between a call and its result, which the APIs
/// reject (and which would read as the person answering in the tool's place). A plain reply or a note after a call
/// still waiting for its result (a conversation cut short) doesn't make it a boundary.
pub fn at_step_boundary(history: &[ChatTurn]) -> bool {
    let mut answered = HashSet::new();
    for turn in history.iter().rev() {
        match turn {
            ChatTurn::Tool { call_id, .. } => {
                answered.insert(call_id.as_str());
            }
            ChatTurn::Assistant { tool_calls, .. } if !tool_calls.is_empty() => {
                return tool_calls.iter().all(|c| answered.contains(c.id.as_str()));
            }
            ChatTurn::Assistant { .. } | ChatTurn::User { .. } => {}
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use marv_core::ToolCall;

    fn call(id: &str) -> ToolCall {
        ToolCall { id: id.into(), name: "read_file".into(), arguments: "{}".into() }
    }

    fn calls(ids: &[&str]) -> ChatTurn {
        ChatTurn::Assistant { text: String::new(), tool_calls: ids.iter().map(|id| call(id)).collect(), reasoning: None }
    }

    fn result(id: &str) -> ChatTurn {
        ChatTurn::Tool { call_id: id.into(), name: "read_file".into(), text: "ok".into() }
    }

    #[test]
    fn a_boundary_is_where_every_call_of_the_last_reply_has_its_result() {
        let user = ChatTurn::User { text: "hi".into() };
        assert!(at_step_boundary(&[]));
        assert!(at_step_boundary(std::slice::from_ref(&user)));
        assert!(!at_step_boundary(&[user.clone(), calls(&["a", "b"])]));
        assert!(!at_step_boundary(&[user.clone(), calls(&["a", "b"]), result("a")]));
        assert!(at_step_boundary(&[user.clone(), calls(&["a", "b"]), result("a"), result("b")]));
        // Results in any order, and a note after them (a check's), still a boundary.
        assert!(at_step_boundary(&[user.clone(), calls(&["a", "b"]), result("b"), result("a"), user.clone()]));
        let plain = ChatTurn::Assistant { text: "done".into(), tool_calls: Vec::new(), reasoning: None };
        assert!(at_step_boundary(&[user.clone(), calls(&["a"]), result("a"), plain.clone()]));
        // A reply after a call that never got its result: still inside that call.
        assert!(!at_step_boundary(&[user, calls(&["a"]), plain]));
    }

    #[test]
    fn messages_wait_in_order_and_come_back_once_closed() {
        let q = SendNow::new();
        assert!(q.is_empty());
        q.push("one".into()).unwrap();
        q.push("two".into()).unwrap();
        assert_eq!(q.take(), vec!["one", "two"]);
        assert!(q.is_empty());
        q.push("three".into()).unwrap();
        assert_eq!(q.close(), vec!["three"]);
        assert_eq!(q.push("late".into()), Err("late".into()));
        assert!(q.close().is_empty());
    }
}
