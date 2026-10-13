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
//!   "Always" also answers the queued requests with the same scope key.
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
//!
//! # What moved to the engine (marv-session, behind `SessionPort`)
//! Marv's App also ran the commands, saved the config, the sessions and the trajectories, managed
//! MCP and memory, and collated the startup notices. Those are the engine's now: the App sends
//! `/…` lines to [`SessionPort::run_command`] and shows what comes back as [`UiEvent`]s (a
//! `Message`, `Clear`, `OpenSetup`, `Sessions`, `Exit`…). What stays here is what the user sees:
//! the transcript and the turn's live state, the approval queue, the pickers and setup, the status
//! bar, keys, mouse and selection.
//!
//! # Stand-ins until #9
//! The rendering modules (`markdown`, `message_view`, `diff_view`, `notice_view`, `thinking_view`,
//! `scroll_view`'s scrolling, `transcript`, `agent_view`) are ported by #9. Until then the
//! private [`standin`] module draws entries as plain wrapped text, and the scroll/glide/culling
//! arithmetic lives here. Each stand-in says which call replaces it.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use futures::future::BoxFuture;
use futures::stream::{FuturesUnordered, StreamExt};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::Line;
use tokio::sync::mpsc::UnboundedReceiver;

use crate::mouse::{MouseEvent, WHEEL_ROWS};
use crate::render_options::{FRAME_INTERVAL, TerminalGuard};
use crate::selection::{SelectionEffect, SelectionStore, Viewport};
use crate::ui::agent_view::AgentLog;
use crate::ui::approval::{ApprovalOutcome, ApprovalView};
use crate::ui::martian::MartianAnimation;
use crate::ui::prompt_input::{PromptInput, PromptOutcome};
use crate::ui::scroll_view::{GLIDE_MS, ScrollView};
use crate::ui::session_picker::{PickerOutcome, SessionPicker};
use crate::ui::setup::{Setup, SetupOutcome};
use crate::ui::splash::Splash;
use crate::ui::status_bar::{self, StatusBar};
use crate::ui::thinking_view::SPINNER_MS;
use crate::view::{
    AgentProgress, ApprovalId, ApprovalRequest, CommandInfo, CompactOutcome, Decision, EmptyNext, Message, Role, SessionPort,
    SetupStart, StatusInfo, ToolState, ToolStatus, Totals, TurnEndReason, TurnStatus, UiEvent, WelcomeInfo,
};
use marv_core::{StopReason, Usage};

/// How long "Press ctrl+c again to exit" waits for the second ctrl+c.
pub const EXIT_CONFIRM_MS: Duration = Duration::from_millis(1500);
/// How long a status bar flash ("Copied 3 lines") stays.
pub const NOTICE_MS: Duration = Duration::from_millis(2000);
/// Streamed text reaches the transcript at most this often (~30 fps).
pub const STREAM_FLUSH_MS: Duration = Duration::from_millis(33);
/// How long `run` waits for `SessionPort::flush` after the terminal is restored.
pub const FLUSH_TIMEOUT: Duration = Duration::from_secs(5);

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

/// Said when `/resume` would swap a session in under a running turn.
const WORKING: &str = "Marv is working: stop the current turn (Esc) before resuming another session.";

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

/// How a selection is copied (Marv's swappable `copy` prop): returns how it copied, e.g. "wl-copy"
/// or "osc52". Tests swap in one that touches no clipboard.
pub type CopyFn = Arc<dyn Fn(String) -> BoxFuture<'static, String> + Send + Sync>;

/// A tool call the model is still writing.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Draft {
    index: usize,
    name: String,
    args: String,
}

/// One transcript entry and its laid-out lines, cached per (width, show_steps) like Marv's `memo`.
struct Entry {
    item: Item,
    cache: Option<(u16, bool, Vec<Line<'static>>)>,
}

enum Item {
    Welcome,
    Message(Box<Message>),
}

/// A subagent's log plus the bookkeeping Marv kept in a WeakMap beside it.
/// TODO(#9): `AgentLog::new`/`AgentLog::apply` own this once agent_view is ported.
struct SubLog {
    log: AgentLog,
    next_id: u64,
    tools: HashMap<String, usize>,
    thinking_since: Option<Instant>,
}

/// What one turn keeps between its events (the locals of Marv's `send()`).
struct Turn {
    /// The reply streaming in and any reasoning before it: they reach the screen in batches.
    reply: String,
    thought: String,
    step_started: Instant,
    thought_ms: Option<Duration>,
    /// The tool call streaming in, if any: the latest one, when a reply has several.
    draft: Option<Draft>,
    /// After a retry the waiting line shows its message; the new attempt's first output clears it.
    retrying: bool,
    /// Each call's transcript entry (message id) and label.
    tool_lines: HashMap<String, (u64, String)>,
    /// Calls that got their tool end; any other entry is closed when the turn ends.
    ended: HashSet<String>,
    /// Subagents' progress, batched into the same flush as streamed text.
    progress: HashMap<String, AgentProgress>,
    steps: HashMap<String, Vec<String>>,
    /// Subagents' logs, by call id → their entry's message id.
    logs: HashMap<String, u64>,
    log_changed: bool,
    /// A compaction the user stopped has already said the message wasn't sent.
    compaction_stopped: bool,
}

impl Turn {
    fn new(now: Instant) -> Self {
        Self {
            reply: String::new(),
            thought: String::new(),
            step_started: now,
            thought_ms: None,
            draft: None,
            retrying: false,
            tool_lines: HashMap::new(),
            ended: HashSet::new(),
            progress: HashMap::new(),
            steps: HashMap::new(),
            logs: HashMap::new(),
            log_changed: false,
            compaction_stopped: false,
        }
    }
}

/// The App's state. Fields mirror Marv's `useState`s.
pub struct App {
    port: Arc<dyn SessionPort>,
    events: UnboundedReceiver<UiEvent>,
    options: AppOptions,
    phase: Phase,
    splash: Option<Splash>,
    commands: Vec<CommandInfo>,
    status: StatusInfo,
    welcome: WelcomeInfo,
    martian: MartianAnimation,

    // --- the transcript ---
    entries: Vec<Entry>,
    next_id: u64,
    scroll: ScrollView,
    /// The subagent view's own scroll position (Marv keyed a second ScrollView by the view).
    view_scroll: ScrollView,
    selection: SelectionStore,
    show_steps: bool,
    /// Each subagent's log, by its entry's message id. Only this session's runs: sessions don't save them.
    agent_logs: HashMap<u64, SubLog>,
    viewing: Option<u64>,
    /// Where each subagent entry was drawn (transcript rows), for hit-testing clicks.
    agent_rows: Vec<(u64, std::ops::Range<usize>)>,

    // --- the turn ---
    turn: Option<Turn>,
    /// Some while the session works (Marv's `streaming !== null`): the reply shown so far.
    streaming: Option<String>,
    thinking: String,
    draft: Option<Draft>,
    flush_at: Option<Instant>,
    tools_running: usize,
    agents_running: usize,
    waiting_for: Option<String>,
    compacting: bool,
    /// Work outside a turn (`UiEvent::Busy`), shown on the waiting line.
    busy_label: Option<String>,
    /// The waiting line's label and when it appeared (a new label restarts its clock).
    waiting: Option<(String, Instant)>,
    /// The streaming reply's finished lines, laid out once: (width, bytes covered, lines). Only the
    /// unfinished last line is wrapped again on each frame (the job `markdown::StreamLexer` does
    /// for the real renderer, #9).
    stream_cache: Option<(u16, String, Vec<Line<'static>>)>,

    // --- status ---
    last_usage: Option<Usage>,
    totals: Totals,
    confirm_exit: Option<Instant>,
    flash: Option<(String, Instant)>,

    // --- the bottom panel ---
    prompt: PromptInput,
    approvals: VecDeque<Pending>,
    approval: Option<(ApprovalId, ApprovalView)>,
    picker: Option<SessionPicker>,
    setup: Option<Setup>,

    // --- the loop ---
    copy: CopyFn,
    copies: FuturesUnordered<BoxFuture<'static, (usize, String)>>,
    /// What the last draw showed that animates on its own.
    welcome_visible: bool,
    spinner_visible: bool,
    exit: bool,
    /// Set by any change that needs a draw.
    dirty: bool,
}

impl App {
    /// The engine hands over its port and the receiving end of its event channel.
    pub fn new(port: Arc<dyn SessionPort>, events: UnboundedReceiver<UiEvent>, options: AppOptions) -> Self {
        let now = Instant::now();
        let commands = port.commands();
        let phase = if options.splash_ms > 0 { Phase::Splash } else { Phase::Main };
        let splash = (options.splash_ms > 0).then(|| {
            Splash::new(
                options.welcome.version.clone(),
                options.welcome.cwd.clone(),
                now,
                Duration::from_millis(options.splash_ms),
            )
        });
        let copy: CopyFn = Arc::new(|text: String| Box::pin(async move { crate::clipboard::copy_to_clipboard(&text).await }));
        Self {
            port,
            events,
            status: options.status.clone(),
            welcome: options.welcome.clone(),
            options,
            phase,
            splash,
            commands,
            martian: MartianAnimation::new(now),
            entries: vec![Entry { item: Item::Welcome, cache: None }],
            next_id: 1,
            scroll: ScrollView::default(),
            view_scroll: ScrollView::default(),
            selection: SelectionStore::new(),
            show_steps: false,
            agent_logs: HashMap::new(),
            viewing: None,
            agent_rows: Vec::new(),
            turn: None,
            streaming: None,
            thinking: String::new(),
            draft: None,
            flush_at: None,
            tools_running: 0,
            agents_running: 0,
            waiting_for: None,
            compacting: false,
            busy_label: None,
            waiting: None,
            stream_cache: None,
            last_usage: None,
            totals: Totals::default(),
            confirm_exit: None,
            flash: None,
            prompt: PromptInput::default(),
            approvals: VecDeque::new(),
            approval: None,
            picker: None,
            setup: None,
            copy,
            copies: FuturesUnordered::new(),
            welcome_visible: true,
            spinner_visible: false,
            exit: false,
            dirty: true,
        }
    }

    /// Swaps how selections are copied (tests, or a host with its own clipboard).
    pub fn with_copy(mut self, copy: CopyFn) -> Self {
        self.copy = copy;
        self
    }

    /// Runs until exit: enters the terminal (`render_options::TerminalGuard`), loops as described
    /// in the module docs, restores the terminal, then awaits `port.flush()` (bounded).
    pub async fn run(mut self) -> anyhow::Result<()> {
        let guard = TerminalGuard::enter(self.options.render)?;
        // No `terminal.clear()`: it asks the terminal where the cursor is, and a terminal that never
        // answers would stop Marv starting. The alternate screen starts blank, and the first draw
        // writes every cell.
        let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
        let mut input = EventStream::new();
        let mut next_frame = Instant::now();

        loop {
            let now = Instant::now();
            self.tick(now);
            if self.exit {
                break;
            }
            if self.dirty && now >= next_frame {
                draw_synchronized(&mut terminal, |frame| self.draw(frame))?;
                self.dirty = false;
                next_frame = now + FRAME_INTERVAL;
            }
            let mut deadlines = Deadlines::new();
            deadlines.add(self.deadline());
            if self.dirty {
                deadlines.add(Some(next_frame));
            }
            let sleep = deadlines.next().map(tokio::time::Instant::from_std);

            tokio::select! {
                event = input.next() => match event {
                    Some(Ok(event)) => self.on_event(event, Instant::now()),
                    // The terminal went away: nothing more can be shown or typed.
                    Some(Err(_)) | None => break,
                },
                event = self.events.recv() => match event {
                    Some(event) => {
                        let now = Instant::now();
                        self.apply(event, now);
                        // Drain what's queued: a burst of tokens is one frame.
                        while let Ok(event) = self.events.try_recv() {
                            self.apply(event, now);
                        }
                    }
                    // The engine is gone.
                    None => break,
                },
                Some((chars, how)) = self.copies.next(), if !self.copies.is_empty() => {
                    self.copied(chars, &how, Instant::now());
                }
                _ = async { tokio::time::sleep_until(sleep.expect("guarded")).await }, if sleep.is_some() => {}
            }
        }

        drop(terminal);
        drop(guard);
        // On the way out: save the session and let pending trajectory records reach the disk.
        let _ = tokio::time::timeout(FLUSH_TIMEOUT, self.port.flush()).await;
        Ok(())
    }

    /// Whether the App wants to exit (`/exit`, ctrl+c twice, first-run setup cancelled).
    pub fn exiting(&self) -> bool {
        self.exit
    }

    /// Whether a draw is due.
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// The session is working: a turn, a compaction, or work outside a turn.
    pub fn busy(&self) -> bool {
        self.streaming.is_some() || self.busy_label.is_some()
    }

    /// A terminal event.
    pub fn on_event(&mut self, event: Event, now: Instant) {
        match event {
            // Press and repeat; releases only arrive with the kitty protocol, which isn't on.
            Event::Key(key) if key.kind != KeyEventKind::Release => self.on_key(key, now),
            Event::Mouse(mouse) => {
                if let Some(event) = crate::mouse::from_crossterm(mouse) {
                    self.on_mouse(event, now);
                }
            }
            Event::Paste(text) => {
                if self.phase == Phase::Main && self.setup.is_none() && self.approval.is_none() && self.picker.is_none() {
                    self.prompt.paste(&text);
                    self.dirty = true;
                }
            }
            Event::Resize(..) => self.dirty = true,
            _ => {}
        }
    }

