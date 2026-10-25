//! The App: state, input handling and the event loop. Ports `src/app.tsx` (tests: `tests/app.test.tsx`,
//! `tests/render-performance.test.tsx`, `tests/approval.test.tsx`, `tests/herdr.test.tsx`).
//!
//! # Layout (as in Marv)
//! Full screen in the alternate screen: the transcript takes the leftover height and scrolls itself
//! (`ui::scroll_view`), and below it, pinned, the plan while it has something left to do (#15,
//! `ui::plan_view`; not on a window under `PLAN_MIN_WINDOW` rows), then one of: the session picker, an
//! approval (+ status bar), setup, or the prompt + status bar. A subagent's view (opened by clicking its entry) hides
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
//! - **Reveal streamed text at a pace.** Deltas are appended to a pending buffer, and the reply and
//!   the reasoning are revealed from it a share at a time, one share per frame: each frame shows
//!   1/7 of what's still hidden (at least [`REVEAL_MIN_CHARS`]), so a burst drains over about
//!   [`REVEAL_MS`]. Providers send tokens in clumps (one network read every 50–150 ms), and Marv
//!   drew each clump at once every 33 ms, so text came in jerks; spreading each clump over the
//!   frames until the next one makes it flow. **This is a deliberate difference from Marv**, which
//!   batched at a fixed 33 ms. A backlog over [`REVEAL_CATCH_UP`] characters (a huge chunk, or a
//!   resumed stream) is shown at once rather than replayed. Step ends ([`STEP_ENDS`]) show
//!   everything at once; subagents' progress and tool drafts join each reveal unpaced. The loop
//!   drains every queued `UiEvent` before drawing, so a burst of tokens is one frame.
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
//! - **Approvals arm after [`APPROVAL_ARM_MS`] (#113, a deliberate difference from Marv):** each newly shown head
//!   ignores the keys that would answer it (Enter, ↑/↓) for 400 ms; Esc, ctrl+c and PgUp/PgDn work at once. An
//!   approval can appear under keys typed for the prompt or the ctrl+k palette, and an Enter meant for them would
//!   otherwise answer Yes. Nothing on screen changes while it arms.
//! - **ctrl+c:** interrupt a reply → clear the prompt → "Press ctrl+c again to exit"
//!   ([`EXIT_CONFIRM_MS`]) → exit. ctrl+o toggles details (subagents' steps, whole diffs).
//! - **ctrl+k** (#44): the command palette over the prompt (`ui::command_palette`); the prompt keeps its text.
//!   Never over an approval, the picker or setup; an approval, the session picker or setup arriving closes it, and
//!   they get keys before it (#113).
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
//! # Rendering
//! The transcript ([`Transcript`]) caches each entry's lines (`message_view`, which draws Markdown,
//! diffs and notices) and the scroll views ([`ScrollView`]) cull it to the viewport and glide. The
//! live end is drawn each frame: the streaming reply through `message_view` with its
//! [`StreamLexer`] (finished blocks are laid out once; only the unfinished end is parsed and wrapped
//! again), then the waiting line (`ThinkingView`) or a tool call being written (`DraftView`). A
//! subagent's view is an [`AgentView`] under its `AgentViewHeader`.

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
use crate::selection::{SelectionEffect, SelectionStore};
use crate::ui::agent_view::{self, AgentLog, AgentView, AgentViewHeader};
use crate::ui::approval::{ApprovalOutcome, ApprovalView};
use crate::ui::command_palette::{self, CommandPalette, PaletteOutcome};
use crate::ui::markdown::StreamLexer;
use crate::ui::martian::MartianAnimation;
use crate::ui::message_view;
use crate::ui::plan_view::{self, PLAN_MIN_WINDOW};
use crate::ui::prompt_input::{self, PromptInput, PromptOutcome};
use crate::ui::radio_view::{self, RadioPanel};
use crate::ui::scroll_view::ScrollView;
use crate::ui::session_picker::{PickerOutcome, SessionPicker};
use crate::ui::setup::{Setup, SetupOutcome};
use crate::ui::splash::Splash;
use crate::ui::status_bar::{self, StatusBar};
use crate::ui::thinking_view::{self, DraftView, ThinkingView};
use crate::ui::transcript::{self, Transcript, TranscriptItem};
use crate::view::{
    AgentProgress, ApprovalId, ApprovalRequest, CommandInfo, CompactOutcome, Decision, EmptyNext, Message, PlanItem, RadioView,
    Role, SessionPort, SetupStart, StatusInfo, ToolState, ToolStatus, Totals, TurnEndReason, TurnStatus, UiEvent, WelcomeInfo,
};
use marv_core::{StopReason, Usage};

/// The App's copy of the queue (#184), changed at once on a key and replaced by the engine's next list.
trait QueueMirror {
    /// Removes the first entry equal to `text` (the oldest: what Enter on the empty prompt took).
    fn retain_first(&mut self, text: &str);
    /// Removes the last entry equal to `text` (the newest: what ↑ or ctrl+x took).
    fn retain_last(&mut self, text: &str);
}

impl QueueMirror for Vec<String> {
    fn retain_first(&mut self, text: &str) {
        if let Some(at) = self.iter().position(|t| t == text) {
            self.remove(at);
        }
    }
    fn retain_last(&mut self, text: &str) {
        if let Some(at) = self.iter().rposition(|t| t == text) {
            self.remove(at);
        }
    }
}

/// How long "Press ctrl+c again to exit" waits for the second ctrl+c.
pub const EXIT_CONFIRM_MS: Duration = Duration::from_millis(1500);
/// A newly shown approval ignores the keys that would answer it for this long (#113): a key typed for the prompt or
/// the palette just as the approval appears (an Enter, most of all, which answers Yes) must not decide it.
pub const APPROVAL_ARM_MS: Duration = Duration::from_millis(400);
/// How long a status bar flash ("Copied 3 lines") stays.
pub const NOTICE_MS: Duration = Duration::from_millis(2000);
/// A burst of streamed text is revealed over about this long (see the module docs): long enough to
/// cover the gap between a provider's network reads, short enough that the reply never lags far.
pub const REVEAL_MS: Duration = Duration::from_millis(120);
/// The least a reveal step shows, so the end of a burst doesn't crawl out one character at a time.
pub const REVEAL_MIN_CHARS: usize = 2;
/// More hidden than this is shown at once: replaying it would leave the reply seconds behind.
pub const REVEAL_CATCH_UP: usize = 2000;
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

/// A transcript entry with this role and text (the App sets its id when it's added).
fn message(role: Role, text: String) -> Message {
    Message { id: 0, role, text, is_error: false, markdown: false, notice: None, tool: None }
}

