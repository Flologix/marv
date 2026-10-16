//! The browser as Marv's approver: each request goes to the page, and the call waits for the person's answer.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use marv_tools::types::{ApprovalRequest, Approver, Decision};
use serde_json::{Value, json};
use tokio::sync::{mpsc::UnboundedSender, oneshot};

struct Pending {
    answer: oneshot::Sender<Decision>,
    /// Yes or no only: an "always" counts as one yes.
    once: bool,
}

pub struct WebApprover {
    out: UnboundedSender<Value>,
    pending: Mutex<HashMap<u64, Pending>>,
    next: AtomicU64,
}

impl WebApprover {
    pub fn new(out: UnboundedSender<Value>) -> Self {
        Self { out, pending: Mutex::new(HashMap::new()), next: AtomicU64::new(1) }
    }

    fn pending(&self) -> std::sync::MutexGuard<'_, HashMap<u64, Pending>> {
        self.pending.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The person's answer to request `id`; an unknown id (already answered, or cancelled) is ignored.
    pub fn answer(&self, id: u64, decision: Decision) {
        if let Some(p) = self.pending().remove(&id) {
            let decision = if p.once && decision == Decision::Always { Decision::Yes } else { decision };
            let _ = p.answer.send(decision);
        }
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
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (answer, answered) = oneshot::channel();
        // A person must answer `must_ask` every time, so there's no "always" for it either.
        let once = request.once || request.must_ask;
        self.pending().insert(id, Pending { answer, once });
        if self.out.send(request_json(id, &request, once)).is_err() {
            self.pending().remove(&id);
            return Decision::Unavailable;
        }
        let decision = answered.await.unwrap_or(Decision::No);
        let _ = self.out.send(json!({"type": "approval_done", "id": id, "decision": decision}));
        decision
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
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use marv_tools::types::{Preview, Scope};
    use std::sync::Arc;

    fn request(once: bool) -> ApprovalRequest {
        ApprovalRequest {
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
    async fn no_page_no_approval() {
        let (out, page) = tokio::sync::mpsc::unbounded_channel();
        drop(page);
        assert_eq!(WebApprover::new(out).approve(request(false)).await, Decision::Unavailable);
    }
}