    /// Advances everything that moves on its own and is due by `now`: the splash, the stream flush,
    /// the martian, the wheel glide, selection auto-scroll, the spinner and the expiries.
    pub fn tick(&mut self, now: Instant) {
        if self.phase == Phase::Splash && self.splash.as_ref().is_some_and(|s| s.done(now)) {
            self.finish_splash(now);
        }
        if self.flush_at.is_some_and(|at| now >= at) {
            self.flush();
        }
        if self.phase != Phase::Main {
            return;
        }
        if self.welcome_visible && self.viewing.is_none() && self.martian.tick(now) {
            self.dirty = true;
        }
        let scroll = if self.viewing.is_some() { &mut self.view_scroll } else { &mut self.scroll };
        if glide_step(scroll, now) {
            self.dirty = true;
        }
        if let SelectionEffect::ScrollBy(rows) = self.selection.tick(now) {
            self.scroll_active(rows);
        }
        if self.spinner_visible
            && self.waiting.as_ref().is_some_and(|(_, since)| now >= next_spinner(*since, now - FRAME_INTERVAL))
        {
            self.dirty = true;
        }
        if self.confirm_exit.is_some_and(|until| now >= until) {
            self.confirm_exit = None;
            self.dirty = true;
        }
        if self.flash.as_ref().is_some_and(|(_, until)| now >= *until) {
            self.flash = None;
            self.dirty = true;
        }
    }

    /// The earliest moment something on screen changes on its own.
    pub fn deadline(&self) -> Option<Instant> {
        let mut d = Deadlines::new();
        if self.phase == Phase::Splash {
            d.add(self.splash.as_ref().map(Splash::deadline));
            return d.next();
        }
        // The martian only while its banner is on screen: a scrolled-away martian never wakes the loop.
        if self.welcome_visible && self.viewing.is_none() {
            d.add(self.martian.deadline());
        }
        d.add(self.flush_at);
        d.add(if self.viewing.is_some() { self.view_scroll.deadline() } else { self.scroll.deadline() });
        d.add(self.selection.deadline());
        if self.spinner_visible
            && let Some((_, since)) = &self.waiting
        {
            d.add(Some(next_spinner(*since, Instant::now())));
        }
        d.add(self.confirm_exit);
        d.add(self.flash.as_ref().map(|(_, until)| *until));
        d.next()
    }

    fn finish_splash(&mut self, now: Instant) {
        self.phase = Phase::Main;
        self.splash = None;
        self.martian = MartianAnimation::new(now);
        self.dirty = true;
    }

    // ------------------------------------------------------------------------------------------
    // The transcript
    // ------------------------------------------------------------------------------------------

    fn add_message(&mut self, mut message: Message) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        message.id = id;
        self.entries.push(Entry { item: Item::Message(Box::new(message)), cache: None });
        self.dirty = true;
        id
    }

    fn system(&mut self, text: impl Into<String>) {
        self.add_message(standin::system(text.into(), false));
    }

    fn system_error(&mut self, text: impl Into<String>) {
        self.add_message(standin::system(text.into(), true));
    }

    fn update_tool(&mut self, id: u64, tool: ToolStatus) {
        // Usually the last few entries: search from the end.
        if let Some(entry) = self.entries.iter_mut().rev().find(|e| matches!(&e.item, Item::Message(m) if m.id == id)) {
            if let Item::Message(m) = &mut entry.item {
                m.tool = Some(tool);
            }
            entry.cache = None;
            self.dirty = true;
        }
    }

    fn clear_transcript(&mut self) {
        self.selection.reset();
        self.agent_logs.clear();
        self.viewing = None;
        self.last_usage = None;
        self.entries = vec![Entry { item: Item::Welcome, cache: None }];
        self.scroll.follow();
        self.dirty = true;
    }

    fn open_view(&mut self, id: Option<u64>) {
        // The two transcripts have different rows: a selection or remembered row of one means nothing in the other.
        self.selection.reset();
        self.viewing = id;
        self.view_scroll = ScrollView::default();
        self.dirty = true;
    }

    // ------------------------------------------------------------------------------------------
    // Engine events (Marv `send()`'s event switch, plus the port's answers)
    // ------------------------------------------------------------------------------------------

    /// Folds one engine event into the state (Marv `send()`'s event switch).
    pub fn apply(&mut self, event: UiEvent, now: Instant) {
        match event {
            UiEvent::Approval { id, request } => {
                self.approvals.push_back(Pending { id, request });
                self.show_approvals();
            }
            UiEvent::Message(message) => {
                self.add_message(message);
            }
            UiEvent::StatusInfo(status) => {
                self.status = status;
                self.dirty = true;
            }
            UiEvent::Welcome(welcome) => {
                self.welcome = welcome;
                self.dirty = true;
            }
            UiEvent::Clear => self.clear_transcript(),
            UiEvent::OpenSetup(context) => {
                let setup = Setup::new(context);
                if let Some(provider) = setup.needs_models() {
                    self.port.load_models(provider);
                }
                self.setup = Some(setup);
                self.dirty = true;
            }
            UiEvent::Models(models) => {
                if let Some(setup) = &mut self.setup {
                    setup.models_loaded(models);
                    self.dirty = true;
                }
            }
            UiEvent::Sessions(list) => {
                // A turn started while the list loaded: don't put the picker over it.
                if self.busy() {
                    self.system_error(WORKING);
                } else if list.is_empty() {
                    self.system("No saved sessions for this project yet.");
                } else {
                    self.picker = Some(SessionPicker::new(&list, unix_ms()));
                    self.dirty = true;
                }
            }
            UiEvent::Resumed(resumed) => {
                self.selection.reset();
                self.agent_logs.clear();
                self.viewing = None;
                // Never lower it: an update still on its way for a message of this process must not hit a restored one.
                let highest = resumed.transcript.iter().map(|m| m.id).max().unwrap_or(0);
                self.next_id = self.next_id.max(highest + 1);
                self.totals = resumed.totals;
                self.last_usage = None;
                self.entries = std::iter::once(Entry { item: Item::Welcome, cache: None })
                    .chain(resumed.transcript.into_iter().map(|m| Entry { item: Item::Message(Box::new(m)), cache: None }))
                    .collect();
                let switched = if resumed.model != self.status.model {
                    format!(" (it used {}; continuing with {})", resumed.model, self.status.model)
                } else {
                    String::new()
                };
                let ago = crate::ui::session_picker::time_ago(resumed.updated_at, unix_ms());
                self.system(format!("Resumed a session from {ago}{switched}."));
                self.scroll.follow();
            }
            UiEvent::Flash(text) => {
                self.flash = Some((text, now + NOTICE_MS));
                self.dirty = true;
            }
            UiEvent::Busy(label) => {
                self.busy_label = label;
                self.dirty = true;
            }
            UiEvent::Exit => self.exit = true,
            event => self.apply_turn(event, now),
        }
    }

    /// A turn's events (Marv `SessionEvent` / `LoopEvent`).
    fn apply_turn(&mut self, event: UiEvent, now: Instant) {
        if self.turn.is_none() {
            if matches!(event, UiEvent::TurnEnd { .. }) {
                return;
            }
            self.start_turn(now);
        }
        let mut t = self.turn.take().expect("started above");
        // The step moved on: the call is complete (or never will be), so its draft goes.
        if t.draft.is_some() && is_step_end(&event) {
            t.draft = None;
            self.draft = None;
            self.dirty = true;
        }
        let retried = |t: &mut Turn, waiting_for: &mut Option<String>| {
            if t.retrying {
                t.retrying = false;
                *waiting_for = None;
            }
        };
        match event {
            UiEvent::TurnStart { .. } => {}
            UiEvent::Status(status) => {
                self.waiting_for = match status {
                    TurnStatus::WaitingForMcp => Some("Waiting for MCP servers to start…".into()),
                    TurnStatus::Checking => Some("Checking…".into()),
                    _ => None,
                };
                self.compacting = status == TurnStatus::Compacting;
                if status == TurnStatus::Running {
                    t.step_started = now; // the wait for the next reply starts after a check
                }
                self.dirty = true;
            }
            UiEvent::Compaction { automatic, outcome } => {
                self.compacting = false;
                t.compaction_stopped = matches!(&outcome, CompactOutcome::NotCompacted { reason, .. } if reason == "stopped");
                self.report_compaction(outcome, automatic);
            }
            UiEvent::SubagentProgress { call_id, progress } => {
                t.steps.insert(call_id.clone(), progress.steps.clone());
                t.progress.insert(call_id, progress);
                self.schedule_flush(now);
            }
            UiEvent::Subagent { call_id, event, .. } => {
                if let Some(&key) = t.logs.get(&call_id)
                    && let Some(log) = self.agent_logs.get_mut(&key)
                {
                    standin::apply_to_log(log, &event, now);
                    // Only the open view costs a draw; the others just keep their log.
                    if self.viewing == Some(key) {
                        t.log_changed = true;
                        self.schedule_flush(now);
                    }
                }
            }
            UiEvent::ThinkingDelta(text) => {
                retried(&mut t, &mut self.waiting_for);
                t.thought.push_str(&text);
                self.schedule_flush(now);
            }
            UiEvent::TextDelta(text) => {
                retried(&mut t, &mut self.waiting_for);
                if !t.thought.is_empty() && t.thought_ms.is_none() {
                    t.thought_ms = Some(now - t.step_started);
                }
                t.reply.push_str(&text);
                self.schedule_flush(now);
            }
            UiEvent::ToolCallDelta { index, name, text } => {
                retried(&mut t, &mut self.waiting_for);
                if !t.thought.is_empty() && t.thought_ms.is_none() {
                    t.thought_ms = Some(now - t.step_started);
                }
                let draft = match &mut t.draft {
                    Some(d) if d.index == index => d,
                    _ => t.draft.insert(Draft { index, name: name.clone(), args: String::new() }),
                };
                if !name.is_empty() {
                    draft.name = name;
                }
                draft.args.push_str(&text);
                self.schedule_flush(now);
            }
            UiEvent::Retry { message, .. } => {
                // The connection was lost and the provider is asking again from the start: what the failed attempt
                // drew (reply, reasoning, a tool call being written) goes, and the waiting line says what happened
                // until the new attempt streams.
                t.reply.clear();
                t.thought.clear();
                t.thought_ms = None;
                t.draft = None;
                self.turn = Some(t);
                self.flush();
                t = self.turn.take().expect("put back above");
                self.waiting_for = Some(message);
                t.retrying = true;
            }
            UiEvent::Assistant { text } => {
                self.turn = Some(t);
                self.flush(); // a step ended: show everything before moving on
                self.note_thought(now);
                t = self.turn.take().expect("put back above");
                self.add_message(standin::message(Role::Assistant, text));
                t.reply.clear();
                self.streaming = Some(String::new());
            }
            UiEvent::ToolStart { call_id, name, label } => {
                self.turn = Some(t);
                self.flush();
                self.note_thought(now);
                t = self.turn.take().expect("put back above");
                self.tools_running += 1;
                let agent = name == "agent";
                if agent {
                    self.agents_running += 1;
                }
                // Providers reuse call ids from step to step (Ollama's call_0…):
                // nothing from an earlier call with this id carries over.
                t.steps.remove(&call_id);
                t.progress.remove(&call_id);
                t.ended.remove(&call_id);
                t.logs.remove(&call_id);
                let mut message = standin::message(Role::Tool, name);
                message.tool = Some(ToolStatus { label: label.clone(), status: ToolState::Running, summary: None, steps: None, diff: None });
                let line = self.add_message(message);
                t.tool_lines.insert(call_id.clone(), (line, label.clone()));
                if agent {
                    // TODO(#9): AgentLog::new(title, prompt) once agent_view is ported. `ToolStart` carries no
                    // arguments, so the task (Marv's first user message) needs the engine to send it.
                    let log = AgentLog { title: label, running: true, ..AgentLog::default() };
                    self.agent_logs.insert(line, SubLog { log, next_id: 1, tools: HashMap::new(), thinking_since: None });
                    t.logs.insert(call_id, line);
                }
            }
            UiEvent::ToolEnd { call_id, name, status } => {
                let entry = t.tool_lines.get(&call_id).cloned();
                t.progress.remove(&call_id); // a late progress flush mustn't turn it back to "running"
                let call_steps = t.steps.remove(&call_id);
                t.ended.insert(call_id.clone());
                if let Some((line, _)) = entry {
                    let steps = status.steps.clone().or(call_steps);
                    self.update_tool(line, ToolStatus { steps, ..status });
                }
                if let Some(key) = t.logs.remove(&call_id)
                    && let Some(log) = self.agent_logs.get_mut(&key)
                {
                    // It may have ended before its loop started (a failed check): either way it's over now.
                    log.log.running = false;
                    self.dirty = true;
                }
                self.tools_running = self.tools_running.saturating_sub(1);
                if name == "agent" {
                    self.agents_running = self.agents_running.saturating_sub(1);
                }
                t.step_started = now;
            }
            UiEvent::ToolSkipped { .. } | UiEvent::StepLimit { .. } | UiEvent::Compacted => {}
            UiEvent::Usage { last, totals } => {
                self.last_usage = Some(last);
                self.totals = totals;
                self.dirty = true;
            }
            UiEvent::Error(message) => self.system_error(message),
            UiEvent::CutOff { continued } => {
                // The run ending here is said by done ("length"); going on, the user should know why the model changes tack.
                if continued {
                    self.system("✻ The reply hit the model's output limit, so nothing in it ran. Marv told the model to continue in smaller steps.");
                }
            }
            UiEvent::TextLoop { continued } => {
                // The reply was stuck repeating itself: it was stopped and dropped. The partial reply on screen goes.
                t.reply.clear();
                self.turn = Some(t);
                self.flush();
                t = self.turn.take().expect("put back above");
                if continued {
                    self.system("✻ The reply got stuck repeating itself, so Marv stopped it, dropped it, and told the model to take another approach.");
                }
            }
            UiEvent::ToolLoop { continued } => {
                if continued {
                    self.system("✻ The model kept making the same tool call and getting the same result, so Marv answered with a note instead of running it again.");
                }
            }
            UiEvent::EmptyReply { next } => match next {
                // A first empty reply is asked for again without a word: usually the provider dropped it.
                EmptyNext::Retry => {}
                EmptyNext::Nudge => self.system("✻ The model sent two empty replies in a row, so Marv asked it to continue."),
                EmptyNext::Stop => self.system("The model's replies stayed empty, so the run ended. Say \"continue\" to try again."),
            },
            UiEvent::Done(reason) => match reason {
                StopReason::Aborted => self.system("Interrupted."),
                StopReason::Declined => self.system("Stopped. Tell Marv what to do instead."),
                StopReason::Length => self.system("The reply was cut off at the model's output limit, so nothing in it ran."),
                StopReason::Loop => self.system(
                    "Stopped: the model stayed stuck in a loop after Marv's note, so the task isn't finished. Say what to try instead, or \"continue\".",
                ),
                _ => {}
            },
            UiEvent::TurnEnd { reason, .. } => {
                // Stopped before the loop began: while waiting for MCP servers (or compacting, which said so itself).
                if reason == TurnEndReason::Interrupted && !t.compaction_stopped {
                    self.system("Interrupted.");
                }
                self.turn = Some(t);
                self.end_turn(now);
                return;
            }
            // Handled in `apply`.
            _ => {}
        }
        self.turn = Some(t);
    }

