//! The App: state, input handling and the event loop. Ports `src/app.tsx` (tests: `tests/app.test.tsx`,
//! `tests/render-performance.test.tsx`, `tests/approval.test.tsx`, `tests/herdr.test.tsx`).
//!
//! # Layout (as in Marv)
//! Full screen in the alternate screen: the transcript takes the leftover height and scrolls itself
//! (`ui::scroll_view`), and below it, pinned, one of: the session picker, an approval (+ status
//! bar), setup, or the prompt + status bar. A subagent's view (opened by clicking its entry) hides
//! the main transcript, keeps its scroll position, and adds `AgentViewHeader`. Before all that the
//! splash shows for `splash_ms` (any key skips it).
//!
//! # The event loop: idle near 0% CPU (Marv #68)
//! Marv's Ink TUI used 40–60% CPU per idle session: React re-rendered on timers, Ink re-laid-out
//! and re-drew whole frames, and a spinner and the martian ticked whether or not anything moved.
//! Here nothing runs unless there is something to show:
//!
//! - **One wait.** The loop `tokio::select!`s on three sources: crossterm's `EventStream` (keys,
//!   mouse, resize), the engine's `UiEvent` channel, and `sleep_until(next deadline)`. Between
//!   them the task is parked in the kernel: no polling, no fixed tick.
//! - **Deadlines, not intervals.** Every animated thing exposes when it next changes, and
//!   [`Deadlines`] takes the earliest: the martian (only while the welcome banner is inside the
//!   viewport: a scrolled-away martian never wakes the loop), the stream flush, the wheel glide
//!   (`GLIDE_MS`), selection auto-scroll, the waiting line's spinner and seconds counter (only while
//!   it's shown), the notice and exit-confirm expiries, the splash. With none pending the loop
//!   sleeps until input arrives.
//! - **Draw only when dirty.** Handlers set `dirty`; a wake that changed nothing (a tick that
//!   didn't change the martian's pose) draws nothing. Draws are capped at
//!   `render_options::MAX_FPS`; ratatui then writes only the changed cells, wrapped in
//!   synchronized output.
//! - **Batch streamed text (~30 fps).** Deltas are appended to a pending buffer and reach the
//!   transcript state at most every [`STREAM_FLUSH_MS`] (33 ms, Marv's value: Ink drew at most 30
//!   frames/s, and updating on every token re-parsed the whole Markdown reply for frames nobody
//!   saw). Step ends ([`STEP_ENDS`]) flush at once; subagents' progress joins the same flush. The
//!   loop drains every queued `UiEvent` before drawing, so a burst of tokens is one frame.
//! - **Cull the viewport.** Marv patched Ink to skip drawing nodes outside the visible rows (a
//!   60-entry session took ~270 ms per keystroke unpatched, ~30 ms patched). Here each transcript
//!   entry caches its wrapped lines per width (memoised like Marv's `MessageView`), the scroll view
//!   keeps cumulative heights, and only entries intersecting the viewport are rendered.
//!
//! # Behaviours to keep (from Marv's CLAUDE.md and app.tsx)
//! - **Approval queue:** parallel subagents can ask at once. Requests queue; the head is shown with
//!   "N more waiting". Esc at an approval declines everything queued and stops the run (unless a
//!   subagent's view is open: Esc then only closes the view). ctrl+c at an approval declines all.
//! - **ctrl+c:** interrupt a reply → clear the prompt → "Press ctrl+c again to exit"
//!   ([`EXIT_CONFIRM_MS`]) → exit. ctrl+o toggles details (subagents' steps, whole diffs).
//! - **Live tool drafts:** a streaming tool call is kept as the latest draft (dropped at a step's
//!   end, [`STEP_ENDS`]) and shown by `DraftView` as `Writing src/app.js… (8s · ~400 tokens)`.
//! - **Subagent view:** subagents' events fold into per-call `AgentLog`s; a click on an agent entry
//!   opens its log; only this session's subagents can be opened.
//! - **Thinking collapses** to `✻ Thought for 8s` once the step moves on.
//! - **Mouse:** wheel glides the transcript; drag selects, release copies (status bar flash for
//!   [`NOTICE_MS`]); any key clears the selection.
//! - **Herdr:** reports blocked / working / idle (Marv `src/herdr.ts`), wired by marv-cli later.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::mpsc::UnboundedReceiver;

use crate::view::{ApprovalId, ApprovalRequest, CommandInfo, Message, SessionPort, StatusInfo, Totals, UiEvent, WelcomeInfo};

/// How long "Press ctrl+c again to exit" waits for the second ctrl+c.
pub const EXIT_CONFIRM_MS: Duration = Duration::from_millis(1500);
/// How long a status bar flash ("Copied 3 lines") stays.
pub const NOTICE_MS: Duration = Duration::from_millis(2000);
/// Streamed text reaches the transcript at most this often (~30 fps).
pub const STREAM_FLUSH_MS: Duration = Duration::from_millis(33);

/// Events after which a streaming tool call is done streaming: complete, cut off, or abandoned.
/// They also flush the stream buffer at once.
pub fn is_step_end(event: &UiEvent) -> bool {
    matches!(
        event,
        UiEvent::Assistant { .. }
            | UiEvent::ToolStart { .. }
            | UiEvent::ToolSkipped { .. }
            | UiEvent::CutOff { .. }
            | UiEvent::EmptyReply { .. }
            | UiEvent::Error(_)
            | UiEvent::Done(_)
    )
}

