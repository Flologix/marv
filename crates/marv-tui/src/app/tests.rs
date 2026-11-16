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
    LoadProcesses(Option<String>),
    StopProcess(String),
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
    fn load_processes(&self, output_of: Option<String>) {
        self.calls.lock().unwrap().push(Call::LoadProcesses(output_of));
    }
    fn stop_process(&self, name: String) {
        self.calls.lock().unwrap().push(Call::StopProcess(name));
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
    h.events([UiEvent::Queued(vec!["one".into(), "two".into(), "three".into()]), UiEvent::Done(StopReason::Aborted), turn_end()]);
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
        let (y, line) = frame.lines().enumerate().find(|(_, l)| l.contains(text)).unwrap_or_else(|| panic!("{text}:\n{frame}"));
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

/// Types into the prompt without sending.
fn type_in(h: &mut Harness, text: &str) {
    for c in text.chars() {
        h.key(KeyCode::Char(c));
    }
}

/// A drag in the prompt's box selects the prompt's text (the transcript doesn't see it), and the release copies
/// it; a click there only moves the cursor.
#[test]
fn dragging_over_the_prompt_selects_it_and_copies_it() {
    let mut h = harness();
    type_in(&mut h, "made up");
    let frame = h.frame();
    let (y, line) = frame.lines().enumerate().find(|(_, l)| l.contains("> made up")).expect("the prompt's line");
    let x = unicode_width::UnicodeWidthStr::width(&line[..line.find("made").unwrap()]) as u16;
    let y = y as u16;
    h.app.on_mouse(MouseEvent::Press { x, y }, h.now);
    h.app.on_mouse(MouseEvent::Drag { x: x + 4, y }, h.now);
    assert_eq!(h.app.selection.selection(), None, "not the transcript's");
    let buf = h.buffer();
    assert_eq!(buf[(x + 1, y)].bg, crate::ui::theme::DIM, "drawn selected");
    h.app.on_mouse(MouseEvent::Release { x: x + 4, y }, h.now);
    futures::executor::block_on(h.app.settle_copies(h.now));
    assert_eq!(*h.copied.lock().unwrap(), ["made"]);
    assert!(h.frame().contains("Copied 4 chars"));
    // Typing replaces it; a click puts the cursor where it lands.
    type_in(&mut h, "fixed");
    assert_eq!(h.app.prompt.value, "fixed up");
    h.frame();
    h.app.on_mouse(MouseEvent::Press { x, y }, h.now);
    h.app.on_mouse(MouseEvent::Release { x, y }, h.now);
    type_in(&mut h, "_");
    assert_eq!(h.app.prompt.value, "_fixed up");
    assert_eq!(h.copied.lock().unwrap().len(), 1, "a click copies nothing");
}

/// ctrl+c copies a selection in the prompt (and drops it) before it does anything else; ctrl+x then cuts rather
/// than dropping a queued message.
#[test]
fn ctrl_c_copies_a_prompt_selection_and_ctrl_x_cuts_it_even_with_a_queue() {
    let mut h = harness();
    type_in(&mut h, "keep this");
    h.app.on_key(KeyEvent::new(KeyCode::Left, KeyModifiers::CONTROL | KeyModifiers::SHIFT), h.now);
    h.ctrl('c');
    assert_eq!(h.app.prompt.value, "keep this", "copied, not cleared");
    assert!(h.app.confirm_exit.is_none());
    h.ctrl('c');
    assert_eq!(h.app.prompt.value, "", "with nothing selected, the ladder as before");
    h.typed("go");
    h.events([UiEvent::TurnStart { turn: "t".into() }]);
    enter(&mut h, "queued one");
    type_in(&mut h, "cut me");
    h.app.on_key(KeyEvent::new(KeyCode::Home, KeyModifiers::SHIFT), h.now);
    h.ctrl('x');
    assert_eq!(h.app.queued, ["queued one"], "the queue is untouched");
    assert_eq!(h.app.prompt.value, "");
    h.ctrl('c');
    assert!(h.calls().contains(&Call::Abort), "ctrl+c still stops the turn once nothing is selected");
    futures::executor::block_on(h.app.settle_copies(h.now));
    assert_eq!(*h.copied.lock().unwrap(), ["this", "cut me"]);
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
    h.events([UiEvent::Retry { attempt: 1, max: 3, message: "Connection lost; asking again (1/3)…".into(), continued: true }]);
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

fn processes(names: &[&str]) -> UiEvent {
    let rows = names
        .iter()
        .map(|n| crate::view::ProcessRow {
            name: n.to_string(),
            command: format!("serve {n}"),
            status: "running · http://localhost:4173 · up 2m".into(),
            running: true,
        })
        .collect();
    let output = names.first().map(|n| (n.to_string(), "Serving on http://localhost:4173\nGET / 200".to_string()));
    UiEvent::Processes(crate::view::ProcessesView { rows, output })
}

#[test]
fn ctrl_p_opens_the_processes_view_which_refreshes_and_stops_one() {
    let mut h = harness();
    h.event(UiEvent::StatusInfo(StatusInfo { processes: 1, ..options(0).status }));
    assert!(h.frame().contains("1 running ctrl+p"));
    h.ctrl('p');
    assert_eq!(h.calls().last(), Some(&Call::LoadProcesses(None)));
    h.event(processes(&["serve-site"]));
    let frame = h.frame();
    assert!(frame.contains("› serve-site  running · http://localhost:4173 · up 2m"), "{frame}");
    assert!(frame.contains("$ serve serve-site") && frame.contains("GET / 200"), "{frame}");
    // Every second while it's open: the list again, with the selected one's output.
    h.advance(1_000);
    assert_eq!(h.calls().last(), Some(&Call::LoadProcesses(Some("serve-site".into()))));
    // Typing doesn't reach the prompt; s twice stops the selected one.
    h.key(KeyCode::Char('s'));
    assert!(h.frame().contains("Press s again to stop serve-site"));
    h.key(KeyCode::Char('s'));
    assert_eq!(h.calls().last(), Some(&Call::StopProcess("serve-site".into())));
    assert!(!h.frame().contains("> s"));
    h.key(KeyCode::Esc);
    assert!(!h.frame().contains("Background processes"));
    let before = h.calls().len();
    h.advance(5_000);
    assert_eq!(h.calls().len(), before, "closed: no more refreshes");
}

#[test]
fn a_click_on_n_running_opens_the_processes_view() {
    let mut h = harness();
    h.event(UiEvent::StatusInfo(StatusInfo { processes: 2, ..options(0).status }));
    h.click_on("2 running");
    assert_eq!(h.calls().last(), Some(&Call::LoadProcesses(None)));
    h.event(processes(&["web", "worker"]));
    assert!(h.frame().contains("Background processes"));
    h.key(KeyCode::Down);
    assert_eq!(h.calls().last(), Some(&Call::LoadProcesses(Some("worker".into()))));
    h.ctrl('p');
    assert!(!h.frame().contains("Background processes"), "ctrl+p again closes it");
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
        .map(|(i, s)| PlanItem { text: texts.get(i).map_or_else(|| format!("step {}", i + 1), |t| t.to_string()), status: *s })
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
fn a_paste_goes_to_the_setup_key_field_not_the_prompt() {
    let mut h = harness();
    let presets = vec![crate::view::ProviderPreset {
        id: ProviderId::OpenRouter,
        label: "OpenRouter".into(),
        key_env: Some("OPENROUTER_API_KEY".into()),
        key_url: None,
        default_model: "z-ai/glm-5.3".into(),
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
    h.event(UiEvent::OpenSetup(context));
    h.key(ENTER);
    h.event(UiEvent::Models(Ok(vec![crate::view::ModelInfo { id: "z-ai/glm-5.3".into(), ..Default::default() }])));
    h.key(ENTER);
    h.app.on_event(Event::Paste("sk-or-v1-pasted\n".into()), h.now);
    h.key(ENTER);
    assert_eq!(
        h.calls().last(),
        Some(&Call::CompleteSetup(SetupAnswer {
            provider: ProviderId::OpenRouter,
            model: "z-ai/glm-5.3".into(),
            api_key: Some("sk-or-v1-pasted".into())
        }))
    );
    assert_eq!(h.app.prompt.value, "");
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
