//! The seam between Marv and any LLM vendor. Ports `src/provider/types.ts` (the trait) and
//! `src/provider/index.ts`.
//!
//! Every adapter turns its own streaming format into [`marv_core::ProviderEvent`]s; the agent
//! loop and the UI only ever see those, so adding a vendor never touches them.

use std::pin::Pin;

use futures::Stream;
use marv_core::{ChatTurn, ProviderEvent, ToolChoice, ToolSpec};
use tokio_util::sync::CancellationToken;

/// A stream of provider events for one request.
pub type EventStream = Pin<Box<dyn Stream<Item = ProviderEvent> + Send>>;

/// Per-request options.
#[derive(Debug, Clone, Default)]
pub struct StreamOptions {
    /// Instructions sent ahead of the conversation. Built once per session and byte-stable.
    pub system: Option<String>,
    /// Tools the model may call. Same list and order on every request (prompt cache).
    pub tools: Vec<ToolSpec>,
    pub tool_choice: ToolChoice,
    /// Cancelled by ctrl+c to stop the reply and the HTTP request behind it.
    pub cancel: CancellationToken,
}

pub trait Provider: Send + Sync {
    /// Shown in the status bar, e.g. "openrouter · z-ai/glm-5.3".
    fn name(&self) -> &str;
    /// Context window in tokens, when known (Ollama: the `num_ctx` Marv asks for).
    fn context_length(&self) -> Option<u64>;
    /// Streams one reply. Never fails up front: errors arrive as [`ProviderEvent::Error`].
    fn stream(&self, history: &[ChatTurn], options: StreamOptions) -> EventStream;
}
