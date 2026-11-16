use super::*;
use marv_core::{ChatTurn, ProviderEvent, ToolCall, ToolResult};
use marv_mesh::herdr::{HerdrSend, HerdrTarget};
use marv_provider::{EventStream, Provider, StreamOptions};
use serde_json::Value;
use std::collections::VecDeque;
use std::sync::atomic::AtomicUsize;

// #98: the TUI's --approve merges with the config's "approve" list; an unknown name is a problem, and the
// valid ones still stand.
#[test]
fn the_tui_merges_config_and_cli_approve_scopes() {
    let (scopes, problems) = merge_pre_approved(vec!["web".into()], &["network".to_string(), "web".to_string()]);
    assert_eq!(scopes, ["web", "network"], "deduped, cli after config");
    assert!(problems.is_empty());
    let (scopes, problems) = merge_pre_approved(vec!["web".into()], &["netwrk".to_string()]);
    assert_eq!(scopes, ["web"], "the typo doesn't reach the session");
    assert!(problems[0].contains("Unknown --approve scope"), "{:?}", problems); // #82 review: the config's own names are checked the same way.
    let (scopes, problems) = merge_pre_approved(vec!["web".into(), "netwrk".into()], &[]);
    assert_eq!(scopes, ["web"]);
    assert!(problems[0].starts_with("\"approve\" in your config: Unknown --approve scope \"netwrk\""), "{problems:?}");
}
use tokio::sync::mpsc::UnboundedReceiver;

/// Replays one scripted reply per request (Marv's `tests/fake-provider.ts`).
struct Scripted(Mutex<VecDeque<Vec<ProviderEvent>>>);
impl Provider for Scripted {
    fn name(&self) -> &str {
        "scripted"
    }
    fn context_length(&self) -> Option<u64> {
        None
    }
    fn stream(&self, _history: &[ChatTurn], _options: StreamOptions) -> EventStream {
        let reply = self.0.lock().unwrap().pop_front().unwrap_or_else(|| vec![ProviderEvent::Done { reason: None }]);
        Box::pin(futures::stream::iter(reply))
    }
}

fn say(text: &str) -> Vec<ProviderEvent> {
    vec![ProviderEvent::TextDelta(text.into()), ProviderEvent::Done { reason: Some("stop".into()) }]
}
fn write(id: &str, path: &str, content: &str) -> Vec<ProviderEvent> {
    let arguments = serde_json::json!({ "path": path, "content": content }).to_string();
    vec![
        ProviderEvent::ToolCall(ToolCall { id: id.into(), name: "write_file".into(), arguments }),
        ProviderEvent::Done { reason: Some("tool_calls".into()) },
    ]
}

/// Records every Herdr request and answers at once.
#[derive(Default)]
struct Recorder(Mutex<Vec<Value>>);
#[async_trait]
impl HerdrSend for Recorder {
    async fn send(&self, request: &Value, _timeout: Duration) -> bool {
        self.0.lock().unwrap().push(request.clone());
        true
    }
}
impl Recorder {
    fn states(&self) -> Vec<String> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r["method"] == "pane.report_agent")
            .map(|r| r["params"]["state"].as_str().unwrap_or_default().to_string())
            .collect()
    }
    fn find(&self, f: impl Fn(&Value) -> bool) -> Option<Value> {
        self.0.lock().unwrap().iter().find(|r| f(r)).cloned()
    }
}

/// A temp project with notes.txt and a config folder; removed when the test ends.
struct Dirs {
    base: PathBuf,
    project: PathBuf,
    config: PathBuf,
}
impl Dirs {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, Ordering::SeqCst);
        let base = std::env::temp_dir().join(format!("marv-cli-tui-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let (project, config) = (base.join("home/project"), base.join("config"));
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&config).unwrap();
        std::fs::write(project.join("notes.txt"), "hi\n").unwrap();
        let project = std::fs::canonicalize(project).unwrap();
        Self { base, project, config }
    }
}
impl Drop for Dirs {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.base);
        // And again when the test's thread ends: the session's background work (the checkpoint store's prune and
        // its `git gc`) can still be running when the test's body ends, and recreated the folder after it was
        // removed (#89). The thread ends after its runtime has shut down and killed that work.
        AT_TEST_END.with(|later| later.borrow_mut().push(RemoveOnDrop(self.base.clone())));
    }
}

struct RemoveOnDrop(PathBuf);
impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