    /// Busy from here, so a second message can't start a second turn; Esc or ctrl+c stop this one.
    fn start_turn(&mut self, now: Instant) {
        self.turn = Some(Turn::new(now));
        self.streaming = Some(String::new());
        self.dirty = true;
    }

    /// The turn is over, however it ended (Marv's `finally`).
    fn end_turn(&mut self, now: Instant) {
        let Some(t) = self.turn.take() else { return };
        self.flush_at = None;
        // The turn ended early: close the entries it never ended.
        for (call_id, (line, label)) in &t.tool_lines {
            if !t.ended.contains(call_id) {
                let steps = t.steps.get(call_id).cloned();
                let status = ToolStatus {
                    label: label.clone(),
                    status: ToolState::Error,
                    summary: Some("interrupted".into()),
                    steps,
                    diff: None,
                };
                self.update_tool(*line, status);
            }
        }
        for key in t.logs.values() {
            if let Some(log) = self.agent_logs.get_mut(key) {
                log.log.running = false;
            }
        }
        // note_thought needs the turn's reasoning.
        let thought = t.thought.clone();
        let since = t.thought_ms.unwrap_or_else(|| now - t.step_started);
        if !thought.is_empty() {
            self.system(format!("✻ Thought for {}s", round_secs(since)));
        }
        self.thinking.clear();
        self.draft = None;
        // Anything still queued belongs to this turn, which is over: declined.
        self.decline_all();
        self.streaming = None;
        self.waiting_for = None;
        self.compacting = false;
        self.tools_running = 0;
        self.agents_running = 0;
        self.dirty = true;
    }

    fn note_thought(&mut self, now: Instant) {
        let Some(t) = &mut self.turn else { return };
        let note = (!t.thought.is_empty()).then(|| round_secs(t.thought_ms.unwrap_or_else(|| now - t.step_started)));
        t.thought.clear();
        t.thought_ms = None;
        self.thinking.clear();
        if let Some(seconds) = note {
            self.system(format!("✻ Thought for {seconds}s"));
        }
    }

    fn schedule_flush(&mut self, now: Instant) {
        self.flush_at.get_or_insert(now + STREAM_FLUSH_MS);
    }

    /// Streamed text, reasoning, the draft and subagents' progress reach the screen.
    fn flush(&mut self) {
        self.flush_at = None;
        let Some(mut t) = self.turn.take() else { return };
        self.streaming = Some(t.reply.clone());
        self.thinking.clone_from(&t.thought);
        self.draft.clone_from(&t.draft);
        t.log_changed = false;
        let progress: Vec<_> = t.progress.drain().collect();
        for (call_id, p) in progress {
            if let Some((line, label)) = t.tool_lines.get(&call_id).cloned() {
                let status =
                    ToolStatus { label, status: ToolState::Running, summary: Some(p.line), steps: Some(p.steps), diff: None };
                self.update_tool(line, status);
            }
        }
        self.turn = Some(t);
        self.dirty = true;
    }

    /// Says how a compaction went (automatic: before a message, because the context was nearly full).
    fn report_compaction(&mut self, outcome: CompactOutcome, automatic: bool) {
        match outcome {
            CompactOutcome::NotCompacted { reason, error } => match reason.as_str() {
                "empty" => self.system("Nothing to compact yet."),
                // Stopped before a message: that's the message too (it would run on the nearly full context).
                "stopped" => self.system(if automatic {
                    "Stopped: nothing was compacted, and your message wasn't sent."
                } else {
                    "Compaction stopped; nothing changed."
                }),
                _ => self.system_error(format!("Couldn't compact the conversation: {error}")),
            },
            CompactOutcome::Compacted { summary, tokens_before } => {
                let why = match (automatic, tokens_before, self.status.context_length) {
                    (true, Some(used), Some(context)) if used > 0 && context > 0 => {
                        format!(" (the context was {}% full)", (200 * used + context) / (2 * context))
                    }
                    _ => String::new(),
                };
                let size = match tokens_before {
                    Some(used) if used > 0 => {
                        format!(
                            ": {} → about {} tokens",
                            status_bar::tokens(used),
                            status_bar::tokens((summary.chars().count() as u64 + 2) / 4)
                        )
                    }
                    _ => String::new(),
                };
                self.system(format!(
                    "✻ Compacted the conversation{why}{size}. Marv continues from a summary; your transcript is unchanged."
                ));
            }
        }
    }

    // ------------------------------------------------------------------------------------------
    // Approvals
    // ------------------------------------------------------------------------------------------

    /// The head of the queue is shown; a new head gets a fresh view (cursor back on Yes).
    fn show_approvals(&mut self) {
        let escape_cancels = self.viewing.is_none();
        match self.approvals.front() {
            None => self.approval = None,
            Some(head) => {
                let waiting = self.approvals.len() - 1;
                match &mut self.approval {
                    Some((id, view)) if *id == head.id => view.waiting = waiting,
                    _ => self.approval = Some((head.id, ApprovalView::new(head.request.clone(), waiting, escape_cancels))),
                }
            }
        }
        self.dirty = true;
    }

    fn decide(&mut self, decision: Decision) {
        let Some(head) = self.approvals.pop_front() else { return };
        if decision == Decision::Always {
            // The session remembers the scope from now on; requests already waiting in it are covered too.
            let key = head.request.scope.key.clone();
            let (covered, rest): (Vec<_>, Vec<_>) = self.approvals.drain(..).partition(|p| p.request.scope.key == key);
            self.approvals = rest.into();
            for pending in covered {
                self.port.answer(pending.id, Decision::Yes);
            }
        }
        self.show_approvals();
        self.port.answer(head.id, decision);
    }

    /// No to everything waiting (ctrl+c at an approval, and part of Esc).
    fn decline_all(&mut self) {
        let pending: Vec<_> = self.approvals.drain(..).collect();
        self.show_approvals();
        for p in pending {
            self.port.answer(p.id, Decision::No);
        }
    }

    /// Esc at an approval: decline everything and stop the run, like ctrl+c, so parallel subagents
    /// that are still running don't raise new prompts.
    fn cancel_all(&mut self) {
        self.decline_all();
        self.port.abort();
    }

    // ------------------------------------------------------------------------------------------
    // Keys
    // ------------------------------------------------------------------------------------------

    /// Handles a key (Marv's useInput handlers: Esc, ctrl+c, ctrl+o, PgUp/PgDn, the prompt).
    pub fn on_key(&mut self, key: KeyEvent, now: Instant) {
        if self.phase == Phase::Splash {
            self.finish_splash(now);
            return;
        }
        // Typing anything clears the highlight, like in a terminal.
        if self.selection.clear() == SelectionEffect::Redraw {
            self.dirty = true;
        }
        self.dirty = true;
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

        if let Some(setup) = &mut self.setup {
            // While setup is open, Setup handles Esc and ctrl+c itself.
            match setup.on_key(key) {
                Some(SetupOutcome::LoadModels(provider)) => self.port.load_models(provider),
                Some(SetupOutcome::Complete(answer)) => {
                    self.setup = None;
                    self.port.complete_setup(answer);
                }
                Some(SetupOutcome::Cancel) => {
                    // On first run there is nothing to go back to, so cancelling setup quits.
                    if setup.context.start == SetupStart::FirstRun {
                        self.exit = true;
                    }
                    self.setup = None;
                }
                None => {}
            }
            return;
        }

        // What the panels see is this key's state, before the App's own handlers change it (as in
        // Marv, where every useInput saw the same render).
        let viewing_before = self.viewing.is_some();
        match key.code {
            // Esc stops a running reply or tool, and declines any approval already queued: one can join the queue
            // just before its prompt is drawn, and must not be left waiting on a stopped run. With a subagent's view
            // open, Esc only closes it (the run carries on).
            KeyCode::Esc => {
                if self.viewing.is_some() {
                    self.open_view(None);
                } else if self.busy() {
                    self.cancel_all();
                }
            }
            // ctrl+o: show or hide details under their entries: subagents' steps and whole diffs.
            KeyCode::Char('o') if ctrl => self.show_steps = !self.show_steps,
            // ctrl+c: interrupt a reply → clear the prompt → ask to confirm → exit.
            KeyCode::Char('c') if ctrl => {
                if self.busy() {
                    self.decline_all(); // pending approvals count as declined
                    self.port.abort();
                } else if !self.prompt.value.is_empty() {
                    self.prompt.set("");
                } else if self.confirm_exit.is_some() {
                    self.exit = true;
                } else {
                    self.confirm_exit = Some(now + EXIT_CONFIRM_MS);
                }
                return;
            }
            KeyCode::PageUp => self.page(-1),
            KeyCode::PageDown => self.page(1),
            _ => {}
        }

        if let Some(picker) = self.picker.as_mut().filter(|_| self.approval.is_none()) {
            match picker.on_key(key) {
                Some(PickerOutcome::Pick(id)) => {
                    self.picker = None;
                    if self.busy() {
                        self.system_error(WORKING);
                    } else {
                        self.port.resume(Some(id));
                    }
                }
                Some(PickerOutcome::Cancel) => self.picker = None,
                None => {}
            }
        } else if let Some((_, view)) = &mut self.approval {
            view.escape_cancels = !viewing_before;
            match view.on_key(key) {
                Some(ApprovalOutcome::Decide(decision)) => self.decide(decision),
                Some(ApprovalOutcome::CancelAll) => self.cancel_all(),
                None => {}
            }
        } else if !matches!(key.code, KeyCode::PageUp | KeyCode::PageDown) {
            let busy = self.busy();
            if let Some(PromptOutcome::Submit(text)) = self.prompt.on_key(key, busy, &self.commands) {
                self.submit(&text, now);
            }
        }
    }

    /// Marv's `handleSubmit`.
    fn submit(&mut self, raw: &str, now: Instant) {
        let text = raw.trim();
        if text.is_empty() || self.busy() {
            return;
        }
        let text = text.to_string();
        self.prompt.set("");
        self.scroll.follow();
        if self.viewing.is_some() {
            self.open_view(None);
        }
        self.prompt.remember(&text);
        self.prompt.history_index = None;
        if text.starts_with('/') {
            self.port.run_command(text);
        } else {
            self.add_message(standin::message(Role::User, text.clone()));
            self.start_turn(now);
            self.port.send(text);
        }
    }

    fn page(&mut self, direction: i32) {
        let scroll = if self.viewing.is_some() { &mut self.view_scroll } else { &mut self.scroll };
        // A page keeps one row of overlap.
        let rows = (scroll.viewport_height as i32 - 1).max(1);
        scroll_by(scroll, direction * rows);
        self.dirty = true;
    }

    fn scroll_active(&mut self, rows: i32) {
        let scroll = if self.viewing.is_some() { &mut self.view_scroll } else { &mut self.scroll };
        scroll_by(scroll, rows);
        self.dirty = true;
    }

    // ------------------------------------------------------------------------------------------
    // Mouse
    // ------------------------------------------------------------------------------------------

    /// Handles a mouse event (wheel glide, selection, clicks on agent entries).
    pub fn on_mouse(&mut self, event: MouseEvent, now: Instant) {
        if self.phase != Phase::Main {
            return;
        }
        let effect = match event {
            MouseEvent::Scroll { step } => {
                // The highlight is attached to the text, so it scrolls along.
                let scroll = if self.viewing.is_some() { &mut self.view_scroll } else { &mut self.scroll };
                glide_by(scroll, step as i32 * WHEEL_ROWS, now);
                self.dirty = true;
                SelectionEffect::None
            }
            MouseEvent::Press { x, y } => self.selection.press(x, y),
            MouseEvent::Drag { x, y } => self.selection.drag(x, y, now),
            MouseEvent::Release { x, y } => match self.selection.release() {
                SelectionEffect::Copy(text) if !text.is_empty() => SelectionEffect::Copy(text),
                effect => {
                    // A click: on a subagent's entry, open its view.
                    let row = if self.viewing.is_some() { None } else { self.selection.row_at(x, y) };
                    let id = row.and_then(|row| self.agent_rows.iter().find(|(_, rows)| rows.contains(&row)).map(|(id, _)| *id));
                    if let Some(id) = id {
                        if self.agent_logs.contains_key(&id) {
                            self.open_view(Some(id));
                        } else {
                            self.flash = Some((
                                "Only this session's subagents can be opened (ctrl+o shows the steps)".into(),
                                now + NOTICE_MS,
                            ));
                        }
                    }
                    effect
                }
            },
        };
        match effect {
            SelectionEffect::None => {}
            SelectionEffect::Redraw => self.dirty = true,
            SelectionEffect::ScrollBy(rows) => self.scroll_active(rows),
            SelectionEffect::Copy(text) => {
                let chars = text.chars().count();
                self.copies.push(Box::pin({
                    let copy = (self.copy)(text);
                    async move { (chars, copy.await) }
                }));
                self.dirty = true;
            }
        }
    }

    /// A copy finished: say how.
    fn copied(&mut self, chars: usize, how: &str, now: Instant) {
        let text = if how == "osc52" {
            format!("Sent {chars} chars to the terminal clipboard")
        } else {
            format!("Copied {chars} chars")
        };
        self.flash = Some((text, now + NOTICE_MS));
        self.dirty = true;
    }

