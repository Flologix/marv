//! The browser as Marv's approver: each request goes to the page, and the call waits for the person's answer.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};

use async_trait::async_trait;
use marv_tools::types::{ApprovalRequest, Approver, Decision};
use serde_json::{Value, json};
use tokio::sync::{mpsc::UnboundedSender, oneshot};

struct Pending {
    answer: oneshot::Sender<Decision>,
    /// Yes or no only: an "always" counts as one yes.
    once: bool,
    /// A reason may come with a no (#185: a model's call); otherwise a decline with a reason is a plain no.
    reason: bool,
}

/// Below 2^53, so the page (JavaScript numbers) reads every id exactly.
const MAX_ID: u64 = (1 << 53) - 1;

/// Approval ids for the whole process, from a random start: every chat and every run of the app numbers its
/// approvals differently. A page keeps its transcript across restarts, so ids counted from 1 each time repeated: the
/// new card's `approval_done` marked an old card, and Allow on the new card did nothing.
static NEXT_ID: LazyLock<AtomicU64> = LazyLock::new(|| AtomicU64::new(first_id()));

/// Somewhere in [1, 2^52): room for 2^52 ids after it before reaching `MAX_ID`. The low 52 bits of a v4 UUID's
/// low half are all random (its version and variant bits are elsewhere).
fn first_id() -> u64 {
    let random = uuid::Uuid::new_v4().as_u64_pair().1;
    (random & ((1 << 52) - 1)).max(1)
}

/// The id after `id`: back to 1 past `MAX_ID` (2^52 approvals on; unreachable in practice), never an id the page
/// can't read exactly.
fn after(id: u64) -> u64 {
    if id >= MAX_ID { 1 } else { id + 1 }
}

fn next_id() -> u64 {
    // One atomic step: callers at the same moment never get the same id, even at the wrap.
    NEXT_ID.update(Ordering::Relaxed, Ordering::Relaxed, after)
}

pub struct WebApprover {
    out: UnboundedSender<Value>,
    pending: Mutex<HashMap<u64, Pending>>,
}

impl WebApprover {
    pub fn new(out: UnboundedSender<Value>) -> Self {
        Self { out, pending: Mutex::new(HashMap::new()) }
    }

    fn pending(&self) -> std::sync::MutexGuard<'_, HashMap<u64, Pending>> {
        self.pending.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The person's answer to request `id`; an unknown id (already answered, or cancelled) is ignored.
    pub fn answer(&self, id: u64, decision: Decision) {
        if let Some(p) = self.pending().remove(&id) {
            let decision = match decision {
                Decision::Always if p.once => Decision::Yes,
                // #185: a reason is read here as the engine reads it (one line, at most MAX_DECLINE_REASON), so the
                // page's `approval_done` shows what the model gets; an empty one is a plain no.
                Decision::Declined(_) if !p.reason => Decision::No,
                Decision::Declined(reason) => match marv_core::decline_reason(&reason) {
                    read if read.is_empty() => Decision::No,
                    read => Decision::Declined(read),
                },
                decision => decision,
            };
            let _ = p.answer.send(decision);
        }
    }

    /// A request is waiting for the person.
    pub fn waiting(&self) -> bool {
        !self.pending().is_empty()
    }

    /// Every open request answered no (an interrupt, or the page went away).
    pub fn cancel_all(&self) {
        for (_, p) in self.pending().drain() {
            let _ = p.answer.send(Decision::No);
        }
    }
}

#[async_trait]
impl Approver for WebApprover {
    async fn approve(&self, request: ApprovalRequest) -> Decision {
        let id = next_id();
        let (answer, answered) = oneshot::channel();
        // A person must answer `must_ask` every time, so there's no "always" for it either.
        let once = request.once || request.must_ask;
        self.pending().insert(id, Pending { answer, once, reason: request.reason });
        if self.out.send(request_json(id, &request, once)).is_err() {
            self.pending().remove(&id);
            return Decision::Unavailable;
        }
        let _withdraw = Withdraw(self, id);
        let decision = answered.await.unwrap_or(Decision::No);
        // A decline with a reason (#185) is "declined" on the page, the reason with it (the person's own text).
        let done = match &decision {
            Decision::Declined(reason) => json!({"type": "approval_done", "id": id, "decision": "declined", "reason": reason}),
            decision => json!({"type": "approval_done", "id": id, "decision": decision}),
        };
        let _ = self.out.send(done);
        decision
    }
}