/// One of Marv's own notes for the transcript (dim, or in the error colour).
fn system_message(text: String, is_error: bool) -> Message {
    Message { is_error, ..message(Role::System, text) }
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
    transcript: Transcript,
    next_id: u64,
    scroll: ScrollView,
    /// The subagent view's own scroll position (Marv keyed a second ScrollView by the view).
    view_scroll: ScrollView,
    selection: SelectionStore,
    show_steps: bool,
    /// Each subagent's log, by its entry's message id. Only this session's runs: sessions don't save them.
    agent_logs: HashMap<u64, AgentLog>,
    viewing: Option<u64>,
    /// The open subagent view's own state (its stream lexer, waiting line, laid-out entries).
    agent_view: AgentView,
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
    /// "Send now" messages sent into the running turn and not in it yet (the status bar counts them).
    sending: usize,
    /// #184: the messages queued for after the running turn, oldest first, as the engine last listed them
    /// ([`UiEvent::Queued`]); shown above the prompt.
    queued: Vec<String>,
    /// `/exit` with messages queued said they'd be dropped (#184): the next `/exit` leaves.
    exit_warned: bool,
    /// The plan tool's list (#15), pinned above the prompt while it has something left to do.
    plan: Option<Vec<PlanItem>>,
    waiting_for: Option<String>,
    compacting: bool,
    /// Work outside a turn (`UiEvent::Busy`), shown on the waiting line.
    busy_label: Option<String>,
    /// The waiting line's label and when it appeared (a new label restarts its clock).
    waiting: Option<(String, Instant)>,
    /// The streaming reply's lexer: its finished blocks are parsed and laid out once, so a frame
    /// redoes only the unfinished end. It starts over by itself when a new reply replaces the text.
    stream_lexer: StreamLexer,

    // --- status ---
    last_usage: Option<Usage>,
    totals: Totals,
    confirm_exit: Option<Instant>,
    flash: Option<(String, Instant)>,

    // --- the bottom panel ---
    prompt: PromptInput,
    /// The ctrl+k palette (#44), over the prompt: the prompt keeps its text underneath.
    palette: Option<CommandPalette>,
    approvals: VecDeque<Pending>,
    approval: Option<(ApprovalId, ApprovalView)>,
    /// Until when the approval on screen ignores keys that would answer it ([`APPROVAL_ARM_MS`]).
    approval_armed_at: Option<Instant>,
    picker: Option<SessionPicker>,
    setup: Option<Setup>,
    /// The mesh radio (#124): the strip above the prompt while it's on.
    radio: RadioView,
    /// `/radio`'s roster view is open (Esc closes it).
    radio_panel: bool,
    /// A message just received flashes on the strip until then (#124): its id, and when the flash ends.
    radio_flash: Option<(String, Instant)>,
    /// The newest received message already seen (flashed or not), so a roster change doesn't flash it again.
    radio_seen: Option<String>,
    /// What [`SessionPort::keyboard_held`] was last told (`None`: nothing yet).
    held_told: Option<bool>,

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
            transcript: Transcript::new(),
            next_id: 1,
            scroll: ScrollView::default(),
            view_scroll: ScrollView::default(),
            selection: SelectionStore::new(),
            show_steps: false,
            agent_logs: HashMap::new(),
            viewing: None,
            agent_view: AgentView::default(),
            agent_rows: Vec::new(),
            turn: None,
            streaming: None,
            thinking: String::new(),
            draft: None,
            flush_at: None,
            tools_running: 0,
            agents_running: 0,
            sending: 0,
            queued: Vec::new(),
            exit_warned: false,
            plan: None,
            waiting_for: None,
            compacting: false,
            busy_label: None,
            waiting: None,
            stream_lexer: StreamLexer::new(),
            last_usage: None,
            totals: Totals::default(),
            confirm_exit: None,
            flash: None,
            prompt: PromptInput::default(),
            palette: None,
            approvals: VecDeque::new(),
            approval: None,
            approval_armed_at: None,
            picker: None,
            setup: None,
            radio: RadioView::default(),
            radio_panel: false,
            radio_flash: None,
            radio_seen: None,
            held_told: None,
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

    /// Whether something other than an empty prompt has the keyboard ([`SessionPort::keyboard_held`]): what's typed
    /// now would answer a screen, pick from a list, or finish a slash command, not start a turn.
    fn keyboard_held(&self) -> bool {
        self.phase != Phase::Main
            || self.setup.is_some()
            || self.picker.is_some()
            || self.palette.is_some()
            || self.radio_panel
            || self.prompt.value.trim_start().starts_with('/')
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
                // A reason being typed at an approval (#185) takes the paste; nothing else under it does.
                if self.approval.is_some() {
                    if self.setup.is_none()
                        && let Some((_, view)) = &mut self.approval
                        && view.paste(&text)
                    {
                        self.dirty = true;
                    }
                } else if self.phase == Phase::Main && self.setup.is_none() && self.approval.is_none() && self.picker.is_none() {
                    match &mut self.palette {
                        Some(palette) => palette.paste(&text),
                        None => self.prompt.paste(&text),
                    }
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
        self.move_on(now);
        // The loop ticks after every event, so the port hears of a change at once.
        let held = self.keyboard_held();
        if self.held_told != Some(held) {
            self.held_told = Some(held);
            self.port.keyboard_held(held);
        }
    }

    /// What moves on its own (the rest of [`Self::tick`]).
    fn move_on(&mut self, now: Instant) {
        if self.phase == Phase::Splash && self.splash.as_ref().is_some_and(|s| s.done(now)) {
            self.finish_splash(now);
        }
        if self.flush_at.is_some_and(|at| now >= at) {
            self.show_streamed(Some(now));
        }
        if self.phase != Phase::Main {
            return;
        }
        if self.welcome_visible && self.viewing.is_none() && self.martian.tick(now) {
            self.dirty = true;
        }
        if self.active_scroll().tick(now) {
            self.dirty = true;
        }
        if let SelectionEffect::ScrollBy(rows) = self.selection.tick(now) {
            self.scroll_active(rows);
        }
        // A spinner frame came due since the last frame was drawn.
        if self.spinner_since().is_some_and(|since| now >= thinking_view::next_frame(since, now - FRAME_INTERVAL)) {
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
        if self.radio_flash.as_ref().is_some_and(|(_, until)| now >= *until) {
            self.radio_flash = None;
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
        d.add(self.spinner_since().map(|since| thinking_view::next_frame(since, Instant::now())));
        d.add(self.confirm_exit);
        d.add(self.flash.as_ref().map(|(_, until)| *until));
        d.add(self.radio_flash.as_ref().map(|(_, until)| *until));
        d.next()
    }

    /// The clock of the waiting line on screen (the main one, or the open subagent view's), if one is.
    fn spinner_since(&self) -> Option<Instant> {
        if self.viewing.is_some() {
            self.agent_view.waiting_since()
        } else {
            self.waiting.as_ref().filter(|_| self.spinner_visible).map(|(_, since)| *since)
        }
    }

    /// The scroll view on screen: the subagent's view, or the transcript.
    fn active_scroll(&mut self) -> &mut ScrollView {
        if self.viewing.is_some() { &mut self.view_scroll } else { &mut self.scroll }
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
        self.transcript.push(message);
        self.dirty = true;
        id
    }

    fn system(&mut self, text: impl Into<String>) {
        self.add_message(system_message(text.into(), false));
    }

    fn system_error(&mut self, text: impl Into<String>) {
        self.add_message(system_message(text.into(), true));
    }

    fn update_tool(&mut self, id: u64, tool: ToolStatus) {
        if let Some(m) = self.transcript.message_mut(id) {
            m.tool = Some(tool);
            self.dirty = true;
        }
    }

    fn clear_transcript(&mut self) {
        self.selection.reset();
        self.agent_logs.clear();
        self.viewing = None;
        self.last_usage = None;
        self.plan = None;
        self.transcript.clear();
        self.scroll.follow();
        self.dirty = true;
    }

    fn open_view(&mut self, id: Option<u64>) {
        // The two transcripts have different rows: a selection or remembered row of one means nothing in the other.
        self.selection.reset();
        self.viewing = id;
        self.view_scroll = ScrollView::default();
        self.agent_view = AgentView::default();
        // The transcript keeps its place while hidden; a glide on its way stops.
        self.scroll.set_hidden(id.is_some());
        self.dirty = true;
    }

    // ------------------------------------------------------------------------------------------
    // Engine events (Marv `send()`'s event switch, plus the port's answers)
    // ------------------------------------------------------------------------------------------

    /// Folds one engine event into the state (Marv `send()`'s event switch).
    pub fn apply(&mut self, event: UiEvent, now: Instant) {
        match event {
            UiEvent::Approval { id, request } => {
                // The approval takes the keyboard, as it does from the prompt: a key meant for the palette must not
                // answer it. The prompt's text is still there underneath.
                self.palette = None;
                self.approvals.push_back(Pending { id, request });
                self.show_approvals(now);
            }
            UiEvent::ApprovalWithdrawn(id) => {
                self.approvals.retain(|p| p.id != id);
                self.show_approvals(now);
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
                // Setup takes the keyboard: a palette left open underneath would be closed by nothing.
                self.palette = None;
                let setup = Setup::new(context);
                if let Some(provider) = setup.needs_models() {
                    self.port.load_models(provider);
                }
                self.setup = Some(setup);
                self.dirty = true;
            }
            UiEvent::SignIn(progress) => {
                if let Some(setup) = &mut self.setup
                    && let Some(SetupOutcome::LoadModels(provider)) = setup.sign_in_progress(progress)
                {
                    self.port.load_models(provider);
                }
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
                    // The picker takes the keyboard (#113): a palette opened while the list loaded goes.
                    self.palette = None;
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
                self.plan = resumed.plan;
                self.transcript.replace(resumed.transcript);
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
            UiEvent::Radio(view) => {
                self.radio_panel &= view.on;
                // A message that wasn't there before flashes on the strip (#124); the radio off ends a flash.
                let newest = radio_view::newest_received(&view).map(|t| t.id.clone());
                if newest.is_some() && newest != self.radio_seen {
                    self.radio_flash = newest.clone().map(|id| (id, now + Duration::from_millis(radio_view::FLASH_MS)));
                }
                self.radio_seen = newest;
                if !view.on {
                    self.radio_flash = None;
                }
                self.radio = view;
                self.dirty = true;
            }
            UiEvent::OpenRadio => {
                if self.radio.on {
                    // It takes the keyboard, like the picker: a palette open goes.
                    self.palette = None;
                    self.radio_panel = true;
                    self.dirty = true;
                }
            }
            UiEvent::Busy(label) => {
                self.busy_label = label;
                self.dirty = true;
            }
            UiEvent::Exit => self.exit = true,
            // The main agent's plan call: a new list (an empty one clears it). Subagents have no plan tool.
            UiEvent::Plan(items) => {
                self.plan = (!items.is_empty()).then_some(items);
                self.dirty = true;
            }
            // After the turn, outside it: through apply_turn they would start a turn that never ends. Late (the user
            // started typing, or another turn started): dropped.
            UiEvent::Suggestion(text) => {
                if !self.busy() {
                    self.prompt.suggest(&text);
                    self.dirty = true;
                }
            }
            // "Send now" messages the turn never read: back into the prompt, ahead of anything typed since, to send
            // (or change) with Enter. Never sent on their own.
            UiEvent::NotSent { texts } => {
                self.sending = 0;
                // Each message on its own line, as sent (the prompt shows the breaks, and Enter sends them), ahead of
                // anything typed since, its paste chips kept.
                self.prompt.give_back(&texts.join("\n"));
                self.system("✻ Not sent: the turn ended before its next step. Your message is back in the prompt.");
            }
            // #184: the queue changed (a message queued, edited, dropped or started).
            UiEvent::Queued(list) => {
                // A queue that changed since the warning is warned about again.
                self.exit_warned &= list == self.queued;
                self.queued = list;
                self.dirty = true;
            }
            // A request outside a turn (the suggestion's) still counts.
            UiEvent::Usage { last, totals } if self.turn.is_none() => {
                self.last_usage = Some(last);
                self.totals = totals;
                self.dirty = true;
            }
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
            UiEvent::TurnStart { .. } => self.prompt.suggestion = None,
            UiEvent::Status(status) => {
                self.waiting_for = match status {
                    TurnStatus::WaitingForMcp => Some("Waiting for MCP servers to start…".into()),
                    TurnStatus::Checking => Some("Checking…".into()),
                    TurnStatus::Checkpointing => Some("Saving a checkpoint…".into()),
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
                    log.apply(&event, now);
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
                self.add_message(message(Role::Assistant, text));
                t.reply.clear();
                self.streaming = Some(String::new());
            }
            // "Send now": the person's message is in the turn now, after the step's reply and results.
            UiEvent::SentNow { text } => {
                self.turn = Some(t);
                self.flush();
                t = self.turn.take().expect("put back above");
                self.sending = self.sending.saturating_sub(1);
                self.add_message(message(Role::User, text));
                t.step_started = now;
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
                let status = ToolStatus { label: label.clone(), status: ToolState::Running, summary: None, steps: None, diff: None };
                let line = self.add_message(Message { tool: Some(status), ..message(Role::Tool, name) });
                t.tool_lines.insert(call_id.clone(), (line, label.clone()));
                if agent {
                    // `ToolStart` carries no arguments, so the task (Marv's first user message) needs the engine to send
                    // it (docs/subsystems/tui.md, gaps); until then the log starts without it.
                    self.agent_logs.insert(line, AgentLog::new(label, String::new()));
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
                    log.running = false;
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
            UiEvent::AnnouncedStep { continued } => {
                if continued {
                    // The model said what it would do next and stopped: Marv nudged it once, like a user would.
                    self.system("✻ The reply announced its next step but made no tool call, so Marv told the model to continue.");
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
        self.prompt.expect_suggestion();
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
                log.running = false;
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
        self.sending = 0;
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
        self.flush_at.get_or_insert(now + FRAME_INTERVAL);
    }

    /// Everything streamed so far reaches the screen at once (a step ended).
    fn flush(&mut self) {
        self.show_streamed(None);
    }

    /// Streamed text, reasoning, the draft and subagents' progress reach the screen. `paced`: the
    /// reply and reasoning show only their next share ([`next_reveal`]), and if more is hidden the
    /// next share is due a frame from `now`.
    fn show_streamed(&mut self, paced: Option<Instant>) {
        self.flush_at = None;
        let Some(mut t) = self.turn.take() else { return };
        let (reply, reply_behind) = next_reveal(self.streaming.as_deref().unwrap_or(""), &t.reply, paced.is_some());
        let (thought, thought_behind) = next_reveal(&self.thinking, &t.thought, paced.is_some());
        self.streaming = Some(reply.to_owned());
        self.thinking = thought.to_owned();
        if let Some(now) = paced
            && (reply_behind || thought_behind)
        {
            self.flush_at = Some(now + FRAME_INTERVAL);
        }
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
            CompactOutcome::Compacted { summary, tokens_before, overflow } => {
                let why = match (automatic, tokens_before, self.status.context_length) {
                    // #63: the request was refused as too long; level 3 dropped the oldest messages.
                    _ if overflow == Some(3) => {
                        " (the last request was too long for the model's window, so the oldest messages were dropped)".into()
                    }
                    _ if overflow.is_some() => " (the last request was too long for the model's window)".into(),
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
                            status_bar::tokens(marv_core::estimate_tokens(&summary))
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

    /// The head of the queue is shown; a new head gets a fresh view (cursor back on Yes), armed only after
    /// [`APPROVAL_ARM_MS`] so a key already on its way can't answer it.
    fn show_approvals(&mut self, now: Instant) {
        let escape_cancels = self.viewing.is_none();
        match self.approvals.front() {
            None => self.approval = None,
            Some(head) => {
                let waiting = self.approvals.len() - 1;
                match &mut self.approval {
                    Some((id, view)) if *id == head.id => view.waiting = waiting,
                    _ => {
                        self.approval = Some((head.id, ApprovalView::new(head.request.clone(), waiting, escape_cancels)));
                        self.approval_armed_at = Some(now + APPROVAL_ARM_MS);
                    }
                }
            }
        }
        self.dirty = true;
    }

    fn decide(&mut self, decision: Decision, now: Instant) {
        let Some(head) = self.approvals.pop_front() else { return };
        if decision == Decision::Always {
            // The session remembers the scope from now on; requests already waiting in it are covered too (not a
            // `once` or `must_ask` one, which is asked each time: the session never answers those from "always").
            let key = head.request.scope.key.clone();
            let (covered, rest): (Vec<_>, Vec<_>) =
                self.approvals.drain(..).partition(|p| p.request.scope.key == key && !p.request.once && !p.request.must_ask);
            self.approvals = rest.into();
            for pending in covered {
                self.port.answer(pending.id, Decision::Yes);
            }
        }
        self.show_approvals(now);
        self.port.answer(head.id, decision);
    }

    /// No to everything waiting (ctrl+c at an approval, and part of Esc).
    fn decline_all(&mut self) {
        let pending: Vec<_> = self.approvals.drain(..).collect();
        self.approval = None;
        self.dirty = true;
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
                Some(SetupOutcome::SignIn(provider, target)) => self.port.sign_in(provider, target),
                Some(SetupOutcome::UseAccount(provider, id)) => self.port.use_account(provider, id),
                Some(SetupOutcome::SignOut(provider)) => {
                    self.setup = None;
                    self.port.sign_out(provider);
                }
                Some(SetupOutcome::Complete(answer)) => {
                    self.setup = None;
                    self.port.complete_setup(answer);
                }
                Some(SetupOutcome::Cancel) => {
                    // #159: a sign-in still waiting for the browser stops listening.
                    if matches!(setup.sign_in, crate::ui::setup::SignInState::Waiting { .. }) {
                        self.port.cancel_sign_in();
                    }
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

        // ctrl+k: the palette, from the prompt (empty, half-typed, or while a reply streams). Never over an approval
        // or the session picker: they keep the keyboard.
        if key.code == KeyCode::Char('k') && ctrl && self.palette.is_none() {
            if self.approval.is_none() && self.picker.is_none() {
                let entries = command_palette::entries(&self.commands, &self.port.settings());
                self.palette = Some(CommandPalette::new(entries));
            }
            return;
        }
        if self.palette.is_some() && self.approval.is_none() && self.picker.is_none() {
            self.on_palette_key(key);
            return;
        }

        // `/radio`'s view: Esc closes it (it never stops a run); ctrl+c and paging still work, the rest waits.
        if self.radio_panel && self.approval.is_none() && self.picker.is_none() && self.setup.is_none() {
            if RadioPanel.on_key(key) {
                self.radio_panel = false;
                return;
            }
            if !(ctrl && key.code == KeyCode::Char('c')) && !matches!(key.code, KeyCode::PageUp | KeyCode::PageDown) {
                return;
            }
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
                } else if self.approval.is_none() && self.picker.is_none() {
                    // Idle, at the prompt: Esc also says no to the suggestion (shown or on its way, Greptile #83).
                    self.port.stop_suggesting();
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
            // An approval listing files (/undo) scrolls that list instead: the transcript stays put.
            KeyCode::PageUp if !self.files_shown() => self.page(-1),
            KeyCode::PageDown if !self.files_shown() => self.page(1),
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
            // Not yet armed: only Esc (and ctrl+c, above) and scrolling a file list get through.
            let armed = self.approval_armed_at.is_none_or(|at| now >= at);
            if !armed && !matches!(key.code, KeyCode::Esc | KeyCode::PageUp | KeyCode::PageDown) {
                return;
            }
            match view.on_key(key) {
                Some(ApprovalOutcome::Decide(decision)) => self.decide(decision, now),
                Some(ApprovalOutcome::CancelAll) => self.cancel_all(),
                None => {}
            }
        } else if !matches!(key.code, KeyCode::PageUp | KeyCode::PageDown) {
            if self.on_queue_key(key) {
                return;
            }
            let busy = self.busy();
            match self.prompt.on_key(key, busy, &self.commands) {
                Some(PromptOutcome::Submit(text)) => self.submit(&text, now),
                Some(PromptOutcome::SendNow(text)) => self.send_now(&text, now),
                None => {}
            }
        }
    }

    /// #184: the queue's keys at the prompt. ↑ on an empty line (not browsing the history) takes the newest queued
    /// message back into the line to edit (Enter queues it again); ctrl+x drops it. Whether the key was used.
    fn on_queue_key(&mut self, key: KeyEvent) -> bool {
        if self.queued.is_empty() {
            return false;
        }
        let plain = key.modifiers.difference(KeyModifiers::SHIFT).is_empty();
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Up if plain && self.prompt.value.is_empty() && self.prompt.history_index.is_none() => {
                if let Some(text) = self.port.unqueue_last() {
                    self.queued.retain_last(&text);
                    self.prompt.set(text);
                }
                true
            }
            KeyCode::Char('x') if ctrl => {
                if let Some(text) = self.port.unqueue_last() {
                    self.queued.retain_last(&text);
                    self.flash = Some(("Dropped a queued message".into(), Instant::now() + NOTICE_MS));
                }
                true
            }
            _ => false,
        }
    }

    /// A key while the palette is open. Esc closes it (it doesn't stop a reply: that's the next Esc), PgUp/PgDn
    /// and ctrl+o still work, ctrl+c closes it and interrupts a reply.
    fn on_palette_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::PageUp => return self.page(-1),
            KeyCode::PageDown => return self.page(1),
            KeyCode::Char('o') if ctrl => {
                self.show_steps = !self.show_steps;
                return;
            }
            KeyCode::Char('c') if ctrl => {
                self.palette = None;
                if self.busy() {
                    self.decline_all();
                    self.port.abort();
                }
                return;
            }
            _ => {}
        }
        let busy = self.busy();
        let Some(palette) = &mut self.palette else { return };
        match palette.on_key(key, busy) {
            None => {}
            Some(PaletteOutcome::Close) => self.palette = None,
            // As the slash command: the engine saves and announces it. The prompt keeps what was typed.
            Some(PaletteOutcome::Run(line)) => {
                self.palette = None;
                self.scroll.follow();
                self.port.run_command(line);
            }
            Some(PaletteOutcome::Prefill(text)) => {
                self.palette = None;
                // What was typed stays reachable with ↑, unless it's a login callback: it holds an authorization
                // code and never goes in the history.
                let typed = self.prompt.text().trim().to_string();
                if !typed.is_empty() && typed != text.trim() && !self.port.is_login_callback(&typed) {
                    self.prompt.remember(&typed);
                }
                self.prompt.history_index = None;
                self.prompt.set(text);
            }
        }
    }

    /// Marv's `handleSubmit`.
    fn submit(&mut self, raw: &str, now: Instant) {
        let text = raw.trim();
        if text.is_empty() {
            // #184: idle with messages a stop kept queued, Enter on the empty prompt sends the oldest.
            if !self.busy()
                && !self.queued.is_empty()
                && let Some(next) = self.port.take_queued()
            {
                self.queued.retain_first(&next);
                self.submit_now(next, now);
            }
            return;
        }
        // A login's callback address holds the authorization code: it goes to the login, never to the model, the
        // prompt history or the saved transcript. Even during a turn.
        if self.port.login_callback(text) {
            self.prompt.set("");
            self.prompt.forget_submission();
            return;
        }
        if self.busy() {
            // #184: a message while a turn runs is queued for after it (a command waits in the prompt, as before;
            // so does anything while work outside a turn runs).
            if self.turn.is_some() && !text.starts_with('/') {
                self.queue(text.to_string());
            }
            return;
        }
        self.submit_now(text.to_string(), now);
    }

    /// Enter while a turn runs (#184): the line leaves the prompt for the queue, listed above it, and goes when the turn
    /// finishes. ↑ on the empty prompt takes the newest back to edit; ctrl+x drops it.
    fn queue(&mut self, text: String) {
        self.prompt.set("");
        self.prompt.remember(&text);
        self.prompt.history_index = None;
        // Shown at once; the engine's list replaces it. Leaving would drop it too: `/exit` warns again.
        self.queued.push(text.clone());
        self.exit_warned = false;
        self.dirty = true;
        self.port.queue(text);
    }

    /// Sends `text` (trimmed, not empty) as a new turn or a command: the prompt empties and remembers it.
    fn submit_now(&mut self, text: String, now: Instant) {
        self.prompt.set("");
        self.scroll.follow();
        if self.viewing.is_some() {
            self.open_view(None);
        }
        self.prompt.remember(&text);
        self.prompt.history_index = None;
        // Leaving drops the queue (#184): the first `/exit` with messages queued says so, the next one leaves.
        if text.eq_ignore_ascii_case("/exit") && !self.queued.is_empty() && !self.exit_warned {
            self.exit_warned = true;
            let n = self.queued.len();
            let what = if n == 1 { "1 queued message".to_string() } else { format!("{n} queued messages") };
            return self.system(format!("✻ Leaving drops your {what}: /exit again to leave anyway."));
        }
        if text.starts_with('/') {
            self.port.run_command(text);
        } else {
            self.add_message(message(Role::User, text.clone()));
            self.start_turn(now);
            self.port.send(text);
        }
    }

    /// Alt+Enter: while a turn runs, the message goes into it at its next step ("send now"); it joins the transcript
    /// when it does ([`UiEvent::SentNow`]). Idle, or a command, or a subagent's view open: as Enter.
    fn send_now(&mut self, raw: &str, now: Instant) {
        let text = raw.trim();
        if !self.busy()
            || self.turn.is_none()
            || text.starts_with('/')
            || self.viewing.is_some()
            || self.port.is_login_callback(text)
        {
            return self.submit(raw, now);
        }
        if text.is_empty() {
            return;
        }
        let text = text.to_string();
        self.prompt.set("");
        self.scroll.follow();
        self.prompt.remember(&text);
        self.prompt.history_index = None;
        self.sending += 1;
        self.dirty = true;
        self.port.send_now(text);
    }

    /// An approval listing files (/undo) is up: PgUp/PgDn and the wheel scroll the list, not the transcript.
    fn files_shown(&self) -> bool {
        self.setup.is_none() && self.approval.as_ref().is_some_and(|(_, view)| view.has_files())
    }

    fn page(&mut self, direction: i32) {
        let scroll = self.active_scroll();
        if direction < 0 {
            scroll.page_up()
        } else {
            scroll.page_down()
        }
        self.dirty = true;
    }

    fn scroll_active(&mut self, rows: i32) {
        self.active_scroll().scroll_by(rows);
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
            MouseEvent::Scroll { step } if self.files_shown() => {
                if let Some((_, view)) = &mut self.approval {
                    view.scroll_files(step as i32 * WHEEL_ROWS);
                }
                self.dirty = true;
                SelectionEffect::None
            }
            MouseEvent::Scroll { step } => {
                // The highlight is attached to the text, so it scrolls along.
                self.active_scroll().glide_by(step as i32 * WHEEL_ROWS, now);
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
                    let id = row.and_then(|row| transcript::agent_entry_at(row, &self.agent_rows));
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
        if let Some((_, view)) = &mut self.approval {
            view.set_window_rows(area.height);
        }
        let header = if self.viewing.is_some() { agent_view::HEADER_HEIGHT } else { 0 };
        let radio_panel = self.radio_panel && self.radio.on && self.picker.is_none();
        let panel_height = if let Some(picker) = self.picker.as_ref().filter(|_| self.setup.is_none() && self.approval.is_none())
        {
            picker.height()
        } else if radio_panel && self.setup.is_none() && self.approval.is_none() {
            RadioPanel::height(&self.radio)
        } else if let Some((_, view)) = self.approval.as_ref().filter(|_| self.setup.is_none()) {
            view.height(width) + status_height
        } else if let Some(setup) = &self.setup {
            setup.height(width)
        } else if let Some(palette) = &self.palette {
            palette.height(area.height.saturating_sub(header + 1 + status_height)) + status_height
        } else {
            self.prompt.height(width, busy, &self.commands) + status_height
        };
        // #184: the queued messages, above the prompt (only where the prompt shows).
        let prompt_shown =
            self.picker.is_none() && !radio_panel && self.approval.is_none() && self.setup.is_none() && self.palette.is_none();
        let queue = if prompt_shown { prompt_input::queue_rows(&self.queued, width) } else { Vec::new() };
        let queue_height = (queue.len() as u16).min(area.height.saturating_sub(header + panel_height + 2));
        let panel_height = panel_height + queue_height;
        let panel_height = panel_height.min(area.height.saturating_sub(header + 1));
        // The radio's strip (#124): one line above the prompt while the radio is on (not over setup or a picker).
        let strip_height = u16::from(
            self.radio.on
                && !radio_panel
                && self.setup.is_none()
                && self.picker.is_none()
                && area.height > header + panel_height + 2,
        );
        let panel_height = panel_height + strip_height;
        // The plan sits between the transcript and the panel (in a subagent's view too: it's the main agent's plan).
        let plan = if area.height >= PLAN_MIN_WINDOW { plan_view::render(self.plan.as_deref(), width) } else { Vec::new() };
        let plan_height = (plan.len() as u16).min(area.height.saturating_sub(header + panel_height + 1));
        // A subagent's view has its header pinned above it.
        let transcript = Rect { y: area.y + header, height: area.height - panel_height - plan_height - header, ..area };
        let plan_area = Rect { y: transcript.bottom(), height: plan_height, ..area };
        let panel = Rect { y: plan_area.bottom(), height: panel_height, ..area };

        self.spinner_visible = false;
        if let Some(id) = self.viewing.filter(|id| self.agent_logs.contains_key(id)) {
            let log = &self.agent_logs[&id];
            AgentViewHeader { log }.render(Rect { height: 1.min(header), ..area }, buf);
            let lines = self.agent_view.render(log, width, self.show_steps, now);
            self.welcome_visible = false;
            let vp = self.view_scroll.draw(transcript, buf, &lines);
            self.selection.set_viewport(vp);
        } else {
            self.draw_transcript(transcript, buf, now);
        }
        // Remember the visible rows' text and draw the highlight (Marv's transformOutput).
        self.selection.transform_buffer(buf);
        for (row, line) in plan.iter().take(plan_height as usize).enumerate() {
            buf.set_line(plan_area.x, plan_area.y + row as u16, line, plan_area.width);
        }

        let bar = StatusBar {
            info: Some(&self.status),
            confirm_exit: self.confirm_exit.is_some(),
            flash: self.flash.as_ref().map(|(text, _)| text.as_str()),
            usage: status_bar::usage_text(self.last_usage.as_ref(), &self.totals, self.status.context_length),
            busy,
            agents: self.agents_running,
            viewing: self.viewing.is_some(),
            sending: self.sending,
            queued: self.queued.len(),
        };
        if strip_height == 1 {
            let flashing =
                self.radio_flash.as_ref().and_then(|(id, _)| self.radio.traffic.iter().find(|t| &t.id == id && !t.outgoing));
            let line = match flashing {
                Some(message) => radio_view::flash(&self.radio, message, panel.width),
                None => radio_view::strip(&self.radio, panel.width),
            };
            buf.set_line(panel.x, panel.y, &line, panel.width);
        }
        let panel = Rect { y: panel.y + strip_height, height: panel.height - strip_height, ..panel };
        let queue_height = queue_height.min(panel.height.saturating_sub(1));
        for (row, line) in queue.iter().take(queue_height as usize).enumerate() {
            buf.set_line(panel.x, panel.y + row as u16, line, panel.width);
        }
        let panel = Rect { y: panel.y + queue_height, height: panel.height - queue_height, ..panel };
        let status_area = Rect { y: panel.bottom().saturating_sub(1), height: 1.min(panel.height), ..panel };
        let above_status = Rect { height: panel.height.saturating_sub(1), ..panel };
        if let Some(picker) = self.picker.as_ref().filter(|_| self.setup.is_none() && self.approval.is_none()) {
            picker.render(panel, buf);
        } else if radio_panel && self.setup.is_none() && self.approval.is_none() {
            RadioPanel::render(&self.radio, unix_ms(), panel, buf);
        } else if let Some((_, view)) = self.approval.as_ref().filter(|_| self.setup.is_none()) {
            view.render(above_status, buf);
            // At an approval the bar is always busy and never asks to confirm an exit.
            StatusBar { confirm_exit: false, busy: true, usage: None, ..bar }.render(status_area, buf);
        } else if let Some(setup) = &self.setup {
            setup.render(panel, buf);
        } else if let Some(palette) = &self.palette {
            palette.render(above_status, buf, busy);
            bar.render(status_area, buf);
        } else {
            self.prompt.render(above_status, buf, busy, &self.commands);
            bar.render(status_area, buf);
        }
    }

    /// The main transcript, culled to the viewport: only entries intersecting it are laid out
    /// (from their cache) and drawn.
    fn draw_transcript(&mut self, area: Rect, buf: &mut Buffer, now: Instant) {
        let width = area.width;
        // Heights first (fills the caches: an unchanged entry is laid out once per width).
        let mut heights = self.transcript.heights(width, self.show_steps);
        let entries = heights.len();
        let tail = self.live_lines(width, now);
        heights.push(tail.len());
        let shown = self.scroll.visible(&heights, area.height);
        let top = self.scroll.scroll_top();
        self.agent_rows = self.transcript.agent_entries(&heights[..entries]);

        self.welcome_visible = false;
        let mut row: usize = heights[..shown.start].iter().sum();
        for i in shown {
            let welcome;
            let lines: &[Line] = if i == entries {
                self.spinner_visible = self.waiting.is_some();
                &tail
            } else if self.transcript.items[i] == TranscriptItem::Welcome {
                self.welcome_visible = true;
                welcome = transcript::welcome_lines(&self.welcome, self.martian.frame(), width);
                &welcome
            } else {
                self.transcript.lines(i)
            };
            for (j, line) in lines.iter().enumerate() {
                if let Some(y) = (row + j).checked_sub(top).filter(|y| *y < area.height as usize) {
                    buf.set_line(area.x, area.y + y as u16, line, width);
                }
            }
            row += heights[i];
        }
        self.selection.set_viewport(self.scroll.viewport(area.y, width));
    }

    /// The live end of the transcript: the reply streaming in, the waiting line, a tool call being written.
    fn live_lines(&mut self, width: u16, now: Instant) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        let Some(streaming) = &self.streaming else {
            self.waiting = self.busy_label.as_ref().map(|label| self.waiting_since(label.clone(), now));
            if let Some((label, since)) = &self.waiting {
                lines.extend(ThinkingView { since: *since, label: label.clone(), code: false }.render("", None, now, width));
            }
            return lines;
        };
        if !streaming.is_empty() {
            // The lexer keeps the finished blocks' lines: only the unfinished end is parsed and wrapped again.
            let reply = message(Role::Assistant, streaming.clone());
            lines.extend(message_view::render(&reply, width, false, Some(&mut self.stream_lexer)));
        }
        let label = if self.tools_running > 0 {
            None
        } else if let Some(draft) = &self.draft {
            Some(thinking_view::draft_label(&draft.name, &draft.args))
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
            let view = ThinkingView { since: *since, label: label.clone(), code: false };
            match &self.draft {
                Some(draft) => lines.extend(DraftView::render(&view, &draft.name, &draft.args, now, width)),
                None => lines.extend(view.render(&self.thinking, None, now, width)),
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

/// What to show of `full` when `shown` is on screen, and whether more is still hidden after it.
/// Unpaced: all of it. Paced: a seventh of what's hidden (`REVEAL_MS` over the frame interval), at
/// least [`REVEAL_MIN_CHARS`], or all of it past [`REVEAL_CATCH_UP`]. Text that no longer starts
/// with `shown` (a new step's reply, a retry) is revealed from its start. Cuts on char boundaries.
fn next_reveal<'a>(shown: &str, full: &'a str, paced: bool) -> (&'a str, bool) {
    let start = if full.starts_with(shown) { shown.len() } else { 0 };
    let hidden = &full[start..];
    let behind = hidden.chars().count();
    if !paced || behind <= REVEAL_MIN_CHARS || behind > REVEAL_CATCH_UP {
        return (full, false);
    }
    let frames = (REVEAL_MS.as_micros() / FRAME_INTERVAL.as_micros()).max(1) as usize;
    let n = behind.div_ceil(frames).max(REVEAL_MIN_CHARS);
    let end = start + hidden.char_indices().nth(n).map_or(hidden.len(), |(i, _)| i);
    (&full[..end], end < full.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::scroll_view::GLIDE_MS;
    use crate::ui::thinking_view::SPINNER_MS;
    use crate::view::{
        DiffLine, DiffShown, Preview, ProviderId, Resumed, Scope, SessionSummary, SettingAction, SettingInfo, SetupAnswer,
    };
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
    fn a_reveal_shows_a_share_of_what_is_hidden_on_char_boundaries() {
        // Unpaced, or nearly caught up: everything.
        assert_eq!(next_reveal("", "hello world", false), ("hello world", false));
        assert_eq!(next_reveal("hello wor", "hello world", true), ("hello world", false));
        // Paced: a seventh of the 70 hidden chars.
        let full = "x".repeat(70);
        assert_eq!(next_reveal("", &full, true), (&full[..10], true));
        // Never fewer than the minimum, and a cut never splits a char.
        let (shown, more) = next_reveal("", "éééééééé", true);
        assert_eq!((shown, more), ("éé", true));
        // A huge backlog isn't replayed; text that doesn't continue what's shown starts over.
        let big = "y".repeat(REVEAL_CATCH_UP + 1);
        assert_eq!(next_reveal("", &big, true), (big.as_str(), false));
        assert_eq!(next_reveal("old reply", &full, true), (&full[..10], true));
    }

    #[test]
    fn step_ends_match_marv() {
        assert!(is_step_end(&UiEvent::Assistant { text: String::new() }));
        assert!(is_step_end(&UiEvent::Done(marv_core::StopReason::End)));
        assert!(!is_step_end(&UiEvent::TextDelta("x".into())));
    }

    // --- a fake engine ---

    #[derive(Debug, Clone, PartialEq)]
    enum Call {
        Send(String),
        Command(String),
        Answer(u64, Decision),
        Abort,
        StopSuggesting,
        LoadModels(ProviderId),
        CompleteSetup(SetupAnswer),
        Resume(Option<String>),
        LoginCallback(String),
        SendNow(String),
        Queue(String),
    }

    #[derive(Default)]
    struct FakePort {
        calls: Mutex<Vec<Call>>,
        /// The engine's queue (#184): `queue` pushes, `unqueue_last` / `take_queued` take.
        queue: Mutex<VecDeque<String>>,
        /// What `keyboard_held` was told, in order.
        held: Mutex<Vec<bool>>,
    }

    impl SessionPort for FakePort {
        fn commands(&self) -> Vec<CommandInfo> {
            vec![
                CommandInfo { while_busy: true, ..CommandInfo::new("help", "Show available commands") },
                CommandInfo::new("undo", "Put back the files the last turn changed"),
            ]
        }
        fn settings(&self) -> Vec<SettingInfo> {
            vec![
                SettingInfo {
                    name: "Yolo".into(),
                    value: "on".into(),
                    description: "Run what the sandbox confines without asking".into(),
                    action: SettingAction::Run("/yolo off".into()),
                    while_busy: true,
                },
                SettingInfo {
                    name: "Step limit".into(),
                    value: "25".into(),
                    description: "Show or set the step limit".into(),
                    action: SettingAction::Prefill("/steps ".into()),
                    while_busy: false,
                },
            ]
        }
        fn send(&self, text: String) {
            self.calls.lock().unwrap().push(Call::Send(text));
        }
        fn send_now(&self, text: String) {
            self.calls.lock().unwrap().push(Call::SendNow(text));
        }
        fn queue(&self, text: String) {
            self.calls.lock().unwrap().push(Call::Queue(text.clone()));
            self.queue.lock().unwrap().push_back(text);
        }
        fn unqueue_last(&self) -> Option<String> {
            self.queue.lock().unwrap().pop_back()
        }
        fn take_queued(&self) -> Option<String> {
            self.queue.lock().unwrap().pop_front()
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
        fn stop_suggesting(&self) {
            self.calls.lock().unwrap().push(Call::StopSuggesting);
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
        fn is_login_callback(&self, text: &str) -> bool {
            text.starts_with("http://127.0.0.1:5555/callback?")
        }
        fn keyboard_held(&self, held: bool) {
            self.held.lock().unwrap().push(held);
        }
        fn login_callback(&self, text: &str) -> bool {
            let taken = text.starts_with("http://127.0.0.1:5555/callback?");
            if taken {
                self.calls.lock().unwrap().push(Call::LoginCallback(text.into()));
            }
            taken
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
            status: StatusInfo {
                model: "ollama/qwen3.5:9b".into(),
                cwd: "~/x".into(),
                yolo: false,
                context_length: None,
                processes: 0,
            },
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
            self.settle();
        }

        /// Frame by frame until the streamed text is all revealed (bounded: a reveal always ends).
        fn settle(&mut self) {
            self.advance(FRAME_INTERVAL.as_millis() as u64 + 1);
            for _ in 0..200 {
                if self.app.flush_at.is_none() {
                    return;
                }
                self.advance(FRAME_INTERVAL.as_millis() as u64 + 1);
            }
            panic!("the reveal never finished");
        }

        /// Past the time a newly shown approval ignores answering keys ([`APPROVAL_ARM_MS`]).
        fn arm(&mut self) {
            self.advance(APPROVAL_ARM_MS.as_millis() as u64);
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
            reason: true,
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
            must_ask: false,
            door: None,
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
        // Not shown until the next frame, and then a share at a time.
        assert!(!h.frame().contains("You said"));
        assert_eq!(h.app.deadline(), Some(h.now + FRAME_INTERVAL).min(h.app.deadline()));
        h.advance(17);
        let first = h.frame();
        assert!(first.contains("● You"), "{first}");
        assert!(!first.contains("You said"), "{first}");
        h.settle();
        assert!(h.frame().contains("● You said: hello world"));
        h.events([UiEvent::Assistant { text: "You said: hello world".into() }, UiEvent::Done(StopReason::End), turn_end()]);
        let frame = h.frame();
        assert_eq!(frame.matches("You said: hello world").count(), 1, "{frame}");
        assert!(!h.app.busy());
        assert!(frame.contains("/help · PgUp/PgDn · ctrl+c exit"));
    }

    fn alt_enter(h: &mut Harness, text: &str) {
        for c in text.chars() {
            h.key(KeyCode::Char(c));
        }
        h.app.on_key(KeyEvent::new(ENTER, KeyModifiers::ALT), h.now);
    }

    #[test]
    fn alt_enter_while_a_turn_runs_sends_now_and_the_message_shows_where_the_turn_read_it() {
        let mut h = harness();
        h.typed("fix the build");
        h.events([UiEvent::TurnStart { turn: "t".into() }, tool_start("c1", "bash", "cargo build")]);
        assert!(h.frame().contains("enter queues · alt+enter send now"));
        alt_enter(&mut h, "also the tests");
        assert_eq!(h.calls(), [Call::Send("fix the build".into()), Call::SendNow("also the tests".into())]);
        assert_eq!(h.app.prompt.value, "");
        // Not in the transcript until the turn read it; the bar says one is waiting.
        assert!(!h.frame().contains("> also the tests"));
        assert!(h.frame().contains("1 for the next step"));
        h.events([tool_end("c1", "bash", "cargo build", "ok"), UiEvent::SentNow { text: "also the tests".into() }]);
        let frame = h.frame();
        let (build, sent) = (frame.find("cargo build").unwrap(), frame.find("> also the tests").unwrap());
        assert!(sent > build, "after the tool's entry:\n{frame}");
        assert!(!frame.contains("for the next step"));
        // ctrl+s does the same (where the terminal keeps Alt+Enter).
        for c in "and lint".chars() {
            h.key(KeyCode::Char(c));
        }
        h.ctrl('s');
        assert_eq!(h.calls().last(), Some(&Call::SendNow("and lint".into())));
        h.event(UiEvent::SentNow { text: "and lint".into() });
        // Idle, Alt+Enter is Enter.
        h.events([UiEvent::Done(StopReason::End), turn_end()]);
        alt_enter(&mut h, "thanks");
        assert_eq!(h.calls().last(), Some(&Call::Send("thanks".into())));
    }

    // --- #184: the queue ---

    /// Types `text` and presses Enter (queued while a turn runs).
    fn enter(h: &mut Harness, text: &str) {
        for c in text.chars() {
            h.key(KeyCode::Char(c));
        }
        h.key(ENTER);
    }

    #[test]
    fn enter_while_a_turn_runs_queues_the_message_and_shows_it_above_the_prompt() {
        let mut h = harness();
        h.typed("fix the build");
        h.events([UiEvent::TurnStart { turn: "t".into() }, tool_start("c1", "bash", "cargo build")]);
        enter(&mut h, "then run the tests");
        enter(&mut h, "and lint");
        assert_eq!(
            h.calls(),
            [Call::Send("fix the build".into()), Call::Queue("then run the tests".into()), Call::Queue("and lint".into())]
        );
        // Out of the prompt, listed above it, not in the transcript yet; the bar says how to change them.
        assert_eq!(h.app.prompt.value, "");
        let frame = h.frame();
        assert!(frame.contains("queued › then run the tests") && frame.contains("queued › and lint"), "{frame}");
        assert!(!frame.contains("> then run the tests"));
        assert!(frame.contains("↑ edit queued · ctrl+x drop"), "{frame}");
        // The engine's list is what's shown: it started the first one when the turn ended.
        h.events([UiEvent::Done(StopReason::End), turn_end()]);
        h.events([
            UiEvent::Queued(vec!["and lint".into()]),
            UiEvent::Message(message(Role::User, "then run the tests".into())),
            UiEvent::TurnStart { turn: "t2".into() },
        ]);
        let frame = h.frame();
        assert!(frame.contains("> then run the tests") && !frame.contains("queued › then run"), "{frame}");
        assert!(frame.contains("queued › and lint"));
        assert!(h.app.busy());
        // A command while a turn runs isn't queued: it waits in the prompt, as before.
        enter(&mut h, "/undo");
        assert_eq!(h.app.prompt.value, "/undo");
        assert!(!h.calls().contains(&Call::Queue("/undo".into())));
    }

    #[test]
    fn up_takes_the_newest_queued_message_back_to_edit_and_ctrl_x_drops_one() {
        let mut h = harness();
        h.typed("go");
        h.events([UiEvent::TurnStart { turn: "t".into() }]);
        enter(&mut h, "first");
        enter(&mut h, "secnd");
        // ↑ on the empty prompt: the newest is back in the line (and out of the queue), to fix and queue again.
        h.key(KeyCode::Up);
        assert_eq!(h.app.prompt.value, "secnd");
        assert!(!h.frame().contains("queued › secnd"));
        assert_eq!(h.port.queue.lock().unwrap().iter().cloned().collect::<Vec<_>>(), ["first"]);
        h.app.prompt.set("second");
        h.key(ENTER);
        assert_eq!(h.port.queue.lock().unwrap().iter().cloned().collect::<Vec<_>>(), ["first", "second"]);
        // With text in the line, ↑ is the history, as always.
        h.key(KeyCode::Char('x'));
        h.key(KeyCode::Up);
        assert_eq!(h.app.queued, ["first", "second"]);
        h.app.prompt.set("");
        h.app.prompt.history_index = None;
        // ctrl+x drops the newest.
        h.ctrl('x');
        assert_eq!(h.app.queued, ["first"]);
        assert_eq!(h.port.queue.lock().unwrap().iter().cloned().collect::<Vec<_>>(), ["first"]);
        assert!(h.frame().contains("Dropped a queued message"));
        h.ctrl('x');
        assert!(h.app.queued.is_empty() && h.port.queue.lock().unwrap().is_empty());
        // Nothing queued: ↑ is the history again.
        h.key(KeyCode::Up);
        assert_eq!(h.app.prompt.value, "second");
    }

    #[test]
    fn esc_stops_the_turn_and_keeps_the_queue_enter_on_the_empty_prompt_sends_the_next() {
        let mut h = harness();
        h.typed("go");
        h.events([UiEvent::TurnStart { turn: "t".into() }]);
        enter(&mut h, "one");
        enter(&mut h, "two");
        h.key(KeyCode::Esc);
        assert!(h.calls().contains(&Call::Abort));
        // The engine keeps them (and says so): still listed, nothing started.
        h.events([
            UiEvent::Done(StopReason::Aborted),
            turn_end(),
            UiEvent::Message(system_message("✻ Stopped. Your 2 queued messages wait: …".into(), false)),
        ]);
        assert!(!h.app.busy());
        let frame = h.frame();
        assert!(frame.contains("queued › one") && frame.contains("queued › two"), "{frame}");
        assert!(frame.contains("enter sends queued · ↑ edit · ctrl+x drop"), "{frame}");
        // Enter on the empty prompt: the oldest goes now, as if typed.
        h.key(ENTER);
        assert_eq!(h.calls().last(), Some(&Call::Send("one".into())));
        assert_eq!(h.app.queued, ["two"]);
        assert!(h.app.busy());
        assert!(h.frame().contains("> one"));
    }

    /// Review on #239: leaving drops the queue, so the first `/exit` (or ctrl+c) says how many go; the next leaves.
    #[test]
    fn leaving_with_messages_queued_warns_first() {
        let mut h = harness();
        h.typed("go");
        h.events([UiEvent::TurnStart { turn: "t".into() }]);
        enter(&mut h, "one");
        enter(&mut h, "two");
        h.events([UiEvent::Done(StopReason::Aborted), turn_end()]);
        h.typed("/exit");
        assert!(!h.calls().contains(&Call::Command("/exit".into())));
        assert!(h.frame().contains("Leaving drops your 2 queued messages"));
        // Queued more since the warning: warned again.
        h.events([UiEvent::TurnStart { turn: "t2".into() }]);
        enter(&mut h, "three");
        h.events([
            UiEvent::Queued(vec!["one".into(), "two".into(), "three".into()]),
            UiEvent::Done(StopReason::Aborted),
            turn_end(),
        ]);
        h.typed("/exit");
        assert!(!h.calls().contains(&Call::Command("/exit".into())));
        assert!(h.frame().contains("Leaving drops your 3 queued messages"));
        h.typed("/exit");
        assert_eq!(h.calls().last(), Some(&Call::Command("/exit".into())));
        // ctrl+c's confirm says it too.
        h.ctrl('c');
        assert!(h.frame().contains("Press ctrl+c again to exit (3 queued messages will be dropped)"), "{}", h.frame());
    }

    #[test]
    fn a_long_queue_lists_three_and_counts_the_rest() {
        let rows = prompt_input::queue_rows(&["a".into(), "b\nmore".into(), "c".into(), "d".into(), "e".into()], 40);
        let text: Vec<String> = rows.iter().map(|l| l.to_string()).collect();
        assert_eq!(text, ["  queued › a", "  queued › b …", "  queued › c", "  +2 more queued"]);
        assert!(prompt_input::queue_rows(&[], 40).is_empty());
    }

    #[test]
    fn a_message_the_turn_never_read_comes_back_to_the_prompt() {
        let mut h = harness();
        h.typed("go");
        h.events([UiEvent::TurnStart { turn: "t".into() }]);
        alt_enter(&mut h, "wait, use pnpm");
        for c in "and".chars() {
            h.key(KeyCode::Char(c));
        }
        h.events([UiEvent::NotSent { texts: vec!["wait, use pnpm".into()] }, UiEvent::Done(StopReason::Aborted), turn_end()]);
        assert_eq!(h.app.prompt.value, "wait, use pnpm\nand");
        let frame = h.frame();
        assert!(frame.contains("> wait, use pnpm") && frame.contains("  and"), "{frame}");
        assert!(h.frame().contains("Not sent: the turn ended before its next step"));
        // A command with Alt+Enter is a command: it waits for the turn like Enter.
        h.app.prompt.set("");
        h.typed("again");
        h.events([UiEvent::TurnStart { turn: "t2".into() }]);
        alt_enter(&mut h, "/undo");
        assert!(!h.calls().iter().any(|c| matches!(c, Call::SendNow(t) if t == "/undo")));
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

    // #214: a multi-line user message (a paste) selects and copies with its newlines.
    #[test]
    fn dragging_across_a_multi_line_user_message_copies_its_lines() {
        let mut h = harness();
        h.app.on_event(Event::Paste("first pasted line\nsecond pasted line\nthird one".into()), h.now);
        h.key(ENTER);
        h.events([UiEvent::Done(StopReason::End), turn_end()]);
        let frame = h.frame();
        let at = |text: &str| {
            let (y, line) =
                frame.lines().enumerate().find(|(_, l)| l.contains(text)).unwrap_or_else(|| panic!("{text}:\n{frame}"));
            (unicode_width::UnicodeWidthStr::width(&line[..line.find(text).unwrap()]) as u16, y as u16)
        };
        let (x0, y0) = at("first pasted line");
        let (x1, y1) = at("third one");
        h.app.on_mouse(MouseEvent::Press { x: x0, y: y0 }, h.now);
        h.app.on_mouse(MouseEvent::Drag { x: x1 + 8, y: y1 }, h.now);
        assert!(h.buffer()[(x0 + 3, y0 + 1)].modifier.contains(ratatui::style::Modifier::REVERSED));
        h.app.on_mouse(MouseEvent::Release { x: x1 + 8, y: y1 }, h.now);
        futures::executor::block_on(h.app.settle_copies(h.now));
        assert_eq!(*h.copied.lock().unwrap(), ["first pasted line\n  second pasted line\n  third one"]);
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
    fn a_suggestion_after_the_turn_fills_the_prompt_without_starting_a_turn() {
        let mut h = harness();
        h.typed("go");
        h.events([UiEvent::TextDelta("Done.".into()), turn_end()]);
        let totals = Totals { requests: 2, ..Default::default() };
        let last = h.app.last_usage.unwrap_or_default();
        h.events([UiEvent::Usage { last, totals }, UiEvent::Suggestion("run the tests".into())]);
        assert!(!h.app.busy() && h.app.turn.is_none(), "no turn started by the late events");
        let frame = h.frame();
        assert!(frame.contains("> run the tests  (tab)"), "{frame}");
        assert!(!frame.contains("Thinking"), "{frame}");
    }

    // Greptile #83: the suggestion is asked for after the turn, while idle, so Esc then (not only while busy) stops
    // the request and hides one already shown; a reply that comes anyway isn't shown.
    #[test]
    fn esc_while_idle_stops_the_suggestion_and_hides_it() {
        let mut h = harness();
        h.typed("go");
        h.events([UiEvent::TextDelta("Done.".into()), turn_end()]);
        h.key(KeyCode::Esc);
        assert_eq!(h.calls().last(), Some(&Call::StopSuggesting));
        assert!(!h.calls().contains(&Call::Abort), "nothing else is stopped");
        h.events([UiEvent::Suggestion("run the tests".into())]);
        assert!(!h.frame().contains("run the tests  (tab)"), "{}", h.frame());

        h.typed("again");
        h.events([UiEvent::TextDelta("Done.".into()), turn_end(), UiEvent::Suggestion("commit this".into())]);
        assert!(h.frame().contains("> commit this  (tab)"), "{}", h.frame());
        h.key(KeyCode::Esc);
        assert!(!h.frame().contains("commit this"), "{}", h.frame());
        h.key(ENTER);
        assert_eq!(h.calls().iter().filter(|c| matches!(c, Call::Send(_))).count(), 2, "nothing was sent");
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
        let whole = message_view::render(&message(Role::Assistant, text.into()), 40, false, None);
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
        let outcome = CompactOutcome::Compacted { summary: "x".repeat(4000), tokens_before: Some(30_000), overflow: None };
        h.events([UiEvent::Compaction { automatic: true, outcome }]);
        assert!(
            h.frame().contains("✻ Compacted the conversation (the context was 92% full): 30k → about 1k tokens."),
            "{}",
            h.frame()
        );
        let overflow = CompactOutcome::Compacted { summary: "x".repeat(400), tokens_before: Some(30_000), overflow: Some(1) };
        h.events([UiEvent::Compaction { automatic: true, outcome: overflow }]);
        assert!(h.frame().contains("(the last request was too long for the model's window): 30k"), "{}", h.frame());
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
        h.arm();
        h.key(ENTER); // Yes
        assert_eq!(h.calls().last(), Some(&Call::Answer(1, Decision::Yes)));
        assert!(!h.frame().contains("Do you want to proceed?"));
    }

    #[test]
    fn no_declines_and_the_engine_says_stopped() {
        let mut h = harness();
        h.typed("make a file");
        h.events([UiEvent::Approval { id: ApprovalId(1), request: write_request("made.txt", None) }]);
        h.arm();
        h.key(DOWN);
        h.key(DOWN);
        h.key(ENTER);
        assert_eq!(h.calls().last(), Some(&Call::Answer(1, Decision::No)));
        h.events([UiEvent::Done(StopReason::Declined), turn_end()]);
        assert!(h.frame().contains("Stopped. Tell Marv what to do instead."));
    }

    /// #185: tab, a reason, Enter: the answer carries the reason, nothing is aborted (the turn goes on), and a paste
    /// lands in the reason, not the prompt.
    #[test]
    fn no_and_say_why_answers_with_the_reason_and_doesnt_stop() {
        let mut h = harness();
        h.typed("make a file");
        h.events([UiEvent::Approval { id: ApprovalId(1), request: write_request("made.txt", None) }]);
        h.arm();
        h.key(KeyCode::Tab);
        for c in "call it notes.txt".chars() {
            h.key(KeyCode::Char(c));
        }
        h.app.on_event(Event::Paste(" please".into()), h.now);
        assert_eq!(h.app.prompt.value, "");
        assert!(h.frame().contains("Why not?"));
        h.key(ENTER);
        let calls = h.calls();
        assert_eq!(calls.last(), Some(&Call::Answer(1, Decision::Declined("call it notes.txt please".into()))));
        assert!(!calls.contains(&Call::Abort));
        assert!(!h.frame().contains("Why not?"));
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
        h.arm();
        h.key(DOWN);
        h.key(DOWN);
        assert!(h.frame().contains("❯ No, and tell Marv"));
        h.key(ENTER); // No, for the first subagent
        let frame = h.frame();
        assert!(frame.contains("Do you want to proceed?")); // the second one, a fresh prompt
        assert!(frame.contains("❯ Yes"));
        assert!(!frame.contains("❯ No"));
        assert!(!frame.contains("more waiting"));
        // The next head is armed afresh: an Enter pressed twice doesn't answer it too.
        h.key(ENTER);
        assert_eq!(&h.calls()[1..], [Call::Answer(1, Decision::No)]);
        h.arm();
        h.key(ENTER);
        assert_eq!(&h.calls()[1..], [Call::Answer(1, Decision::No), Call::Answer(2, Decision::Yes)]);
    }

    /// #201: an approval nobody waits for any more (another Marv asked for a wire and hung up) leaves the screen
    /// unanswered; the next one in the queue shows, and with none left the prompt is back.
    #[test]
    fn a_withdrawn_approval_leaves_the_queue_unanswered() {
        let mut h = harness();
        h.events([
            UiEvent::Approval { id: ApprovalId(1), request: write_request("one.txt", None) },
            UiEvent::Approval { id: ApprovalId(2), request: write_request("two.txt", None) },
        ]);
        assert!(h.frame().contains("1 more waiting"));
        h.events([UiEvent::ApprovalWithdrawn(ApprovalId(1))]);
        let frame = h.frame();
        assert!(frame.contains("Do you want to proceed?") && frame.contains("two.txt"), "{frame}");
        assert!(!frame.contains("more waiting"));
        h.events([UiEvent::ApprovalWithdrawn(ApprovalId(2)), UiEvent::ApprovalWithdrawn(ApprovalId(9))]);
        assert!(!h.frame().contains("Do you want to proceed?"));
        assert!(h.calls().iter().all(|c| !matches!(c, Call::Answer(..))), "nothing was answered");
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
        h.arm();
        h.key(DOWN);
        h.key(ENTER); // Yes, and don't ask again for file changes
        assert_eq!(&h.calls()[1..], [Call::Answer(3, Decision::Yes), Call::Answer(1, Decision::Always)]);
        assert!(h.frame().contains("Do you want to proceed?")); // the other scope still asks
        assert!(!h.frame().contains("more waiting"));
    }

    #[test]
    fn always_leaves_a_queued_once_request_in_the_same_scope_to_ask() {
        let mut h = harness();
        h.typed("make files");
        let once = ApprovalRequest { once: true, ..write_request("two.txt", None) };
        h.events([
            UiEvent::Approval { id: ApprovalId(1), request: write_request("one.txt", None) },
            UiEvent::Approval { id: ApprovalId(2), request: once },
        ]);
        h.arm();
        h.key(DOWN);
        h.key(ENTER); // Yes, and don't ask again for file changes
        assert_eq!(&h.calls()[1..], [Call::Answer(1, Decision::Always)]);
        assert!(h.frame().contains("Do you want to proceed?")); // the once request still asks
    }

    /// A protected path (or egress after untrusted content) queued behind an ordinary file change: "Always" on the
    /// head doesn't answer it, so its warning still reaches the person. Proven: dropped `!p.request.must_ask` from
    /// `decide` → failed (request 2 answered Yes unseen).
    #[test]
    fn always_leaves_a_queued_must_ask_request_in_the_same_scope_to_ask() {
        let mut h = harness();
        h.typed("make files");
        let protected = ApprovalRequest { must_ask: true, ..write_request(".envrc", Some("general-purpose · two")) };
        h.events([
            UiEvent::Approval { id: ApprovalId(1), request: write_request("one.txt", Some("general-purpose · one")) },
            UiEvent::Approval { id: ApprovalId(2), request: protected },
        ]);
        h.arm();
        h.key(DOWN);
        h.key(ENTER); // Yes, and don't ask again for file changes
        assert_eq!(&h.calls()[1..], [Call::Answer(1, Decision::Always)]);
        assert!(h.frame().contains("Do you want to proceed?")); // the protected request still asks
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
    fn a_message_while_a_turn_runs_is_queued_and_while_other_work_runs_stays_in_the_prompt() {
        let mut h = harness();
        h.typed("first");
        h.typed("second");
        assert_eq!(h.calls(), [Call::Send("first".into()), Call::Queue("second".into())]);
        assert_eq!(h.app.prompt.value, "");
        h.events([UiEvent::Done(StopReason::End), turn_end()]);
        // Work outside a turn (`/mcp add`'s lookup): nothing to queue behind, so the text waits in the prompt.
        h.event(UiEvent::Busy(Some("Looking it up…".into())));
        h.typed("third");
        assert_eq!(h.calls().len(), 2);
        assert_eq!(h.app.prompt.value, "third");
    }

    #[test]
    fn page_up_scrolls_back_and_a_submit_follows_the_bottom_again() {
        let mut h = harness();
        h.height = 20;
        for i in 0..30 {
            h.event(UiEvent::Message(system_message(format!("note {i}"), false)));
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
            h.event(UiEvent::Message(system_message(format!("note {i}"), false)));
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
        h.arm();
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
        let mut agent = message(Role::Tool, "agent".into());
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
            plan: None,
        }));
        assert!(h.frame().contains("(it used other; continuing with ollama/qwen3.5:9b)"));
        h.click_on("agent old · run");
        assert!(h.frame().contains("Only this session's subagents can be opened"));
        // New messages never reuse a restored id.
        assert!(h.app.next_id > 3);
    }

    // --- pickers, setup, events from outside a turn ---

    /// #124: the radio's strip sits above the prompt while it's on; `/radio`'s view opens over the prompt and Esc
    /// closes it without stopping anything.
    #[test]
    fn the_radio_strip_shows_while_on_and_its_view_closes_with_esc() {
        use crate::view::{RadioPeer, RadioState};
        let mut h = harness();
        h.event(UiEvent::OpenRadio);
        assert!(!h.frame().contains("◉ radio"), "off: no strip, no view");
        let peer = RadioPeer {
            name: "proj-58a7".into(),
            state: RadioState::Working,
            title: "fix the login test".into(),
            project: "proj".into(),
            pane: None,
            since_ms: 0,
        };
        h.event(UiEvent::Radio(RadioView {
            on: true,
            me: "proj-c792".into(),
            my_title: String::new(),
            peers: vec![peer],
            traffic: Vec::new(),
        }));
        assert!(h.frame().contains("◉ radio · 58a7 working \"fix the login test\""), "{}", h.frame());
        h.event(UiEvent::OpenRadio);
        assert!(h.frame().contains("ON THE RADIO") && h.frame().contains("proj-58a7"), "{}", h.frame());
        h.key(KeyCode::Char('x'));
        assert!(h.frame().contains("ON THE RADIO"), "the view keeps the keyboard");
        h.key(KeyCode::Esc);
        assert!(!h.frame().contains("ON THE RADIO"), "{}", h.frame());
        assert!(h.calls().is_empty(), "Esc closed the view and nothing else: {:?}", h.calls());
        h.event(UiEvent::Radio(RadioView::default()));
        assert!(!h.frame().contains("◉ radio"));
    }

    /// #124: a message received flashes on the strip for 10 s, once; the roster changing doesn't flash it again.
    #[test]
    fn a_radio_message_flashes_on_the_strip_for_a_while() {
        use crate::view::{RadioPeer, RadioState, RadioTraffic};
        let mut h = harness();
        let peer = RadioPeer {
            name: "proj-58a7".into(),
            state: RadioState::Idle,
            title: String::new(),
            project: "proj".into(),
            pane: None,
            since_ms: 0,
        };
        let said = RadioTraffic {
            at_ms: 0,
            id: "proj-58a7:3".into(),
            outgoing: false,
            peers: vec!["proj-58a7".into()],
            by: "model".into(),
            kind: "msg".into(),
            request: None,
            text: "tests pass on main now".into(),
            refused: None,
        };
        let view = |traffic: Vec<RadioTraffic>| RadioView {
            on: true,
            me: "proj-c792".into(),
            my_title: String::new(),
            peers: vec![peer.clone()],
            traffic,
        };
        h.event(UiEvent::Radio(view(vec![said.clone()])));
        assert!(h.frame().contains("◉ radio · ← 58a7: tests pass on main now"), "{}", h.frame());
        h.advance(radio_view::FLASH_MS + 10);
        assert!(h.frame().contains("◉ radio · 58a7 idle"), "{}", h.frame());
        h.event(UiEvent::Radio(view(vec![said])));
        assert!(!h.frame().contains("← 58a7"), "seen already: {}", h.frame());
    }

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
            sign_in: false,
        }];
        let context = crate::view::SetupContext {
            start: SetupStart::FirstRun,
            presets,
            current: None,
            saved_keys: vec![],
            accounts: vec![],
            env_keys: vec![],
        };
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
            sign_in: false,
        }];
        let current = Some((ProviderId::Ollama, "tiny".into()));
        h.event(UiEvent::OpenSetup(crate::view::SetupContext {
            start: SetupStart::Model,
            presets,
            current,
            saved_keys: vec![],
            accounts: vec![],
            env_keys: vec![],
        }));
        assert_eq!(h.calls(), [Call::LoadModels(ProviderId::Ollama)]);
        h.key(ESC);
        assert!(!h.app.exiting());
        assert!(h.frame().contains("Type a message"));
    }

    // Marv tests/app.test.tsx "MCP logins (#18)": the code in a pasted callback never reaches the model, the
    // transcript or the prompt history, even during a turn.
    #[test]
    fn a_pasted_login_callback_goes_to_the_engine_even_while_busy_and_is_never_kept() {
        let mut h = harness();
        h.typed("hello");
        assert_eq!(h.calls(), [Call::Send("hello".into())]);
        let pasted = "http://127.0.0.1:5555/callback?code=code-secret&state=abc";
        h.typed(pasted);
        assert_eq!(h.calls()[1], Call::LoginCallback(pasted.into()));
        assert_eq!(h.calls().len(), 2);
        assert!(!h.frame().contains("code-secret"));
        h.key(KeyCode::Up);
        assert!(!h.frame().contains("code-secret"), "not in the prompt history");
        // #214 review: one long enough to be pasted as a chip goes to the engine too, and its chip isn't kept.
        let long = format!("http://127.0.0.1:5555/callback?code=code-long&state={}", "s".repeat(1000));
        h.app.prompt.set(""); // ↑ above brought back "hello"
        h.app.on_event(Event::Paste(long.clone()), h.now);
        assert!(h.frame().contains("[Pasted text #1"), "{}", h.frame());
        h.key(ENTER);
        assert_eq!(h.calls().last(), Some(&Call::LoginCallback(long)));
        assert!(!h.app.prompt.holds_submission(), "the callback's chip isn't held for the history");
        h.key(KeyCode::Up);
        assert!(!h.app.prompt.text().contains("code-long"), "not in the prompt history");
    }

    // #214: a multi-line paste never sends; Enter sends all of it, the transcript shows its lines, ↑ brings it back.
    #[test]
    fn a_multi_line_paste_waits_for_enter_and_the_transcript_shows_every_line() {
        let mut h = harness();
        h.app.on_event(Event::Paste("cargo build\ncargo test\n".into()), h.now);
        assert!(h.calls().is_empty(), "a paste never submits");
        assert!(h.frame().contains("[Pasted text #1, 2 lines]"), "{}", h.frame());
        h.key(ENTER);
        assert_eq!(h.calls(), [Call::Send("cargo build\ncargo test".into())]);
        let frame = h.frame();
        assert!(frame.contains("> cargo build\n  cargo test"), "{frame}");
        h.events([UiEvent::Done(StopReason::End), turn_end()]);
        h.key(KeyCode::Up);
        assert_eq!(h.app.prompt.text(), "cargo build\ncargo test\n");
        assert!(h.frame().contains("[Pasted text #1, 2 lines]"));
    }

    #[test]
    fn a_multi_line_paste_into_a_reason_or_the_palette_stays_one_line() {
        let mut h = harness();
        h.typed("make a file");
        h.events([UiEvent::Approval { id: ApprovalId(1), request: write_request("made.txt", None) }]);
        h.arm();
        h.key(KeyCode::Tab);
        h.app.on_event(Event::Paste("too\nbig".into()), h.now);
        h.key(ENTER);
        assert_eq!(h.calls().last(), Some(&Call::Answer(1, Decision::Declined("too big".into()))));
        assert_eq!(h.app.prompt.text(), "");
        h.events([UiEvent::Done(StopReason::End), turn_end()]);
        h.ctrl('k');
        h.app.on_event(Event::Paste("yo\nlo".into()), h.now);
        assert_eq!(h.app.palette.as_ref().unwrap().query, "yo lo");
        assert_eq!(h.app.prompt.text(), "");
    }

    #[test]
    fn clear_empties_the_transcript_and_busy_shows_the_waiting_line() {
        let mut h = harness();
        h.event(UiEvent::Message(system_message("Hello there".into(), false)));
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
        h.event(UiEvent::Message(system_message("evil\x1b[2J\u{202e}text".into(), true)));
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
            h.event(UiEvent::Message(system_message(format!("note {i}"), false)));
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
            h.event(UiEvent::Message(system_message(format!("note {i}"), false)));
        }
        h.typed("go");
        h.frame();
        let at = h.app.deadline().expect("spinner");
        assert!(at <= h.now + SPINNER_MS);
        h.events([UiEvent::TextDelta("streaming".into())]);
        h.frame();
        assert_eq!(h.app.deadline(), None, "no waiting line once text streams");
    }

    // --- the plan (#15) ---

    fn plan(statuses: &[crate::view::PlanStatus]) -> Vec<PlanItem> {
        let texts = ["Read the report command", "Add the --json flag", "Update README"];
        statuses
            .iter()
            .enumerate()
            .map(|(i, s)| PlanItem {
                text: texts.get(i).map_or_else(|| format!("step {}", i + 1), |t| t.to_string()),
                status: *s,
            })
            .collect()
    }
    /// The rows between the transcript and the prompt: the panel sits right above the prompt's box.
    fn above_prompt(frame: &str) -> String {
        let lines: Vec<&str> = frame.lines().collect();
        let prompt = lines.iter().rposition(|l| l.contains("Type a message")).expect("the prompt is on screen");
        // The prompt's top border is the line above its text; the panel is at most PLAN_ROWS rows.
        lines[prompt.saturating_sub(1 + plan_view::PLAN_ROWS)..prompt - 1].join("\n")
    }

    #[test]
    fn a_plan_call_pins_the_panel_above_the_prompt_and_an_all_done_one_hides_it() {
        use crate::view::PlanStatus::{Done, InProgress, Pending};
        let mut h = harness();
        h.events([
            tool_start("p1", "plan", "Plan"),
            tool_end("p1", "plan", "Plan", "1/3 done"),
            UiEvent::Plan(plan(&[Done, InProgress, Pending])),
            UiEvent::Assistant { text: "Working on it.".into() },
            turn_end(),
        ]);
        let frame = h.frame();
        assert!(frame.contains("1/3 done"));
        let above = above_prompt(&frame);
        assert!(above.contains(" Plan 1/3 ─"), "{frame}");
        assert!(above.contains("▸ Add the --json flag"), "{frame}");
        h.events([
            tool_start("p2", "plan", "Plan"),
            tool_end("p2", "plan", "Plan", "3/3 done"),
            UiEvent::Plan(plan(&[Done, Done, Done])),
            turn_end(),
        ]);
        let frame = h.frame();
        assert!(frame.contains("3/3 done"));
        assert!(!frame.contains(" Plan 3/3 ─"));
        assert!(!frame.contains("▸ Add the --json flag"));
    }

    #[test]
    fn an_empty_plan_or_clear_hides_it() {
        use crate::view::PlanStatus::{Done, InProgress, Pending};
        let mut h = harness();
        h.event(UiEvent::Plan(plan(&[Done, InProgress, Pending])));
        assert!(h.frame().contains(" Plan 1/3 ─"));
        h.event(UiEvent::Plan(vec![]));
        assert!(!h.frame().contains(" Plan "));
        h.event(UiEvent::Plan(plan(&[Done, InProgress, Pending])));
        h.event(UiEvent::Clear);
        assert!(!h.frame().contains(" Plan 1/3 ─"));
    }

    #[test]
    fn not_on_a_window_under_20_rows() {
        use crate::view::PlanStatus::{Done, InProgress, Pending};
        let mut h = harness();
        h.height = 19;
        h.event(UiEvent::Plan(plan(&[Done, InProgress, Pending])));
        assert!(!h.frame().contains(" Plan 1/3 ─"));
        h.height = 20;
        assert!(h.frame().contains(" Plan 1/3 ─"));
    }

    #[test]
    fn the_collapsed_rows_with_12_items() {
        use crate::view::PlanStatus::{Done, InProgress, Pending};
        let mut h = harness();
        let mut statuses = vec![Done; 4];
        statuses.push(InProgress);
        statuses.extend([Pending; 7]);
        h.event(UiEvent::Plan(plan(&statuses)));
        let above = above_prompt(&h.frame());
        assert!(above.contains("▸ step 5"));
        assert!(above.ends_with("  +4 more"), "{above}");
    }

    #[test]
    fn a_resumed_session_shows_its_saved_plan() {
        use crate::view::PlanStatus::{Done, InProgress, Pending};
        let mut h = harness();
        h.event(UiEvent::Resumed(Resumed {
            id: "s".into(),
            updated_at: 0,
            model: "ollama/qwen3.5:9b".into(),
            transcript: vec![],
            totals: Totals::default(),
            plan: Some(plan(&[Done, InProgress, Pending])),
        }));
        let frame = h.frame();
        assert!(frame.contains("Resumed a session"));
        assert!(frame.contains(" Plan 1/3 ─"));
        assert!(frame.contains("▸ Add the --json flag"));
    }

    // --- the ctrl+k palette (#44) ---

    /// Types without submitting.
    fn type_only(h: &mut Harness, text: &str) {
        for c in text.chars() {
            h.key(KeyCode::Char(c));
        }
    }

    #[test]
    fn ctrl_k_opens_the_palette_over_half_typed_text_and_esc_brings_the_text_back() {
        let mut h = harness();
        type_only(&mut h, "fix the bu");
        h.ctrl('k');
        let frame = h.frame();
        assert!(frame.contains("Search commands, skills, settings and keys"), "{frame}");
        assert!(frame.contains("Yolo: on"), "{frame}");
        assert!(frame.contains("ctrl+o"), "{frame}");
        assert!(!frame.contains("fix the bu"), "{frame}");
        // Typing goes to the search, not the prompt.
        type_only(&mut h, "help");
        assert!(h.frame().contains("❯ help"));
        h.key(ESC);
        let frame = h.frame();
        assert!(frame.contains("> fix the bu"), "{frame}");
        assert!(!frame.contains("Search commands"), "{frame}");
        assert_eq!(h.app.prompt.value, "fix the bu");
        assert!(h.calls().is_empty(), "{:?}", h.calls());
        // ctrl+k from an empty prompt too, and ctrl+k again closes it.
        let mut h = harness();
        h.ctrl('k');
        assert!(h.frame().contains("Search commands"));
        h.ctrl('k');
        assert!(!h.frame().contains("Search commands"));
    }

    #[test]
    fn the_palette_filters_and_enter_on_a_toggle_runs_its_command_and_keeps_the_typed_text() {
        let mut h = harness();
        type_only(&mut h, "half");
        h.ctrl('k');
        type_only(&mut h, "sandbox confines");
        let frame = h.frame();
        assert!(frame.contains("❯ Yolo: on"), "{frame}");
        assert!(!frame.contains("Show available commands"), "{frame}");
        h.key(ENTER);
        assert_eq!(h.calls(), [Call::Command("/yolo off".into())]);
        assert_eq!(h.app.prompt.value, "half");
        assert!(!h.frame().contains("Search commands"));
        // Not in the prompt's history: the user didn't type it.
        assert!(h.app.prompt.history.is_empty());
    }

    #[test]
    fn a_value_setting_prefills_its_command_and_the_typed_text_stays_in_the_history() {
        let mut h = harness();
        type_only(&mut h, "draft message");
        h.ctrl('k');
        type_only(&mut h, "step");
        h.key(ENTER);
        assert_eq!(h.app.prompt.value, "/steps ");
        assert!(h.calls().is_empty());
        h.key(KeyCode::Up);
        assert_eq!(h.app.prompt.value, "draft message");
    }

    // #113 review: a drafted login callback holds an authorization code, so prefilling over it never keeps it.
    #[test]
    fn a_prefill_over_a_drafted_login_callback_keeps_it_out_of_the_history() {
        let mut h = harness();
        h.app.on_event(Event::Paste("http://127.0.0.1:5555/callback?code=code-secret&state=abc".into()), h.now);
        h.ctrl('k');
        type_only(&mut h, "step");
        h.key(ENTER);
        assert_eq!(h.app.prompt.value, "/steps ");
        h.key(KeyCode::Up);
        assert!(!h.app.prompt.value.contains("code-secret"), "{}", h.app.prompt.value);
        assert!(!h.frame().contains("code-secret"));
    }

    #[test]
    fn while_a_reply_streams_the_palette_runs_what_can_and_esc_only_closes_it() {
        let mut h = harness();
        h.typed("hello");
        h.events([UiEvent::TextDelta("Working on".into())]);
        assert!(h.app.busy());
        h.ctrl('k');
        type_only(&mut h, "undo");
        assert!(h.frame().contains("(after the reply)"));
        h.key(ENTER);
        assert!(h.frame().contains("/undo can run after the reply"));
        // Esc closes the palette; the reply carries on.
        h.key(ESC);
        assert!(!h.calls().contains(&Call::Abort));
        assert!(h.app.busy());
        // A setting that applies from the next turn runs now.
        h.ctrl('k');
        type_only(&mut h, "yolo");
        h.key(ENTER);
        assert_eq!(h.calls().last(), Some(&Call::Command("/yolo off".into())));
        // The prompt's `/` menu stays hidden while busy, as before.
        type_only(&mut h, "/");
        assert!(!h.frame().contains("Show available commands"));
    }

    #[test]
    fn ctrl_k_never_opens_over_an_approval_and_an_approval_closes_the_palette() {
        let mut h = harness();
        h.typed("make a file");
        h.events([UiEvent::Approval { id: ApprovalId(1), request: write_request("made.txt", None) }]);
        h.ctrl('k');
        let frame = h.frame();
        assert!(!frame.contains("Search commands"), "{frame}");
        assert!(frame.contains("Create made.txt"), "{frame}");
        assert!(!h.calls().iter().any(|c| matches!(c, Call::Answer(..))));
        h.arm();
        h.key(ENTER);
        assert!(h.calls().contains(&Call::Answer(1, Decision::Yes)));

        // Open, then an approval arrives: it takes over. An Enter meant for the palette, pressed just as it appears,
        // answers nothing (#113) and runs nothing; one after the arm delay answers it.
        let mut h = harness();
        h.typed("make a file");
        h.ctrl('k');
        type_only(&mut h, "yolo");
        h.events([UiEvent::Approval { id: ApprovalId(2), request: write_request("other.txt", None) }]);
        let frame = h.frame();
        assert!(!frame.contains("Search commands"), "{frame}");
        assert!(frame.contains("Create other.txt"), "{frame}");
        h.key(ENTER);
        assert!(!h.calls().iter().any(|c| matches!(c, Call::Answer(..))), "{:?}", h.calls());
        assert!(!h.calls().contains(&Call::Command("/yolo off".into())));
        assert!(h.frame().contains("Create other.txt"));
        h.arm();
        h.key(ENTER);
        assert!(h.calls().contains(&Call::Answer(2, Decision::Yes)));
        assert!(!h.calls().contains(&Call::Command("/yolo off".into())));
    }

    #[test]
    fn an_approval_ignores_answering_keys_typed_for_the_prompt_as_it_appears() {
        let mut h = harness();
        h.typed("make a file");
        type_only(&mut h, "next messag");
        h.events([UiEvent::Approval { id: ApprovalId(1), request: write_request("made.txt", None) }]);
        // Still typing: the rest of the word, then Enter. None of it answers, moves the choice or reaches the prompt.
        type_only(&mut h, "e");
        h.key(DOWN);
        h.key(ENTER);
        assert!(!h.calls().iter().any(|c| matches!(c, Call::Answer(..))), "{:?}", h.calls());
        let frame = h.frame();
        assert!(frame.contains("❯ Yes"), "{frame}");
        assert_eq!(h.app.prompt.value, "next messag");
        // Just before the window ends, still nothing; at its end the Enter answers as before.
        h.advance(APPROVAL_ARM_MS.as_millis() as u64 - FRAME_INTERVAL.as_millis() as u64 - 2);
        h.key(ENTER);
        assert!(!h.calls().iter().any(|c| matches!(c, Call::Answer(..))));
        h.arm();
        h.key(ENTER);
        assert_eq!(h.calls().last(), Some(&Call::Answer(1, Decision::Yes)));
    }

    #[test]
    fn esc_still_declines_an_approval_before_it_is_armed() {
        let mut h = harness();
        h.typed("make a file");
        h.events([UiEvent::Approval { id: ApprovalId(1), request: write_request("made.txt", None) }]);
        h.key(ESC);
        assert_eq!(&h.calls()[1..], [Call::Answer(1, Decision::No), Call::Abort]);
    }

    #[test]
    fn the_session_picker_takes_over_from_a_palette_opened_while_its_list_loaded() {
        let mut h = harness();
        h.typed("/resume");
        h.ctrl('k');
        assert!(h.frame().contains("Search commands"));
        let summary = |id: &str, title: &str| SessionSummary {
            id: id.into(),
            title: title.into(),
            updated_at: 0,
            messages: 2,
            model: "m".into(),
        };
        h.event(UiEvent::Sessions(vec![summary("s1", "First"), summary("s2", "Second")]));
        let frame = h.frame();
        assert!(frame.contains("Resume a session"), "{frame}");
        assert!(!frame.contains("Search commands"), "{frame}");
        h.key(DOWN);
        h.key(ENTER);
        assert_eq!(h.calls().last(), Some(&Call::Resume(Some("s2".into()))));
        assert!(h.app.palette.is_none());
    }

    #[test]
    fn setup_takes_over_from_an_open_palette() {
        let mut h = harness();
        h.ctrl('k');
        h.event(UiEvent::OpenSetup(crate::view::SetupContext {
            start: SetupStart::Model,
            presets: vec![],
            current: None,
            saved_keys: vec![],
            accounts: vec![],
            env_keys: vec![],
        }));
        assert!(h.app.palette.is_none());
        assert!(!h.frame().contains("Search commands"));
    }

    /// A pane send is a prompt, not a command: while the palette, setup, the session picker, the radio's roster or a
    /// half-typed slash command has the keyboard, the port hears it's held (Herdr isn't told idle), and again when
    /// the prompt is free.
    #[test]
    fn the_port_hears_when_something_but_an_empty_prompt_has_the_keyboard() {
        let mut h = harness_with(1000);
        let held = |h: &Harness| h.port.held.lock().unwrap().clone();
        h.advance(1);
        assert_eq!(held(&h), [true], "the splash takes the first key");
        h.advance(2000);
        assert_eq!(held(&h), [true, false]);
        h.ctrl('k');
        h.advance(1);
        assert_eq!(held(&h).last(), Some(&true), "the palette");
        h.key(ESC);
        h.advance(1);
        assert_eq!(held(&h).last(), Some(&false));
        h.key(KeyCode::Char(' '));
        h.key(KeyCode::Char('/'));
        h.advance(1);
        assert_eq!(held(&h).last(), Some(&true), "a slash command being typed");
        h.key(KeyCode::Backspace);
        h.advance(1);
        assert_eq!(held(&h).last(), Some(&false));
        h.key(KeyCode::Char('x'));
        h.advance(1);
        assert_eq!(held(&h).last(), Some(&false), "a message being typed");
        let presets = vec![crate::view::ProviderPreset {
            id: ProviderId::Ollama,
            label: "Ollama".into(),
            key_env: None,
            key_url: None,
            default_model: String::new(),
            sign_in: false,
        }];
        let context = crate::view::SetupContext {
            start: SetupStart::FirstRun,
            presets,
            current: None,
            saved_keys: vec![],
            accounts: vec![],
            env_keys: vec![],
        };
        h.events([UiEvent::OpenSetup(context)]);
        assert_eq!(held(&h).last(), Some(&true), "setup");
        let told = held(&h).len();
        h.advance(1);
        assert_eq!(held(&h).len(), told, "told only when it changes");
    }

    #[test]
    fn a_paste_goes_to_the_palette_search_while_it_is_open() {
        let mut h = harness();
        type_only(&mut h, "kept");
        h.ctrl('k');
        h.app.on_event(Event::Paste("yolo".into()), h.now);
        assert!(h.frame().contains("❯ Yolo: on"));
        h.key(ESC);
        assert_eq!(h.app.prompt.value, "kept");
    }
}
