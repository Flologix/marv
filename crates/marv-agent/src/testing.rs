//! Test doubles for the loop and compaction: a provider that replays scripted replies and records
//! every request. Ports `tests/fake-provider.ts` (`ScriptedProvider`).

use std::sync::Mutex;

use marv_core::{ChatTurn, ProviderEvent};
use marv_provider::{EventStream, Provider, StreamOptions};

/// Replays one scripted reply per request, in order; after the script, replies with an empty `Done`.
pub struct ScriptedProvider {
    replies: Mutex<std::collections::VecDeque<Vec<ProviderEvent>>>,
    requests: Mutex<Vec<(Vec<ChatTurn>, StreamOptions)>>,
}

impl ScriptedProvider {
    pub fn new(replies: Vec<Vec<ProviderEvent>>) -> Self {
        Self { replies: Mutex::new(replies.into()), requests: Mutex::new(Vec::new()) }
    }

    /// Every request so far: the history sent and the options.
    pub fn requests(&self) -> Vec<(Vec<ChatTurn>, StreamOptions)> {
        self.requests.lock().unwrap().clone()
    }
}

impl Provider for ScriptedProvider {
    fn name(&self) -> &str {
        "scripted"
    }

    fn context_length(&self) -> Option<u64> {
        None
    }

    fn stream(&self, history: &[ChatTurn], options: StreamOptions) -> EventStream {
        self.requests.lock().unwrap().push((history.to_vec(), options));
        let reply = self.replies.lock().unwrap().pop_front().unwrap_or_else(|| vec![ProviderEvent::Done { reason: None }]);
        Box::pin(futures::stream::iter(reply))
    }
}
