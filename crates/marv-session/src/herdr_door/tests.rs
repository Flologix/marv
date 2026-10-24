//! The Herdr door (#74) against a fake `herdr` (`fake.rs`): never the user's real Herdr.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use marv_core::{Door, Provenance, ToolCall};
use marv_tools::{ApprovalRequest, Approver, Decision, ToolContext, ToolRef, run_tool};
use serde_json::json;

use super::fake::{fake_herdr_log, fake_setup};
use super::*;

struct Dir(PathBuf);
impl Dir {
    fn new(tag: &str) -> Self {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("marv-herdr-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(std::fs::canonicalize(dir).unwrap())
    }
    fn path(&self) -> &Path {
        &self.0
    }
}
impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Recorder(Decision, Mutex<Vec<ApprovalRequest>>);
impl Recorder {
    fn new(decision: Decision) -> Arc<Self> {
        Arc::new(Self(decision, Mutex::default()))
    }
    fn asked(&self) -> Vec<ApprovalRequest> {
        self.1.lock().unwrap().clone()
    }
}
#[async_trait]
impl Approver for Recorder {
    async fn approve(&self, request: ApprovalRequest) -> Decision {
        self.1.lock().unwrap().push(request);
        self.0.clone()
    }
}

/// A project, a fake Herdr, the door (at most `max` panes) and a context with `decision`'s approver and the door open.
struct Rig {
    project: Dir,
    herdr: Dir,
    tools: Vec<ToolRef>,
    peers: Arc<Peers>,
    approver: Arc<Recorder>,
    ctx: ToolContext,
}

fn rig(tag: &str, max: u32, decision: Decision) -> Rig {
    let (project, herdr) = (Dir::new(&format!("{tag}-project")), Dir::new(&format!("{tag}-herdr")));
    std::fs::create_dir_all(project.path().join("src")).unwrap();
    let peers: Arc<Peers> = Arc::default();
    let tools: Vec<ToolRef> = vec![Arc::new(HerdrDoor::new(fake_setup(herdr.path(), max), peers.clone()))];
    let approver = Recorder::new(decision);
    let mut ctx = ToolContext::new(project.path());
    ctx.approver = Some(approver.clone());
    ctx.doors.herdr = true;
    Rig { project, herdr, tools, peers, approver, ctx }
}

fn call(args: serde_json::Value) -> ToolCall {
    ToolCall { id: "h1".into(), name: HERDR.into(), arguments: args.to_string() }
}

impl Rig {
    async fn run(&self, args: serde_json::Value) -> marv_tools::ToolOutcome {
        run_tool(&call(args), &self.ctx, &self.tools).await
    }
    fn log(&self) -> Vec<String> {
        fake_herdr_log(self.herdr.path())
    }
}

#[tokio::test]
async fn reads_run_without_asking_and_come_back_fenced_and_tainting() {
    let r = rig("reads", 4, Decision::No);
    for args in [
        json!({"action": "list", "what": "workspaces"}),
        json!({"action": "list"}),
        json!({"action": "status", "pane": "w1:p2"}),
        json!({"action": "read", "pane": "w1:p2", "lines": 5}),
    ] {
        let out = r.run(args.clone()).await;
        assert!(!out.result.is_error, "{args}: {}", out.result.output);
        assert_eq!(out.result.provenance, Provenance::Herdr, "{args}");
        assert_eq!(out.result.door, Some(Door::Herdr));
    }
    assert!(r.approver.asked().is_empty(), "reads ask no one");
    assert_eq!(r.ctx.taint.sources(), vec![Provenance::Herdr]);
    assert!(r.log().contains(&"pane read w1:p2 --source recent-unwrapped --lines 5 --format text".to_string()), "{:?}", r.log());
    // As the session's runner hands it to the model: fenced as Herdr's, the closing tag inside defused.
    let read = r.run(json!({"action": "read", "pane": "w1:p2"})).await;
    // Herdr's plain text, not `null` (#138), with its control characters dropped.
    assert!(read.result.output.starts_with("[2Jall tests pass\n"), "{:?}", read.result.output);
    assert!(!read.result.output.contains('\u{1b}'));
    let run = crate::runner::tool_run_from(read, None);
    assert!(run.result.output.starts_with("<untrusted-data source=\"herdr\""), "{}", run.result.output);
    assert_eq!(run.result.output.to_ascii_lowercase().matches("</untrusted-data").count(), 1, "{}", run.result.output);
}

#[tokio::test]
async fn a_read_of_a_pane_herdr_doesnt_have_fails_with_its_code_and_asks_no_one() {
    let r = rig("read-gone", 4, Decision::No);
    let out = r.run(json!({"action": "read", "pane": "w1:p9"})).await;
    assert!(out.result.is_error, "{}", out.result.output);
    assert!(out.result.output.contains("herdr pane read: pane_not_found"), "{}", out.result.output);
    assert!(!out.result.output.contains("not found"), "Herdr's own message stays out: {}", out.result.output);
    assert!(r.approver.asked().is_empty());
}

#[tokio::test]
async fn light_actions_ask_with_the_door_and_may_be_remembered_except_in_a_turn_another_agent_started() {
    let r = rig("light", 4, Decision::Yes);
    r.run(json!({"action": "focus", "target": "w1:t1"})).await;
    r.run(json!({"action": "rename", "target": "w1:p2", "name": "reviewer"})).await;
    let typed = r.run(json!({"action": "prompt", "pane": "w1:p2", "text": "review the diff"})).await;
    assert!(!typed.result.is_error, "{}", typed.result.output);
    let asked: Vec<(String, bool, Option<Door>)> =
        r.approver.asked().iter().map(|a| (a.scope.key.clone(), a.once, a.door)).collect();
    assert_eq!(
        asked,
        [
            ("herdr:focus".into(), false, Some(Door::Herdr)),
            ("herdr:rename".into(), false, Some(Door::Herdr)),
            ("herdr:prompt:w1:p2".into(), false, Some(Door::Herdr)),
        ]
    );
    let log = r.log();
    for line in
        ["tab focus w1:t1", "pane rename w1:p2 reviewer", "pane send-text w1:p2 review the diff", "pane send-keys w1:p2 enter"]
    {
        assert!(log.contains(&line.to_string()), "{line}: {log:?}");
    }
    // A woken turn: asked once, so no "always" answers it.
    let woken = ToolContext { mode: marv_tools::CallMode::Woken, ..r.ctx.clone() };
    run_tool(&call(json!({"action": "focus", "target": "w1"})), &woken, &r.tools).await;
    assert!(r.approver.asked().last().unwrap().once);
}

#[tokio::test]
async fn a_prompt_that_starts_like_a_command_is_refused_before_anyone_is_asked() {
    let r = rig("prompt-command", 4, Decision::Yes);
    for text in ["/exit", "!rm -rf ~", "# remember", "  /model x"] {
        let out = r.run(json!({"action": "prompt", "pane": "w1:p2", "text": text})).await;
        assert!(out.result.is_error && out.result.output.contains("not a command"), "{text}: {}", out.result.output);
    }
    // Marv's own pane and a pane without an agent take no prompt either.
    assert!(r.run(json!({"action": "prompt", "pane": "w1:p1", "text": "hi"})).await.result.is_error);
    assert!(r.approver.asked().is_empty());
    assert!(!r.log().iter().any(|l| l.starts_with("pane send")), "{:?}", r.log());
}

#[tokio::test]
async fn run_asks_every_time_with_the_exact_command_even_in_yolo_and_opens_a_pane() {
    let mut r = rig("run", 4, Decision::Always);
    r.ctx.yolo = true;
    let args = json!({"action": "run", "command": "cargo watch -x test", "cwd": "src"});
    let out = r.run(args.clone()).await;
    assert!(!out.result.is_error, "{}", out.result.output);
    r.run(args).await;
    let asked = r.approver.asked();
    assert_eq!(asked.len(), 2, "never remembered, even after \"always\" and in yolo");
    assert!(asked.iter().all(|a| a.once && a.door == Some(Door::Herdr)));
    assert_eq!(asked[0].preview.command.as_deref(), Some("cargo watch -x test"));
    assert!(
        asked[0].preview.note.as_deref().unwrap().starts_with("in src · a new pane right of Marv's"),
        "{:?}",
        asked[0].preview.note
    );
    let src = r.project.path().join("src").to_string_lossy().to_string();
    let log = r.log();
    assert!(log.contains(&format!("pane split w1:p1 --direction right --cwd {src} --no-focus")), "{log:?}");
    assert!(log.contains(&"pane run w1:p5 cargo watch -x test".to_string()), "{log:?}");
    assert_eq!(r.peers.list().iter().map(|p| p.kind.as_str()).collect::<Vec<_>>(), ["command", "command"]);
}

#[tokio::test]
async fn run_and_spawn_are_refused_before_anyone_is_asked_without_the_door() {
    let mut r = rig("no-door", 4, Decision::Yes);
    r.ctx.doors.herdr = false;
    for args in
        [json!({"action": "run", "command": "echo hi"}), json!({"action": "spawn", "kind": "marv", "task": "fix the tests"})]
    {
        let out = r.run(args).await;
        assert_eq!(out.result.output, NO_HERDR_DOOR);
    }
    assert!(r.approver.asked().is_empty());
    assert!(r.log().is_empty(), "Herdr heard nothing: {:?}", r.log());
}

#[tokio::test]
async fn a_spawned_marv_gets_no_yolo_unless_its_parent_has_it_and_its_task_after_its_prompt() {
    let r = rig("spawn-marv", 4, Decision::Yes);
    let out = r.run(json!({"action": "spawn", "kind": "marv", "task": "fix the failing test", "name": "fixer"})).await;
    assert!(!out.result.is_error, "{}", out.result.output);
    assert!(out.result.output.contains("herdr.pane.w1-p5.send"), "{}", out.result.output);
    let asked = r.approver.asked();
    assert_eq!(asked[0].preview.command.as_deref(), Some("MARV_YOLO=off /opt/marv/bin/marv"));
    assert_eq!(asked[0].preview.text.as_deref(), Some("fix the failing test"));
    let log = r.log();
    let at = |line: &str| log.iter().position(|l| l == line).unwrap_or_else(|| panic!("{line}: {log:?}"));
    assert!(at("pane run w1:p5 MARV_YOLO=off /opt/marv/bin/marv") < at("pane send-text w1:p5 fix the failing test"));
    assert!(!log.iter().any(|l| l.contains("--approve")), "{log:?}");
    // A parent in yolo starts its worker in yolo (the config's setting), and nothing more.
    let yolo = ToolContext { yolo: true, ..r.ctx.clone() };
    assert_eq!(marv_command(Path::new("/opt/marv/bin/marv"), &yolo), "/opt/marv/bin/marv");
    let woken = ToolContext { mode: marv_tools::CallMode::Woken, ..yolo };
    assert_eq!(marv_command(Path::new("/opt/marv/bin/marv"), &woken), "MARV_YOLO=off /opt/marv/bin/marv");
    assert_eq!(r.peers.list()[0].name, "fixer");
}

#[tokio::test]
async fn another_agent_kind_starts_without_arguments_and_a_branch_gets_a_worktree() {
    let r = rig("spawn-claude", 4, Decision::Yes);
    let out = r.run(json!({"action": "spawn", "kind": "claude", "task": "review it", "branch": "feat/x", "base": "main"})).await;
    assert!(!out.result.is_error, "{}", out.result.output);
    let root = r.project.path().to_string_lossy().to_string();
    let log = r.log();
    // The default name carries this session's own prefix, so a worker's own workers don't collide with its parent's.
    let name = r.peers.list()[0].name.clone();
    assert!(name.starts_with("peer-") && name.ends_with("-1") && name.len() == "peer-abcd-1".len(), "{name}");
    assert!(
        log.contains(&format!("worktree create --cwd {root} --branch feat/x --base main --no-focus --label {name}")),
        "{log:?}"
    );
    assert!(log.contains(&format!("agent start {name} --kind claude --pane w1:p5")), "{log:?}");
    assert!(log.contains(&"pane send-text w1:p5 review it".to_string()), "{log:?}");
    let warning = r.approver.asked()[0].preview.warning.clone().unwrap();
    assert!(warning.contains("claude runs with its own permission settings"), "{warning}");
    assert_eq!(r.peers.list()[0].branch.as_deref(), Some("feat/x"));
}

#[tokio::test]
async fn the_peer_limit_holds_before_anyone_is_asked() {
    let r = rig("limit", 1, Decision::Yes);
    assert!(!r.run(json!({"action": "run", "command": "echo one"})).await.result.is_error);
    let second = r.run(json!({"action": "spawn", "kind": "marv", "task": "two"})).await;
    assert!(second.result.is_error && second.result.output.contains("opened its 1 Herdr pane"), "{}", second.result.output);
    assert_eq!(r.approver.asked().len(), 1);
    assert_eq!(r.log().iter().filter(|l| l.starts_with("pane split")).count(), 1);
}

#[test]
fn inputs_are_checked_before_anything_runs() {
    let door = HerdrDoor::new(
        HerdrSetup { program: "/nonexistent/herdr".into(), own_pane: None, marv: "/m".into(), max_peers: 1, runs: true },
        Arc::default(),
    );
    let bad = [
        json!({"action": "run", "command": "echo a\necho b"}),
        json!({"action": "run", "command": "echo \u{1b}[2J"}),
        json!({"action": "spawn", "kind": "bash", "task": "x"}),
        json!({"action": "spawn", "kind": "marv", "task": "x", "name": "Bad Name"}),
        json!({"action": "spawn", "kind": "marv", "task": "x", "base": "main"}),
        json!({"action": "spawn", "kind": "marv", "task": "x", "branch": "--force"}),
        json!({"action": "read", "pane": "w1 p2"}),
        json!({"action": "read"}),
        json!({"action": "rename", "target": "w1:p2", "name": "a\nb"}),
        json!({"action": "run", "command": "echo", "branch": "feat/x"}),
        json!({"action": "spawn", "kind": "claude", "task": "x", "args": ["--dangerously-skip-permissions"]}),
    ];
    for args in bad {
        assert!(marv_tools::DynTool::parse(&door, args.clone()).is_err(), "{args}");
    }
    assert!(
        marv_tools::DynTool::parse(&door, json!({"action": "spawn", "kind": "codex", "task": "x", "branch": "feat/y"})).is_ok()
    );
}

#[test]
fn the_peers_list_goes_through_printable() {
    let peer = Peer {
        name: "w\u{1b}]0;x\u{7}".into(),
        pane: "w1:p5".into(),
        kind: "marv".into(),
        what: "fix \u{1b}[2J it".into(),
        place: ".".into(),
        branch: None,
        started_ms: 0,
    };
    let text = peers_text(&[peer], &[Some("idle".into())]);
    assert!(!text.contains('\u{1b}') && !text.contains('\u{7}'), "{text:?}");
    assert!(text.contains("w1:p5") && text.contains("idle"), "{text}");
}

#[tokio::test]
async fn nothing_starting_with_a_dash_reaches_herdr() {
    let r = rig("dash", 4, Decision::Yes);
    for args in [
        json!({"action": "read", "pane": "--help"}),
        json!({"action": "status", "pane": "-h"}),
        json!({"action": "list", "what": "tabs", "workspace": "--version"}),
        json!({"action": "focus", "target": "-x:p1"}),
        json!({"action": "rename", "target": "w1:p2", "name": "--help"}),
        json!({"action": "prompt", "pane": "w1:p2", "text": "--help"}),
        json!({"action": "run", "command": "--help"}),
        json!({"action": "spawn", "kind": "marv", "task": "-v"}),
    ] {
        let out = r.run(args.clone()).await;
        assert!(out.result.is_error, "{args}: {}", out.result.output);
    }
    assert!(r.approver.asked().is_empty());
    // The prompt's preview reads the pane first; nothing else reached Herdr.
    assert!(r.log().iter().all(|l| l == "pane get w1:p2"), "{:?}", r.log());
}

#[test]
fn a_run_command_shows_what_runs_and_points_an_agent_to_spawn() {
    let door = HerdrDoor::new(
        HerdrSetup { program: "/nonexistent/herdr".into(), own_pane: None, marv: "/m".into(), max_peers: 1, runs: true },
        Arc::default(),
    );
    for bad in [
        "echo hi!",
        "echo a\u{200B}b",
        "echo \u{202E}x",
        "ls \u{E0041}",
        "^ls^rm -rf ~^",
        " ^a^b",
        "echo a\u{2028}b",
        "echo a\u{2029}b",
        "echo \u{115F}\u{1160}",
        "echo \u{3164}",
        "echo \u{FFA0}",
    ] {
        assert!(marv_tools::DynTool::parse(&door, json!({"action": "run", "command": bad})).is_err(), "{bad:?}");
    }
    assert_eq!(agent_in_command("claude --dangerously-skip-permissions"), Some("claude"));
    assert_eq!(agent_in_command("FOO=1 /usr/local/bin/marv"), Some("marv"));
    assert_eq!(agent_in_command("cargo watch"), None);
}

#[tokio::test]
async fn in_a_tainted_session_a_prompt_or_spawn_says_whose_words_and_a_marv_starts_tainted() {
    let r = rig("tainted", 4, Decision::Yes);
    r.ctx.taint.mark(Provenance::Web);
    let prompt = r.run(json!({"action": "prompt", "pane": "w1:p2", "text": "review"})).await;
    assert!(!prompt.result.is_error, "{}", prompt.result.output);
    r.run(json!({"action": "spawn", "kind": "marv", "task": "fix"})).await;
    let asked = r.approver.asked();
    assert!(asked.iter().all(|a| a.must_ask), "tainted: asked as egress");
    assert_eq!(asked[0].preview.warning.as_deref(), Some(OWN_WORDS));
    assert!(asked[1].preview.warning.as_deref().unwrap().contains(OWN_WORDS));
    assert_eq!(asked[1].preview.command.as_deref(), Some("MARV_TAINTED=1 MARV_YOLO=off /opt/marv/bin/marv"));
}

#[tokio::test]
async fn panes_gone_from_herdr_dont_count_against_the_limit() {
    let r = rig("prune", 1, Decision::Yes);
    assert!(!r.run(json!({"action": "run", "command": "echo one"})).await.result.is_error);
    super::fake::close_pane_outside(r.herdr.path(), "w1:p5");
    let again = r.run(json!({"action": "run", "command": "echo two"})).await;
    assert!(!again.result.is_error, "{}", again.result.output);
    assert_eq!(r.peers.list().iter().map(|p| p.pane.as_str()).collect::<Vec<_>>(), ["w1:p6"]);
}

#[test]
fn each_session_names_its_peers_with_its_own_prefix() {
    let (a, b) = (Peers::default(), Peers::default());
    assert_ne!(a.free_name("peer"), b.free_name("peer"));
}