thread_local! {
    // Dropped when the test's thread ends (libtest gives each test its own thread), passed or panicked.
    static AT_TEST_END: std::cell::RefCell<Vec<RemoveOnDrop>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Ollama with yolo off: every change asks.
fn asks() -> FileConfig {
    FileConfig {
        provider: Some(ProviderKind::Ollama),
        model: Some("qwen3.5:9b".into()),
        yolo: Some(false),
        diagnostics: Some(DiagnosticsSetting::All(false)),
        checks: Some(false),
        // A suggestion is one more request after each turn: off, so each test's scripted replies stay its turns'.
        suggestions: Some(false),
        ..FileConfig::default()
    }
}

fn startup(dirs: &Dirs, file: Option<FileConfig>, replies: Vec<Vec<ProviderEvent>>, herdr: Option<HerdrReporter>) -> Startup {
    let provider: Arc<dyn Provider> = Arc::new(Scripted(Mutex::new(replies.into())));
    Startup {
        root: dirs.project.clone(),
        home: dirs.base.join("home"),
        config_dir: dirs.config.clone(),
        store: ConfigStore::new(&dirs.config),
        file,
        env: Env::new(),
        version: "9.9.9".into(),
        instructions: None,
        instruction_problems: Vec::new(),
        skills: Vec::new(),
        skill_problems: Vec::new(),
        agents: vec![marv_session::agents::general_purpose()],
        agent_problems: Vec::new(),
        project_files: ProjectFiles::default(),
        project_trust: TrustStatus::Nothing,
        nested_trust: Arc::default(),
        memory: Some((memory_paths(&dirs.config, &dirs.project), Memories::default())),
        mcp: None,
        mcp_problems: Vec::new(),
        hidden_path_folders: Vec::new(),
        sandbox_available: false,
        pre_approved: Vec::new(),
        writable: Vec::new(),
        approval_problems: Vec::new(),
        github: false,
        sessions: Some(Arc::new(SessionStore::new(dirs.config.join("sessions")))),
        trajectories: None,
        herdr,
        mesh: None,
        make_provider: Arc::new(move |_, _| provider.clone()),
        load_models: Arc::new(|_, _| {
            Box::pin(async { Ok(vec![marv_provider::ModelInfo { id: "qwen3.5:9b".into(), local: true, ..Default::default() }]) })
        }),
        model: None,
        mcp_deps: Arc::new(|_| panic!("no network in tests")),
        open_url: Arc::new(|_| panic!("no browser in tests")),
    }
}

/// The next event, or a panic after a while.
async fn next(rx: &mut UnboundedReceiver<UiEvent>) -> UiEvent {
    tokio::time::timeout(Duration::from_secs(10), rx.recv()).await.expect("an event in time").expect("the channel open")
}

/// Events until (and including) the first one `stop` matches.
async fn until(rx: &mut UnboundedReceiver<UiEvent>, stop: impl Fn(&UiEvent) -> bool) -> Vec<UiEvent> {
    let mut seen = Vec::new();
    loop {
        let event = next(rx).await;
        let done = stop(&event);
        seen.push(event);
        if done {
            return seen;
        }
    }
}

fn texts(events: &[UiEvent]) -> Vec<String> {
    events.iter().filter_map(|e| if let UiEvent::Message(m) = e { Some(m.text.clone()) } else { None }).collect()
}

// Marv tests/app.test.tsx "/undo" (#16): the project is a temp git repository; turns write notes.txt.
mod undo {
    use super::*;

    fn repo(dirs: &Dirs) {
        for args in [
            &["init", "-q", "-b", "main"][..],
            &["add", "-A"],
            &["-c", "user.email=t@t", "-c", "user.name=t", "-c", "commit.gpgSign=false", "commit", "-q", "-m", "first"],
        ] {
            std::process::Command::new("git").args(args).current_dir(&dirs.project).output().unwrap();
        }
    }

    /// One reply that reads `path`, then writes it: the edit tools change an existing file only once it's read.
    fn read_and_write(path: &str, content: &str) -> Vec<ProviderEvent> {
        let mut reply = write("w", path, content);
        let arguments = serde_json::json!({ "path": path }).to_string();
        reply.insert(0, ProviderEvent::ToolCall(ToolCall { id: "r".into(), name: "read_file".into(), arguments }));
        reply
    }

    fn note(dirs: &Dirs) -> String {
        std::fs::read_to_string(dirs.project.join("notes.txt")).unwrap()
    }

    fn is_message(e: &UiEvent, part: &str) -> bool {
        matches!(e, UiEvent::Message(m) if m.text.contains(part))
    }

    /// A bridge whose first turn ("change the note") read notes.txt and wrote it, approved.
    async fn after_a_turn(dirs: &Dirs) -> (Port, UnboundedReceiver<UiEvent>) {
        repo(dirs);
        let replies = vec![read_and_write("notes.txt", "buy bread\n"), say("Changed the note.")];
        let (tx, mut rx) = unbounded_channel();
        let port = Port(Bridge::start(startup(dirs, Some(asks()), replies, None), tx).unwrap());
        port.send("change the note".into());
        let seen = until(&mut rx, |e| matches!(e, UiEvent::Approval { .. })).await;
        let Some(UiEvent::Approval { id, .. }) = seen.last() else { unreachable!() };
        port.answer(*id, view::Decision::Yes);
        until(&mut rx, |e| matches!(e, UiEvent::TurnEnd { .. })).await;
        assert_eq!(note(dirs), "buy bread\n");
        (port, rx)
    }

    #[tokio::test]
    async fn shows_the_turns_files_and_diffs_yes_puts_them_back() {
        let dirs = Dirs::new();
        let (port, mut rx) = after_a_turn(&dirs).await;
        port.run_command("/undo".into());
        let seen = until(&mut rx, |e| matches!(e, UiEvent::Approval { .. })).await;
        let Some(UiEvent::Approval { id, request }) = seen.last() else { unreachable!() };
        assert_eq!(request.preview.title, "Undo turn 1 (\"change the note\")? 1 file");
        assert!(request.once, "no \"don't ask again\"");
        let files = request.preview.files.as_ref().unwrap();
        assert_eq!(files[0].title, "Restore notes.txt");
        let lines: Vec<(view::DiffKind, &str)> =
            files[0].diff.as_ref().unwrap().lines.iter().map(|l| (l.kind, l.text.as_str())).collect();
        assert_eq!(lines, [(view::DiffKind::Del, "buy bread"), (view::DiffKind::Add, "hi")]);
        port.answer(*id, view::Decision::Yes);
        until(&mut rx, |e| is_message(e, "↶ Undid turn 1: 1 file restored")).await;
        assert_eq!(note(&dirs), "hi\n");
        // Nothing more to undo.
        port.run_command("/undo".into());
        until(&mut rx, |e| is_message(e, "Nothing to undo")).await;
    }

    #[tokio::test]
    async fn no_changes_nothing() {
        let dirs = Dirs::new();
        let (port, mut rx) = after_a_turn(&dirs).await;
        port.run_command("/undo".into());
        let seen = until(&mut rx, |e| matches!(e, UiEvent::Approval { .. })).await;
        let Some(UiEvent::Approval { id, .. }) = seen.last() else { unreachable!() };
        port.answer(*id, view::Decision::No);
        until(&mut rx, |e| is_message(e, "Nothing was undone.")).await;
        assert_eq!(note(&dirs), "buy bread\n");
    }

    #[tokio::test]
    async fn with_nothing_to_undo_it_says_why() {
        let dirs = Dirs::new();
        repo(&dirs);
        let (tx, mut rx) = unbounded_channel();
        let port = Port(Bridge::start(startup(&dirs, Some(asks()), vec![], None), tx).unwrap());
        port.run_command("/undo".into());
        let seen = until(&mut rx, |e| is_message(e, "Nothing to undo")).await;
        assert!(texts(&seen).last().unwrap().contains("no turn in this session has changed files yet"));
    }

    #[tokio::test]
    async fn refused_during_a_turn() {
        let dirs = Dirs::new();
        repo(&dirs);
        let replies = vec![read_and_write("notes.txt", "x\n"), say("Done.")];
        let (tx, mut rx) = unbounded_channel();
        let port = Port(Bridge::start(startup(&dirs, Some(asks()), replies, None), tx).unwrap());
        port.send("go".into());
        let seen = until(&mut rx, |e| matches!(e, UiEvent::Approval { .. })).await;
        let Some(UiEvent::Approval { id, .. }) = seen.last() else { unreachable!() };
        // The turn waits at its approval: /undo now is refused, and asks nothing.
        port.run_command("/undo".into());
        let seen = until(&mut rx, |e| is_message(e, "before /undo")).await;
        assert!(!seen.iter().any(|e| matches!(e, UiEvent::Approval { .. })));
        port.answer(*id, view::Decision::Yes);
        until(&mut rx, |e| matches!(e, UiEvent::TurnEnd { .. })).await;
        assert_eq!(note(&dirs), "x\n");
    }
}

// marv-rs #78: an idle Marv starts a turn when a wired inbox gets a message (opt-in `mesh.wake`).
mod mesh_wake {
    use super::*;
    use marv_mesh::approval::WireRequest;
    use marv_mesh::switchboard::Switchboard;
    use marv_mesh::{ReceptacleName, WireMode};
    use marv_session::mesh::{WAKES_PER_MINUTE, parent_id};

    const WOKEN: &str = "Woken by a message from worker-1 on agent.parent.inbox";

    /// A served switchboard and a worker plug; stopped and removed on drop.
    struct Mesh {
        sb: Arc<Switchboard>,
        worker: marv_mesh::plug::Plug,
        cancel: CancellationToken,
        dir: PathBuf,
    }
    impl Drop for Mesh {
        fn drop(&mut self) {
            self.cancel.cancel();
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
    fn mesh() -> Mesh {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!("mt-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::SeqCst)));
        let sb = Arc::new(Switchboard::new("s", dir.clone(), Arc::new(marv_mesh::switchboard::NoTrajectory)));
        let cancel = CancellationToken::new();
        tokio::spawn(Arc::clone(&sb).serve(cancel.clone()));
        let worker = sb.plug_next("worker").unwrap();
        Mesh { sb, worker, cancel, dir }
    }
    fn inbox() -> ReceptacleName {
        ReceptacleName::new("agent.parent.inbox").unwrap()
    }
    impl Mesh {
        /// The user grants worker-1's wire into Marv's inbox.
        async fn wire(&self) {
            let id = self.worker.id.clone();
            let request = WireRequest { from: id.clone(), to: inbox(), mode: WireMode::Send, asked_by: id };
            self.sb.grant(request, marv_mesh::wire::Approver::Tui).await.unwrap();
        }
        async fn send(&self, text: &str) {
            let body = serde_json::json!({ "text": text });
            self.sb.deliver(&self.worker.id, inbox().as_str(), WireMode::Send, body).await.unwrap();
        }
        fn unread(&self) -> usize {
            self.sb.unread(&parent_id()).len()
        }
    }

    fn wakes() -> FileConfig {
        FileConfig { mesh_wake: Some(true), ..asks() }
    }

    fn bridge(
        dirs: &Dirs,
        file: FileConfig,
        replies: Vec<Vec<ProviderEvent>>,
        mesh: &Mesh,
    ) -> (Arc<Bridge>, UnboundedReceiver<UiEvent>) {
        let (tx, rx) = unbounded_channel();
        let mut s = startup(dirs, Some(file), replies, None);
        s.mesh = Some(mesh.sb.clone());
        let bridge = Bridge::start(s, tx).unwrap();
        bridge.watch_mesh();
        (bridge, rx)
    }

    /// What arrives within a short while (nothing more expected).
    async fn quiet(rx: &mut UnboundedReceiver<UiEvent>) -> Vec<UiEvent> {
        let mut seen = Vec::new();
        while let Ok(Some(e)) = tokio::time::timeout(Duration::from_millis(300), rx.recv()).await {
            seen.push(e);
        }
        seen
    }
    fn started(events: &[UiEvent]) -> bool {
        events.iter().any(|e| matches!(e, UiEvent::TurnStart { .. }))
    }

    #[tokio::test]
    async fn off_by_default_a_message_waits_for_the_next_turn() {
        let (dirs, mesh) = (Dirs::new(), mesh());
        let (bridge, mut rx) = bridge(&dirs, asks(), vec![say("Read it.")], &mesh);
        assert!(!bridge.config().mesh_wake);
        mesh.wire().await;
        mesh.send("hi").await;
        assert!(!started(&quiet(&mut rx).await), "no turn while off");
        assert_eq!(mesh.unread(), 1);
        // The user's next turn reads it, as before.
        Port(bridge.clone()).send("anything new?".into());
        until(&mut rx, |e| matches!(e, UiEvent::TurnEnd { .. })).await;
        assert_eq!(mesh.unread(), 0);
    }

    #[tokio::test]
    async fn on_a_message_starts_a_turn_that_says_who_woke_it_and_taints() {
        let (dirs, mesh) = (Dirs::new(), mesh());
        let (bridge, mut rx) = bridge(&dirs, wakes(), vec![say("worker-1 says hi.")], &mesh);
        mesh.wire().await;
        mesh.send("hi").await;
        let seen = until(&mut rx, |e| matches!(e, UiEvent::TurnEnd { .. })).await;
        let shown = texts(&seen);
        assert_eq!(shown.first().map(String::as_str), Some(WOKEN), "{shown:?}");
        assert!(shown.iter().any(|t| t.contains("now contains a mesh message")), "{shown:?}");
        assert!(seen.iter().any(|e| matches!(e, UiEvent::Assistant { text } if text == "worker-1 says hi.")));
        assert_eq!(mesh.unread(), 0);
        assert!(bridge.session().tainted().contains(&marv_core::Provenance::Mesh));
    }

    #[tokio::test]
    async fn never_while_an_approval_waits() {
        let (dirs, mesh) = (Dirs::new(), mesh());
        let (bridge, mut rx) = bridge(&dirs, wakes(), vec![say("x")], &mesh);
        mesh.wire().await;
        // A wire asked for from another process waits for the person, the session idle.
        let ask = WireRequest {
            from: marv_mesh::PlugId::new("peer-alpha").unwrap(),
            to: inbox(),
            mode: WireMode::Send,
            asked_by: marv_mesh::PlugId::new("peer-alpha").unwrap(),
        };
        let approver = TuiApprover(bridge.activity.clone());
        let waiting =
            tokio::spawn(async move { marv_tools::Approver::approve(&approver, marv_session::mesh::wire_approval(&ask)).await });
        let seen = until(&mut rx, |e| matches!(e, UiEvent::Approval { .. })).await;
        let Some(UiEvent::Approval { id, .. }) = seen.last() else { unreachable!() };
        mesh.send("hi").await;
        assert!(!started(&quiet(&mut rx).await), "no turn under an open approval");
        assert_eq!(mesh.unread(), 1, "it waits for the next turn");
        Port(bridge.clone()).answer(*id, view::Decision::No);
        waiting.await.unwrap();
    }

    /// #124: a radio message straight into the radio queue, as a link delivers one.
    fn radio_message(mesh: &Mesh, seq: u64, to_all: bool, text: &str) {
        let message =
            marv_mesh::radio::RadioMessage::msg(&format!("beta:{seq}"), "beta", to_all, marv_mesh::radio::By::User, 0, text);
        assert!(mesh.sb.deliver_radio(message, 50));
    }

    /// #124: with wake on, an addressed radio message starts a turn (Marv's line says who, `(user)` shown), and a
    /// broadcast doesn't; the woken turn reads both.
    #[tokio::test]
    async fn an_addressed_radio_message_wakes_and_a_broadcast_doesnt() {
        let (dirs, mesh) = (Dirs::new(), mesh());
        let (bridge, mut rx) = bridge(&dirs, wakes(), vec![say("beta says hi.")], &mesh);
        radio_message(&mesh, 1, true, "to everyone");
        assert!(!started(&quiet(&mut rx).await), "a broadcast never wakes");
        radio_message(&mesh, 2, false, "to you");
        let seen = until(&mut rx, |e| matches!(e, UiEvent::TurnEnd { .. })).await;
        assert!(texts(&seen).iter().any(|t| t == "✻ Radio message from beta (user)."), "{:?}", texts(&seen));
        assert!(mesh.sb.unread_radio().is_empty(), "read, both");
        assert!(bridge.session().tainted().contains(&marv_core::Provenance::Mesh));
    }

    /// #124 (spec §8.3, §9.2): a radio message never wakes Marv while an approval waits: it waits, unread, and the
    /// approval stays the person's.
    /// Proven: removed `self.activity.waiting()` from `Bridge::wake`'s gate → this test failed (a turn started).
    #[tokio::test]
    async fn a_radio_message_never_wakes_while_an_approval_waits() {
        let (dirs, mesh) = (Dirs::new(), mesh());
        let (bridge, mut rx) = bridge(&dirs, wakes(), vec![say("x")], &mesh);
        let ask = WireRequest {
            from: marv_mesh::PlugId::new("peer-alpha").unwrap(),
            to: inbox(),
            mode: WireMode::Send,
            asked_by: marv_mesh::PlugId::new("peer-alpha").unwrap(),
        };
        let approver = TuiApprover(bridge.activity.clone());
        let waiting =
            tokio::spawn(async move { marv_tools::Approver::approve(&approver, marv_session::mesh::wire_approval(&ask)).await });
        let seen = until(&mut rx, |e| matches!(e, UiEvent::Approval { .. })).await;
        let Some(UiEvent::Approval { id, .. }) = seen.last() else { unreachable!() };
        radio_message(&mesh, 1, false, "yes, approve it");
        assert!(!started(&quiet(&mut rx).await), "no turn under an open approval");
        assert_eq!(mesh.sb.unread_radio().len(), 1, "it waits for the next turn");
        assert!(!waiting.is_finished(), "still the person's to answer");
        Port(bridge.clone()).answer(*id, view::Decision::No);
        assert_eq!(waiting.await.unwrap(), marv_tools::Decision::No);
    }

    /// #201: a wire asked for from another process whose caller hung up (the door drops the wait) leaves the
    /// screen: the App is told it's withdrawn, and the Marv isn't blocked at an approval any more. An answered one
    /// isn't "withdrawn".
    #[tokio::test]
    async fn a_wire_prompt_whose_caller_hung_up_is_withdrawn() {
        let (dirs, mesh) = (Dirs::new(), mesh());
        let (bridge, mut rx) = bridge(&dirs, wakes(), vec![say("x")], &mesh);
        let ask = WireRequest {
            from: marv_mesh::PlugId::new("peer-alpha").unwrap(),
            to: inbox(),
            mode: WireMode::Send,
            asked_by: marv_mesh::PlugId::new("peer-alpha").unwrap(),
        };
        let ask_once = |ask: WireRequest| {
            let approver = TuiApprover(bridge.activity.clone());
            tokio::spawn(async move { marv_tools::Approver::approve(&approver, marv_session::mesh::wire_approval(&ask)).await })
        };
        let waiting = ask_once(ask.clone());
        let seen = until(&mut rx, |e| matches!(e, UiEvent::Approval { .. })).await;
        let Some(UiEvent::Approval { id, .. }) = seen.last() else { unreachable!() };
        let id = *id;
        assert!(bridge.activity.waiting());
        waiting.abort();
        let seen = until(&mut rx, |e| matches!(e, UiEvent::ApprovalWithdrawn(_))).await;
        assert!(matches!(seen.last(), Some(UiEvent::ApprovalWithdrawn(w)) if *w == id));
        assert!(!bridge.activity.waiting(), "nothing left waiting");

        let answered = ask_once(ask);
        let seen = until(&mut rx, |e| matches!(e, UiEvent::Approval { .. })).await;
        let Some(UiEvent::Approval { id, .. }) = seen.last() else { unreachable!() };
        Port(bridge.clone()).answer(*id, view::Decision::No);
        assert_eq!(answered.await.unwrap(), marv_tools::Decision::No);
        assert!(!quiet(&mut rx).await.iter().any(|e| matches!(e, UiEvent::ApprovalWithdrawn(_))));
    }

    #[tokio::test]
    async fn over_the_rate_limit_messages_wait_and_the_user_is_told_once() {
        let (dirs, mesh) = (Dirs::new(), mesh());
        let replies = (0..WAKES_PER_MINUTE).map(|_| say("ok")).collect();
        let (_bridge, mut rx) = bridge(&dirs, wakes(), replies, &mesh);
        mesh.wire().await;
        for _ in 0..WAKES_PER_MINUTE {
            mesh.send("again").await;
            let seen = until(&mut rx, |e| matches!(e, UiEvent::TurnEnd { .. })).await;
            assert!(texts(&seen).iter().any(|t| t == WOKEN));
        }
        mesh.send("one too many").await;
        let seen = quiet(&mut rx).await;
        assert!(!started(&seen));
        let told = texts(&seen);
        assert_eq!(told.len(), 1, "{told:?}");
        assert!(told[0].contains("worker-1") && told[0].contains(&format!("{WAKES_PER_MINUTE} times a minute")), "{told:?}");
        mesh.send("and another").await;
        let seen = quiet(&mut rx).await;
        assert!(!started(&seen) && texts(&seen).is_empty(), "told once: {seen:?}");
        assert_eq!(mesh.unread(), 2);
    }

    /// A model whose first reply waits until `release` is notified; records each request's last user message.
    #[derive(Default)]
    struct Gated {
        release: Arc<tokio::sync::Notify>,
        held: AtomicBool,
        replies: Mutex<VecDeque<Vec<ProviderEvent>>>,
        seen: Mutex<Vec<String>>,
    }
    impl Provider for Gated {
        fn name(&self) -> &str {
            "gated"
        }
        fn context_length(&self) -> Option<u64> {
            None
        }
        fn stream(&self, history: &[ChatTurn], _options: StreamOptions) -> EventStream {
            let last = history.iter().rev().find_map(|t| if let ChatTurn::User { text } = t { Some(text.clone()) } else { None });
            self.seen.lock().unwrap().push(last.unwrap_or_default());
            let reply = self.replies.lock().unwrap().pop_front().unwrap_or_else(|| say("(script ended)"));
            let (hold, release) = (!self.held.swap(true, Ordering::SeqCst), self.release.clone());
            Box::pin(
                futures::stream::once(async move {
                    if hold {
                        release.notified().await;
                    }
                    futures::stream::iter(reply)
                })
                .flatten(),
            )
        }
    }

    fn gated_bridge(dirs: &Dirs, mesh: &Mesh, model: Arc<Gated>) -> (Arc<Bridge>, UnboundedReceiver<UiEvent>) {
        let (tx, rx) = unbounded_channel();
        let mut s = startup(dirs, Some(wakes()), vec![], None);
        s.mesh = Some(mesh.sb.clone());
        s.make_provider = Arc::new(move |_, _| model.clone() as Arc<dyn Provider>);
        let bridge = Bridge::start(s, tx).unwrap();
        bridge.watch_mesh();
        (bridge, rx)
    }
    fn ends(events: &[UiEvent]) -> Vec<String> {
        events.iter().filter_map(|e| if let UiEvent::TurnEnd { turn, .. } = e { Some(turn.clone()) } else { None }).collect()
    }

    /// Greptile on #110: the user presses Enter after a wake took the session but before the App saw its
    /// TurnStart. The input isn't lost and the woken turn isn't ended for the App: it runs when the wake ends.
    #[tokio::test]
    async fn input_typed_while_a_wake_starts_runs_after_it() {
        let (dirs, mesh) = (Dirs::new(), mesh());
        let model = Arc::new(Gated::default());
        model.replies.lock().unwrap().extend([say("worker-1 says hi."), say("Here's your answer.")]);
        let (bridge, mut rx) = gated_bridge(&dirs, &mesh, model.clone());
        mesh.wire().await;
        mesh.send("hi").await;
        until(&mut rx, |e| matches!(e, UiEvent::TurnStart { .. })).await;
        // The woken turn is waiting on its model: the App submits as if idle.
        Port(bridge.clone()).send("what's new?".into());
        let seen = quiet(&mut rx).await;
        assert!(ends(&seen).is_empty(), "nothing ends the woken turn: {seen:?}");
        assert!(texts(&seen).iter().any(|t| t.contains("yours goes next")), "{seen:?}");
        model.release.notify_one();
        let seen = until(&mut rx, |e| matches!(e, UiEvent::Assistant { text } if text == "Here's your answer.")).await;
        let seen = [seen, until(&mut rx, |e| matches!(e, UiEvent::TurnEnd { .. })).await].concat();
        let ended = ends(&seen);
        assert_eq!(ended.len(), 2, "{seen:?}");
        assert!(ended.iter().all(|t| !t.is_empty()), "only real turns end: {ended:?}");
        assert!(seen.iter().any(|e| matches!(e, UiEvent::Assistant { text } if text == "worker-1 says hi.")));
        let asked = model.seen.lock().unwrap().clone();
        assert_eq!(asked.len(), 2, "{asked:?}");
        assert!(asked[1].ends_with("what's new?"), "{asked:?}");
    }

    /// The other way round: a submission on its way first, a message then can't start a turn alongside it.
    #[tokio::test]
    async fn a_wake_never_starts_while_a_submission_runs() {
        let (dirs, mesh) = (Dirs::new(), mesh());
        let model = Arc::new(Gated::default());
        model.replies.lock().unwrap().extend([say("done")]);
        let (bridge, mut rx) = gated_bridge(&dirs, &mesh, model.clone());
        mesh.wire().await;
        Port(bridge.clone()).send("go".into());
        until(&mut rx, |e| matches!(e, UiEvent::TurnStart { .. })).await;
        mesh.send("hi").await;
        let seen = quiet(&mut rx).await;
        assert!(!texts(&seen).iter().any(|t| t == WOKEN), "{seen:?}");
        model.release.notify_one();
        let seen = until(&mut rx, |e| matches!(e, UiEvent::TurnEnd { .. })).await;
        assert!(!texts(&seen).iter().any(|t| t == WOKEN));
        assert!(model.seen.lock().unwrap().len() <= 2);
    }

    fn queued_lists(events: &[UiEvent]) -> Vec<Vec<String>> {
        events.iter().filter_map(|e| if let UiEvent::Queued(list) = e { Some(list.clone()) } else { None }).collect()
    }

    /// #184: Enter during a mesh-woken turn queues on the same gate the woken turn holds: listed for the App, run
    /// after the turn finishes, its line shown first, never alongside it.
    #[tokio::test]
    async fn a_message_queued_during_a_woken_turn_starts_once_it_finishes() {
        let (dirs, mesh) = (Dirs::new(), mesh());
        let model = Arc::new(Gated::default());
        model.replies.lock().unwrap().extend([say("worker-1 says hi."), say("Tests pass.")]);
        let (bridge, mut rx) = gated_bridge(&dirs, &mesh, model.clone());
        mesh.wire().await;
        mesh.send("hi").await;
        until(&mut rx, |e| matches!(e, UiEvent::TurnStart { .. })).await;
        Port(bridge.clone()).queue("now run the tests".into());
        let seen = quiet(&mut rx).await;
        assert_eq!(queued_lists(&seen), [vec!["now run the tests".to_string()]]);
        assert!(!started(&seen) && ends(&seen).is_empty(), "{seen:?}");
        model.release.notify_one();
        let first = until(&mut rx, |e| matches!(e, UiEvent::TurnEnd { .. })).await;
        let next = until(&mut rx, |e| matches!(e, UiEvent::TurnEnd { .. })).await;
        assert!(first.iter().any(|e| matches!(e, UiEvent::Assistant { text } if text == "worker-1 says hi.")));
        // Listed empty, the person's line, then its turn.
        let at = |f: &dyn Fn(&UiEvent) -> bool| next.iter().position(f).unwrap();
        let emptied = at(&|e| matches!(e, UiEvent::Queued(l) if l.is_empty()));
        let line = at(&|e| matches!(e, UiEvent::Message(m) if m.role == Role::User && m.text == "now run the tests"));
        let start = at(&|e| matches!(e, UiEvent::TurnStart { .. }));
        assert!(emptied < start && line < start, "{next:?}");
        assert!(next.iter().any(|e| matches!(e, UiEvent::Assistant { text } if text == "Tests pass.")));
        assert_eq!(model.seen.lock().unwrap().last().map(String::as_str), Some("now run the tests"));
    }

    /// #184: a stop keeps the queue (and says so): nothing starts on its own. The App takes the oldest to send and
    /// the newest to edit; each change is listed.
    #[tokio::test]
    async fn a_stopped_turn_keeps_the_queue_for_the_person() {
        let (dirs, mesh) = (Dirs::new(), mesh());
        let model = Arc::new(Gated::default());
        model.replies.lock().unwrap().extend([say("never"), say("ok")]);
        let (bridge, mut rx) = gated_bridge(&dirs, &mesh, model.clone());
        let port = Port(bridge.clone());
        port.send("go".into());
        until(&mut rx, |e| matches!(e, UiEvent::TurnStart { .. })).await;
        port.queue("one".into());
        port.queue("two".into());
        port.abort();
        // The held reply comes only now: the turn sees the stop either way.
        model.release.notify_one();
        let end = until(&mut rx, |e| matches!(e, UiEvent::TurnEnd { .. })).await;
        assert!(matches!(end.last(), Some(UiEvent::TurnEnd { reason: view::TurnEndReason::Loop(StopReason::Aborted), .. })));
        let seen = quiet(&mut rx).await;
        assert!(!started(&seen), "{seen:?}");
        assert!(texts(&seen).iter().any(|t| t.contains("Stopped. Your 2 queued messages wait")), "{seen:?}");
        assert_eq!(port.unqueue_last().as_deref(), Some("two"));
        assert_eq!(queued_lists(&quiet(&mut rx).await), [vec!["one".to_string()]]);
        assert_eq!(port.take_queued().as_deref(), Some("one"));
        assert_eq!(queued_lists(&quiet(&mut rx).await), [Vec::<String>::new()]);
        assert_eq!(port.take_queued(), None);
        // Idle with nothing queued: a queued message starts at once (the App hadn't seen the turn end, and listed
        // it): the list it gets is empty, so nothing stale stays above the prompt.
        port.queue("three".into());
        let seen = until(&mut rx, |e| matches!(e, UiEvent::TurnEnd { .. })).await;
        assert!(seen.iter().any(|e| matches!(e, UiEvent::Message(m) if m.role == Role::User && m.text == "three")));
        assert_eq!(queued_lists(&seen), [Vec::<String>::new()], "{seen:?}");
        assert_eq!(model.seen.lock().unwrap().last().map(String::as_str), Some("three"));
        // "Send now" that finds no turn (it just ended) is queued the same way, never dropped nor sent elsewhere.
        port.send_now("four".into());
        let seen = until(&mut rx, |e| matches!(e, UiEvent::TurnEnd { .. })).await;
        assert!(seen.iter().any(|e| matches!(e, UiEvent::Message(m) if m.role == Role::User && m.text == "four")));
        assert!(!seen.iter().any(|e| matches!(e, UiEvent::NotSent { .. })));
    }

    /// A turn stopped with "one" queued: held. Then a mesh message wakes Marv, its reply held until `release`.
    async fn held_then_woken(model: &Arc<Gated>, mesh: &Mesh, port: &Port, rx: &mut UnboundedReceiver<UiEvent>) {
        port.send("go".into());
        until(rx, |e| matches!(e, UiEvent::TurnStart { .. })).await;
        port.queue("one".into());
        port.abort();
        model.release.notify_one();
        until(rx, |e| matches!(e, UiEvent::TurnEnd { .. })).await;
        let seen = quiet(rx).await;
        assert!(texts(&seen).iter().any(|t| t.contains("Stopped. Your queued message waits")), "{seen:?}");
        // The next request waits again: the wake is still running when the person acts.
        model.held.store(false, Ordering::SeqCst);
        mesh.wire().await;
        mesh.send("hi").await;
        until(rx, |e| matches!(e, UiEvent::TurnStart { .. })).await;
    }

    /// Review on #239 (round 2): a stop holds the queue while input the App already showed runs next: the hold is
    /// said at once, and the held message still waits after that input's turn finishes.
    #[tokio::test]
    async fn a_stop_says_the_hold_even_when_shown_input_runs_next() {
        let (dirs, mesh) = (Dirs::new(), mesh());
        let model = Arc::new(Gated::default());
        model.replies.lock().unwrap().extend([say("stopped"), say("Shown done.")]);
        let (bridge, mut rx) = gated_bridge(&dirs, &mesh, model.clone());
        let port = Port(bridge.clone());
        port.send("go".into());
        until(&mut rx, |e| matches!(e, UiEvent::TurnStart { .. })).await;
        port.queue("held one".into());
        // The App, thinking itself idle, sent this: shown, ahead of the queued message.
        port.send("shown".into());
        port.abort();
        model.release.notify_one();
        until(&mut rx, |e| matches!(e, UiEvent::TurnEnd { .. })).await;
        let seen = until(&mut rx, |e| matches!(e, UiEvent::TurnEnd { .. })).await;
        assert!(texts(&seen).iter().any(|t| t.contains("Stopped. Your queued message waits")), "{seen:?}");
        assert!(seen.iter().any(|e| matches!(e, UiEvent::Assistant { text } if text == "Shown done.")), "{seen:?}");
        let after = quiet(&mut rx).await;
        assert!(!started(&after), "the held message waits: {after:?}");
        assert_eq!(model.seen.lock().unwrap().last().map(String::as_str), Some("shown"));
    }

    /// Review on #239: input the App already showed (it sent it as if idle, racing the wake) runs after the wake
    /// even with a held queue, ahead of it; the person sent something, so the held message follows.
    #[tokio::test]
    async fn input_shown_during_a_wake_runs_even_with_a_held_queue() {
        let (dirs, mesh) = (Dirs::new(), mesh());
        let model = Arc::new(Gated::default());
        model.replies.lock().unwrap().extend([say("stopped"), say("worker-1 says hi."), say("News."), say("One.")]);
        let (bridge, mut rx) = gated_bridge(&dirs, &mesh, model.clone());
        let port = Port(bridge.clone());
        held_then_woken(&model, &mesh, &port, &mut rx).await;
        port.send("what's new?".into());
        model.release.notify_one();
        let mut ends = 0;
        let mut seen = Vec::new();
        while ends < 3 {
            let e = next(&mut rx).await;
            ends += usize::from(matches!(e, UiEvent::TurnEnd { .. }));
            seen.push(e);
        }
        assert!(seen.iter().any(|e| matches!(e, UiEvent::Assistant { text } if text == "One.")), "{seen:?}");
        let asked = model.seen.lock().unwrap().clone();
        let n = asked.len();
        assert!(asked[n - 2].ends_with("what's new?") && asked[n - 1] == "one", "{asked:?}");
    }

    /// Review on #239: Enter during a wake with a held queue is the person's next message: the hold ends, and once
    /// the wake finishes the queue runs in order, with no "didn't finish" notice.
    #[tokio::test]
    async fn a_message_queued_during_a_wake_releases_a_held_queue() {
        let (dirs, mesh) = (Dirs::new(), mesh());
        let model = Arc::new(Gated::default());
        model.replies.lock().unwrap().extend([say("stopped"), say("worker-1 says hi."), say("One."), say("Two.")]);
        let (bridge, mut rx) = gated_bridge(&dirs, &mesh, model.clone());
        let port = Port(bridge.clone());
        held_then_woken(&model, &mesh, &port, &mut rx).await;
        port.queue("two".into());
        assert_eq!(queued_lists(&quiet(&mut rx).await), [vec!["one".to_string(), "two".to_string()]]);
        model.release.notify_one();
        let mut ends = 0;
        let mut seen = Vec::new();
        while ends < 3 {
            let e = next(&mut rx).await;
            ends += usize::from(matches!(e, UiEvent::TurnEnd { .. }));
            seen.push(e);
        }
        assert!(seen.iter().any(|e| matches!(e, UiEvent::Assistant { text } if text == "Two.")), "{seen:?}");
        assert!(!texts(&seen).iter().any(|t| t.contains("didn't finish") || t.contains("wait")), "{seen:?}");
        let asked = model.seen.lock().unwrap().clone();
        assert_eq!(&asked[asked.len() - 2..], ["one", "two"]);
    }

    /// A message that lands after a turn's last inbox read (here, during its final reply: no step follows, so it
    /// isn't drained) rang the mail bell while busy. Once that turn ends and Marv is idle, it wakes a turn: it
    /// doesn't sit unread until some other message arrives.
    #[tokio::test]
    async fn a_message_that_lands_during_a_turns_last_reply_wakes_once_it_ends() {
        let (dirs, mesh) = (Dirs::new(), mesh());
        let model = Arc::new(Gated::default());
        model.replies.lock().unwrap().extend([say("done"), say("worker-1 says hi.")]);
        let (bridge, mut rx) = gated_bridge(&dirs, &mesh, model.clone());
        mesh.wire().await;
        Port(bridge.clone()).send("go".into());
        until(&mut rx, |e| matches!(e, UiEvent::TurnStart { .. })).await;
        mesh.send("hi").await;
        quiet(&mut rx).await;
        model.release.notify_one();
        until(&mut rx, |e| matches!(e, UiEvent::TurnEnd { .. })).await;
        let seen = until(&mut rx, |e| matches!(e, UiEvent::TurnEnd { .. })).await;
        assert!(texts(&seen).iter().any(|t| t == WOKEN), "{seen:?}");
        assert!(seen.iter().any(|e| matches!(e, UiEvent::Assistant { text } if text == "worker-1 says hi.")));
        assert_eq!(mesh.unread(), 0);
    }

    /// Greptile on #110: once the App is leaving (its flush), a message starts no turn.
    #[tokio::test]
    async fn no_wake_after_the_app_exits() {
        let (dirs, mesh) = (Dirs::new(), mesh());
        let (bridge, mut rx) = bridge(&dirs, wakes(), vec![say("x")], &mesh);
        mesh.wire().await;
        Port(bridge.clone()).flush().await;
        mesh.send("hi").await;
        assert!(!started(&quiet(&mut rx).await), "no turn after exit");
        assert_eq!(mesh.unread(), 1);
        // And a wake asked for directly is refused too.
        bridge.wake();
        assert!(!started(&quiet(&mut rx).await));
    }

    #[tokio::test]
    async fn mesh_wake_on_saves_and_mesh_shows_the_wires() {
        let (dirs, mesh) = (Dirs::new(), mesh());
        let (bridge, mut rx) = bridge(&dirs, asks(), vec![], &mesh);
        mesh.wire().await;
        let port = Port(bridge.clone());
        port.run_command("/mesh wake on".into());
        let seen = until(&mut rx, |e| matches!(e, UiEvent::Message(_))).await;
        assert!(texts(&seen)[0].starts_with("Mesh wake on: a message on a wired inbox starts a turn"), "{seen:?}");
        assert_eq!(ConfigStore::new(&dirs.config).load().unwrap().unwrap().mesh_wake, Some(true));
        assert!(bridge.config().mesh_wake);
        port.run_command("/mesh".into());
        let shown = texts(&[next(&mut rx).await]);
        assert!(shown[0].contains("- `worker-1 -> agent.parent.inbox`") && shown[0].contains("**Wake**: on"), "{shown:?}");
        port.run_command("/mesh wake off".into());
        until(&mut rx, |e| matches!(e, UiEvent::Message(_))).await;
        assert!(!bridge.config().mesh_wake);
    }
}

// Marv tests/herdr.test.tsx: "App in a Herdr pane".
#[tokio::test]
async fn reports_idle_working_blocked_at_an_approval_and_idle_when_done() {
    let dirs = Dirs::new();
    let recorder = Arc::new(Recorder::default());
    let target = HerdrTarget { socket_path: "/unused".into(), pane_id: "w1:p7".into() };
    let herdr = HerdrReporter::new(target, Some(recorder.clone() as Arc<dyn HerdrSend>));
    let replies = vec![write("c1", "made.txt", "hello\n"), say("Created it.")];
    let (tx, mut rx) = unbounded_channel();
    let bridge = Bridge::start(startup(&dirs, Some(asks()), replies, Some(herdr.clone())), tx).unwrap();
    bridge.report_state();
    bridge.report_session().await;
    herdr.settled().await;
    assert_eq!(recorder.states(), ["idle"]);
    let session = recorder.find(|r| r["method"] == "pane.report_agent_session").unwrap();
    let id = session["params"]["agent_session_id"].as_str().unwrap();
    let path = bridge.sessions.as_ref().unwrap().file(&dirs.project, id);
    assert_eq!(session["params"]["agent_session_path"], path.to_string_lossy().as_ref());

    let port = Port(bridge.clone());
    port.send("make a file".into());
    let seen = until(&mut rx, |e| matches!(e, UiEvent::Approval { .. })).await;
    let Some(UiEvent::Approval { id, request }) = seen.last() else { unreachable!() };
    assert_eq!((request.tool.as_str(), request.label.as_str()), ("write_file", "made.txt"));
    assert!(request.preview.diff.is_some(), "the approval shows the diff");
    herdr.settled().await;
    let blocked = recorder.find(|r| r["params"]["state"] == "blocked").unwrap();
    assert_eq!(blocked["params"]["message"], "made.txt");

    port.answer(*id, view::Decision::Yes);
    let seen = until(&mut rx, |e| matches!(e, UiEvent::TurnEnd { .. })).await;
    let end = seen.iter().find_map(|e| if let UiEvent::ToolEnd { status, .. } = e { Some(status.clone()) } else { None });
    let end = end.expect("the write's tool end");
    assert_eq!(end.status, view::ToolState::Done);
    let diff = end.diff.expect("the diff view");
    assert!(diff.lines.iter().any(|l| l.kind == view::DiffKind::Add && l.text == "hello"));
    assert!(seen.iter().any(|e| matches!(e, UiEvent::Assistant { text } if text == "Created it.")));
    assert_eq!(std::fs::read_to_string(dirs.project.join("made.txt")).unwrap(), "hello\n");

    herdr.settled().await;
    let states = recorder.states();
    let at = |s: &str| states.iter().position(|x| x == s).unwrap();
    assert!(at("working") < at("blocked"));
    assert_eq!(states.last().unwrap(), "idle");
}

/// A pane send is a prompt, not a command: while the palette, setup or a half-typed slash command has the
/// keyboard, Herdr hears working, not idle, so nothing is typed into it; idle again once the prompt is free.
#[tokio::test]
async fn a_held_keyboard_is_working_to_herdr_and_free_again_is_idle() {
    let dirs = Dirs::new();
    let recorder = Arc::new(Recorder::default());
    let target = HerdrTarget { socket_path: "/unused".into(), pane_id: "w1:p7".into() };
    let herdr = HerdrReporter::new(target, Some(recorder.clone() as Arc<dyn HerdrSend>));
    let (tx, _rx) = unbounded_channel();
    let bridge = Bridge::start(startup(&dirs, Some(asks()), vec![], Some(herdr.clone())), tx).unwrap();
    bridge.report_state();
    herdr.settled().await;
    let port = Port(bridge.clone());
    port.keyboard_held(true);
    assert_eq!(bridge.activity.herdr_state(), (HerdrState::Working, None));
    assert_eq!(bridge.activity.session_state(), (HerdrState::Idle, None), "the radio still shows idle");
    herdr.settled().await;
    assert_eq!(recorder.states().last().unwrap(), "working");
    port.keyboard_held(false);
    assert_eq!(bridge.activity.herdr_state(), (HerdrState::Idle, None));
    herdr.settled().await;
    assert_eq!(recorder.states().last().unwrap(), "idle");
}

fn calls(id: &str, name: &str, args: serde_json::Value) -> Vec<ProviderEvent> {
    vec![
        ProviderEvent::ToolCall(ToolCall { id: id.into(), name: name.into(), arguments: args.to_string() }),
        ProviderEvent::Done { reason: Some("tool_calls".into()) },
    ]
}

// Marv #47, #52: in the TUI a host command and a github write each ask, with their door (the approval shows the
// door's warning line, marv_tui::ui::approval::door_warning). Yolo on: neither runs unasked.
#[tokio::test]
async fn a_host_command_and_a_github_write_ask_with_their_door() {
    if !marv_sandbox::sandbox_available() {
        eprintln!("skipped: bubblewrap isn't available here");
        return;
    }
    let dirs = Dirs::new();
    let outside = dirs.base.join("outside-marker");
    let replies = vec![
        // A "no" ends the turn there: no reply follows either call.
        calls("h1", "bash", serde_json::json!({"command": format!("touch {}", outside.display()), "sandbox": false})),
        calls("g1", "github", serde_json::json!({"args": ["pr", "comment", "1", "--body", "hi"]})),
    ];
    let mut start = startup(&dirs, Some(FileConfig { yolo: Some(true), ..asks() }), replies, None);
    start.github = true;
    let (tx, mut rx) = unbounded_channel();
    let bridge = Bridge::start(start, tx).unwrap();
    let port = Port(bridge.clone());
    for (door, tool, once) in [(marv_core::Door::Host, "bash", true), (marv_core::Door::Github, "github", false)] {
        port.send("go".into());
        let seen = until(&mut rx, |e| matches!(e, UiEvent::Approval { .. })).await;
        let Some(UiEvent::Approval { id, request }) = seen.last() else { unreachable!() };
        assert_eq!((request.tool.as_str(), request.door, request.once), (tool, Some(door), once));
        port.answer(*id, view::Decision::No);
        until(&mut rx, |e| matches!(e, UiEvent::TurnEnd { .. })).await;
    }
    assert!(!outside.exists());
}

#[tokio::test]
async fn the_status_bar_counts_running_processes_and_processes_stop_takes_one_off() {
    // Marv's app.test.tsx (#51): the status bar counts it; /processes stops it.
    if !marv_sandbox::sandbox_available() || std::process::Command::new("python3").arg("--version").output().is_err() {
        eprintln!("skipped: needs bubblewrap and python3");
        return;
    }
    let dirs = Dirs::new();
    // network: true runs bash directly in the sandbox (no relay binary needed in a unit test); a server that listens
    // ends the start's wait for a port at once.
    let command = "python3 -m http.server 0 --bind 127.0.0.1";
    let start = serde_json::json!({"action": "start", "command": command, "name": "web", "network": true});
    let replies = vec![
        vec![
            ProviderEvent::ToolCall(ToolCall { id: "p1".into(), name: "process".into(), arguments: start.to_string() }),
            ProviderEvent::Done { reason: Some("tool_calls".into()) },
        ],
        say("It's up."),
    ];
    let (tx, mut rx) = unbounded_channel();
    let bridge = Bridge::start(startup(&dirs, Some(asks()), replies, None), tx).unwrap();
    assert_eq!(bridge.status_info().processes, 0);
    let port = Port(bridge.clone());
    port.send("start the server".into());
    let seen = until(&mut rx, |e| matches!(e, UiEvent::Approval { .. })).await;
    let Some(UiEvent::Approval { id, request }) = seen.last() else { unreachable!() };
    assert_eq!(request.preview.title, "Start a background process (web)");
    port.answer(*id, view::Decision::Yes);
    until(&mut rx, |e| matches!(e, UiEvent::StatusInfo(info) if info.processes == 1)).await;
    until(&mut rx, |e| matches!(e, UiEvent::TurnEnd { .. })).await;
    port.run_command("/processes stop web".into());
    let seen = until(&mut rx, |e| matches!(e, UiEvent::StatusInfo(info) if info.processes == 0)).await;
    let seen =
        [seen, until(&mut rx, |e| texts(std::slice::from_ref(e)).iter().any(|t| t.contains("Stopped web."))).await].concat();
    assert!(texts(&seen).iter().any(|t| t == "Stopped web."), "{:?}", texts(&seen));
    bridge.session().stop_processes().await;
}

#[tokio::test]
async fn no_declines_and_stops_the_turn_and_the_queue_forgets_it() {
    let dirs = Dirs::new();
    let (tx, mut rx) = unbounded_channel();
    let bridge = Bridge::start(startup(&dirs, Some(asks()), vec![write("c1", "made.txt", "x")], None), tx).unwrap();
    let port = Port(bridge.clone());
    port.send("make a file".into());
    let seen = until(&mut rx, |e| matches!(e, UiEvent::Approval { .. })).await;
    let Some(UiEvent::Approval { id, .. }) = seen.last() else { unreachable!() };
    assert_eq!(bridge.activity.herdr_state(), (HerdrState::Blocked, Some("made.txt".into())));
    port.answer(*id, view::Decision::No);
    let seen = until(&mut rx, |e| matches!(e, UiEvent::TurnEnd { .. })).await;
    assert!(seen.iter().any(|e| matches!(e, UiEvent::ToolEnd { status, .. } if status.status == view::ToolState::Declined)));
    assert!(seen.iter().any(|e| matches!(e, UiEvent::Done(StopReason::Declined))));
    assert!(!dirs.project.join("made.txt").exists());
    assert_eq!(bridge.activity.herdr_state(), (HerdrState::Idle, None));
}

#[tokio::test]
async fn esc_at_an_approval_interrupts_and_the_session_is_free_again() {
    let dirs = Dirs::new();
    let (tx, mut rx) = unbounded_channel();
    let replies = vec![write("c1", "made.txt", "x"), say("again")];
    let bridge = Bridge::start(startup(&dirs, Some(asks()), replies, None), tx).unwrap();
    let port = Port(bridge.clone());
    port.send("make a file".into());
    until(&mut rx, |e| matches!(e, UiEvent::Approval { .. })).await;
    // What the App does on Esc: abort (its queue is declined by the session's stop, or by its own answers).
    port.abort();
    let seen = until(&mut rx, |e| matches!(e, UiEvent::TurnEnd { .. })).await;
    assert!(seen.iter().any(|e| matches!(e, UiEvent::Done(StopReason::Aborted))));
    assert_eq!(bridge.activity.herdr_state(), (HerdrState::Idle, None));
    assert!(bridge.activity.asks.lock().unwrap().is_empty(), "a stopped run's ask is forgotten");
    port.send("hello".into());
    let seen = until(&mut rx, |e| matches!(e, UiEvent::TurnEnd { .. })).await;
    assert!(seen.iter().any(|e| matches!(e, UiEvent::Assistant { text } if text == "again")));
}

#[tokio::test]
async fn after_a_finished_turn_the_next_message_is_suggested_unless_thats_off() {
    let dirs = Dirs::new();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let on = FileConfig { suggestions: Some(true), ..asks() };
    let port = Port(Bridge::start(startup(&dirs, Some(on), vec![say("Fixed it."), say("run the tests")], None), tx).unwrap());
    port.send("fix it".into());
    let events = until(&mut rx, |e| matches!(e, UiEvent::Suggestion(_))).await;
    assert!(matches!(events.last(), Some(UiEvent::Suggestion(t)) if t == "run the tests"), "{events:?}");
    assert!(events.iter().any(|e| matches!(e, UiEvent::TurnEnd { .. })), "after the turn");

    let dirs = Dirs::new();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let port = Port(Bridge::start(startup(&dirs, Some(asks()), vec![say("Fixed it."), say("run the tests")], None), tx).unwrap());
    port.send("fix it".into());
    until(&mut rx, |e| matches!(e, UiEvent::TurnEnd { .. })).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    while let Ok(event) = rx.try_recv() {
        assert!(!matches!(event, UiEvent::Suggestion(_)), "suggestions are off");
    }
}

#[tokio::test]
async fn a_session_comes_back_with_its_transcript() {
    let dirs = Dirs::new();
    {
        let (tx, mut rx) = unbounded_channel();
        let bridge = Bridge::start(startup(&dirs, Some(asks()), vec![say("Hi there.")], None), tx).unwrap();
        Port(bridge.clone()).send("hello".into());
        until(&mut rx, |e| matches!(e, UiEvent::TurnEnd { .. })).await;
        Port(bridge).flush().await;
    }
    let (tx, mut rx) = unbounded_channel();
    let bridge = Bridge::start(startup(&dirs, Some(asks()), vec![], None), tx).unwrap();
    bridge.resume_latest().await;
    let UiEvent::Resumed(resumed) = next(&mut rx).await else { panic!("not resumed") };
    let shown: Vec<(Role, &str)> = resumed.transcript.iter().map(|m| (m.role, m.text.as_str())).collect();
    assert_eq!(shown, [(Role::User, "hello"), (Role::Assistant, "Hi there.")]);
    assert!(matches!(next(&mut rx).await, UiEvent::StatusInfo(_)));

    // /resume lists the others, not this one.
    Port(bridge.clone()).run_command("/resume".into());
    let UiEvent::Sessions(list) = next(&mut rx).await else { panic!("not the list") };
    assert!(list.iter().all(|s| s.id != bridge.session.id()));
}

/// #202: Marv quit while an approval waited (the first bridge is never answered, flushed nor closed): `marv -c`
/// brings the turn back, with a notice that it was cut off.
#[tokio::test]
async fn a_turn_cut_off_mid_approval_comes_back_marked() {
    let dirs = Dirs::new();
    let (tx, mut rx) = unbounded_channel();
    let first = Bridge::start(startup(&dirs, Some(asks()), vec![write("w", "a.txt", "hi\n")], None), tx).unwrap();
    Port(first.clone()).send("write a.txt".into());
    until(&mut rx, |e| matches!(e, UiEvent::Approval { .. })).await;
    // The save before the question is a background write: wait for it to land.
    let store = SessionStore::new(dirs.config.join("sessions"));
    let mut landed = false;
    for _ in 0..500 {
        if store.load(&dirs.project, &first.session.id()).await.is_some_and(|s| s.cut_off.is_some()) {
            landed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(landed, "saved before the approval");

    let (tx, mut rx) = unbounded_channel();
    let bridge = Bridge::start(startup(&dirs, Some(asks()), vec![], None), tx).unwrap();
    bridge.resume_latest().await;
    let UiEvent::Resumed(resumed) = next(&mut rx).await else { panic!("not resumed") };
    let shown: Vec<(Role, &str)> = resumed.transcript.iter().map(|m| (m.role, m.text.as_str())).collect();
    let notice = marv_session::cut_off::cut_off_notice("Marv quit while `write_file` waited for approval");
    assert_eq!(shown, [(Role::User, "write a.txt"), (Role::System, notice.as_str())]);
}

/// #74: in a Marv started with MARV_YOLO=off, /yolo on says it stays off, names the variable, and writes nothing.
#[tokio::test]
async fn yolo_on_is_refused_under_marv_yolo_off() {
    let dirs = Dirs::new();
    let (tx, mut rx) = unbounded_channel();
    let mut start = startup(&dirs, Some(FileConfig { yolo: Some(false), ..asks() }), vec![], None);
    start.env.insert("MARV_YOLO".into(), "off".into());
    let bridge = Bridge::start(start, tx).unwrap();
    Port(bridge.clone()).run_command("/yolo on".into());
    let seen = until(&mut rx, |e| matches!(e, UiEvent::Message(_))).await;
    assert!(texts(&seen).iter().any(|t| t.contains("MARV_YOLO=off")), "{:?}", texts(&seen));
    assert!(ConfigStore::new(&dirs.config).load().unwrap().is_none_or(|f| f.yolo != Some(true)), "nothing written");
    assert!(!bridge.status_info().yolo);
}

#[tokio::test]
async fn the_model_flag_is_for_this_run_and_never_saved() {
    let dirs = Dirs::new();
    let (tx, _rx) = unbounded_channel();
    let saved = asks().model;
    let bridge =
        Bridge::start(Startup { model: Some("flag:3b".into()), ..startup(&dirs, Some(asks()), vec![], None) }, tx).unwrap();
    assert_eq!(bridge.config().model, "flag:3b");
    assert_eq!(bridge.file().and_then(|f| f.model), saved);
    // The picker saves a default, and the run's model gives way to it.
    Port(bridge.clone()).complete_setup(SetupAnswer { provider: ProviderId::Ollama, model: "picked:1b".into(), api_key: None });
    assert_eq!(bridge.config().model, "picked:1b");
    assert_eq!(ConfigStore::new(&dirs.config).load().unwrap().unwrap().model.as_deref(), Some("picked:1b"));
}

#[tokio::test]
async fn commands_change_the_config_and_say_so() {
    let dirs = Dirs::new();
    let (tx, mut rx) = unbounded_channel();
    let bridge = Bridge::start(startup(&dirs, Some(asks()), vec![], None), tx).unwrap();
    let port = Port(bridge.clone());

    port.run_command("/yolo on".into());
    let seen = until(&mut rx, |e| matches!(e, UiEvent::Message(_))).await;
    assert!(seen.iter().any(|e| matches!(e, UiEvent::StatusInfo(s) if s.yolo)));
    assert!(texts(&seen)[0].starts_with("Yolo on: file edits run without asking"));
    assert_eq!(ConfigStore::new(&dirs.config).load().unwrap().unwrap().yolo, Some(true));

    // /model <id>: this session only, the saved default untouched.
    let saved = ConfigStore::new(&dirs.config).load().unwrap().unwrap().model;
    port.run_command("/model other:7b".into());
    let seen = until(&mut rx, |e| matches!(e, UiEvent::Message(_))).await;
    assert!(texts(&seen)[0].starts_with("Ollama · other:7b, for this session."), "{:?}", texts(&seen));
    assert!(seen.iter().any(|e| matches!(e, UiEvent::StatusInfo(_))));
    assert_eq!(bridge.config().model, "other:7b");
    assert_eq!(ConfigStore::new(&dirs.config).load().unwrap().unwrap().model, saved);

    port.run_command("/model".into());
    let UiEvent::OpenSetup(ctx) = next(&mut rx).await else { panic!("not setup") };
    assert_eq!((ctx.start, ctx.current), (SetupStart::Model, Some((ProviderId::Ollama, "other:7b".into()))));

    port.run_command("/remember use tabs".into());
    assert_eq!(texts(&[next(&mut rx).await]), [format!("Saved to personal memory. {NEXT_TIME}")]);
    port.run_command("/forget tabs".into());
    assert_eq!(texts(&[next(&mut rx).await]), ["Forgot: use tabs"]);

    port.run_command("/mcp trust".into());
    assert_eq!(texts(&[next(&mut rx).await]), ["No MCP servers are waiting to be trusted (/mcp lists them)."]);
    port.run_command("/nope".into());
    assert!(matches!(next(&mut rx).await, UiEvent::Message(m) if m.is_error && m.text.starts_with("Unknown command: /nope")));
    port.run_command("/good".into());
    assert!(matches!(next(&mut rx).await, UiEvent::Message(m) if m.is_error));
    port.run_command("/exit".into());
    assert_eq!(next(&mut rx).await, UiEvent::Exit);
}

// #44: the palette lists the settings with their values, and a toggle goes the way its slash command goes.
#[tokio::test]
async fn the_palettes_settings_show_their_values_and_a_toggle_is_saved_and_announced_like_its_command() {
    let dirs = Dirs::new();
    let (tx, mut rx) = unbounded_channel();
    let bridge = Bridge::start(startup(&dirs, Some(asks()), vec![], None), tx).unwrap();
    let port = Port(bridge.clone());
    let setting = |name: &str| port.settings().into_iter().find(|s| s.name == name).unwrap();
    let yolo = setting("Yolo");
    assert_eq!((yolo.value.as_str(), &yolo.action, yolo.while_busy), ("off", &SettingAction::Run("/yolo on".into()), true));
    assert_eq!(setting("Model").value, "qwen3.5:9b");
    assert_eq!(setting("Model").action, SettingAction::Run("/model".into()));
    assert_eq!(setting("Step limit").action, SettingAction::Prefill("/steps ".into()));

    let SettingAction::Run(line) = yolo.action else { unreachable!() };
    port.run_command(line);
    let seen = until(&mut rx, |e| matches!(e, UiEvent::Message(_))).await;
    assert!(seen.iter().any(|e| matches!(e, UiEvent::StatusInfo(s) if s.yolo)));
    assert!(texts(&seen)[0].starts_with("Yolo on: file edits run without asking"));
    assert_eq!(ConfigStore::new(&dirs.config).load().unwrap().unwrap().yolo, Some(true));
    // Asked again when the palette next opens: the new value, and the flip back.
    assert_eq!(setting("Yolo").value, "on");
    assert_eq!(setting("Yolo").action, SettingAction::Run("/yolo off".into()));

    // The menu's entries carry what the palette needs: built-ins with their busy rule.
    let menu = port.commands();
    let entry = |name: &str| menu.iter().find(|c| c.name == name).unwrap().clone();
    assert!(entry("cost").while_busy && !entry("undo").while_busy);
    assert_eq!(entry("cost").kind, CommandKind::Command);
}

#[tokio::test]
async fn compact_runs_like_a_turn_and_clear_starts_a_new_conversation() {
    let dirs = Dirs::new();
    let (tx, mut rx) = unbounded_channel();
    let bridge = Bridge::start(startup(&dirs, Some(asks()), vec![], None), tx).unwrap();
    let port = Port(bridge.clone());
    port.run_command("/compact".into());
    let seen = until(&mut rx, |e| matches!(e, UiEvent::TurnEnd { .. })).await;
    assert!(matches!(seen[0], UiEvent::TurnStart { .. }));
    assert!(seen.iter().any(|e| matches!(
        e,
        UiEvent::Compaction { automatic: false, outcome: view::CompactOutcome::NotCompacted { reason, .. } } if reason == "empty"
    )));
    let before = bridge.session.id();
    port.run_command("/clear".into());
    // (The session may say something changed first: the status bar is sent again.)
    let seen = until(&mut rx, |e| matches!(e, UiEvent::Welcome(_))).await;
    assert!(seen.contains(&UiEvent::Clear));
    assert_ne!(bridge.session.id(), before);
}

fn notices(events: &[UiEvent]) -> Vec<Notice> {
    events.iter().filter_map(|e| if let UiEvent::Message(m) = e { m.notice.clone() } else { None }).collect()
}
fn is_hooks_notice(e: &UiEvent) -> bool {
    matches!(e, UiEvent::Message(m) if m.notice.as_ref().is_some_and(|n| n.title.starts_with("Session-start hooks")))
}

// #108: the user's hooks run at start and again after /clear, each time shown as one line; a project's hooks are
// told about and never run.
#[tokio::test]
async fn session_start_hooks_show_at_start_and_after_clear_and_a_projects_are_refused() {
    let dirs = Dirs::new();
    let count = dirs.base.join("runs.txt");
    let pwned = dirs.project.join("PWNED");
    std::fs::create_dir_all(dirs.project.join(".marv")).unwrap();
    let planted = serde_json::json!({
        "hooks": {"sessionStart": [format!("touch '{}'", pwned.display())]},
        "approve": ["network"],
    });
    std::fs::write(dirs.project.join(".marv/config.json"), planted.to_string()).unwrap();
    let command = format!("echo x >> '{0}'; echo \"queue run $(wc -l < '{0}')\"", count.display());
    let file = FileConfig {
        hooks: Some(marv_session::hooks::Hooks { session_start: vec![marv_session::hooks::HookCommand::new(command)] }),
        ..asks()
    };
    let (tx, mut rx) = unbounded_channel();
    let bridge = Bridge::start(startup(&dirs, Some(file), vec![], None), tx).unwrap();
    bridge.announce(None);
    let startup = notices(&until(&mut rx, |e| matches!(e, UiEvent::Message(m) if m.notice.is_some())).await);
    assert!(startup[0].items.iter().any(|i| i.text.contains(".marv/config.json has \"hooks\": not run")), "{startup:?}");
    // #98: a project's "approve" is told about and never read (nothing pre-approved).
    assert!(startup[0].items.iter().any(|i| i.text.contains("has \"approve\": ignored")), "{startup:?}");
    assert!(!startup[0].items.iter().any(|i| i.text.starts_with("Pre-approved")), "{startup:?}");
    bridge.show_hooks();
    let shown = notices(&until(&mut rx, is_hooks_notice).await).pop().unwrap();
    assert_eq!(shown.items.len(), 1);
    assert!(shown.items[0].text.ends_with("ms") && shown.items[0].text.contains(": 1 line, "), "{shown:?}");
    Port(bridge.clone()).run_command("/clear".into());
    until(&mut rx, is_hooks_notice).await;
    assert_eq!(std::fs::read_to_string(&count).unwrap().lines().count(), 2);
    assert!(!pwned.exists(), "the project's hook ran");
}

#[tokio::test]
async fn first_run_opens_setup_and_setup_saves_the_file() {
    let dirs = Dirs::new();
    let (tx, mut rx) = unbounded_channel();
    let bridge = Bridge::start(startup(&dirs, None, vec![], None), tx).unwrap();
    bridge.announce(None);
    let UiEvent::OpenSetup(ctx) = next(&mut rx).await else { panic!("not setup") };
    assert_eq!((ctx.start, ctx.current.clone()), (SetupStart::FirstRun, None));
    let port = Port(bridge.clone());
    port.load_models(ProviderId::Ollama);
    let UiEvent::Models(Ok(models)) = next(&mut rx).await else { panic!("no models") };
    assert_eq!(models[0].id, "qwen3.5:9b");
    port.complete_setup(SetupAnswer { provider: ProviderId::Ollama, model: "qwen3.5:9b".into(), api_key: None });
    let seen = until(&mut rx, |e| matches!(e, UiEvent::Message(_))).await;
    assert!(texts(&seen)[0].starts_with("Saved to ") && texts(&seen)[0].ends_with("· Ollama · qwen3.5:9b"));
    let saved = ConfigStore::new(&dirs.config).load().unwrap().unwrap();
    assert_eq!((saved.provider, saved.model.as_deref()), (Some(ProviderKind::Ollama), Some("qwen3.5:9b")));
}

// #82: /permissions shows the pre-approved scopes and an "always" answer, and never reaches the model.
#[tokio::test]
async fn permissions_lists_the_sessions_grants_and_stays_off_the_conversation() {
    let dirs = Dirs::new();
    let replies = vec![write("c1", "made.txt", "hello\n"), say("Created it.")];
    let mut s = startup(&dirs, Some(asks()), replies, None);
    s.pre_approved = vec!["web".into()];
    let (tx, mut rx) = unbounded_channel();
    let bridge = Bridge::start(s, tx).unwrap();
    let port = Port(bridge.clone());
    port.send("make a file".into());
    let seen = until(&mut rx, |e| matches!(e, UiEvent::Approval { .. })).await;
    let Some(UiEvent::Approval { id, .. }) = seen.last() else { unreachable!() };
    port.answer(*id, view::Decision::Always);
    until(&mut rx, |e| matches!(e, UiEvent::TurnEnd { .. })).await;
    let (conversation, system) = (bridge.session.conversation(), bridge.session.system_prompt());
    port.run_command("/permissions".into());
    let shown = texts(&[until(&mut rx, |e| matches!(e, UiEvent::Message(_))).await.pop().unwrap()]);
    assert!(shown[0].contains("**Permissions**"), "{shown:?}");
    assert!(shown[0].contains("- `web` (from --approve): web_fetch from any site"), "{shown:?}");
    assert!(shown[0].contains("- File changes: (all)"), "{shown:?}");
    assert!(shown[0].contains("**Yolo**: off"), "{shown:?}");
    assert_eq!(bridge.session.conversation(), conversation, "nothing of it joins the history");
    assert_eq!(bridge.session.system_prompt(), system, "nor the system prompt");
}

// #82 review: config ["web", "netwrk"]: web is seeded and listed, netwrk is warned about and listed nowhere.
#[tokio::test]
async fn an_unknown_config_scope_is_warned_about_and_not_listed() {
    let dirs = Dirs::new();
    // No config file: the notice ends with setup opening, as in the test below.
    let mut s = startup(&dirs, None, vec![], None);
    (s.pre_approved, s.approval_problems) = merge_pre_approved(vec!["web".into(), "netwrk".into()], &[]);
    let (tx, mut rx) = unbounded_channel();
    let bridge = Bridge::start(s, tx).unwrap();
    bridge.announce(None);
    let seen = until(&mut rx, |e| matches!(e, UiEvent::OpenSetup(_))).await;
    let notice: Vec<String> = seen
        .iter()
        .filter_map(|e| if let UiEvent::Message(m) = e { m.notice.as_ref() } else { None })
        .flat_map(|n| n.items.iter().flat_map(|i| std::iter::once(i.text.clone()).chain(i.detail.clone().unwrap_or_default())))
        .collect();
    let notice = notice.join("\n");
    assert!(notice.contains("Pre-approved for this session: web\n"), "{notice}");
    assert!(notice.contains("Unknown --approve scope \"netwrk\""), "warned: {notice}");
    assert!(bridge.session.permissions().pre_approved == ["web"]);
    Port(bridge.clone()).run_command("/permissions".into());
    let shown = texts(&[until(&mut rx, |e| matches!(e, UiEvent::Message(_))).await.pop().unwrap()]);
    assert!(shown[0].contains("- `web`"), "{shown:?}");
    assert!(!shown[0].contains("netwrk"), "{shown:?}");
    assert!(!bridge.session.system_prompt().contains("netwrk"));
}

// #98: the startup notice names what's pre-approved, which extra folders are writable, and any problem.
#[tokio::test]
async fn the_startup_notice_names_pre_approved_scopes_and_writable_folders() {
    let dirs = Dirs::new();
    let mut s = startup(&dirs, None, vec![], None);
    s.pre_approved = vec!["web".into(), "network".into()];
    s.writable = vec![dirs.config.join("beads")];
    s.approval_problems = vec!["Refusing writable sandbox folder \"~/.ssh\": it is, or holds, a protected path.".into()];
    let (tx, mut rx) = unbounded_channel();
    let bridge = Bridge::start(s, tx).unwrap();
    bridge.announce(None);
    let seen = until(&mut rx, |e| matches!(e, UiEvent::OpenSetup(_))).await;
    let (texts, details): (Vec<String>, Vec<String>) = seen
        .iter()
        .filter_map(|e| if let UiEvent::Message(m) = e { m.notice.as_ref() } else { None })
        .map(|n| {
            (
                n.items.iter().map(|i| i.text.clone()).collect::<Vec<_>>().join("\n"),
                n.items.iter().flat_map(|i| i.detail.clone().unwrap_or_default()).collect::<Vec<_>>().join("\n"),
            )
        })
        .fold((Vec::new(), Vec::new()), |(mut a, mut b), (t, d)| {
            a.push(t);
            b.push(d);
            (a, b)
        });
    let text = texts.join("\n");
    let detail = details.join("\n");
    assert!(text.contains("Pre-approved for this session: web, network"), "{text}");
    assert!(text.contains("Writable in the sandbox besides the project"), "{text}");
    assert!(text.contains("A problem with your pre-approved scopes or writable folders"), "{text}");
    assert!(detail.contains("Refusing writable sandbox folder"), "{detail}");
    assert!(
        detail.contains("Protected paths, package.json lifecycle scripts, mesh wires and tainted egress still ask"),
        "{detail}"
    );
}

#[test]
fn setup_keeps_the_saved_key_and_drops_another_providers_endpoint() {
    let file = FileConfig {
        provider: Some(ProviderKind::Ollama),
        model: Some("m".into()),
        base_url: Some("http://gpu:11434".into()),
        api_key: Some("sk-or-v1-0123456789abcdef".into()),
        ..FileConfig::default()
    };
    let answer = SetupAnswer { provider: ProviderId::OpenRouter, model: "z-ai/glm-5.3".into(), api_key: None };
    let next = setup_file(Some(&file), &answer);
    assert_eq!(next.base_url, None);
    assert_eq!(next.api_key, file.api_key);
    let same = setup_file(Some(&file), &SetupAnswer { provider: ProviderId::Ollama, model: "n".into(), api_key: None });
    assert_eq!(same.base_url.as_deref(), Some("http://gpu:11434"));

    let env = Env::from([("OPENROUTER_API_KEY".to_string(), "sk-or-v1-fromtheenvironment".to_string())]);
    let ctx = setup_context(SetupStart::Reconfigure, Some(&file), &resolve_config(Some(&file), &env), &env);
    assert_eq!(ctx.saved_keys, vec![(ProviderId::OpenRouter, "sk-or-v1-0…cdef".to_string())]);
    assert_eq!(ctx.env_keys, [(ProviderId::OpenRouter, "sk-or-v1-f…ment".to_string())]);
    assert!(!format!("{ctx:?}").contains("fromtheenvironment"), "keys reach the UI masked");
}

/// #275: the user's endpoints are offered after the built-in providers, without a key step; choosing one saves its
/// name and keeps every endpoint, and choosing a built-in one again clears the name.
#[test]
fn setup_offers_custom_providers_and_saves_the_choice_by_name() {
    let file = FileConfig::from_value(&serde_json::json!({
        "provider": "openrouter", "model": "z-ai/glm-5.3",
        "providers": {"acme-gateway": {"baseUrl": "https://llm.acme.internal/v1", "models": ["acme-large"]}}
    }))
    .unwrap();
    let env = Env::new();
    let ctx = setup_context(SetupStart::Reconfigure, Some(&file), &resolve_config(Some(&file), &env), &env);
    let acme = ctx.presets.last().unwrap();
    assert_eq!(acme.id, ProviderId::Custom("acme-gateway".into()));
    assert_eq!((acme.key_env.as_ref(), acme.default_model.as_str(), acme.sign_in), (None, "acme-large", false));
    let answer = SetupAnswer { provider: acme.id.clone(), model: "acme-large".into(), api_key: None };
    let chosen = setup_file(Some(&file), &answer);
    assert_eq!((chosen.provider, chosen.custom_provider.as_deref()), (Some(ProviderKind::Custom), Some("acme-gateway")));
    assert_eq!(chosen.to_value()["provider"], "acme-gateway");
    assert_eq!(chosen.providers, file.providers);
    let ctx = setup_context(SetupStart::Model, Some(&chosen), &resolve_config(Some(&chosen), &env), &env);
    assert_eq!(ctx.current, Some((ProviderId::Custom("acme-gateway".into()), "acme-large".into())));
    let back = setup_file(Some(&chosen), &SetupAnswer { provider: ProviderId::OpenRouter, model: "m".into(), api_key: None });
    assert_eq!((back.provider, back.custom_provider), (Some(ProviderKind::OpenRouter), None));
}

/// #159: Z.ai and ChatGPT are offered when their switch is on (ChatGPT's on and Z.ai's off by default), or when the
/// config already uses one; Kimi, Qwen and DeepSeek always. A plan's key goes to its own field.
#[test]
fn setup_offers_the_plans_behind_their_switches_and_keeps_keys_apart() {
    let ids = |file: Option<&FileConfig>| {
        let env = Env::new();
        let ctx = setup_context(SetupStart::Reconfigure, file, &resolve_config(file, &env), &env);
        ctx.presets.iter().map(|p| p.id.clone()).collect::<Vec<_>>()
    };
    let mut expected = vec![ProviderId::OpenRouter];
    if marv_provider::offered::ZAI_CODING_PLAN {
        expected.push(ProviderId::Zai);
    }
    if marv_provider::offered::CHATGPT_SIGN_IN {
        expected.push(ProviderId::ChatGpt);
    }
    expected.extend([ProviderId::Kimi, ProviderId::Qwen, ProviderId::DeepSeek, ProviderId::Ollama]);
    assert_eq!(ids(None), expected);
    let zai = FileConfig { provider: Some(ProviderKind::Zai), ..FileConfig::default() };
    assert!(ids(Some(&zai)).contains(&ProviderId::Zai), "a config that uses it keeps it in setup");

    let file = FileConfig { api_key: Some("sk-or-keep".into()), ..FileConfig::default() };
    let answer = SetupAnswer { provider: ProviderId::Zai, model: "glm-5.3".into(), api_key: Some("zai-new".into()) };
    let next = setup_file(Some(&file), &answer);
    assert_eq!((next.api_key.as_deref(), next.zai_api_key.as_deref()), (Some("sk-or-keep"), Some("zai-new")));
}

#[test]
fn approvals_carry_the_protected_paths_reason() {
    let request = marv_tools::ApprovalRequest {
        reason: false,
        persists: false,
        tool: "write_file".into(),
        label: ".envrc".into(),
        preview: marv_tools::Preview { title: "Create .envrc".into(), ..Default::default() },
        scope: marv_tools::Scope { key: "files".into(), description: "file changes".into() },
        agent: Some("implementer · Task 2".into()),
        network: false,
        once: true,
        must_ask: true,
        protected: marv_sandbox::protected::rule_for_relative(Path::new(".envrc")),
        door: None,
    };
    let shown = approval_request(&request);
    assert!(shown.preview.warning.unwrap().starts_with("changes .envrc"));
    assert_eq!((shown.agent.as_deref(), shown.once), (Some("implementer · Task 2"), true));
    // The TUI's queue needs `must_ask` too: an "always" on another request never answers this one.
    assert!(shown.must_ask);
}

/// #77: the session's one prompt for a call and its new wire reaches the TUI as "Yes (wire it)" or "No".
#[test]
fn a_call_needing_a_new_wire_shows_yes_wire_it_or_no() {
    use marv_mesh::{PlugId, ReceptacleName, WireMode, approval::WireRequest};
    let request = WireRequest {
        from: PlugId::new("parent").unwrap(),
        to: ReceptacleName::new("peer.web.agent.parent.inbox").unwrap(),
        mode: WireMode::Send,
        asked_by: PlugId::new("parent").unwrap(),
    };
    let call = marv_tools::Preview { title: "Send a message".into(), text: Some("hi".into()), ..Default::default() };
    let shown = approval_request(&marv_session::mesh::call_wire_approval(&request, call));
    assert_eq!(marv_session::mesh::MESH_WIRE_CALL, marv_tui::ui::approval::MESH_WIRE_CALL);
    let view = marv_tui::ui::approval::ApprovalView::new(shown, 0, true);
    let labels: Vec<&str> = view.select.items.iter().map(|i| i.label.as_str()).collect();
    assert_eq!(labels, ["Yes (wire it)", "No"]);
}

/// #185: a model's call (run_tool's request sets `reason`) reaches the TUI still offering "No, and say why";
/// the step-limit question, /undo and a call needing a new wire don't.
#[test]
fn only_a_models_call_offers_a_reason_in_the_tui() {
    use marv_mesh::{PlugId, ReceptacleName, WireMode, approval::WireRequest};
    use marv_session::checkpoints::store::{Checkpoint, CheckpointKind};
    let model_call = marv_tools::ApprovalRequest {
        reason: true,
        persists: false,
        tool: "bash".into(),
        label: "bun test".into(),
        preview: marv_tools::Preview { title: "Run a command".into(), command: Some("bun test".into()), ..Default::default() },
        scope: marv_tools::Scope { key: "bash::bun test".into(), description: "this exact command".into() },
        agent: None,
        network: false,
        once: false,
        must_ask: false,
        protected: None,
        door: None,
    };
    let shown = approval_request(&model_call);
    assert!(shown.reason);
    let view = marv_tui::ui::approval::ApprovalView::new(shown, 0, true);
    assert!(view.select.items.iter().any(|i| i.label.starts_with("No, and say why")));

    let checkpoint =
        |kind| Checkpoint { sha: "a".into(), tree: "b".into(), turn: 1, kind, label: "x".into(), nested: Vec::new() };
    let undo = marv_session::UndoPlan {
        turn: 1,
        label: "add the parser".into(),
        before: checkpoint(CheckpointKind::Before),
        after: checkpoint(CheckpointKind::After),
        files: Vec::new(),
        not_restored: Vec::new(),
        head: "a".into(),
        targets: Default::default(),
    };
    let wire = WireRequest {
        from: PlugId::new("parent").unwrap(),
        to: ReceptacleName::new("peer.web.agent.parent.inbox").unwrap(),
        mode: WireMode::Send,
        asked_by: PlugId::new("parent").unwrap(),
    };
    let call = marv_tools::Preview { title: "Send a message".into(), ..Default::default() };
    for request in [
        marv_session::runner::step_limit_request(25, 25),
        marv_session::undo_request(&undo),
        marv_session::mesh::call_wire_approval(&wire, call),
    ] {
        let tool = request.tool.clone();
        let shown = approval_request(&request);
        assert!(!shown.reason, "{tool}");
        let view = marv_tui::ui::approval::ApprovalView::new(shown, 0, true);
        assert!(!view.select.items.iter().any(|i| i.label.contains("say why")), "{tool}");
    }
}

/// #185: a call declined with a reason shows declined (it didn't run), not done, though the turn goes on.
#[test]
fn a_call_declined_with_a_reason_shows_declined() {
    let result =
        || ToolResult { output: marv_tools::declined_because("not now"), summary: Some("declined".into()), is_error: false };
    let mut run = marv_agent::ToolRun::new(result(), "bun test");
    run.approval = marv_agent::ApprovalRecord::Declined;
    assert!(!run.declined);
    assert_eq!(tool_status(&run).status, view::ToolState::Declined);
    let ran = marv_agent::ToolRun { approval: marv_agent::ApprovalRecord::Yes, ..marv_agent::ToolRun::new(result(), "bun test") };
    assert_eq!(tool_status(&ran).status, view::ToolState::Done);
}

#[test]
fn tool_entries_show_a_failures_message_and_subagents_keep_their_title() {
    let call = ToolCall { id: "a1".into(), name: "agent".into(), arguments: "{}".into() };
    let failed = marv_agent::ToolRun::new(
        ToolResult { output: "No such file\nmore".into(), summary: Some("error".into()), is_error: true },
        "x.ts",
    );
    let status = tool_status(&failed);
    assert_eq!((status.status, status.summary.as_deref()), (view::ToolState::Error, Some("No such file")));

    let mut map = TurnMap::default();
    let t = || view::Totals { requests: 3, ..Default::default() };
    let last = || Some(marv_core::Usage { prompt_tokens: 10, completion_tokens: 1, cached_tokens: None, cost: None });
    map_event(SessionEvent::Loop(LoopEvent::ToolStart { call, label: "general-purpose · fix".into() }), &mut map, &t, &last);
    let out = map_event(
        SessionEvent::Subagent { call_id: "a1".into(), event: LoopEvent::Usage(Default::default(), Default::default()) },
        &mut map,
        &t,
        &last,
    );
    assert!(matches!(&out[0], UiEvent::Usage { totals, last } if totals.requests == 3 && last.prompt_tokens == 10));
    assert!(matches!(&out[1], UiEvent::Subagent { title, .. } if title == "general-purpose · fix"));
    let tainted = map_event(SessionEvent::Tainted { source: marv_core::Provenance::Web }, &mut map, &t, &last);
    assert!(texts(&tainted)[0].contains("a web page"));
}

#[test]
fn the_main_agents_plan_call_pins_its_list_and_a_subagents_doesnt() {
    let call = ToolCall { id: "p1".into(), name: "plan".into(), arguments: "{}".into() };
    let mut run = marv_agent::ToolRun::new(
        ToolResult { output: "Plan saved: 0 of 1 done; now: A.".into(), summary: Some("0/1 done".into()), is_error: false },
        "Plan",
    );
    let items = vec![marv_tools::PlanItem { text: "A".into(), status: marv_tools::PlanStatus::InProgress }];
    run.display = Some(Arc::new(marv_session::ToolDisplay { diff: None, plan: Some(items) }));
    let mut map = TurnMap::default();
    let (t, last) = (view::Totals::default, || None);
    let ended = LoopEvent::ToolEnd { call: call.clone(), run: run.clone() };
    let out = map_event(SessionEvent::Loop(ended.clone()), &mut map, &t, &last);
    let expected = vec![view::PlanItem { text: "A".into(), status: view::PlanStatus::InProgress }];
    assert!(matches!(&out[..], [UiEvent::ToolEnd { .. }, UiEvent::Plan(items)] if *items == expected));
    let sub = map_event(SessionEvent::Subagent { call_id: "a1".into(), event: ended }, &mut map, &t, &last);
    assert!(!sub.iter().any(|e| matches!(e, UiEvent::Plan(_))));
    // A failed call (no list) changes nothing.
    let failed = marv_agent::ToolRun::new(ToolResult { output: "x".into(), summary: None, is_error: true }, "Plan");
    let out = map_event(SessionEvent::Loop(LoopEvent::ToolEnd { call, run: failed }), &mut map, &t, &last);
    assert!(!out.iter().any(|e| matches!(e, UiEvent::Plan(_))));
}

// Marv tests/app.test.tsx "MCP servers", "MCP logins (#18)" and "/mcp add, search and remove" (#27). Nothing
// leaves the machine: npm, PyPI, the registry and GitHub are the install fixtures, `npx` is a shell script that
// logs how it was run and speaks MCP over stdio, the OAuth server is a local fake, and the browser is a test's.
mod mcp {
    use super::*;
    use marv_mcp::install::proposal::{Fetched, ResolveDeps};
    use std::collections::HashMap as Map;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio_util::sync::CancellationToken;

    const NPM_FILESYSTEM: &str = include_str!("../../../marv-mcp/src/install/fixtures/npm-server-filesystem.json");
    const REGISTRY_SEARCH: &str = include_str!("../../../marv-mcp/src/install/fixtures/registry-search.json");

    /// The resolvers' network, from the fixtures.
    #[derive(Default)]
    struct Net {
        asked: Mutex<Vec<String>>,
        readme: Option<String>,
    }
    #[async_trait]
    impl ResolveDeps for Net {
        async fn get(&self, url: &str, _a: &str, _m: usize, _c: &CancellationToken) -> marv_mcp::Result<Fetched> {
            self.asked.lock().unwrap().push(url.to_string());
            let body = if url.starts_with("http://fake/npm/@modelcontextprotocol%2Fserver-filesystem")
                || url.starts_with("http://fake/npm/@modelcontextprotocol/server-filesystem")
            {
                NPM_FILESYSTEM
            } else if url.starts_with("http://fake/registry/v0/servers?") {
                REGISTRY_SEARCH
            } else {
                return Ok(Fetched { status: 404, body: "{\"error\":\"Not found\"}".into() });
            };
            Ok(Fetched { status: 200, body: body.into() })
        }
        async fn readme(&self, _o: &str, _r: &str, _c: &CancellationToken) -> marv_mcp::Result<Option<String>> {
            Ok(self.readme.clone())
        }
        fn npm_registry(&self) -> &str {
            "http://fake/npm"
        }
        fn pypi(&self) -> &str {
            "http://fake/pypi"
        }
        fn registry(&self) -> &str {
            "http://fake/registry"
        }
        fn now_ms(&self) -> u64 {
            1_791_806_400_000 // 2026-10-12T12:00:00Z, as the fixtures expect
        }
        fn cancel(&self) -> CancellationToken {
            CancellationToken::new()
        }
    }

    /// A stdio MCP server in sh with one tool, `read`; it logs its name and arguments to `log`. The handshake is
    /// told by its `protocolVersion` (tests/guards' `server_instructions_never_leave_the_parser` keeps the
    /// method's name out of this crate).
    fn fake_server(path: &Path, log: &Path) {
        let script = format!(
            r#"#!/bin/sh
echo "$(basename "$0") $*" >> '{}'
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  case "$line" in
    *'"protocolVersion"'*) printf '{{"jsonrpc":"2.0","id":%s,"result":{{"protocolVersion":"2025-06-18","capabilities":{{"tools":{{}}}},"serverInfo":{{"name":"fake","version":"1.0.0"}}}}}}\n' "$id" ;;
    *'"method":"tools/list"'*) printf '{{"jsonrpc":"2.0","id":%s,"result":{{"tools":[{{"name":"read","description":"Reads.","inputSchema":{{"type":"object","properties":{{}}}}}}]}}}}\n' "$id" ;;
    *) [ -n "$id" ] && printf '{{"jsonrpc":"2.0","id":%s,"result":{{}}}}\n' "$id" ;;
  esac
done
"#,
            log.display()
        );
        marv_tools::github::fake::write_executable(path, &script);
    }

    struct Mcp {
        port: Port,
        rx: UnboundedReceiver<UiEvent>,
        mcp: McpHandle,
        opened: Arc<Mutex<Vec<String>>>,
        net: Arc<Net>,
        log: PathBuf,
        bin: PathBuf,
    }

    impl Mcp {
        fn bridge(&self) -> &Arc<Bridge> {
            &self.port.0
        }
        /// Events until a message containing `part`; their texts.
        async fn says(&mut self, part: &str) -> Vec<String> {
            let seen = until(&mut self.rx, |e| matches!(e, UiEvent::Message(m) if m.text.contains(part))).await;
            texts(&seen)
        }
        /// Events until a notice whose items mention `part`; that notice.
        async fn notice(&mut self, part: &str) -> Notice {
            let has = |e: &UiEvent| {
                matches!(e, UiEvent::Message(view::Message { notice: Some(n), .. })
                    if n.items.iter().any(|i| i.text.contains(part)))
            };
            let seen = until(&mut self.rx, has).await;
            let Some(UiEvent::Message(view::Message { notice: Some(n), .. })) = seen.last() else { unreachable!() };
            n.clone()
        }
        async fn approval(&mut self) -> (ApprovalId, view::ApprovalRequest) {
            let seen = until(&mut self.rx, |e| matches!(e, UiEvent::Approval { .. })).await;
            let Some(UiEvent::Approval { id, request }) = seen.last() else { unreachable!() };
            (*id, request.clone())
        }
        fn mcp_file(&self, dirs: &Dirs) -> Value {
            serde_json::from_str(&std::fs::read_to_string(dirs.config.join("mcp.json")).unwrap()).unwrap()
        }
        fn ran(&self) -> String {
            std::fs::read_to_string(&self.log).unwrap_or_default()
        }
    }

    /// A bridge as `run` makes it: an MCP manager even with no servers, so `/mcp add` can start the first.
    fn start(dirs: &Dirs, project_servers: Option<Value>, net: Net) -> Mcp {
        let bin = dirs.base.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let log = dirs.base.join("ran.log");
        fake_server(&bin.join("npx"), &log);
        fake_server(&bin.join("fake-mcp"), &log);
        if let Some(servers) = project_servers {
            std::fs::write(dirs.project.join(".mcp.json"), json!({ "mcpServers": servers }).to_string()).unwrap();
        }
        let mut s = startup(dirs, Some(asks()), vec![], None);
        s.env = [
            ("PATH".to_string(), format!("{}:/usr/bin:/bin", bin.display())),
            ("OPENROUTER_API_KEY".to_string(), "sk-or-test".to_string()),
        ]
        .into();
        let (mcp, problems) = start_mcp(&s.root, &s.home, &s.config_dir, &s.env);
        assert_eq!(problems, Vec::<String>::new());
        s.mcp = Some(mcp.clone());
        let net = Arc::new(net);
        let deps = net.clone();
        s.mcp_deps = Arc::new(move |_cancel| deps.clone() as Arc<dyn ResolveDeps>);
        let opened = Arc::new(Mutex::new(Vec::new()));
        let o = opened.clone();
        s.open_url = Arc::new(move |url| o.lock().unwrap().push(url.to_string()));
        let (tx, rx) = unbounded_channel();
        let bridge = Bridge::start(s, tx).unwrap();
        bridge.watch_mcp();
        Mcp { port: Port(bridge), rx, mcp, opened, net, log, bin }
    }

    use serde_json::json;

    #[tokio::test]
    async fn a_projects_server_waits_for_mcp_trust_and_says_so() {
        let dirs = Dirs::new();
        let fake = dirs.base.join("bin/fake-mcp");
        let mut t =
            start(&dirs, Some(json!({ "test": { "command": fake, "args": ["--key=${OPENROUTER_API_KEY}"] } })), Net::default());
        let notice = t.notice("you haven't trusted yet").await;
        assert_eq!((notice.title.as_str(), notice.tone), ("MCP · 1 note", NoticeTone::Warning));
        assert_eq!(notice.items[0].text, "This project's .mcp.json lists 1 MCP server you haven't trusted yet");
        let detail = notice.items[0].detail.clone().unwrap();
        assert!(detail[0].starts_with("test ("), "{detail:?}");
        assert!(detail[0].ends_with(", which reads $OPENROUTER_API_KEY from your environment"), "{detail:?}");
        assert_eq!(detail[1], "They'd run with your permissions, outside the sandbox. /mcp trust starts them.");
        assert!(t.ran().is_empty(), "nothing ran before the trust");

        t.port.run_command("/mcp trust".into());
        let said = t.says("test: connected").await;
        assert!(said.contains(&"Starting test…".to_string()), "{said:?}");
        assert_eq!(said.last().unwrap(), "test: connected · 1 tools");
        assert!(t.ran().starts_with("fake-mcp --key="));
        let trust = marv_mcp::trust::McpTrust::new(dirs.config.join("mcp-trust.json"));
        let include = marv_mcp::config::Include { personal: true, project: true };
        let env = t.bridge().env.clone();
        let config = marv_mcp::config::load_mcp_config(&dirs.project, &dirs.config, &env, include).servers.remove(0);
        assert!(trust.is_trusted(&project_key(&dirs.project), &config));
        t.port.run_command("/mcp trust".into());
        t.says("No MCP servers are waiting to be trusted (/mcp lists them).").await;
        t.mcp.close().await;
    }

    #[tokio::test]
    async fn mcp_add_shows_one_approval_with_the_command_and_the_json_and_no_writes_nothing() {
        let dirs = Dirs::new();
        let mut t = start(&dirs, None, Net::default());
        t.port.run_command("/mcp add @modelcontextprotocol/server-filesystem".into());
        let (id, request) = t.approval().await;
        assert!(request.once, "no \"don't ask again\"");
        assert_eq!(request.preview.title, "Add MCP server \"filesystem\" to ~/.marv/mcp.json (yours, every project)");
        assert_eq!(request.preview.command.as_deref(), Some("npx -y @modelcontextprotocol/server-filesystem@2026.1.14"));
        let text = request.preview.text.clone().unwrap();
        assert!(text.contains("2026.1.14, published 2026-01-14"), "{text}");
        assert!(text.contains("writes   \"filesystem\": {\"command\":\"npx\""), "{text}");
        assert!(
            request.preview.note.as_deref().unwrap().starts_with("runs outside the sandbox, with your permissions and network")
        );
        t.port.answer(id, view::Decision::No);
        t.says("Not added: filesystem.").await;
        assert!(!dirs.config.join("mcp.json").exists());
        assert!(t.mcp.status().is_empty());
        assert!(t.ran().is_empty());
    }

    #[tokio::test]
    async fn yes_writes_exactly_the_entry_shown_starts_it_and_mcp_remove_takes_it_out() {
        let dirs = Dirs::new();
        let mut t = start(&dirs, None, Net::default());
        t.port.run_command("/mcp add @modelcontextprotocol/server-filesystem".into());
        let seen = until(&mut t.rx, |e| matches!(e, UiEvent::Approval { .. })).await;
        assert!(seen.contains(&UiEvent::Busy(Some("Looking up @modelcontextprotocol/server-filesystem…".into()))));
        let Some(UiEvent::Approval { id, request }) = seen.last() else { unreachable!() };
        let shown = request.preview.text.clone().unwrap();
        t.port.answer(*id, view::Decision::Yes);
        let said = t.says("connected · 1 tool").await;
        assert!(said.contains(&"Starting filesystem…".to_string()));
        assert_eq!(
            said.last().unwrap(),
            "Added filesystem to ~/.marv/mcp.json: connected · 1 tool. Marv can use it from your next message."
        );
        let written = t.mcp_file(&dirs)["mcpServers"]["filesystem"].clone();
        let shown_json = shown.rsplit_once("writes   \"filesystem\": ").unwrap().1;
        assert_eq!(serde_json::from_str::<Value>(shown_json).unwrap(), written, "exactly the entry shown");
        let home = dirs.base.join("home");
        assert_eq!(t.ran().trim(), "npx -y @modelcontextprotocol/server-filesystem@2026.1.14");
        assert!(home.is_dir());
        let status = t.mcp.status();
        assert_eq!(
            (status[0].name.as_str(), status[0].source, status[0].state),
            ("filesystem", marv_mcp::config::ServerSource::Personal, marv_mcp::manager::McpState::Connected)
        );
        assert!(t.mcp.specs().iter().any(|s| s.name == "mcp__filesystem__read"));

        // A name you already have is refused before asking.
        t.port.run_command("/mcp add @modelcontextprotocol/server-filesystem".into());
        let said = t.says("Couldn't add the server").await;
        assert!(said.last().unwrap().contains("You already have a server named \"filesystem\""));
        assert!(said.last().unwrap().contains("--name"));

        t.port.run_command("/mcp remove filesystem".into());
        let said = t.says("Removed filesystem").await;
        assert_eq!(
            said.last().unwrap(),
            "Removed filesystem from ~/.marv/mcp.json and stopped it. Its tools stay listed (failing) until /clear or a restart."
        );
        assert_eq!(t.mcp_file(&dirs)["mcpServers"], json!({}));
        assert_eq!(t.mcp.status()[0].state, marv_mcp::manager::McpState::Removed);
        // Removed in this conversation: it can't come back until the next one.
        t.port.run_command("/mcp add @modelcontextprotocol/server-filesystem".into());
        let said = t.says("Couldn't add the server").await;
        assert!(said.last().unwrap().contains("was removed in this conversation"), "{said:?}");
        t.mcp.close().await;
    }

    #[tokio::test]
    async fn lookups_that_fail_bad_options_and_unset_variables() {
        let dirs = Dirs::new();
        let mut t = start(&dirs, None, Net::default());
        t.port.run_command("/mcp add nope".into());
        let said = t.says("Couldn't add the server").await;
        assert!(said.last().unwrap().contains("no npm package named nope"), "{said:?}");
        assert!(!dirs.config.join("mcp.json").exists());

        let asked = t.net.asked.lock().unwrap().len();
        t.port.run_command("/mcp add weather --env API_KEY=sk-123".into());
        t.says("--env takes a variable's name").await;
        assert_eq!(t.net.asked.lock().unwrap().len(), asked, "nothing was looked up");

        // An unset ${VAR}: written, not started, and Marv says to set it and restart.
        t.port.run_command("/mcp add @modelcontextprotocol/server-filesystem --env NOT_SET_HERE".into());
        let (id, request) = t.approval().await;
        assert!(request.preview.text.unwrap().contains("NOT_SET_HERE (not set"));
        t.port.answer(id, view::Decision::Yes);
        let said = t.says("restart Marv").await;
        assert_eq!(
            said.last().unwrap(),
            "Added filesystem to ~/.marv/mcp.json, but didn't start it: ${NOT_SET_HERE} isn't set. Set it in your shell and restart Marv."
        );
        assert_eq!(t.mcp_file(&dirs)["mcpServers"]["filesystem"]["env"], json!({"NOT_SET_HERE": "${NOT_SET_HERE}"}));
        assert!(t.mcp.status().is_empty());
    }

    #[tokio::test]
    async fn a_name_this_projects_mcp_json_uses_is_refused_before_asking() {
        let dirs = Dirs::new();
        let fake = dirs.base.join("bin/fake-mcp");
        let mut t = start(&dirs, Some(json!({ "filesystem": { "command": fake } })), Net::default());
        t.notice("you haven't trusted yet").await;
        t.port.run_command("/mcp add @modelcontextprotocol/server-filesystem".into());
        let said = t.says("Couldn't add the server").await;
        assert!(said.last().unwrap().contains("This project has a server named \"filesystem\": pick another with --name."));
        assert!(!dirs.config.join("mcp.json").exists());
        // And /mcp remove points at the project's file.
        t.port.run_command("/mcp remove filesystem".into());
        t.says("\"filesystem\" is this project's server: remove it from .mcp.json.").await;
        t.port.run_command("/mcp remove nope".into());
        t.says("You have no server named \"nope\" in ~/.marv/mcp.json (/mcp lists them).").await;
    }

    #[tokio::test]
    async fn a_readme_naming_several_servers_lists_them_and_search_prints_the_registrys_matches() {
        let dirs = Dirs::new();
        let servers = json!({"mcpServers": {"a": {"command": "npx", "args": ["-y", "weather-mcp"]}, "b": {"command": "uvx", "args": ["mcp-server-fetch"]}}});
        let readme = format!("# Weather\n\n```json\n{servers}\n```\n");
        let mut t = start(&dirs, None, Net { readme: Some(readme), ..Net::default() });
        t.port.run_command("/mcp add https://github.com/acme/weather".into());
        let seen = until(&mut t.rx, |e| matches!(e, UiEvent::Message(m) if m.text.contains("names several servers"))).await;
        let Some(UiEvent::Message(m)) = seen.last() else { unreachable!() };
        assert!(m.markdown);
        assert!(m.text.contains("- `/mcp add weather-mcp`"), "{}", m.text);
        assert!(m.text.contains("- `/mcp add pypi:mcp-server-fetch`"), "{}", m.text);

        t.port.run_command("/mcp search filesystem".into());
        let seen = until(&mut t.rx, |e| matches!(e, UiEvent::Message(m) if m.text.contains("MCP Registry"))).await;
        assert!(seen.contains(&UiEvent::Busy(Some("Searching the MCP Registry…".into()))));
        let said = texts(&seen);
        assert!(said.last().unwrap().contains("isn't curated"));
        assert!(said.last().unwrap().contains("`/mcp add io.modelcontextprotocol/filesystem`"));
    }

    #[tokio::test]
    async fn esc_stops_a_lookup_and_nothing_is_added() {
        let dirs = Dirs::new();
        let mut t = start(&dirs, None, Net::default());
        t.port.run_command("/mcp add @modelcontextprotocol/server-filesystem".into());
        let (id, _) = t.approval().await;
        // Esc at the approval: the App declines it and aborts.
        t.port.answer(id, view::Decision::No);
        t.port.abort();
        let said = t.says("filesystem").await;
        assert!(said.last().unwrap() == "Not added: filesystem." || said.last().unwrap() == "Stopped: nothing was added.");
        assert!(!dirs.config.join("mcp.json").exists());
    }

    // ---- logins (Marv #18), against a local OAuth server ----

    /// An MCP server that is its own OAuth authorization server: metadata, registration, an `/authorize` that
    /// redirects straight back, `/token`, `/revoke`. Tokens are random; `/mcp` wants one.
    struct OAuth {
        base: String,
        revoked: Arc<Mutex<Vec<String>>>,
        /// Every token stops working and refreshes are refused (a login that ran out).
        expired: Arc<AtomicBool>,
    }

    async fn oauth_server() -> OAuth {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let (tokens, revoked) = (Arc::new(Mutex::new(Vec::new())), Arc::new(Mutex::new(Vec::new())));
        let expired = Arc::new(AtomicBool::new(false));
        let (b, tk, rv, ex) = (base.clone(), tokens.clone(), revoked.clone(), expired.clone());
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let (b, tk, rv, ex) = (b.clone(), tk.clone(), rv.clone(), ex.clone());
                tokio::spawn(async move {
                    let _ = oauth_one(stream, &b, &tk, &rv, ex.load(Ordering::SeqCst)).await;
                });
            }
        });
        OAuth { base, revoked, expired }
    }

    fn form(body: &str) -> Map<String, String> {
        url_pairs(body)
    }

    fn url_pairs(query: &str) -> Map<String, String> {
        query.split('&').filter_map(|kv| kv.split_once('=')).map(|(k, v)| (k.to_string(), percent_decode(v))).collect()
    }

    fn percent_decode(v: &str) -> String {
        let b = v.replace('+', " ").into_bytes();
        let mut out = Vec::new();
        let mut i = 0;
        while i < b.len() {
            if b[i] == b'%'
                && let Some(n) =
                    std::str::from_utf8(&b[i + 1..(i + 3).min(b.len())]).ok().and_then(|h| u8::from_str_radix(h, 16).ok())
            {
                out.push(n);
                i += 3;
                continue;
            }
            out.push(b[i]);
            i += 1;
        }
        String::from_utf8_lossy(&out).into_owned()
    }

    fn percent_encode(v: &str) -> String {
        v.bytes()
            .map(
                |c| {
                    if c.is_ascii_alphanumeric() || b"-._~".contains(&c) { (c as char).to_string() } else { format!("%{c:02X}") }
                },
            )
            .collect()
    }

    async fn oauth_one(
        mut stream: tokio::net::TcpStream,
        base: &str,
        tokens: &Mutex<Vec<String>>,
        revoked: &Mutex<Vec<String>>,
        expired: bool,
    ) -> std::io::Result<()> {
        let (method, target, headers, body) = read_request(&mut stream).await?;
        let (path, query) = target.split_once('?').unwrap_or((&target, ""));
        let json =
            |status: u16, v: Value| (status, vec![("content-type".to_string(), "application/json".to_string())], v.to_string());
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let random = || format!("{:08x}", NEXT.fetch_add(1, Ordering::SeqCst) * 2_654_435_761 % 4_294_967_291);
        let (status, extra, body) = match (method.as_str(), path) {
            ("POST", "/mcp") => {
                let auth = headers.get("authorization").cloned().unwrap_or_default();
                let ok = !expired && auth.strip_prefix("Bearer ").is_some_and(|t| tokens.lock().unwrap().iter().any(|k| k == t));
                if !ok {
                    let www = format!("Bearer resource_metadata=\"{base}/.well-known/oauth-protected-resource/mcp\"");
                    (401, vec![("www-authenticate".to_string(), www)], "unauthorized".to_string())
                } else {
                    let message: Value = serde_json::from_str(&body).unwrap_or_default();
                    let result = match message["method"].as_str().unwrap_or_default() {
                        // The handshake (see `fake_server`).
                        _ if message["params"]["protocolVersion"].is_string() => {
                            json!({"protocolVersion": message["params"]["protocolVersion"], "capabilities": {"tools": {}}, "serverInfo": {"name": "remote", "version": "1"}})
                        }
                        "tools/list" => {
                            json!({"tools": [{"name": "echo", "description": "Echo.", "inputSchema": {"type": "object", "properties": {}}}]})
                        }
                        _ => json!({}),
                    };
                    match message.get("id") {
                        Some(id) => json(200, json!({"jsonrpc": "2.0", "id": id, "result": result})),
                        None => (202, vec![], String::new()),
                    }
                }
            }
            (_, "/.well-known/oauth-protected-resource/mcp" | "/.well-known/oauth-protected-resource") => {
                json(200, json!({"resource": format!("{base}/mcp"), "authorization_servers": [base]}))
            }
            (_, "/.well-known/oauth-authorization-server") => json(
                200,
                json!({
                    "issuer": base,
                    "authorization_endpoint": format!("{base}/authorize"),
                    "token_endpoint": format!("{base}/token"),
                    "registration_endpoint": format!("{base}/register"),
                    "revocation_endpoint": format!("{base}/revoke"),
                    "response_types_supported": ["code"],
                    "code_challenge_methods_supported": ["S256"],
                }),
            ),
            ("POST", "/register") => {
                let mut v: Value = serde_json::from_str(&body).unwrap_or_default();
                v["client_id"] = format!("client-{}", random()).into();
                json(201, v)
            }
            ("GET", "/authorize") => {
                let q = url_pairs(query);
                let back = format!(
                    "{}?code=code-{}&state={}",
                    q.get("redirect_uri").cloned().unwrap_or_default(),
                    random(),
                    percent_encode(q.get("state").map_or("", String::as_str))
                );
                (302, vec![("location".to_string(), back)], String::new())
            }
            ("POST", "/token") if expired => json(400, json!({"error": "invalid_grant"})),
            ("POST", "/token") => {
                let (a, r) = (format!("at-{}", random()), format!("rt-{}", random()));
                tokens.lock().unwrap().push(a.clone());
                json(200, json!({"access_token": a, "refresh_token": r, "token_type": "Bearer", "expires_in": 3600}))
            }
            ("POST", "/revoke") => {
                revoked.lock().unwrap().push(form(&body).get("token").cloned().unwrap_or_default());
                (200, vec![], String::new())
            }
            _ => (404, vec![], "not found".to_string()),
        };
        let mut out = format!("HTTP/1.1 {status} X\r\ncontent-length: {}\r\nconnection: close\r\n", body.len());
        for (k, v) in extra {
            out.push_str(&format!("{k}: {v}\r\n"));
        }
        out.push_str("\r\n");
        out.push_str(&body);
        stream.write_all(out.as_bytes()).await?;
        stream.shutdown().await
    }

    async fn read_request(stream: &mut tokio::net::TcpStream) -> std::io::Result<(String, String, Map<String, String>, String)> {
        let mut data = Vec::new();
        let mut buf = [0u8; 4096];
        let end = loop {
            if let Some(i) = data.windows(4).position(|w| w == b"\r\n\r\n") {
                break i;
            }
            let n = stream.read(&mut buf).await?;
            if n == 0 {
                return Err(std::io::ErrorKind::UnexpectedEof.into());
            }
            data.extend_from_slice(&buf[..n]);
        };
        let head = String::from_utf8_lossy(&data[..end]).to_string();
        let mut lines = head.split("\r\n");
        let mut first = lines.next().unwrap_or_default().split(' ');
        let (method, target) = (first.next().unwrap_or_default().to_string(), first.next().unwrap_or("/").to_string());
        let headers: Map<String, String> =
            lines.filter_map(|l| l.split_once(':')).map(|(k, v)| (k.trim().to_lowercase(), v.trim().to_string())).collect();
        let length: usize = headers.get("content-length").and_then(|l| l.parse().ok()).unwrap_or(0);
        let mut body = data[end + 4..].to_vec();
        while body.len() < length {
            let n = stream.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            body.extend_from_slice(&buf[..n]);
        }
        Ok((method, target, headers, String::from_utf8_lossy(&body).into_owned()))
    }

    /// A GET without following redirects: the `location` it answers with.
    async fn location(url: &str) -> String {
        let rest = url.strip_prefix("http://").unwrap();
        let (host, path) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
        let mut stream = tokio::net::TcpStream::connect(host).await.unwrap();
        let request = format!("GET {path} HTTP/1.1\r\nhost: {host}\r\nconnection: close\r\n\r\n");
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut answer = String::new();
        stream.read_to_string(&mut answer).await.unwrap();
        answer.split("\r\n").find_map(|l| l.strip_prefix("location: ")).map(str::to_string).unwrap_or_default()
    }

    async fn opened(t: &Mcp) -> String {
        for _ in 0..1000 {
            if let Some(url) = t.opened.lock().unwrap().first().cloned() {
                return url;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("the login never opened anything");
    }

    async fn remote(dirs: &Dirs) -> (Mcp, OAuth) {
        let server = oauth_server().await;
        std::fs::write(
            dirs.config.join("mcp.json"),
            json!({"mcpServers": {"remote": {"type": "http", "url": format!("{}/mcp", server.base)}}}).to_string(),
        )
        .unwrap();
        (start(dirs, None, Net::default()), server)
    }

    #[tokio::test]
    async fn startup_lists_the_server_that_needs_a_login_and_the_menu_offers_login_and_logout() {
        let dirs = Dirs::new();
        let (mut t, _server) = remote(&dirs).await;
        let notice = t.notice("needs a login").await;
        assert_eq!(notice.title, "MCP · 1 note");
        assert_eq!(notice.items[0].text, "1 MCP server needs a login");
        assert_eq!(notice.items[0].detail.as_deref(), Some(&["remote: /mcp login remote".to_string()][..]));
        let menu: Vec<String> = t.port.commands().into_iter().map(|c| c.name).collect();
        assert!(menu.contains(&"mcp login remote".to_string()) && menu.contains(&"mcp logout remote".to_string()));
    }

    #[tokio::test]
    async fn mcp_login_shows_the_link_opens_it_takes_the_pasted_callback_and_the_code_stays_out() {
        let dirs = Dirs::new();
        let (mut t, server) = remote(&dirs).await;
        t.notice("needs a login").await;
        t.port.run_command("/mcp login remote".into());
        let said = t.says("Log in to remote at 127.0.0.1:").await;
        assert!(said.last().unwrap().contains("paste its address here:\nhttp://127.0.0.1:"), "{said:?}");
        let url = opened(&t).await;
        assert!(url.starts_with(&format!("{}/authorize?", server.base)));
        let pasted = location(&url).await;
        let code = pasted.split("code=").nth(1).unwrap().split('&').next().unwrap().to_string();
        assert!(code.starts_with("code-"));
        assert!(t.port.login_callback(&pasted));
        let seen = until(&mut t.rx, |e| matches!(e, UiEvent::Message(m) if m.text.contains("logged in · connected"))).await;
        assert!(seen.iter().any(|e| matches!(e, UiEvent::Message(m) if m.role == Role::User && m.text == "(login callback)")));
        assert!(texts(&seen).contains(&"Got it: finishing the login to remote…".to_string()));
        assert_eq!(
            texts(&seen).last().unwrap(),
            "remote: logged in · connected · 1 tool. Marv can use them from your next message."
        );
        assert!(!format!("{seen:?}").contains(&code), "the code is never shown");
        assert!(t.mcp.specs().iter().any(|s| s.name == "mcp__remote__echo"));

        // The same address again: this login has had its callback.
        assert!(t.port.login_callback(&pasted));
        t.says("No login is waiting for that address (/mcp login <server> starts one). It wasn't sent to the model.").await;

        t.port.run_command("/mcp logout remote".into());
        let said = t.says("Logged out of remote").await;
        assert_eq!(
            said.last().unwrap(),
            "Logged out of remote: its tokens are forgotten. The authorization server revoked the grant."
        );
        assert_eq!(server.revoked.lock().unwrap().len(), 1);
        assert_eq!(t.mcp.status()[0].state, marv_mcp::manager::McpState::NeedsLogin);
        t.port.run_command("/mcp logout remote".into());
        t.says("remote isn't logged in.").await;
        t.mcp.close().await;
    }

    async fn logged_in(t: &mut Mcp) {
        t.notice("needs a login").await;
        t.port.run_command("/mcp login remote".into());
        let url = opened(t).await;
        assert!(t.port.login_callback(&location(&url).await));
        t.says("logged in · connected").await;
    }

    #[tokio::test]
    async fn a_login_that_runs_out_mid_session_is_told_once() {
        let dirs = Dirs::new();
        let (mut t, server) = remote(&dirs).await;
        logged_in(&mut t).await;
        server.expired.store(true, Ordering::SeqCst);
        let echo = t.mcp.tools().iter().find(|tool| tool.name == "mcp__remote__echo").cloned().unwrap();
        for _ in 0..2 {
            let failed = echo.run(Default::default(), CancellationToken::new()).await;
            assert!(failed.unwrap_err().to_string().contains("remote needs a new login: the user can run /mcp login remote"));
        }
        let notice = t.notice("needs a new login").await;
        assert_eq!(notice.items[0].text, "remote needs a new login: /mcp login remote");
        assert_eq!(notice.items[0].detail.as_deref(), Some(&["Its tools fail until then.".to_string()][..]));
        t.port.run_command("/exit".into());
        let rest = until(&mut t.rx, |e| *e == UiEvent::Exit).await;
        assert!(!format!("{rest:?}").contains("needs a new login"), "told once: {rest:?}");
        t.mcp.close().await;
    }

    #[tokio::test]
    async fn a_callback_with_no_login_waiting_is_refused_and_ordinary_text_isnt_taken() {
        let dirs = Dirs::new();
        let mut t = start(&dirs, None, Net::default());
        assert!(!t.port.login_callback("hello"));
        assert!(t.port.login_callback("http://127.0.0.1:5555/callback?code=code-secret&state=abc"));
        let seen = until(&mut t.rx, |e| matches!(e, UiEvent::Message(m) if m.text.starts_with("No login is waiting"))).await;
        assert!(!format!("{seen:?}").contains("code-secret"));
    }

    #[tokio::test]
    async fn a_login_to_an_unknown_server_says_so() {
        let dirs = Dirs::new();
        let mut t = start(&dirs, None, Net::default());
        t.port.run_command("/mcp login nope".into());
        let seen = until(&mut t.rx, |e| matches!(e, UiEvent::Message(_))).await;
        assert!(matches!(seen.last(), Some(UiEvent::Message(m)) if m.is_error), "{seen:?}");
        let _ = (&t.bin, &t.bridge());
    }
}

#[test]
fn startup_problems_are_one_notice_in_the_worst_tone() {
    assert_eq!(startup_notice(vec![]), None);
    let notice = startup_notice(vec![
        (NoticeTone::Warning, hidden_path_folders_item(&["~/.local/share/mise/bin".into()])),
        (NoticeTone::Error, NoticeItem { text: "1 skill couldn't be loaded (see /skills)".into(), detail: None }),
    ])
    .unwrap();
    assert_eq!((notice.title.as_str(), notice.tone), ("Startup · 2 notes", NoticeTone::Error));
    assert_eq!(notice.items[0].text, "1 PATH folder is hidden from the sandbox: ~/.local/share/mise/bin");
}