    /// Waits for the copies started by releases, as the loop does (for tests and hosts without `run`).
    pub async fn settle_copies(&mut self, now: Instant) {
        while let Some((chars, how)) = self.copies.next().await {
            self.copied(chars, &how, now);
        }
    }

    // ------------------------------------------------------------------------------------------
    // Drawing
    // ------------------------------------------------------------------------------------------

    /// Draws one frame.
    pub fn draw(&mut self, frame: &mut ratatui::Frame) {
        let area = frame.area();
        let now = Instant::now();
        self.draw_at(area, frame.buffer_mut(), now);
    }

    /// Draws into `buf` as of `now` (what [`draw`](Self::draw) does; tests pass their own clock).
    pub fn draw_at(&mut self, area: Rect, buf: &mut Buffer, now: Instant) {
        self.dirty = false;
        if self.phase == Phase::Splash {
            if let Some(splash) = &self.splash {
                ratatui::widgets::Widget::render(splash, area, buf);
            }
            return;
        }

        // The bottom panel sizes itself; the transcript takes what's left.
        let width = area.width;
        let busy = self.busy();
        let status_height = 1;
        let panel_height = if let Some(picker) = self.picker.as_ref().filter(|_| self.setup.is_none() && self.approval.is_none())
        {
            picker.height()
        } else if let Some((_, view)) = self.approval.as_ref().filter(|_| self.setup.is_none()) {
            view.height(width) + status_height
        } else if let Some(setup) = &self.setup {
            setup.height(width)
        } else {
            self.prompt.height(width, busy, &self.commands) + status_height
        };
        let header = u16::from(self.viewing.is_some());
        let panel_height = panel_height.min(area.height.saturating_sub(header + 1));
        let transcript = Rect { height: area.height - panel_height - header, ..area };
        let panel = Rect { y: transcript.bottom() + header, height: panel_height, ..area };

        self.spinner_visible = false;
        if let Some(id) = self.viewing.filter(|id| self.agent_logs.contains_key(id)) {
            let log = &self.agent_logs[&id].log;
            buf.set_line(area.x, transcript.bottom(), &standin::agent_header(log, width), width);
            let lines = standin::log_lines(log, width, self.show_steps);
            self.welcome_visible = false;
            let vp = draw_lines(&mut self.view_scroll, transcript, buf, &lines);
            self.selection.set_viewport(vp);
        } else {
            self.draw_transcript(transcript, buf, now);
        }
        // Remember the visible rows' text and draw the highlight (Marv's transformOutput).
        self.selection.transform_buffer(buf);

        let bar = StatusBar {
            info: Some(&self.status),
            confirm_exit: self.confirm_exit.is_some(),
            flash: self.flash.as_ref().map(|(text, _)| text.as_str()),
            usage: status_bar::usage_text(self.last_usage.as_ref(), &self.totals, self.status.context_length),
            busy,
            agents: self.agents_running,
            viewing: self.viewing.is_some(),
        };
        let status_area = Rect { y: panel.bottom().saturating_sub(1), height: 1.min(panel.height), ..panel };
        let above_status = Rect { height: panel.height.saturating_sub(1), ..panel };
        if let Some(picker) = self.picker.as_ref().filter(|_| self.setup.is_none() && self.approval.is_none()) {
            picker.render(panel, buf);
        } else if let Some((_, view)) = self.approval.as_ref().filter(|_| self.setup.is_none()) {
            view.render(above_status, buf);
            // At an approval the bar is always busy and never asks to confirm an exit.
            StatusBar { confirm_exit: false, busy: true, usage: None, ..bar }.render(status_area, buf);
        } else if let Some(setup) = &self.setup {
            setup.render(panel, buf);
        } else {
            self.prompt.render(above_status, buf, busy, &self.commands);
            bar.render(status_area, buf);
        }
    }

    /// The main transcript, culled to the viewport: only entries intersecting it are laid out
    /// (from their cache) and drawn.
    fn draw_transcript(&mut self, area: Rect, buf: &mut Buffer, now: Instant) {
        let width = area.width;
        let show_steps = self.show_steps;
        // Heights first (fills the caches: an unchanged entry is laid out once per width).
        let mut heights = Vec::with_capacity(self.entries.len());
        for entry in &mut self.entries {
            let height = match &entry.item {
                Item::Welcome => crate::ui::welcome::HEIGHT as usize + 1,
                Item::Message(message) => {
                    let fresh = matches!(&entry.cache, Some((w, s, _)) if *w == width && *s == show_steps);
                    if !fresh {
                        entry.cache = Some((width, show_steps, standin::message_lines(message, width, show_steps)));
                    }
                    entry.cache.as_ref().map_or(0, |(_, _, lines)| lines.len())
                }
            };
            heights.push(height);
        }
        let tail = self.live_lines(width, now);
        let content: usize = heights.iter().sum::<usize>() + tail.len();
        self.scroll.content_height = content;
        self.scroll.viewport_height = area.height;
        let top = self.scroll.scroll_top();
        let bottom = top + area.height as usize;

        self.welcome_visible = false;
        self.agent_rows.clear();
        let mut row = 0;
        for (entry, &height) in self.entries.iter().zip(&heights) {
            let rows = row..row + height;
            if let Item::Message(m) = &entry.item
                && m.role == Role::Tool
                && m.text == "agent"
            {
                self.agent_rows.push((m.id, rows.clone()));
            }
            if rows.end > top && rows.start < bottom {
                let welcome;
                let lines: &[Line] = match &entry.item {
                    Item::Welcome => {
                        self.welcome_visible = true;
                        welcome = standin::welcome_lines(&self.welcome, self.martian.frame(), width);
                        &welcome
                    }
                    Item::Message(_) => entry.cache.as_ref().map_or(&[], |(_, _, lines)| lines.as_slice()),
                };
                for (i, line) in lines.iter().enumerate() {
                    let at = row + i;
                    if at >= top && at < bottom {
                        buf.set_line(area.x, area.y + (at - top) as u16, line, width);
                    }
                }
            }
            row += height;
        }
        for (i, line) in tail.iter().enumerate() {
            let at = row + i;
            if at >= top && at < bottom {
                buf.set_line(area.x, area.y + (at - top) as u16, line, width);
                self.spinner_visible |= self.waiting.is_some();
            }
        }
        self.selection.set_viewport(Viewport {
            top: area.y,
            height: area.height,
            scroll_top: top,
            content_height: content,
            width,
        });
    }

    /// The live end of the transcript: the reply streaming in, the waiting line, a tool call being written.
    fn live_lines(&mut self, width: u16, now: Instant) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        let Some(streaming) = &self.streaming else {
            self.waiting = self.busy_label.as_ref().map(|label| self.waiting_since(label.clone(), now));
            if let Some((label, since)) = &self.waiting {
                lines.extend(standin::waiting_lines(label, "", None, false, *since, now, width));
            }
            return lines;
        };
        if !streaming.is_empty() {
            // Up to the last newline the reply only grows: lay that out once, then just the rest.
            let cut = streaming.rfind('\n').map_or(0, |i| i + 1);
            // A new step's reply (or a retry) can replace the text between two draws: check it's the same text.
            let (done, mut finished) = match self.stream_cache.take() {
                Some((w, prefix, lines)) if w == width && prefix.len() <= cut && streaming.starts_with(&prefix) => {
                    (prefix.len(), lines)
                }
                _ => (0, Vec::new()),
            };
            if done < cut {
                let first = finished.is_empty();
                finished.extend(standin::reply_rows(&streaming[done..cut - 1], width, first));
            }
            lines.extend(finished.iter().cloned());
            lines.extend(standin::reply_rows(&streaming[cut..], width, finished.is_empty()));
            lines.push(Line::default());
            self.stream_cache = Some((width, streaming[..cut].to_string(), finished));
        } else {
            self.stream_cache = None;
        }
        let label = if self.tools_running > 0 {
            None
        } else if let Some(draft) = &self.draft {
            Some(standin::draft_label(&draft.name, &draft.args))
        } else if streaming.is_empty() {
            Some(if self.compacting {
                "Compacting the conversation…".to_string()
            } else {
                self.waiting_for.clone().or_else(|| self.busy_label.clone()).unwrap_or_else(|| "Thinking…".into())
            })
        } else {
            None
        };
        self.waiting = label.map(|label| self.waiting_since(label, now));
        if let Some((label, since)) = &self.waiting {
            match &self.draft {
                Some(draft) => {
                    let preview = standin::draft_preview(&draft.name, &draft.args);
                    lines.extend(standin::waiting_lines(label, &preview, Some(&draft.args), true, *since, now, width));
                }
                None => lines.extend(standin::waiting_lines(label, &self.thinking, None, false, *since, now, width)),
            }
        }
        lines
    }

    /// The waiting line's clock: one wait per label (Marv keyed the view by its label).
    fn waiting_since(&self, label: String, now: Instant) -> (String, Instant) {
        match &self.waiting {
            Some((shown, since)) if *shown == label => (label, *since),
            _ => (label, now),
        }
    }
}

/// Draws a frame wrapped in synchronized output (DEC 2026), so terminals show it whole.
fn draw_synchronized(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    render: impl FnOnce(&mut ratatui::Frame),
) -> io::Result<()> {
    use crossterm::execute;
    use crossterm::terminal::{BeginSynchronizedUpdate, EndSynchronizedUpdate};
    execute!(io::stdout(), BeginSynchronizedUpdate)?;
    let result = terminal.draw(render).map(|_| ());
    execute!(io::stdout(), EndSynchronizedUpdate)?;
    result
}

/// Marv's `Math.max(1, Math.round(ms / 1000))`.
fn round_secs(d: Duration) -> u64 {
    ((d.as_millis() as u64 + 500) / 1000).max(1)
}

fn unix_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

/// The next spinner frame after `now` for a waiting line shown since `since`.
fn next_spinner(since: Instant, now: Instant) -> Instant {
    let ms = SPINNER_MS.as_millis();
    let elapsed = now.saturating_duration_since(since).as_millis();
    since + Duration::from_millis(((elapsed / ms + 1) * ms) as u64)
}

// ----------------------------------------------------------------------------------------------
// Scrolling. TODO(#9): ScrollView::scroll_by / glide_by / tick / visible own this once ported.
// ----------------------------------------------------------------------------------------------

/// Scrolls by `rows`; reaching the bottom resumes following.
fn scroll_by(scroll: &mut ScrollView, rows: i32) {
    let max = scroll.content_height.saturating_sub(scroll.viewport_height as usize);
    let next = (scroll.scroll_top() as i64 + rows as i64).clamp(0, max as i64) as usize;
    scroll.top = if next >= max { None } else { Some(next) };
}

/// A wheel notch: adds to the distance still to travel (reversing drops the rest); the first row
/// moves at once.
fn glide_by(scroll: &mut ScrollView, rows: i32, now: Instant) {
    if scroll.glide_remaining.signum() == -rows.signum() {
        scroll.glide_remaining = 0;
    }
    scroll.glide_remaining += rows;
    scroll.glide_at = Some(now);
    glide_step(scroll, now);
}

/// One glide step if due: a third of what's left (at least a row). True if it scrolled.
fn glide_step(scroll: &mut ScrollView, now: Instant) -> bool {
    match scroll.glide_at {
        Some(at) if now >= at && scroll.glide_remaining != 0 => {
            let left = scroll.glide_remaining;
            let step = left.signum() * (left.abs() / 3).max(1);
            scroll.glide_remaining -= step;
            let before = scroll.scroll_top();
            scroll_by(scroll, step);
            scroll.glide_at = (scroll.glide_remaining != 0).then_some(now + GLIDE_MS);
            if scroll.scroll_top() == before {
                // At an end: nothing more to glide.
                scroll.glide_remaining = 0;
                scroll.glide_at = None;
            }
            true
        }
        Some(_) if scroll.glide_remaining == 0 => {
            scroll.glide_at = None;
            false
        }
        _ => false,
    }
}