/// Marv's `STEP_ENDS` names, for reference against `src/app.tsx`.
pub const STEP_ENDS: [&str; 7] = ["assistant", "tool_start", "tool_skipped", "cut_off", "empty_reply", "error", "done"];

/// The earliest of several optional wake-up times: what the loop sleeps until.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Deadlines(Option<Instant>);

impl Deadlines {
    pub fn new() -> Self {
        Self(None)
    }

    /// Adds a candidate; None (nothing pending) is ignored.
    pub fn add(&mut self, at: Option<Instant>) -> &mut Self {
        self.0 = match (self.0, at) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        self
    }

    /// When to wake, or None to wait for input alone.
    pub fn next(&self) -> Option<Instant> {
        self.0
    }
}

/// Which screen is up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Splash,
    Main,
}

/// Options marv-cli passes in (Marv's `App` props that aren't the session).
#[derive(Debug, Clone)]
pub struct AppOptions {
    pub welcome: WelcomeInfo,
    pub status: StatusInfo,
    /// 0 skips the splash.
    pub splash_ms: u64,
    pub render: crate::render_options::RenderOptions,
}

/// A queued approval.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pending {
    pub id: ApprovalId,
    pub request: ApprovalRequest,
}

/// The App's state. Fields mirror Marv's `useState`s.
pub struct App {
    port: Arc<dyn SessionPort>,
    events: UnboundedReceiver<UiEvent>,
    options: AppOptions,
    phase: Phase,
    commands: Vec<CommandInfo>,
    /// Finished transcript entries.
    messages: Vec<Message>,
    /// The reply streaming in (None when no turn runs), and text not yet flushed into it.
    streaming: Option<String>,
    pending_text: String,
    flush_at: Option<Instant>,
    thinking: String,
    /// The latest tool call still streaming: (index, name, args so far).
    draft: Option<(usize, String, String)>,
    approvals: std::collections::VecDeque<Pending>,
    totals: Totals,
    /// Set by any change that needs a draw.
    dirty: bool,
}

impl App {
    /// The engine hands over its port and the receiving end of its event channel.
    pub fn new(port: Arc<dyn SessionPort>, events: UnboundedReceiver<UiEvent>, options: AppOptions) -> Self {
        let commands = port.commands();
        let phase = if options.splash_ms > 0 { Phase::Splash } else { Phase::Main };
        Self {
            port,
            events,
            options,
            phase,
            commands,
            messages: Vec::new(),
            streaming: None,
            pending_text: String::new(),
            flush_at: None,
            thinking: String::new(),
            draft: None,
            approvals: Default::default(),
            totals: Totals::default(),
            dirty: true,
        }
    }

    /// Runs until exit: enters the terminal (`render_options::TerminalGuard`), loops as described
    /// in the module docs, restores the terminal, then awaits `port.flush()` (bounded).
    pub async fn run(self) -> anyhow::Result<()> {
        let _ = (&self.port, &self.events, &self.options, self.phase, &self.commands, &self.messages);
        let _ = (
            &self.streaming,
            &self.pending_text,
            self.flush_at,
            &self.thinking,
            &self.draft,
            &self.approvals,
            self.totals,
            self.dirty,
        );
        todo!("port: the App's event loop (select! over EventStream, UiEvent channel, sleep_until(Deadlines))")
    }

    /// Folds one engine event into the state (Marv `send()`'s event switch).
    pub fn apply(&mut self, event: UiEvent, now: Instant) {
        let _ = (event, now);
        todo!("port: App.send() event handling, stream batching (STREAM_FLUSH_MS), approvals queue")
    }

    /// Handles a key (Marv's useInput handlers: Esc, ctrl+c, ctrl+o, PgUp/PgDn, the prompt).
    pub fn on_key(&mut self, key: crossterm::event::KeyEvent, now: Instant) {
        let _ = (key, now);
        todo!("port: key handling")
    }

    /// Handles a mouse event (wheel glide, selection, clicks on agent entries).
    pub fn on_mouse(&mut self, event: crate::mouse::MouseEvent, now: Instant) {
        let _ = (event, now);
        todo!("port: mouse handling")
    }

    /// The earliest moment something on screen changes on its own.
    pub fn deadline(&self) -> Option<Instant> {
        todo!("port: Deadlines over martian (if visible), flush, glide, auto-scroll, spinner, notices")
    }

    /// Draws one frame.
    pub fn draw(&mut self, frame: &mut ratatui::Frame) {
        let _ = frame;
        todo!("port: App's render tree")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deadlines_take_the_earliest_and_ignore_none() {
        let t = Instant::now();
        let mut d = Deadlines::new();
        assert_eq!(d.next(), None);
        d.add(None).add(Some(t + Duration::from_millis(50))).add(Some(t + Duration::from_millis(20))).add(None);
        assert_eq!(d.next(), Some(t + Duration::from_millis(20)));
    }

    #[test]
    fn step_ends_match_marv() {
        assert!(is_step_end(&UiEvent::Assistant { text: String::new() }));
        assert!(is_step_end(&UiEvent::Done(marv_core::StopReason::End)));
        assert!(!is_step_end(&UiEvent::TextDelta("x".into())));
        assert_eq!(STREAM_FLUSH_MS, Duration::from_millis(33));
    }
}
