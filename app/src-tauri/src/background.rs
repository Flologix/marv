//! Chats that keep running while you look at another one (#155). A chat's session lives here from its first open
//! until you delete or stop it, or the app quits: it stays on the mesh (other Marvs see it and a wired message wakes
//! it), and a turn keeps going. The page attaches to a chat while it shows it.
//!
//! Every event gets a number (`seq`) and the last `BACKLOG` are kept whether a page is attached or not. A page
//! attaches with the last number it saved and gets what came after: the events of a turn that went on in the
//! background, a pending approval to answer, and anything sent in the moment between the page leaving and the detach.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;

use marv_web::protocol::ClientMessage;
use serde::Serialize;
use serde_json::{Value, json};
use tokio::sync::mpsc;

/// Events kept per chat. Past this, the oldest go (a long unattended turn), and a page that missed them says so.
const BACKLOG: usize = 5000;
/// Chats kept running at once. Opening one more ends the least recently shown chat that's idle and has nothing to
/// answer; if every chat is busy or waiting, none is ended.
pub const MAX_LIVE: usize = 8;

/// Where a chat's events go while a page shows it: false once the page is gone.
pub type Sink = Box<dyn Fn(Value) -> bool + Send>;

/// A chat's state as the events tell it, and its page, if one is attached.
#[derive(Default)]
pub struct Shared {
    sink: Option<Sink>,
    /// The last events, numbered from 1.
    recent: VecDeque<Value>,
    seq: u64,
    /// The session's `ready` event: sent first to every page that attaches (never numbered or kept).
    ready: Option<Value>,
    busy: bool,
    /// Approvals asked and not answered yet, by id.
    waiting: BTreeSet<u64>,
    ended: bool,
}

impl Shared {
    /// One event from the session: noted, numbered and kept, and sent to the page if one is attached.
    pub fn deliver(&mut self, mut event: Value) {
        match event["type"].as_str() {
            Some("ready") => {
                self.ready = Some(event.clone());
                self.send(event);
                return;
            }
            Some("turn_start") => self.busy = true,
            Some("turn_end") => self.busy = false,
            Some("approval") => {
                if let Some(id) = event["id"].as_u64() {
                    self.waiting.insert(id);
                }
            }
            Some("approval_done") => {
                if let Some(id) = event["id"].as_u64() {
                    self.waiting.remove(&id);
                }
            }
            _ => {}
        }
        self.seq += 1;
        event["seq"] = json!(self.seq);
        if self.recent.len() == BACKLOG {
            self.recent.pop_front();
        }
        self.recent.push_back(event.clone());
        self.send(event);
    }

    /// To the attached page; false (and the page detached) if there's none or it's gone.
    fn send(&mut self, event: Value) -> bool {
        match &self.sink {
            Some(sink) if sink(event) => true,
            _ => {
                self.sink = None;
                false
            }
        }
    }

    /// A page shows the chat, having saved everything up to `after`: its `ready` (with whether a turn is running),
    /// then what came after, then live events.
    pub fn attach(&mut self, sink: Sink, after: u64) {
        self.sink = Some(sink);
        if let Some(mut ready) = self.ready.clone() {
            ready["busy"] = json!(self.busy);
            if !self.send(ready) {
                return;
            }
        }
        let first = self.seq + 1 - self.recent.len() as u64;
        let missed = first.saturating_sub(after + 1);
        if missed > 0 {
            let text = format!("{missed} earlier updates from while this chat was in the background aren't shown.");
            if !self.send(json!({"type": "notice", "text": text})) {
                return;
            }
        }
        let skip = after.saturating_sub(first - 1) as usize;
        let replay: Vec<Value> = self.recent.iter().skip(skip).cloned().collect();
        for event in replay {
            if !self.send(event) {
                return;
            }
        }
        if self.ended {
            self.send(json!({"type": "closed"}));
        }
    }

    pub fn detach(&mut self) {
        self.sink = None;
    }

    /// The session ended on its own (an error, or it closed).
    pub fn end(&mut self) {
        self.ended = true;
        self.busy = false;
        self.send(json!({"type": "closed"}));
    }

    fn idle(&self) -> bool {
        !self.busy && self.waiting.is_empty()
    }
}

/// One running chat.
pub struct LiveChat {
    pub to_chat: mpsc::UnboundedSender<ClientMessage>,
    /// What the session was started with that can't change while it runs (switched-off servers, the page's reload
    /// count): a different key starts it again (it continues the same conversation).
    pub key: String,
    pub shared: Arc<Mutex<Shared>>,
    /// When a page last showed it.
    pub seen: Instant,
}

impl LiveChat {
    pub fn shared(&self) -> MutexGuard<'_, Shared> {
        lock(&self.shared)
    }

    pub fn running(&self) -> bool {
        !self.to_chat.is_closed() && !self.shared().ended
    }
}

pub fn lock(shared: &Mutex<Shared>) -> MutexGuard<'_, Shared> {
    shared.lock().unwrap_or_else(|e| e.into_inner())
}

/// The chat to end so another can start: past `MAX_LIVE`, the least recently shown one that no page shows, isn't
/// running a turn and has nothing waiting for an answer.
pub fn to_evict(live: &HashMap<String, LiveChat>, keep: &str) -> Option<String> {
    if live.len() < MAX_LIVE {
        return None;
    }
    live.iter()
        .filter(|(id, chat)| id.as_str() != keep && chat.shared().sink.is_none() && chat.shared().idle())
        .min_by_key(|(_, chat)| chat.seen)
        .map(|(id, _)| id.clone())
}