/// Draws already laid-out lines in a scroll view (the subagent view); returns its viewport.
fn draw_lines(scroll: &mut ScrollView, area: Rect, buf: &mut Buffer, lines: &[Line<'static>]) -> Viewport {
    scroll.content_height = lines.len();
    scroll.viewport_height = area.height;
    let top = scroll.scroll_top();
    for (i, line) in lines.iter().skip(top).take(area.height as usize).enumerate() {
        buf.set_line(area.x, area.y + i as u16, line, area.width);
    }
    Viewport { top: area.y, height: area.height, scroll_top: top, content_height: lines.len(), width: area.width }
}

/// Plain-text stand-ins for the renderers #9 ports. Each function names its replacement.
mod standin {
    use std::time::Instant;

    use marv_core::printable;
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use ratatui::style::{Modifier, Style};
    use ratatui::text::{Line, Span};
    use ratatui::widgets::Widget;

    use super::{SubLog, is_step_end, round_secs};
    use crate::ui::martian::FrameName;
    use crate::ui::select::{truncate_end, wrap};
    use crate::ui::status_bar::tokens;
    use crate::ui::theme;
    use crate::ui::thinking_view::{PREVIEW_LINES, SPINNER, SPINNER_MS, TAIL_CHARS, elapsed};
    use crate::ui::welcome::{self, Welcome};
    use crate::view::{DiffKind, Message, NoticeTone, Role, ToolState, ToolStatus, UiEvent, WelcomeInfo};
    use marv_core::StopReason;

    pub fn message(role: Role, text: String) -> Message {
        Message { id: 0, role, text, is_error: false, markdown: false, notice: None, tool: None }
    }

    pub fn system(text: String, is_error: bool) -> Message {
        Message { is_error, ..message(Role::System, text) }
    }

    /// `text` wrapped at `width - 2` beside a 2-column gutter (a hanging indent).
    fn gutter(mark: &str, mark_style: Style, text: &str, style: Style, width: u16) -> Vec<Line<'static>> {
        let mut out = Vec::new();
        for (i, row) in printable(text)
            .split('\n')
            .flat_map(|l| wrap(&Line::styled(l.to_string(), style), width.saturating_sub(2)))
            .enumerate()
        {
            let lead = if i == 0 { Span::styled(mark.to_string(), mark_style) } else { Span::raw("  ") };
            out.push(Line::from([vec![lead], row.spans].concat()));
        }
        out
    }

    /// A streaming reply's rows: the `● ` gutter on the reply's first row, a hanging indent after.
    /// TODO(#9): `message_view::render` with the reply's `StreamLexer`.
    pub fn reply_rows(text: &str, width: u16, first: bool) -> Vec<Line<'static>> {
        let mark = if first { "● " } else { "  " };
        gutter(mark, Style::new().fg(theme::ASSISTANT), text, Style::new(), width)
    }

    fn plain(text: &str, style: Style, width: u16) -> Vec<Line<'static>> {
        printable(text).split('\n').flat_map(|l| wrap(&Line::styled(l.to_string(), style), width)).collect()
    }

    /// One entry as lines, a blank line after. TODO(#9): `message_view::render(message, width, show_steps, None)`.
    pub fn message_lines(m: &Message, width: u16, show_steps: bool) -> Vec<Line<'static>> {
        let dim = Style::new().fg(theme::DIM);
        let mut lines = match m.role {
            Role::User => gutter("> ", Style::new().fg(theme::USER), &m.text, Style::new().fg(theme::USER), width),
            Role::Assistant => gutter("● ", Style::new().fg(theme::ASSISTANT), &m.text, Style::new(), width),
            Role::Tool => tool_lines(m, width, show_steps),
            Role::System => match &m.notice {
                Some(notice) => {
                    let color = match notice.tone {
                        NoticeTone::Info => theme::ACCENT,
                        NoticeTone::Warning => theme::WARNING,
                        NoticeTone::Error => theme::ERROR,
                    };
                    let mut lines = plain(&notice.title, Style::new().fg(color).add_modifier(Modifier::BOLD), width);
                    for item in &notice.items {
                        lines.extend(gutter("• ", Style::new().fg(color), &item.text, Style::new(), width));
                        for detail in item.detail.iter().flatten() {
                            lines.extend(gutter("  ", dim, detail, dim, width));
                        }
                    }
                    lines
                }
                None if m.markdown => plain(&m.text, Style::new(), width),
                None => plain(&m.text, Style::new().fg(if m.is_error { theme::ERROR } else { theme::DIM }), width),
            },
        };
        lines.push(Line::default());
        lines
    }

    /// `● name label`, `  ⎿ summary`, steps with ctrl+o, the diff collapsed to 15 lines.
    fn tool_lines(m: &Message, width: u16, show_steps: bool) -> Vec<Line<'static>> {
        let dim = Style::new().fg(theme::DIM);
        let default = ToolStatus { label: String::new(), status: ToolState::Running, summary: None, steps: None, diff: None };
        let tool = m.tool.as_ref().unwrap_or(&default);
        let failed = tool.status == ToolState::Error;
        let mut lines = vec![truncate_end(
            Line::from(vec![
                Span::styled("● ", Style::new().fg(if failed { theme::ERROR } else { theme::ACCENT })),
                Span::styled(printable(&m.text), Style::new().add_modifier(Modifier::BOLD)),
                Span::styled(format!(" {}", printable(&tool.label)), dim),
            ]),
            width,
        )];
        let summary = match (tool.status, &tool.summary) {
            (ToolState::Running, None) => "running…".to_string(),
            (_, summary) => summary.clone().unwrap_or_default(),
        };
        let color = match tool.status {
            ToolState::Error => theme::ERROR,
            ToolState::Declined => theme::WARNING,
            _ => theme::DIM,
        };
        lines.push(truncate_end(Line::styled(format!("  ⎿ {}", printable(&summary)), Style::new().fg(color)), width));
        if show_steps {
            for step in tool.steps.iter().flatten() {
                lines.push(truncate_end(Line::styled(format!("    · {}", printable(step)), dim), width));
            }
        }
        if let Some(diff) = &tool.diff {
            let max = if show_steps { usize::MAX } else { 15 };
            for line in diff.lines.iter().take(max) {
                let style = match line.kind {
                    DiffKind::Add => Style::new().fg(theme::DIFF_ADD),
                    DiffKind::Del => Style::new().fg(theme::DIFF_DEL),
                    _ => Style::new(),
                };
                let text = if line.kind == DiffKind::Gap { "…".into() } else { printable(&line.text) };
                lines.push(truncate_end(
                    Line::styled(format!("    {}{text}", crate::ui::diff_view::mark(line.kind)), style),
                    width,
                ));
            }
            let more = diff.lines.len().saturating_sub(max) + diff.more;
            if more > 0 {
                let hint = if show_steps { "" } else { " (ctrl+o)" };
                lines.push(Line::styled(format!("    … {more} more lines{hint}"), dim));
            }
        }
        lines
    }

    /// The welcome banner as lines (its widget drawn into a scratch buffer), a blank line after.
    pub fn welcome_lines(info: &WelcomeInfo, frame: FrameName, width: u16) -> Vec<Line<'static>> {
        let area = Rect::new(0, 0, width, welcome::HEIGHT);
        let mut buf = Buffer::empty(area);
        Welcome { info, frame }.render(area, &mut buf);
        let mut lines: Vec<Line<'static>> = (0..area.height)
            .map(|y| {
                let mut spans: Vec<Span<'static>> = Vec::new();
                let mut x = 0;
                while x < width {
                    let cell = &buf[(x, y)];
                    let style = cell.style();
                    let symbol = cell.symbol().to_string();
                    x += (unicode_width::UnicodeWidthStr::width(symbol.as_str()) as u16).max(1);
                    match spans.last_mut() {
                        Some(last) if last.style == style => last.content.to_mut().push_str(&symbol),
                        _ => spans.push(Span::styled(symbol, style)),
                    }
                }
                Line::from(spans)
            })
            .collect();
        lines.push(Line::default());
        lines
    }

    /// The waiting line: spinner, label, `(12s · ~400 tokens)`, and the reasoning's last lines.
    /// TODO(#9): `ThinkingView::render` / `DraftView::render`.
    pub fn waiting_lines(
        label: &str,
        thought: &str,
        count: Option<&str>,
        code: bool,
        since: Instant,
        now: Instant,
        width: u16,
    ) -> Vec<Line<'static>> {
        let dim = Style::new().fg(theme::DIM);
        let waited = now.saturating_duration_since(since);
        let frame = SPINNER[(waited.as_millis() / SPINNER_MS.as_millis()) as usize % SPINNER.len()];
        // An estimate (4 chars a token): the real count arrives with the reply's usage.
        let estimate = count.unwrap_or(thought).trim().chars().count().div_ceil(4);
        let mut shown = Vec::new();
        if waited.as_millis() >= 1000 {
            shown.push(elapsed(waited));
        }
        if estimate > 0 {
            shown.push(format!("~{} tokens", tokens(estimate as u64)));
        }
        let suffix = if shown.is_empty() { String::new() } else { format!(" ({})", shown.join(" · ")) };
        let mut lines = vec![truncate_end(
            Line::from(vec![
                Span::styled(frame, Style::new().fg(theme::ACCENT)),
                Span::styled(format!(" {}{suffix}", printable(label)), dim),
            ]),
            width,
        )];
        // Only the end is shown, so only the end is cleaned up: a draft can be a 100k-character file.
        let start = thought.char_indices().rev().nth(TAIL_CHARS.saturating_sub(1)).map_or(0, |(i, _)| i);
        let tail = printable(&thought[start..]);
        let rows: Vec<String> = tail
            .split('\n')
            .map(|l| if code { l.replace('\t', "  ").trim_end().to_string() } else { l.trim().to_string() })
            .filter(|l| !l.trim().is_empty())
            .collect();
        for row in &rows[rows.len().saturating_sub(PREVIEW_LINES)..] {
            lines.push(truncate_end(Line::styled(format!("  {row}"), dim.add_modifier(Modifier::ITALIC)), width));
        }
        lines.push(Line::default());
        lines
    }

    /// The value of `key` in unfinished JSON, decoded as far as it has arrived (Marv `partialString`).
    fn partial_string(json: &str, key: &str) -> Option<String> {
        let needle = format!("\"{key}\"");
        let at = json.find(&needle)? + needle.len();
        let rest = json[at..].trim_start().strip_prefix(':')?.trim_start().strip_prefix('"')?;
        let mut out = String::new();
        let mut chars = rest.chars();
        while let Some(c) = chars.next() {
            match c {
                '"' => break,
                '\\' => match chars.next() {
                    Some('n') => out.push('\n'),
                    Some('t') => out.push('\t'),
                    Some('r') => out.push('\r'),
                    Some('u') => {
                        // An escape cut in half (fewer than 4 hex digits) waits for the rest.
                        let hex: String = chars.by_ref().take(4).collect();
                        match u32::from_str_radix(&hex, 16).ok().filter(|_| hex.len() == 4).and_then(char::from_u32) {
                            Some(c) => out.push(c),
                            None => break,
                        }
                    }
                    Some(other) => out.push(other),
                    None => break,
                },
                c => out.push(c),
            }
        }
        Some(out)
    }

    /// "Writing src/app.js…" (Marv `draftLabel`, `src/tool-draft.ts`).
    pub fn draft_label(name: &str, args: &str) -> String {
        let path =
            partial_string(args, "path").map(|p| p.split('\n').next().unwrap_or_default().to_string()).filter(|p| !p.is_empty());
        let path = path.as_deref().unwrap_or("a file");
        match name {
            "write_file" => format!("Writing {path}…"),
            "edit_file" => format!("Editing {path}…"),
            "bash" => "Writing a command…".into(),
            "" => "Preparing a tool call…".into(),
            name => format!("Preparing {name}…"),
        }
    }

    /// The code written so far (Marv `draftPreview`).
    pub fn draft_preview(name: &str, args: &str) -> String {
        let key = match name {
            "write_file" => "content",
            "edit_file" => "new_string",
            "bash" => "command",
            _ => return String::new(),
        };
        partial_string(args, key).unwrap_or_default()
    }

    /// Pinned above a subagent's view. TODO(#9): `AgentViewHeader`.
    pub fn agent_header(log: &crate::ui::agent_view::AgentLog, width: u16) -> Line<'static> {
        let state = if log.running { "running" } else { "finished" };
        truncate_end(
            Line::from(vec![
                Span::styled("● ", Style::new().fg(theme::ACCENT)),
                Span::styled(format!("agent {} · {state}", printable(&log.title)), Style::new().add_modifier(Modifier::BOLD)),
                Span::styled("  esc to go back", Style::new().fg(theme::DIM)),
            ]),
            width,
        )
    }

    /// A subagent's log as lines. TODO(#9): `AgentView`.
    pub fn log_lines(log: &crate::ui::agent_view::AgentLog, width: u16, show_steps: bool) -> Vec<Line<'static>> {
        let mut lines: Vec<Line<'static>> = log.messages.iter().flat_map(|m| message_lines(m, width, show_steps)).collect();
        if !log.streaming.is_empty() {
            lines.extend(message_lines(&message(Role::Assistant, log.streaming.clone()), width, false));
        }
        if let Some((_, name, args)) = &log.draft {
            lines.push(Line::styled(format!("  {}", draft_label(name, args)), Style::new().fg(theme::DIM)));
        } else if log.running && log.streaming.is_empty() {
            lines.push(Line::styled("  Thinking…", Style::new().fg(theme::DIM)));
        }
        lines
    }

    /// Folds a subagent's event into its log (Marv `applyEvent`, `src/agent-log.ts`).
    /// TODO(#9): `AgentLog::apply`.
    pub fn apply_to_log(sub: &mut SubLog, event: &UiEvent, now: Instant) {
        let SubLog { log, next_id, tools, thinking_since } = sub;
        let mut add = |log: &mut crate::ui::agent_view::AgentLog, m: Message| {
            log.messages.push(Message { id: *next_id, ..m });
            *next_id += 1;
        };
        // Like the main transcript: reasoning collapses to one line once the step moves on.
        let note_thought = |log: &mut crate::ui::agent_view::AgentLog,
                            since: &mut Option<Instant>,
                            add: &mut dyn FnMut(&mut crate::ui::agent_view::AgentLog, Message)| {
            if !log.thinking.is_empty() {
                let seconds = round_secs(now - since.unwrap_or(now));
                add(log, system(format!("✻ Thought for {seconds}s"), false));
            }
            log.thinking.clear();
            *since = None;
        };
        // What streamed before an error or a stop isn't lost.
        let keep_partial = |log: &mut crate::ui::agent_view::AgentLog,
                            add: &mut dyn FnMut(&mut crate::ui::agent_view::AgentLog, Message)| {
            if !log.streaming.is_empty() {
                let text = std::mem::take(&mut log.streaming);
                add(log, message(Role::Assistant, text));
            }
        };
        if is_step_end(event) {
            log.draft = None;
        }
        log.version += 1;
        match event {
            UiEvent::ToolCallDelta { index, name, text } => {
                let draft = match &mut log.draft {
                    Some(d) if d.0 == *index => d,
                    _ => log.draft.insert((*index, name.clone(), String::new())),
                };
                if !name.is_empty() {
                    draft.1.clone_from(name);
                }
                draft.2.push_str(text);
            }
            UiEvent::ThinkingDelta(text) => {
                thinking_since.get_or_insert(now);
                log.thinking.push_str(text);
            }
            UiEvent::TextDelta(text) => log.streaming.push_str(text),
            UiEvent::TextLoop { continued } => {
                log.streaming.clear();
                let text = if *continued {
                    "✻ Stuck repeating itself: the reply was stopped and dropped, and the model was told."
                } else {
                    "Stopped: still stuck repeating itself."
                };
                add(log, system(text.into(), false));
            }
            UiEvent::ToolLoop { continued } => {
                let text = if *continued {
                    "✻ The same tool call kept getting the same result: answered with a note instead."
                } else {
                    "Stopped: still repeating the same tool call."
                };
                add(log, system(text.into(), false));
            }
            UiEvent::Retry { message: text, .. } => {
                // The provider asks again from the start: drop what the failed attempt drew, and say why.
                log.streaming.clear();
                log.thinking.clear();
                *thinking_since = None;
                log.draft = None;
                add(log, system(text.clone(), false));
            }
            UiEvent::Assistant { text } => {
                note_thought(log, thinking_since, &mut add);
                add(log, message(Role::Assistant, text.clone()));
                log.streaming.clear();
            }
            UiEvent::ToolStart { call_id, name, label } => {
                note_thought(log, thinking_since, &mut add);
                keep_partial(log, &mut add);
                tools.insert(call_id.clone(), log.messages.len());
                let mut m = message(Role::Tool, name.clone());
                m.tool =
                    Some(ToolStatus { label: label.clone(), status: ToolState::Running, summary: None, steps: None, diff: None });
                add(log, m);
            }
            UiEvent::ToolEnd { call_id, status, .. } => {
                if let Some(index) = tools.remove(call_id)
                    && let Some(m) = log.messages.get_mut(index)
                {
                    m.tool = Some(status.clone());
                }
            }
            UiEvent::Error(text) => {
                note_thought(log, thinking_since, &mut add);
                keep_partial(log, &mut add);
                add(log, system(text.clone(), true));
            }
            UiEvent::Done(reason) => {
                note_thought(log, thinking_since, &mut add);
                keep_partial(log, &mut add);
                let stopped = match reason {
                    StopReason::Aborted => Some("Interrupted."),
                    StopReason::Declined => Some("Stopped: the user declined."),
                    StopReason::Length => Some("The reply was cut off at the model's output limit, so nothing in it ran."),
                    StopReason::MaxSteps => Some("It hit its step limit."),
                    _ => None,
                };
                if let Some(text) = stopped {
                    add(log, system(text.into(), false));
                }
                log.running = false;
            }
            _ => {}
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn draft_label_and_preview_read_unfinished_json() {
            let args = r#"{"path": "page.js", "content": "const a = 1;\nfunction draw() {\n  return a;"#;
            assert_eq!(draft_label("write_file", args), "Writing page.js…");
            assert_eq!(draft_preview("write_file", args), "const a = 1;\nfunction draw() {\n  return a;");
            assert_eq!(draft_label("edit_file", r#"{"pa"#), "Editing a file…");
            assert_eq!(draft_label("bash", ""), "Writing a command…");
            assert_eq!(draft_label("grep", ""), "Preparing grep…");
            assert_eq!(draft_preview("bash", r#"{"command": "echo é\u00"#), "echo é");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::view::{DiffLine, DiffShown, Preview, ProviderId, Resumed, Scope, SessionSummary, SetupAnswer};
    use std::sync::Mutex;
    use tokio::sync::mpsc;

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

    // --- a fake engine ---

    #[derive(Debug, Clone, PartialEq)]
    enum Call {
        Send(String),
        Command(String),
        Answer(u64, Decision),
        Abort,
        LoadModels(ProviderId),
        CompleteSetup(SetupAnswer),
        Resume(Option<String>),
    }

    #[derive(Default)]
    struct FakePort {
        calls: Mutex<Vec<Call>>,
    }

    impl SessionPort for FakePort {
        fn commands(&self) -> Vec<CommandInfo> {
            vec![CommandInfo { name: "help".into(), description: "Show available commands".into() }]
        }
        fn send(&self, text: String) {
            self.calls.lock().unwrap().push(Call::Send(text));
        }
        fn run_command(&self, line: String) {
            self.calls.lock().unwrap().push(Call::Command(line));
        }
        fn answer(&self, id: ApprovalId, decision: Decision) {
            self.calls.lock().unwrap().push(Call::Answer(id.0, decision));
        }
        fn abort(&self) {
            self.calls.lock().unwrap().push(Call::Abort);
        }
        fn load_models(&self, provider: ProviderId) {
            self.calls.lock().unwrap().push(Call::LoadModels(provider));
        }
        fn complete_setup(&self, answer: SetupAnswer) {
            self.calls.lock().unwrap().push(Call::CompleteSetup(answer));
        }
        fn resume(&self, id: Option<String>) {
            self.calls.lock().unwrap().push(Call::Resume(id));
        }
        fn flush(&self) -> BoxFuture<'static, ()> {
            Box::pin(async {})
        }
    }

    struct Harness {
        app: App,
        port: Arc<FakePort>,
        copied: Arc<Mutex<Vec<String>>>,
        now: Instant,
        width: u16,
        height: u16,
    }

    fn options(splash_ms: u64) -> AppOptions {
        AppOptions {
            welcome: WelcomeInfo { version: "9.9.9".into(), cwd: "~/x".into(), ..Default::default() },
            status: StatusInfo { model: "ollama/qwen3.5:9b".into(), cwd: "~/x".into(), yolo: false, context_length: None },
            splash_ms,
            render: Default::default(),
        }
    }

    fn harness_with(splash_ms: u64) -> Harness {
        let port = Arc::new(FakePort::default());
        let (_tx, rx) = mpsc::unbounded_channel();
        let copied = Arc::new(Mutex::new(Vec::new()));
        let sink = copied.clone();
        let copy: CopyFn = Arc::new(move |text: String| {
            sink.lock().unwrap().push(text);
            Box::pin(async { "wl-copy".to_string() })
        });
        let app = App::new(port.clone(), rx, options(splash_ms)).with_copy(copy);
        Harness { app, port, copied, now: Instant::now(), width: 100, height: 40 }
    }

    fn harness() -> Harness {
        harness_with(0)
    }

    const ENTER: KeyCode = KeyCode::Enter;
    const DOWN: KeyCode = KeyCode::Down;
    const ESC: KeyCode = KeyCode::Esc;

    impl Harness {
        fn advance(&mut self, ms: u64) {
            self.now += Duration::from_millis(ms);
            self.app.tick(self.now);
        }

        fn event(&mut self, event: UiEvent) {
            self.app.apply(event, self.now);
        }

        fn events(&mut self, events: impl IntoIterator<Item = UiEvent>) {
            for e in events {
                self.event(e);
            }
            // Past the stream flush.
            self.advance(40);
        }

        fn key(&mut self, code: KeyCode) {
            self.app.on_key(KeyEvent::new(code, KeyModifiers::NONE), self.now);
        }

        fn ctrl(&mut self, c: char) {
            self.app.on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL), self.now);
        }

        fn typed(&mut self, text: &str) {
            for c in text.chars() {
                self.key(KeyCode::Char(c));
            }
            self.key(ENTER);
        }

        fn buffer(&mut self) -> Buffer {
            let area = Rect::new(0, 0, self.width, self.height);
            let mut buf = Buffer::empty(area);
            self.app.draw_at(area, &mut buf, self.now);
            buf
        }

        fn frame(&mut self) -> String {
            let buf = self.buffer();
            (0..self.height)
                .map(|y| {
                    let mut row = String::new();
                    let mut x = 0;
                    while x < self.width {
                        let s = buf[(x, y)].symbol();
                        row.push_str(s);
                        x += (unicode_width::UnicodeWidthStr::width(s) as u16).max(1);
                    }
                    row.trim_end().to_string()
                })
                .collect::<Vec<_>>()
                .join("\n")
        }

        fn calls(&self) -> Vec<Call> {
            self.port.calls.lock().unwrap().clone()
        }

        /// Clicks the first screen row containing `text`.
        fn click_on(&mut self, text: &str) {
            let frame = self.frame();
            let (y, line) = frame
                .lines()
                .enumerate()
                .find(|(_, l)| l.contains(text))
                .unwrap_or_else(|| panic!("{text:?} isn't on screen:\n{frame}"));
            let x = unicode_width::UnicodeWidthStr::width(&line[..line.find(text).unwrap()]) as u16;
            self.app.on_mouse(MouseEvent::Press { x, y: y as u16 }, self.now);
            self.app.on_mouse(MouseEvent::Release { x, y: y as u16 }, self.now);
        }
    }

    fn tool_start(id: &str, name: &str, label: &str) -> UiEvent {
        UiEvent::ToolStart { call_id: id.into(), name: name.into(), label: label.into() }
    }

    fn tool_end(id: &str, name: &str, label: &str, summary: &str) -> UiEvent {
        let status =
            ToolStatus { label: label.into(), status: ToolState::Done, summary: Some(summary.into()), steps: None, diff: None };
        UiEvent::ToolEnd { call_id: id.into(), name: name.into(), status }
    }

    fn turn_end() -> UiEvent {
        UiEvent::TurnEnd { turn: "t".into(), reason: TurnEndReason::Loop(StopReason::End) }
    }

    fn write_request(path: &str, agent: Option<&str>) -> ApprovalRequest {
        ApprovalRequest {
            tool: "write_file".into(),
            label: path.into(),
            preview: Preview {
                title: format!("Create {path}"),
                diff: Some(vec![DiffLine {
                    kind: crate::view::DiffKind::Add,
                    text: "hello".into(),
                    old_line: None,
                    new_line: Some(1),
                }]),
                ..Default::default()
            },
            scope: Scope { key: "files".into(), description: "file changes".into() },
            agent: agent.map(Into::into),
            network: false,
            once: false,
        }
    }

    // --- app.test.tsx ---

    #[test]
    fn shows_the_splash_then_the_main_view_after_a_key_press() {
        let mut h = harness_with(60_000);
        assert!(h.frame().contains("press any key"));
        assert_eq!(h.app.deadline(), h.app.splash.as_ref().map(Splash::deadline));
        h.key(KeyCode::Char('x'));
        assert!(h.frame().contains("Welcome to Marv"));
        assert!(h.frame().contains("Type a message"));
    }

    #[test]
    fn the_splash_ends_on_its_own() {
        let mut h = harness_with(1200);
        h.advance(1200);
        assert!(h.frame().contains("Welcome to Marv"));
    }

    #[test]
    fn a_message_is_sent_and_its_reply_streams_in_batches() {
        let mut h = harness();
        h.typed("hello world");
        assert_eq!(h.calls(), [Call::Send("hello world".into())]);
        assert!(h.app.busy());
        assert!(h.frame().contains("> hello world"));
        assert!(h.frame().contains("Thinking…"));
        assert!(h.frame().contains("esc to interrupt"));

        h.event(UiEvent::TurnStart { turn: "t".into() });
        h.event(UiEvent::TextDelta("You said: ".into()));
        h.event(UiEvent::TextDelta("hello world".into()));
        // Not shown until the flush: tokens reach the screen at most every 33 ms.
        assert!(!h.frame().contains("You said"));
        assert_eq!(h.app.deadline(), Some(h.now + STREAM_FLUSH_MS).min(h.app.deadline()));
        h.advance(33);
        assert!(h.frame().contains("● You said: hello world"));
        h.events([UiEvent::Assistant { text: "You said: hello world".into() }, UiEvent::Done(StopReason::End), turn_end()]);
        let frame = h.frame();
        assert_eq!(frame.matches("You said: hello world").count(), 1, "{frame}");
        assert!(!h.app.busy());
        assert!(frame.contains("/help · PgUp/PgDn · ctrl+c exit"));
    }

    #[test]
    fn slash_lines_go_to_the_engine_as_commands_and_the_history_keeps_them() {
        let mut h = harness();
        h.typed("/help");
        assert_eq!(h.calls(), [Call::Command("/help".into())]);
        assert!(!h.app.busy());
        h.key(KeyCode::Up);
        assert_eq!(h.app.prompt.value, "/help");
    }

    #[test]
    fn dragging_over_text_copies_it_and_says_so() {
        let mut h = harness();
        let frame = h.frame();
        let (y, line) = frame.lines().enumerate().find(|(_, l)| l.contains("Welcome")).unwrap();
        let x = unicode_width::UnicodeWidthStr::width(&line[..line.find("Welcome").unwrap()]) as u16;
        let y = y as u16;
        h.app.on_mouse(MouseEvent::Press { x, y }, h.now);
        h.app.on_mouse(MouseEvent::Drag { x: x + 6, y }, h.now);
        assert!(h.buffer()[(x + 3, y)].modifier.contains(ratatui::style::Modifier::REVERSED));
        h.app.on_mouse(MouseEvent::Release { x: x + 6, y }, h.now);
        futures::executor::block_on(h.app.settle_copies(h.now));
        assert_eq!(*h.copied.lock().unwrap(), ["Welcome"]);
        assert!(h.frame().contains("Copied 7 chars"));
        h.advance(2000);
        assert!(!h.frame().contains("Copied 7 chars"));
    }

    #[test]
    fn shows_thinking_while_the_model_reasons_then_keeps_only_a_short_note() {
        let mut h = harness();
        h.typed("hello");
        h.events([UiEvent::ThinkingDelta("Step 1: read the question.\nStep 2: answer briefly.".into())]);
        let frame = h.frame();
        assert!(frame.contains("Thinking… (~13 tokens)"), "{frame}");
        assert!(frame.contains("Step 2: answer briefly."));
        h.advance(1200);
        h.events([
            UiEvent::TextDelta("Hi!".into()),
            UiEvent::Assistant { text: "Hi!".into() },
            UiEvent::Done(StopReason::End),
            turn_end(),
        ]);
        let frame = h.frame();
        assert!(frame.contains("✻ Thought for 1s"), "{frame}");
        assert!(frame.contains("Hi!"));
        assert!(!frame.contains("Step 2"));
    }

    #[test]
    fn a_tool_call_shows_as_it_streams_what_it_writes_and_the_code_so_far() {
        let mut h = harness();
        h.typed("write page.js");
        h.events([
            UiEvent::TextDelta("I'll write it:".into()),
            UiEvent::ToolCallDelta {
                index: 0,
                name: "write_file".into(),
                text: r#"{"path": "page.js", "content": "const a = 1;\nfunction draw() {\n  return a;"#.into(),
            },
        ]);
        let frame = h.frame();
        assert!(frame.contains("I'll write it:")); // the text before the call stays
        assert!(frame.contains("Writing page.js…"), "{frame}");
        assert!(frame.contains("tokens)"));
        assert!(frame.contains("    return a;")); // indentation kept: it's code
        h.events([UiEvent::Assistant { text: "I'll write it:".into() }, tool_start("w1", "write_file", "page.js")]);
        assert!(!h.frame().contains("Writing page.js…"));
        h.events([tool_end("w1", "write_file", "page.js", "created · 1 line"), UiEvent::TextDelta("Written.".into())]);
        assert!(h.frame().contains("Written."));
        assert!(h.frame().contains("⎿ created · 1 line"));
    }

    #[test]
    fn empty_replies_a_retry_says_nothing_a_nudge_and_giving_up_are_shown() {
        let mut h = harness();
        h.typed("hello?");
        h.events([
            UiEvent::EmptyReply { next: EmptyNext::Retry },
            UiEvent::EmptyReply { next: EmptyNext::Nudge },
            UiEvent::EmptyReply { next: EmptyNext::Stop },
            turn_end(),
        ]);
        let frame = h.frame();
        assert!(frame.contains("two empty replies in a row"));
        assert!(frame.contains("replies stayed empty"));
    }

    #[test]
    fn shows_tools_and_their_errors_in_the_transcript() {
        let mut h = harness();
        h.typed("what's in my notes?");
        h.events([tool_start("c1", "read_file", "notes.txt")]);
        assert!(h.frame().contains("⎿ running…"));
        h.events([tool_end("c1", "read_file", "notes.txt", "1 line"), tool_start("c2", "read_file", "/etc/passwd")]);
        let status = ToolStatus {
            label: "/etc/passwd".into(),
            status: ToolState::Error,
            summary: Some("/etc/passwd is outside the project".into()),
            steps: None,
            diff: None,
        };
        h.events([UiEvent::ToolEnd { call_id: "c2".into(), name: "read_file".into(), status }, turn_end()]);
        let frame = h.frame();
        assert!(frame.contains("● read_file notes.txt"));
        assert!(frame.contains("⎿ 1 line"));
        assert!(frame.contains("is outside the project"));
    }

    #[test]
    fn shows_token_usage_and_cache_hits_in_the_status_bar() {
        let mut h = harness();
        h.typed("hi");
        let last = Usage { prompt_tokens: 1200, completion_tokens: 30, cached_tokens: Some(1000), cost: None };
        let totals = Totals {
            requests: 1,
            prompt_tokens: 1200,
            cached_tokens: 1000,
            completion_tokens: 30,
            cost: Some(0.042),
            ..Default::default()
        };
        h.events([UiEvent::Usage { last, totals }, turn_end()]);
        assert!(h.frame().contains("1.2k tokens · 83% cached · $0.042 · ollama/qwen3.5:9b · ~/x"));
    }

    #[test]
    fn a_reply_stuck_repeating_itself_is_dropped_with_a_notice() {
        let mut h = harness();
        h.typed("go");
        h.events([UiEvent::TextDelta("Fix the test scripts now. ".repeat(20)), UiEvent::TextLoop { continued: true }]);
        let frame = h.frame();
        assert!(frame.contains("stuck repeating itself"));
        assert!(!frame.contains("Fix the test scripts now. Fix"));
    }

    #[test]
    fn a_streaming_reply_lays_out_like_the_finished_one_whatever_the_chunks() {
        let text = "First line of the reply, long enough to wrap at forty columns wide.\n\nSecond paragraph.\nThird";
        let mut h = harness();
        h.width = 40;
        h.typed("go");
        for chunk in text.as_bytes().chunks(7) {
            h.events([UiEvent::TextDelta(String::from_utf8(chunk.to_vec()).unwrap())]);
            h.frame();
        }
        let streamed = h.frame();
        let whole = standin::message_lines(&standin::message(Role::Assistant, text.into()), 40, false);
        for line in &whole {
            assert!(streamed.contains(line.to_string().trim_end()), "{line}\n{streamed}");
        }
        // A new step's reply replaces the old one, even when it's longer by the next draw.
        h.events([UiEvent::Assistant { text: text.into() }, UiEvent::TextDelta("Brand new\nreply".into())]);
        let frame = h.frame();
        assert!(frame.contains("● Brand new"), "{frame}");
        assert!(frame.contains("  reply"));
    }

    #[test]
    fn retry_drops_the_failed_attempt_and_says_why_until_the_next_one_streams() {
        let mut h = harness();
        h.typed("go");
        h.events([UiEvent::TextDelta("half a".into())]);
        h.events([UiEvent::Retry {
            attempt: 1,
            max: 3,
            message: "Connection lost; asking again (1/3)…".into(),
            continued: true,
        }]);
        let frame = h.frame();
        assert!(!frame.contains("half a"));
        assert!(frame.contains("Connection lost; asking again (1/3)…"));
        h.events([UiEvent::ThinkingDelta("hmm".into())]);
        assert!(h.frame().contains("Thinking…"));
    }

    #[test]
    fn compaction_is_reported() {
        let mut h = harness();
        h.event(UiEvent::StatusInfo(StatusInfo { context_length: Some(32_768), ..options(0).status }));
        h.typed("next");
        h.events([UiEvent::Status(TurnStatus::Compacting)]);
        assert!(h.frame().contains("Compacting the conversation…"));
        let outcome = CompactOutcome::Compacted { summary: "x".repeat(4000), tokens_before: Some(30_000) };
        h.events([UiEvent::Compaction { automatic: true, outcome }]);
        assert!(
            h.frame().contains("✻ Compacted the conversation (the context was 92% full): 30k → about 1k tokens."),
            "{}",
            h.frame()
        );
        let stopped = CompactOutcome::NotCompacted { reason: "stopped".into(), error: String::new() };
        h.events([
            UiEvent::Compaction { automatic: true, outcome: stopped },
            UiEvent::TurnEnd { turn: "t".into(), reason: TurnEndReason::Interrupted },
        ]);
        let frame = h.frame();
        assert!(frame.contains("Stopped: nothing was compacted, and your message wasn't sent."));
        assert!(!frame.contains("Interrupted.")); // the compaction already said so
    }

    // --- approvals ---

    #[test]
    fn a_change_waits_for_approval_then_the_answer_reaches_the_engine() {
        let mut h = harness();
        h.typed("make a file");
        h.events([
            tool_start("c1", "write_file", "made.txt"),
            UiEvent::Approval { id: ApprovalId(1), request: write_request("made.txt", None) },
        ]);
        let frame = h.frame();
        assert!(frame.contains("Create made.txt"));
        assert!(frame.contains("Do you want to proceed?"));
        assert!(!frame.contains("Type a message"));
        h.key(ENTER); // Yes
        assert_eq!(h.calls().last(), Some(&Call::Answer(1, Decision::Yes)));
        assert!(!h.frame().contains("Do you want to proceed?"));
    }

    #[test]
    fn no_declines_and_the_engine_says_stopped() {
        let mut h = harness();
        h.typed("make a file");
        h.events([UiEvent::Approval { id: ApprovalId(1), request: write_request("made.txt", None) }]);
        h.key(DOWN);
        h.key(DOWN);
        h.key(ENTER);
        assert_eq!(h.calls().last(), Some(&Call::Answer(1, Decision::No)));
        h.events([UiEvent::Done(StopReason::Declined), turn_end()]);
        assert!(h.frame().contains("Stopped. Tell Marv what to do instead."));
    }

    #[test]
    fn esc_at_the_prompt_declines_and_stops_the_run() {
        let mut h = harness();
        h.typed("make a file");
        h.events([UiEvent::Approval { id: ApprovalId(1), request: write_request("made.txt", None) }]);
        h.key(ESC);
        let calls = h.calls();
        assert!(calls.contains(&Call::Answer(1, Decision::No)));
        assert!(calls.contains(&Call::Abort));
        h.events([UiEvent::Done(StopReason::Aborted), turn_end()]);
        assert!(h.frame().contains("Interrupted."));
        assert!(h.frame().contains("Type a message"));
    }

    #[test]
    fn ctrl_c_at_the_prompt_declines_and_stops() {
        let mut h = harness();
        h.typed("make a file");
        h.events([UiEvent::Approval { id: ApprovalId(1), request: write_request("made.txt", None) }]);
        h.ctrl('c');
        assert_eq!(&h.calls()[1..], [Call::Answer(1, Decision::No), Call::Abort]);
        assert!(!h.frame().contains("Do you want to proceed?"));
    }

    #[test]
    fn parallel_approvals_queue_up_one_at_a_time_each_starting_on_yes() {
        let mut h = harness();
        h.typed("make two files");
        h.events([
            tool_start("a1", "agent", "general-purpose · one"),
            tool_start("a2", "agent", "general-purpose · two"),
            UiEvent::Approval { id: ApprovalId(1), request: write_request("one.txt", Some("general-purpose · one")) },
            UiEvent::Approval { id: ApprovalId(2), request: write_request("two.txt", Some("general-purpose · two")) },
        ]);
        let frame = h.frame();
        assert!(frame.contains("1 more waiting"));
        assert!(frame.contains("2 agents running"));
        assert!(frame.contains("[general-purpose · one]"));
        h.key(DOWN);
        h.key(DOWN);
        assert!(h.frame().contains("❯ No, and tell Marv"));
        h.key(ENTER); // No, for the first subagent
        let frame = h.frame();
        assert!(frame.contains("Do you want to proceed?")); // the second one, a fresh prompt
        assert!(frame.contains("❯ Yes"));
        assert!(!frame.contains("❯ No"));
        assert!(!frame.contains("more waiting"));
        h.key(ENTER);
        assert_eq!(&h.calls()[1..], [Call::Answer(1, Decision::No), Call::Answer(2, Decision::Yes)]);
    }

    #[test]
    fn always_answers_the_other_queued_requests_in_the_same_scope() {
        let mut h = harness();
        h.typed("make files");
        let mut other = write_request("x", None);
        other.scope.key = "bash:ls".into();
        h.events([
            UiEvent::Approval { id: ApprovalId(1), request: write_request("one.txt", None) },
            UiEvent::Approval { id: ApprovalId(2), request: other },
            UiEvent::Approval { id: ApprovalId(3), request: write_request("two.txt", None) },
        ]);
        h.key(DOWN);
        h.key(ENTER); // Yes, and don't ask again for file changes
        assert_eq!(&h.calls()[1..], [Call::Answer(3, Decision::Yes), Call::Answer(1, Decision::Always)]);
        assert!(h.frame().contains("Do you want to proceed?")); // the other scope still asks
        assert!(!h.frame().contains("more waiting"));
    }

    #[test]
    fn anything_still_queued_when_the_turn_ends_is_declined() {
        let mut h = harness();
        h.typed("go");
        h.events([UiEvent::Approval { id: ApprovalId(7), request: write_request("a", None) }, turn_end()]);
        assert_eq!(h.calls().last(), Some(&Call::Answer(7, Decision::No)));
        assert!(h.frame().contains("Type a message"));
    }

    // --- keys ---

    #[test]
    fn esc_interrupts_a_running_reply() {
        let mut h = harness();
        h.typed("go");
        h.events([UiEvent::TextDelta("word ".repeat(5))]);
        assert!(h.frame().contains("esc to interrupt"));
        h.key(ESC);
        assert_eq!(h.calls().last(), Some(&Call::Abort));
        h.events([UiEvent::Done(StopReason::Aborted), turn_end()]);
        assert!(h.frame().contains("Interrupted."));
        assert!(h.frame().contains("Type a message"));
    }

    #[test]
    fn interrupted_before_the_loop_began_says_so() {
        let mut h = harness();
        h.typed("go");
        h.events([UiEvent::Status(TurnStatus::WaitingForMcp)]);
        assert!(h.frame().contains("Waiting for MCP servers to start…"));
        h.events([UiEvent::TurnEnd { turn: "t".into(), reason: TurnEndReason::Interrupted }]);
        assert!(h.frame().contains("Interrupted."));
    }

    #[test]
    fn ctrl_c_clears_the_prompt_then_asks_to_confirm_then_exits() {
        let mut h = harness();
        for c in "draft".chars() {
            h.key(KeyCode::Char(c));
        }
        h.ctrl('c');
        assert_eq!(h.app.prompt.value, "");
        h.ctrl('c');
        assert!(h.frame().contains("Press ctrl+c again to exit"));
        h.advance(1500);
        assert!(!h.frame().contains("Press ctrl+c again"));
        h.ctrl('c');
        h.ctrl('c');
        assert!(h.app.exiting());
    }

    #[test]
    fn a_message_while_busy_isnt_sent_and_stays_in_the_prompt() {
        let mut h = harness();
        h.typed("first");
        h.typed("second");
        assert_eq!(h.calls(), [Call::Send("first".into())]);
        assert_eq!(h.app.prompt.value, "second");
    }

    #[test]
    fn page_up_scrolls_back_and_a_submit_follows_the_bottom_again() {
        let mut h = harness();
        h.height = 20;
        for i in 0..30 {
            h.event(UiEvent::Message(standin::system(format!("note {i}"), false)));
        }
        assert!(h.frame().contains("note 29"));
        h.key(KeyCode::PageUp);
        h.key(KeyCode::PageUp);
        let frame = h.frame();
        assert!(!frame.contains("note 29"));
        h.typed("/help");
        assert!(h.frame().contains("note 29"));
    }

    #[test]
    fn the_wheel_glides_a_row_at_a_time_and_stops() {
        let mut h = harness();
        h.height = 20;
        for i in 0..30 {
            h.event(UiEvent::Message(standin::system(format!("note {i}"), false)));
        }
        h.frame();
        let bottom = h.app.scroll.scroll_top();
        h.app.on_mouse(MouseEvent::Scroll { step: -1 }, h.now);
        assert_eq!(h.app.scroll.scroll_top(), bottom - 1); // the first row at once
        assert_eq!(h.app.deadline(), Some(h.now + GLIDE_MS).min(h.app.deadline()));
        h.advance(20);
        h.advance(20);
        assert_eq!(h.app.scroll.scroll_top(), bottom - 3);
        assert_eq!(h.app.scroll.deadline(), None);
    }

    // --- subagents ---

    fn progress(line: &str, steps: &[&str]) -> UiEvent {
        UiEvent::SubagentProgress {
            call_id: "a1".into(),
            progress: AgentProgress { line: line.into(), steps: steps.iter().map(|s| s.to_string()).collect() },
        }
    }

    fn sub(event: UiEvent) -> UiEvent {
        UiEvent::Subagent { call_id: "a1".into(), title: "general-purpose · Read notes".into(), event: Box::new(event) }
    }

    #[test]
    fn a_subagent_is_one_entry_showing_its_progress_and_ctrl_o_shows_its_steps() {
        let mut h = harness();
        h.typed("check the notes");
        h.events([
            tool_start("a1", "agent", "general-purpose · Read notes"),
            progress("shared folder · 1 tool · thinking…", &["read_file notes.txt · 1 line"]),
        ]);
        let frame = h.frame();
        assert!(frame.contains("agent general-purpose · Read notes"));
        assert!(frame.contains("⎿ shared folder · 1 tool · thinking…"));
        assert!(frame.contains("1 agent running"));
        assert!(!frame.contains("    · read_file notes.txt · 1 line"));
        h.ctrl('o');
        assert!(h.frame().contains("    · read_file notes.txt · 1 line")); // its steps so far
        h.ctrl('o'); // and again hides them
        assert!(!h.frame().contains("    · read_file"));
        h.events([tool_end("a1", "agent", "general-purpose · Read notes", "done · 1 tool")]);
        let frame = h.frame();
        assert!(frame.contains("done · 1 tool"));
        assert!(!frame.contains("agent running"));
        h.ctrl('o');
        assert!(h.frame().contains("    · read_file notes.txt · 1 line")); // the steps it ended with
    }

    #[test]
    fn a_late_progress_flush_doesnt_turn_an_ended_entry_back_to_running() {
        let mut h = harness();
        h.typed("go");
        h.events([tool_start("a1", "agent", "x")]);
        h.event(progress("working", &[]));
        h.event(tool_end("a1", "agent", "x", "done · 0 tools"));
        h.advance(40);
        assert!(h.frame().contains("⎿ done · 0 tools"));
    }

    #[test]
    fn a_call_id_reused_in_a_later_step_doesnt_inherit_a_subagents_steps() {
        let mut h = harness();
        h.typed("check");
        h.events([
            tool_start("call_0", "agent", "a"),
            UiEvent::SubagentProgress {
                call_id: "call_0".into(),
                progress: AgentProgress { line: "l".into(), steps: vec!["read_file notes.txt · 1 line".into()] },
            },
        ]);
        h.events([
            tool_end("call_0", "agent", "a", "done"),
            tool_start("call_0", "read_file", "notes.txt"),
            tool_end("call_0", "read_file", "notes.txt", "1 line"),
        ]);
        h.ctrl('o');
        let frame = h.frame();
        let lines: Vec<&str> = frame.lines().collect();
        let own = lines.iter().position(|l| l.contains("● read_file notes.txt")).unwrap();
        assert!(lines[own + 1].contains("⎿ 1 line"));
        assert!(!lines[own + 2].contains("· read_file"));
    }

    #[test]
    fn if_the_turn_ends_early_its_entries_end_as_interrupted() {
        let mut h = harness();
        h.typed("go");
        h.events([tool_start("a1", "agent", "general-purpose · write"), UiEvent::Error("Error: boom".into()), turn_end()]);
        let frame = h.frame();
        assert!(frame.contains("Error: boom"));
        assert!(frame.contains("⎿ interrupted"));
        assert!(!frame.contains("esc to interrupt"));
        assert!(frame.contains("Type a message"));
    }

    #[test]
    fn clicking_a_subagent_opens_its_view_live_and_esc_goes_back_without_stopping_it() {
        let mut h = harness();
        h.typed("check the notes");
        h.events([
            tool_start("a1", "agent", "general-purpose · Read notes"),
            progress("shared folder · 1 tool", &[]),
            sub(tool_start("r1", "read_file", "notes.txt")),
            sub(tool_end("r1", "read_file", "notes.txt", "1 line")),
        ]);
        h.click_on("shared folder · 1 tool"); // its second line counts too
        let frame = h.frame();
        assert!(frame.contains("● agent general-purpose · Read notes · running"), "{frame}");
        assert!(frame.contains("esc to go back"));
        assert!(frame.contains("read_file notes.txt"));
        assert!(frame.contains("⎿ 1 line"));
        assert!(!frame.contains("check the notes")); // the main transcript is hidden
        assert!(frame.contains(" esc to go back · 1 agent running")); // Esc doesn't interrupt here

        h.events([sub(UiEvent::TextDelta("It says remember the milk.".into()))]);
        assert!(h.frame().contains("It says remember the milk.")); // its reply arrives live
        h.events([sub(UiEvent::Assistant { text: "It says remember the milk.".into() }), sub(UiEvent::Done(StopReason::End))]);
        assert!(h.frame().contains("Read notes · finished"));

        h.key(ESC);
        let frame = h.frame();
        assert!(!frame.contains("esc to go back"));
        assert!(frame.contains("> check the notes"));
        assert!(!h.calls().contains(&Call::Abort));
    }

    #[test]
    fn an_approval_below_a_subagents_view_esc_closes_the_view_but_doesnt_decline() {
        let mut h = harness();
        h.typed("make a file");
        h.events([
            tool_start("a1", "agent", "general-purpose · Make file"),
            UiEvent::Approval { id: ApprovalId(1), request: write_request("made.txt", Some("general-purpose · Make file")) },
        ]);
        h.click_on("agent general-purpose · Make file");
        assert!(h.frame().contains("esc to go back"));
        assert!(h.frame().contains("Do you want to proceed?"));
        h.key(ESC);
        assert!(!h.frame().contains("esc to go back"));
        assert!(h.frame().contains("Do you want to proceed?")); // still asking
        assert_eq!(h.calls(), [Call::Send("make a file".into())]);
        h.key(ENTER);
        assert_eq!(h.calls().last(), Some(&Call::Answer(1, Decision::Yes)));
    }

    #[test]
    fn sending_a_message_closes_the_view_and_a_click_elsewhere_opens_nothing() {
        let mut h = harness();
        h.typed("check the notes");
        h.events([tool_start("a1", "agent", "general-purpose · Read notes"), sub(UiEvent::Assistant { text: "Milk.".into() })]);
        h.events([tool_end("a1", "agent", "general-purpose · Read notes", "done"), turn_end()]);
        h.click_on("check the notes");
        assert!(!h.frame().contains("esc to go back"));
        h.click_on("agent general-purpose · Read notes");
        assert!(h.frame().contains("● Milk."));
        h.typed("hello");
        assert!(!h.frame().contains("esc to go back"));
        assert!(h.frame().contains("> hello"));
    }

    #[test]
    fn a_restored_subagent_cant_be_opened() {
        let mut h = harness();
        let mut agent = standin::message(Role::Tool, "agent".into());
        agent.id = 3;
        agent.tool = Some(ToolStatus {
            label: "old · run".into(),
            status: ToolState::Done,
            summary: Some("done".into()),
            steps: None,
            diff: None,
        });
        h.event(UiEvent::Resumed(Resumed {
            id: "s".into(),
            updated_at: 0,
            model: "other".into(),
            transcript: vec![agent],
            totals: Totals::default(),
        }));
        assert!(h.frame().contains("(it used other; continuing with ollama/qwen3.5:9b)"));
        h.click_on("agent old · run");
        assert!(h.frame().contains("Only this session's subagents can be opened"));
        // New messages never reuse a restored id.
        assert!(h.app.next_id > 3);
    }

    // --- pickers, setup, events from outside a turn ---

    #[test]
    fn resume_picks_an_earlier_session() {
        let mut h = harness();
        let summary = |id: &str, title: &str| SessionSummary {
            id: id.into(),
            title: title.into(),
            updated_at: 0,
            messages: 2,
            model: "m".into(),
        };
        h.event(UiEvent::Sessions(vec![summary("s1", "First"), summary("s2", "Second")]));
        assert!(h.frame().contains("Resume a session"));
        h.key(DOWN);
        h.key(ENTER);
        assert_eq!(h.calls(), [Call::Resume(Some("s2".into()))]);
        assert!(!h.frame().contains("Resume a session"));
        h.event(UiEvent::Sessions(vec![]));
        assert!(h.frame().contains("No saved sessions for this project yet."));
    }

    #[test]
    fn first_run_setup_loads_models_completes_and_cancelling_it_quits() {
        let mut h = harness();
        let presets = vec![crate::view::ProviderPreset {
            id: ProviderId::Ollama,
            label: "Ollama".into(),
            key_env: None,
            key_url: None,
            default_model: String::new(),
        }];
        let context =
            crate::view::SetupContext { start: SetupStart::FirstRun, presets, current: None, saved_key: None, env_keys: vec![] };
        h.event(UiEvent::OpenSetup(context.clone()));
        assert!(h.frame().contains("Marv setup"));
        assert!(!h.frame().contains("Type a message"));
        h.key(ENTER);
        assert_eq!(h.calls(), [Call::LoadModels(ProviderId::Ollama)]);
        h.event(UiEvent::Models(Ok(vec![crate::view::ModelInfo { id: "qwen3.5:9b".into(), local: true, ..Default::default() }])));
        h.key(ENTER);
        assert_eq!(
            h.calls().last(),
            Some(&Call::CompleteSetup(SetupAnswer { provider: ProviderId::Ollama, model: "qwen3.5:9b".into(), api_key: None }))
        );
        assert!(h.frame().contains("Type a message"));

        h.event(UiEvent::OpenSetup(context));
        h.key(ESC);
        assert!(h.app.exiting());
    }

    #[test]
    fn model_setup_asks_for_models_at_once_and_esc_just_closes_it() {
        let mut h = harness();
        let presets = vec![crate::view::ProviderPreset {
            id: ProviderId::Ollama,
            label: "Ollama".into(),
            key_env: None,
            key_url: None,
            default_model: String::new(),
        }];
        let current = Some((ProviderId::Ollama, "tiny".into()));
        h.event(UiEvent::OpenSetup(crate::view::SetupContext {
            start: SetupStart::Model,
            presets,
            current,
            saved_key: None,
            env_keys: vec![],
        }));
        assert_eq!(h.calls(), [Call::LoadModels(ProviderId::Ollama)]);
        h.key(ESC);
        assert!(!h.app.exiting());
        assert!(h.frame().contains("Type a message"));
    }

    #[test]
    fn clear_empties_the_transcript_and_busy_shows_the_waiting_line() {
        let mut h = harness();
        h.event(UiEvent::Message(standin::system("Hello there".into(), false)));
        assert!(h.frame().contains("Hello there"));
        h.event(UiEvent::Clear);
        assert!(!h.frame().contains("Hello there"));
        assert!(h.frame().contains("Welcome to Marv"));
        h.event(UiEvent::Busy(Some("Searching the MCP Registry…".into())));
        assert!(h.frame().contains("Searching the MCP Registry…"));
        assert!(h.frame().contains("esc to interrupt"));
        h.key(ESC);
        assert_eq!(h.calls(), [Call::Abort]);
        h.event(UiEvent::Busy(None));
        assert!(!h.frame().contains("Searching"));
        h.event(UiEvent::Exit);
        assert!(h.app.exiting());
    }

    #[test]
    fn engine_text_is_cleaned_before_it_is_drawn() {
        let mut h = harness();
        h.event(UiEvent::Message(standin::system("evil\x1b[2J\u{202e}text".into(), true)));
        h.typed("x");
        h.events([UiEvent::TextDelta("bad\x1b]0;title\x07reply".into())]);
        let buf = h.buffer();
        let all: String = buf.content().iter().map(|c| c.symbol()).collect();
        assert!(!all.contains('\x1b') && !all.contains('\u{202e}'));
        assert!(h.frame().contains("eviltext"));
    }

    #[test]
    fn a_diff_under_an_entry_is_collapsed_until_ctrl_o() {
        let mut h = harness();
        h.typed("edit");
        let lines = (1..=20)
            .map(|i| DiffLine { kind: crate::view::DiffKind::Add, text: format!("l{i}"), old_line: None, new_line: Some(i) })
            .collect();
        let status = ToolStatus {
            label: "a.ts".into(),
            status: ToolState::Done,
            summary: Some("edited".into()),
            steps: None,
            diff: Some(DiffShown { lines, more: 0 }),
        };
        h.events([
            tool_start("e", "edit_file", "a.ts"),
            UiEvent::ToolEnd { call_id: "e".into(), name: "edit_file".into(), status },
            turn_end(),
        ]);
        assert!(h.frame().contains("… 5 more lines (ctrl+o)"));
        h.ctrl('o');
        assert!(h.frame().contains("+ l20"));
    }

    // --- the loop's wake-ups (Marv #68) ---

    #[test]
    fn idle_with_the_martian_scrolled_away_nothing_wakes_the_loop() {
        let mut h = harness();
        h.height = 16;
        h.frame();
        assert!(h.app.deadline().is_some(), "the martian animates while it's on screen");
        for i in 0..20 {
            h.event(UiEvent::Message(standin::system(format!("note {i}"), false)));
        }
        h.frame();
        assert!(!h.app.welcome_visible);
        assert_eq!(h.app.deadline(), None);
        assert!(!h.app.is_dirty());
        // A tick with nothing due changes nothing.
        h.advance(10_000);
        assert!(!h.app.is_dirty());
    }

    #[test]
    fn the_spinner_wakes_the_loop_only_while_shown() {
        let mut h = harness();
        h.height = 16;
        for i in 0..20 {
            h.event(UiEvent::Message(standin::system(format!("note {i}"), false)));
        }
        h.typed("go");
        h.frame();
        let at = h.app.deadline().expect("spinner");
        assert!(at <= h.now + SPINNER_MS);
        h.events([UiEvent::TextDelta("streaming".into())]);
        h.frame();
        assert_eq!(h.app.deadline(), None, "no waiting line once text streams");
    }
}