/// #201: the wait for request `id` was dropped unanswered (another Marv asked for a wire, then hung up; a stopped
/// turn): the page is told it's `withdrawn`, so no card is left waiting for an answer nobody reads.
struct Withdraw<'a>(&'a WebApprover, u64);
impl Drop for Withdraw<'_> {
    fn drop(&mut self) {
        if self.0.pending().remove(&self.1).is_some() {
            let _ = self.0.out.send(json!({"type": "approval_done", "id": self.1, "decision": "withdrawn"}));
        }
    }
}

fn request_json(id: u64, r: &ApprovalRequest, once: bool) -> Value {
    let p = &r.preview;
    json!({
        "type": "approval",
        "id": id,
        "tool": r.tool,
        "label": r.label,
        "title": p.title,
        "command": p.command,
        "diff": p.diff,
        "text": p.text,
        "note": p.note,
        "warning": p.warning,
        "scope": r.scope.description,
        "agent": r.agent,
        "network": r.network,
        "once": once,
        "mustAsk": r.must_ask,
        "reason": r.reason,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use marv_tools::types::{Preview, Scope};
    use std::sync::Arc;

    fn request(once: bool) -> ApprovalRequest {
        ApprovalRequest {
            reason: true,
            tool: "bash".into(),
            label: "git commit".into(),
            preview: Preview { title: "Run a command".into(), command: Some("git commit".into()), ..Preview::default() },
            scope: Scope { key: "bash:git commit".into(), description: "this exact command".into() },
            agent: None,
            network: false,
            once,
            must_ask: false,
            protected: None,
            door: None,
        }
    }

    #[tokio::test]
    async fn the_page_answers() {
        let (out, mut page) = tokio::sync::mpsc::unbounded_channel();
        let approver = Arc::new(WebApprover::new(out));
        let asking = tokio::spawn({
            let approver = approver.clone();
            async move { approver.approve(request(false)).await }
        });
        let shown = page.recv().await.unwrap();
        assert_eq!((shown["type"].as_str(), shown["command"].as_str()), (Some("approval"), Some("git commit")));
        approver.answer(shown["id"].as_u64().unwrap(), Decision::Always);
        assert_eq!(asking.await.unwrap(), Decision::Always);
        assert_eq!(page.recv().await.unwrap()["type"], "approval_done");
    }

    /// #201: a request whose wait is dropped unanswered (another Marv asked for a wire, then hung up) is withdrawn on
    /// the page, and nothing is left waiting; a late answer to it is ignored.
    #[tokio::test]
    async fn a_dropped_request_is_withdrawn_on_the_page() {
        let (out, mut page) = tokio::sync::mpsc::unbounded_channel();
        let approver = Arc::new(WebApprover::new(out));
        let asking = tokio::spawn({
            let approver = approver.clone();
            async move { approver.approve(request(true)).await }
        });
        let id = page.recv().await.unwrap()["id"].as_u64().unwrap();
        assert!(approver.waiting());
        asking.abort();
        let done = page.recv().await.unwrap();
        assert_eq!(done, json!({"type": "approval_done", "id": id, "decision": "withdrawn"}));
        assert!(!approver.waiting(), "nothing left waiting");
        approver.answer(id, Decision::Yes);
        assert!(page.try_recv().is_err(), "a late answer changes nothing");
    }

    #[tokio::test]
    async fn once_turns_always_into_yes_and_cancel_says_no() {
        let (out, mut page) = tokio::sync::mpsc::unbounded_channel();
        let approver = Arc::new(WebApprover::new(out));
        let asking = tokio::spawn({
            let approver = approver.clone();
            async move { approver.approve(request(true)).await }
        });
        let id = page.recv().await.unwrap()["id"].as_u64().unwrap();
        approver.answer(id, Decision::Always);
        assert_eq!(asking.await.unwrap(), Decision::Yes);
        assert_eq!(page.recv().await.unwrap()["type"], "approval_done");

        let asking = tokio::spawn({
            let approver = approver.clone();
            async move { approver.approve(request(false)).await }
        });
        assert_eq!(page.recv().await.unwrap()["type"], "approval");
        approver.cancel_all();
        assert_eq!(asking.await.unwrap(), Decision::No);
    }

    #[tokio::test]
    async fn a_reason_reaches_the_call_and_the_page_shows_it_declined() {
        let (out, mut page) = tokio::sync::mpsc::unbounded_channel();
        let approver = Arc::new(WebApprover::new(out));
        let asking = tokio::spawn({
            let approver = approver.clone();
            async move { approver.approve(request(true)).await }
        });
        let id = page.recv().await.unwrap()["id"].as_u64().unwrap();
        approver.answer(id, Decision::Declined(format!("not on\nmain{}", "!".repeat(marv_tools::MAX_DECLINE_REASON))));
        let read = format!("not on main{}", "!".repeat(marv_tools::MAX_DECLINE_REASON - 11));
        assert_eq!(asking.await.unwrap(), Decision::Declined(read.clone()), "one line, cut at the limit");
        let done = page.recv().await.unwrap();
        assert_eq!((done["decision"].as_str(), done["reason"].as_str()), (Some("declined"), Some(read.as_str())));

        // An empty reason is a plain no.
        let asking = tokio::spawn({
            let approver = approver.clone();
            async move { approver.approve(request(false)).await }
        });
        let id = page.recv().await.unwrap()["id"].as_u64().unwrap();
        approver.answer(id, Decision::Declined(" \n ".into()));
        assert_eq!(asking.await.unwrap(), Decision::No);
        assert_eq!(page.recv().await.unwrap()["decision"], "no");

        // Not a model's call (/undo, /mcp add): no reason is offered, and one sent anyway is a plain no.
        let asking = tokio::spawn({
            let approver = approver.clone();
            async move { approver.approve(ApprovalRequest { reason: false, ..request(false) }).await }
        });
        let shown = page.recv().await.unwrap();
        assert_eq!(shown["reason"], false);
        approver.answer(shown["id"].as_u64().unwrap(), Decision::Declined("because".into()));
        assert_eq!(asking.await.unwrap(), Decision::No);
    }

    /// Ids never repeat between approvers (each chat, each run has its own): an answer meant for one card can't
    /// match another chat's request. They stay exact as JavaScript numbers.
    #[tokio::test]
    async fn two_approvers_never_share_an_id() {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..3 {
            let (out, mut page) = tokio::sync::mpsc::unbounded_channel();
            let approver = Arc::new(WebApprover::new(out));
            for _ in 0..3 {
                let asking = tokio::spawn({
                    let approver = approver.clone();
                    async move { approver.approve(request(false)).await }
                });
                let id = page.recv().await.unwrap()["id"].as_u64().unwrap();
                assert!((1..=MAX_ID).contains(&id), "{id}");
                assert!(seen.insert(id), "id {id} repeated");
                approver.answer(id, Decision::Yes);
                assert_eq!(asking.await.unwrap(), Decision::Yes);
                page.recv().await.unwrap();
            }
        }
    }

    #[test]
    fn ids_start_below_2_52_and_wrap_to_1_past_2_53() {
        for _ in 0..100 {
            assert!((1..1 << 52).contains(&first_id()));
        }
        assert_eq!((after(41), after(MAX_ID - 1), after(MAX_ID)), (42, MAX_ID, 1));
    }

    /// An answer for an id that isn't waiting (from an earlier run, another chat, or answered already) is ignored:
    /// the request still waits for its own answer.
    #[tokio::test]
    async fn an_answer_with_an_unknown_id_is_ignored() {
        let (out, mut page) = tokio::sync::mpsc::unbounded_channel();
        let approver = Arc::new(WebApprover::new(out));
        let asking = tokio::spawn({
            let approver = approver.clone();
            async move { approver.approve(request(false)).await }
        });
        let id = page.recv().await.unwrap()["id"].as_u64().unwrap();
        // Ids that aren't this request's: ones taken from the counter and never asked with (so no other test's
        // request has them either), and one far from it.
        let others = [next_id(), next_id(), (id ^ (1 << 51)) & MAX_ID];
        for other in others {
            assert_ne!(other, id);
            approver.answer(other, Decision::Always);
        }
        tokio::task::yield_now().await;
        assert!(approver.waiting() && !asking.is_finished(), "still waiting");
        approver.answer(id, Decision::No);
        assert_eq!(asking.await.unwrap(), Decision::No);
        // Answered: the same id again does nothing.
        approver.answer(id, Decision::Yes);
        assert!(!approver.waiting());
    }

    #[tokio::test]
    async fn no_page_no_approval() {
        let (out, page) = tokio::sync::mpsc::unbounded_channel();
        drop(page);
        assert_eq!(WebApprover::new(out).approve(request(false)).await, Decision::Unavailable);
    }
}