/// What the sidebar shows for a running chat.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LiveState {
    pub id: String,
    pub busy: bool,
    pub waiting: usize,
}

pub fn states(live: &HashMap<String, LiveChat>) -> Vec<LiveState> {
    live.iter()
        .filter(|(_, chat)| chat.running())
        .map(|(id, chat)| {
            let shared = chat.shared();
            LiveState { id: id.clone(), busy: shared.busy, waiting: shared.waiting.len() }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    type Got = Arc<Mutex<Vec<Value>>>;

    /// A page: what it got, and whether it's still there.
    fn page() -> (Sink, Got, Arc<Mutex<bool>>) {
        let got = Arc::new(Mutex::new(Vec::new()));
        let open = Arc::new(Mutex::new(true));
        let (g, o) = (got.clone(), open.clone());
        let sink: Sink = Box::new(move |e| {
            if !*o.lock().unwrap() {
                return false;
            }
            g.lock().unwrap().push(e);
            true
        });
        (sink, got, open)
    }

    fn types(got: &Got) -> Vec<String> {
        got.lock().unwrap().iter().map(|e| e["type"].as_str().unwrap().to_string()).collect()
    }

    #[test]
    fn a_page_that_comes_back_gets_what_came_after_its_last_event_and_a_waiting_approval_is_counted() {
        let mut chat = Shared::default();
        let (sink, got, open) = page();
        chat.attach(sink, 0);
        chat.deliver(json!({"type": "ready", "sessionId": "s"}));
        chat.deliver(json!({"type": "turn_start"}));
        chat.deliver(json!({"type": "text_delta", "text": "a"}));
        // The page saved up to 2 and left; 3 went to it as it went (lost there), 4 and 5 found it gone.
        chat.deliver(json!({"type": "text_delta", "text": "b"}));
        *open.lock().unwrap() = false;
        chat.deliver(json!({"type": "text_delta", "text": "c"}));
        chat.deliver(json!({"type": "approval", "id": 7}));
        assert!(chat.sink.is_none());
        assert_eq!((chat.busy, chat.waiting.len()), (true, 1));
        assert_eq!(types(&got), ["ready", "turn_start", "text_delta", "text_delta"]);

        let (sink, again, _) = page();
        chat.attach(sink, 2);
        assert_eq!(types(&again), ["ready", "text_delta", "text_delta", "approval"]);
        let again = again.lock().unwrap();
        assert_eq!(again[0]["busy"], true);
        assert_eq!((again[1]["seq"].as_u64(), again[1]["text"].as_str()), (Some(3), Some("b")));
        drop(again);

        chat.deliver(json!({"type": "approval_done", "id": 7}));
        chat.deliver(json!({"type": "turn_end", "reason": "done"}));
        assert!(chat.idle());
    }

    #[test]
    fn a_ready_while_away_is_sent_once_and_a_new_page_gets_everything() {
        let mut chat = Shared::default();
        chat.deliver(json!({"type": "ready"}));
        chat.deliver(json!({"type": "notice", "text": "woken"}));
        let (sink, got, _) = page();
        chat.attach(sink, 0);
        assert_eq!(types(&got), ["ready", "notice"]);
        let (sink, up_to_date, _) = page();
        chat.attach(sink, 1);
        assert_eq!(types(&up_to_date), ["ready"]);
    }

    #[test]
    fn past_the_backlog_the_page_is_told_how_many_it_missed() {
        let mut chat = Shared::default();
        for i in 0..BACKLOG + 3 {
            chat.deliver(json!({"type": "text_delta", "text": i.to_string()}));
        }
        let (sink, got, _) = page();
        chat.attach(sink, 1);
        let got = got.lock().unwrap();
        assert_eq!(got.len(), BACKLOG + 1);
        assert!(got[0]["text"].as_str().unwrap().starts_with("2 earlier updates"), "{}", got[0]);
        assert_eq!(got[1]["text"], "3");
    }

    #[test]
    fn an_ended_chat_tells_the_page_that_attaches() {
        let mut chat = Shared::default();
        chat.end();
        let (sink, got, _) = page();
        chat.attach(sink, 0);
        assert_eq!(types(&got), ["closed"]);
    }

    #[test]
    fn eviction_picks_the_oldest_idle_unshown_chat_and_never_a_busy_or_waiting_one() {
        let start = Instant::now();
        let mut live = HashMap::new();
        let mut senders = Vec::new();
        for i in 0..MAX_LIVE {
            let (to_chat, from) = mpsc::unbounded_channel();
            senders.push(from);
            let chat = LiveChat {
                to_chat,
                key: String::new(),
                shared: Arc::default(),
                seen: start + std::time::Duration::from_secs(i as u64),
            };
            live.insert(format!("c{i}"), chat);
        }
        // c0 is running a turn, c1 waits for an approval, c2 is on screen: c3 is the oldest that can go.
        live["c0"].shared().busy = true;
        live["c1"].shared().waiting.insert(1);
        live["c2"].shared().attach(Box::new(|_| true), 0);
        assert_eq!(to_evict(&live, "new").as_deref(), Some("c3"));
        assert_eq!(to_evict(&live, "c3").as_deref(), Some("c4"));
        live.remove("c7");
        assert_eq!(to_evict(&live, "new"), None, "under the cap, nothing ends");
    }
}
