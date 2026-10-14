//! Ports `tests/session-checkpoints.test.ts`: a session's turns take checkpoints, and `undo_plan()`/`undo()` take
//! them back. Temp git repositories only.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use futures::StreamExt;
use marv_agent::LoopEvent;
use marv_core::{ChatTurn, ProviderEvent, StopReason, ToolCall};
use marv_provider::{EventStream, Provider, ProviderFactory, StreamOptions};
use marv_tools::{ApprovalRequest, Approver, Decision};

use super::store::{CheckpointStore, SnapshotFailure, SnapshotResult, StoreOptions};
use super::test_support::{Temp, git, read, temp, write};
use crate::session::{
    Configure, MarvSession, ProviderChoice, ResumeTarget, SendOptions, SessionEvent, SessionInit, Turn, TurnEndReason,
};
use crate::sessions::SessionStore;
use crate::trajectory::TrajectoryStore;

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// A model that follows a script; records every request's history.
#[derive(Clone, Default)]
struct Scripted {
    steps: Arc<Mutex<VecDeque<Vec<ProviderEvent>>>>,
    requests: Arc<Mutex<Vec<Vec<ChatTurn>>>>,
}

impl Scripted {
    fn new(steps: Vec<Vec<ProviderEvent>>) -> Self {
        Self { steps: Arc::new(Mutex::new(steps.into())), requests: Arc::default() }
    }
    fn requests(&self) -> Vec<Vec<ChatTurn>> {
        lock(&self.requests).clone()
    }
}

impl Provider for Scripted {
    fn name(&self) -> &str {
        "scripted"
    }
    fn context_length(&self) -> Option<u64> {
        None
    }
    fn stream(&self, history: &[ChatTurn], _options: StreamOptions) -> EventStream {
        lock(&self.requests).push(history.to_vec());
        let step = lock(&self.steps).pop_front().unwrap_or_else(|| say("(script ended)"));
        Box::pin(futures::stream::iter(step).then(|e| async move {
            tokio::task::yield_now().await;
            e
        }))
    }
}

/// Answers by the first user message: the scripts whose key it contains.
struct Routed(Vec<(String, Scripted)>);
impl Provider for Routed {
    fn name(&self) -> &str {
        "routed"
    }
    fn context_length(&self) -> Option<u64> {
        None
    }
    fn stream(&self, history: &[ChatTurn], options: StreamOptions) -> EventStream {
        let first = history
            .iter()
            .find_map(|t| if let ChatTurn::User { text } = t { Some(text.clone()) } else { None })
            .unwrap_or_default();
        let (_, script) = self.0.iter().find(|(key, _)| first.contains(key.as_str())).expect("a script for this prompt");
        script.stream(history, options)
    }
}

struct Fixed(Arc<dyn Provider>);
impl ProviderFactory for Fixed {
    fn id(&self) -> &str {
        "test"
    }
    fn model(&self) -> &str {
        "test-model"
    }
    fn make(&self, _model: Option<&str>, _info: Option<&marv_provider::ModelInfo>) -> Arc<dyn Provider> {
        self.0.clone()
    }
}

struct Yes;
#[async_trait::async_trait]
impl Approver for Yes {
    async fn approve(&self, _request: ApprovalRequest) -> Decision {
        Decision::Yes
    }
}

fn say(text: &str) -> Vec<ProviderEvent> {
    vec![ProviderEvent::TextDelta(text.into()), ProviderEvent::Done { reason: Some("stop".into()) }]
}
fn use_tools(calls: Vec<ToolCall>) -> Vec<ProviderEvent> {
    let mut events: Vec<ProviderEvent> = calls.into_iter().map(ProviderEvent::ToolCall).collect();
    events.push(ProviderEvent::Done { reason: Some("tool_calls".into()) });
    events
}
fn call(id: &str, name: &str, args: serde_json::Value) -> ToolCall {
    ToolCall { id: id.into(), name: name.into(), arguments: args.to_string() }
}
fn write_call(id: &str, path: &str, content: &str) -> ToolCall {
    call(id, "write_file", serde_json::json!({ "path": path, "content": content }))
}
fn bash(id: &str, command: &str) -> ToolCall {
    call(id, "bash", serde_json::json!({ "command": command }))
}

/// A temp git repository (one commit: a.txt "one") and a temp folder for stores.
struct Dirs {
    project: Temp,
    dir: Temp,
}

impl Dirs {
    fn new() -> Self {
        let project = temp("cps-project");
        git(&project.0, &["init", "-q", "-b", "main"]);
        write(project.0.join("a.txt"), "one\n");
        git(&project.0, &["add", "-A"]);
        git(
            &project.0,
            &["-c", "user.email=t@t", "-c", "user.name=t", "-c", "commit.gpgSign=false", "commit", "-q", "-m", "first"],
        );
        Self { project, dir: temp("cps") }
    }
    fn root(&self) -> &Path {
        &self.project.0
    }
    fn refs(&self) -> String {
        git(self.root(), &["for-each-ref", "--format=%(refname)", "refs/marv/"])
    }
    fn subjects(&self, session: &MarvSession) -> Vec<String> {
        git(self.root(), &["log", "--format=%s", &format!("refs/marv/checkpoints/{}", session.id())])
            .lines()
            .map(String::from)
            .collect()
    }
    fn sessions(&self) -> Arc<SessionStore> {
        Arc::new(SessionStore::new(self.dir.0.join("sessions")))
    }
}

fn make(dirs: &Dirs, provider: Arc<dyn Provider>, edit: impl FnOnce(&mut SessionInit)) -> MarvSession {
    let mut init = SessionInit::new(dirs.root(), ProviderChoice::Factory(Arc::new(Fixed(provider))));
    init.sandbox = false;
    init.approve = Some(Arc::new(Yes));
    init.diagnostics = false;
    init.checks = false;
    edit(&mut init);
    MarvSession::new(init).unwrap()
}
fn scripted(dirs: &Dirs, provider: &Scripted, edit: impl FnOnce(&mut SessionInit)) -> MarvSession {
    make(dirs, Arc::new(provider.clone()), edit)
}

fn go(session: &MarvSession, text: &str) -> Turn {
    session.send(text, SendOptions::default()).unwrap()
}
async fn collect(mut turn: Turn) -> Vec<SessionEvent> {
    let mut out = Vec::new();
    while let Some(e) = turn.next().await {
        out.push(e);
    }
    out
}

fn last_user_text(history: &[ChatTurn]) -> String {
    match history.last() {
        Some(ChatTurn::User { text }) => text.clone(),
        other => panic!("not a user message: {other:?}"),
    }
}

async fn plan_of(session: &MarvSession) -> super::undo::UndoPlan {
    session.undo_plan().await.unwrap().unwrap_or_else(|r| panic!("{}", r.reason))
}
async fn refusal_of(session: &MarvSession) -> String {
    match session.undo_plan().await.unwrap() {
        Ok(plan) => panic!("a plan for turn {}", plan.turn),
        Err(refusal) => refusal.reason,
    }
}

#[tokio::test]
async fn a_read_only_turn_takes_none() {
    let dirs = Dirs::new();
    let provider =
        Scripted::new(vec![use_tools(vec![call("r", "read_file", serde_json::json!({"path": "a.txt"}))]), say("read")]);
    collect(go(&scripted(&dirs, &provider, |_| {}), "look")).await;
    assert_eq!(dirs.refs(), "");
}

#[tokio::test]
async fn write_file_and_a_bash_edit_one_pair_covering_both() {
    let dirs = Dirs::new();
    let provider = Scripted::new(vec![
        use_tools(vec![write_call("w", "new.txt", "new\n")]),
        use_tools(vec![bash("b", "echo two > a.txt")]),
        say("done"),
    ]);
    let session = scripted(&dirs, &provider, |_| {});
    collect(go(&session, "change things")).await;
    assert_eq!(dirs.subjects(&session), ["marv checkpoint: turn 1 after", "marv checkpoint: turn 1 before"]);
    let plan = plan_of(&session).await;
    let mut files: Vec<(String, super::undo::UndoAction)> = plan.files.iter().map(|f| (f.path.clone(), f.action)).collect();
    files.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        files,
        [("a.txt".to_string(), super::undo::UndoAction::Restore), ("new.txt".to_string(), super::undo::UndoAction::Delete)]
    );
    assert_eq!(plan.label, "change things");
}

#[tokio::test]
async fn a_no_op_bash_turns_pair_is_dropped() {
    let dirs = Dirs::new();
    let session = scripted(&dirs, &Scripted::new(vec![use_tools(vec![bash("b", "ls")]), say("listed")]), |_| {});
    collect(go(&session, "list")).await;
    assert_eq!(dirs.refs(), "");
    assert!(refusal_of(&session).await.contains("nothing to undo"));
}

/// Writes on its first request, then works until it's stopped.
struct WriteThenHang(Mutex<usize>);
impl Provider for WriteThenHang {
    fn name(&self) -> &str {
        "hang"
    }
    fn context_length(&self) -> Option<u64> {
        None
    }
    fn stream(&self, _history: &[ChatTurn], options: StreamOptions) -> EventStream {
        let first = {
            let mut n = lock(&self.0);
            *n += 1;
            *n == 1
        };
        if first {
            return Box::pin(futures::stream::iter(use_tools(vec![write_call("w", "x.txt", "x\n")])));
        }
        let cancel = options.cancel.clone();
        Box::pin(futures::stream::once(async move {
            cancel.cancelled().await;
            ProviderEvent::Done { reason: None }
        }))
    }
}

#[tokio::test]
async fn the_after_is_taken_on_esc_and_at_the_step_limit() {
    let dirs = Dirs::new();
    let session = make(&dirs, Arc::new(WriteThenHang(Mutex::new(0))), |_| {});
    let mut turn = go(&session, "go");
    while let Some(event) = turn.next().await {
        if matches!(event, SessionEvent::Loop(LoopEvent::ToolEnd { .. })) {
            let session = session.clone();
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                session.interrupt();
            });
        }
    }
    assert_eq!(dirs.subjects(&session)[0], "marv checkpoint: turn 1 after");

    let limited = scripted(
        &dirs,
        &Scripted::new(vec![use_tools(vec![write_call("w", "y.txt", "y\n")]), use_tools(vec![write_call("w2", "z.txt", "z\n")])]),
        |init| {
            init.approve = None;
            init.yolo = true;
            init.max_steps = Some(1);
        },
    );
    collect(go(&limited, "go")).await;
    assert_eq!(dirs.subjects(&limited)[0], "marv checkpoint: turn 1 after");
}

#[tokio::test]
async fn undo_restores_the_next_message_tells_the_model_every_request_extends_the_previous_one() {
    let dirs = Dirs::new();
    let provider =
        Scripted::new(vec![use_tools(vec![write_call("w", "a.txt", "changed\n")]), say("changed it"), say("ok"), say("ok")]);
    let session = scripted(&dirs, &provider, |_| {});
    collect(go(&session, "add the parser")).await;
    let plan = plan_of(&session).await;
    let result = session.undo(&plan).await.unwrap();
    assert_eq!(result.restored, ["a.txt"]);
    assert_eq!(read(dirs.root().join("a.txt")), "one\n");
    collect(go(&session, "what now?")).await;
    let last = last_user_text(provider.requests().last().unwrap());
    assert!(
        last.starts_with(
            "Marv: the user undid your changes from an earlier turn (\"add the parser\"): a.txt is back as it was before it."
        ),
        "{last}"
    );
    assert!(last.ends_with("what now?"));
    let requests = provider.requests();
    for pair in requests.windows(2) {
        assert_eq!(pair[1][..pair[0].len()], pair[0][..]);
    }
    // Only once.
    collect(go(&session, "and now?")).await;
    assert_eq!(last_user_text(provider.requests().last().unwrap()), "and now?");
}

/// Works until it's stopped.
struct Hang;
impl Provider for Hang {
    fn name(&self) -> &str {
        "hang"
    }
    fn context_length(&self) -> Option<u64> {
        None
    }
    fn stream(&self, _history: &[ChatTurn], options: StreamOptions) -> EventStream {
        let cancel = options.cancel.clone();
        Box::pin(futures::stream::once(async move {
            cancel.cancelled().await;
            ProviderEvent::Done { reason: None }
        }))
    }
}

#[tokio::test]
async fn undo_plan_and_undo_refuse_during_a_turn() {
    let dirs = Dirs::new();
    let session = make(&dirs, Arc::new(Hang), |_| {});
    let turn = go(&session, "go");
    let refused = session.undo_plan().await.unwrap_err();
    assert!(refused.to_string().contains("working on a turn"));
    turn.finish().await;
}

#[tokio::test]
async fn checkpoints_off_init_or_configure_takes_none() {
    let dirs = Dirs::new();
    let off = scripted(&dirs, &Scripted::new(vec![use_tools(vec![write_call("w", "n.txt", "n\n")]), say("ok")]), |init| {
        init.checkpoints = false
    });
    collect(go(&off, "go")).await;
    assert_eq!(dirs.refs(), "");
    assert!(refusal_of(&off).await.contains("off"));
    let on = scripted(&dirs, &Scripted::new(vec![use_tools(vec![write_call("w", "m.txt", "m\n")]), say("ok")]), |_| {});
    on.configure(Configure { checkpoints: Some(false), ..Default::default() }).unwrap();
    collect(go(&on, "go")).await;
    assert_eq!(dirs.refs(), "");
}

#[tokio::test]
async fn clear_starts_a_new_chain_resume_finds_the_old_one() {
    let dirs = Dirs::new();
    let provider = Scripted::new(vec![
        use_tools(vec![write_call("w", "first.txt", "1\n")]),
        say("ok"),
        use_tools(vec![write_call("w2", "second.txt", "2\n")]),
        say("ok"),
    ]);
    let session = scripted(&dirs, &provider, |init| init.sessions = Some(dirs.sessions()));
    collect(go(&session, "first")).await;
    let old = session.id();
    session.clear().await.unwrap();
    assert!(refusal_of(&session).await.contains("nothing to undo"));
    collect(go(&session, "second")).await;
    assert_eq!(plan_of(&session).await.label, "second");
    session.flush().await;
    session.resume(ResumeTarget::Id(old)).await.unwrap().unwrap();
    assert_eq!(plan_of(&session).await.label, "first");
    assert_eq!(dirs.refs().lines().count(), 2);
}

/// A session whose store's chain can't be read at resume: "first" on its own chain, then resumed through a failing
/// read, then "again".
async fn recovered(dirs: &Dirs) -> (MarvSession, String, Arc<CheckpointStore>) {
    let store = Arc::new(CheckpointStore::new(StoreOptions::new(dirs.root(), None, "x")));
    let provider = Scripted::new(vec![
        use_tools(vec![write_call("w", "first.txt", "1\n")]),
        say("ok"),
        use_tools(vec![write_call("w2", "again.txt", "2\n")]),
        say("ok"),
    ]);
    let session = scripted(dirs, &provider, |init| {
        init.sessions = Some(dirs.sessions());
        init.checkpoint_store = Some(store.clone());
    });
    collect(go(&session, "first")).await;
    let old = session.id();
    session.flush().await;
    session.clear().await.unwrap();
    lock(&store.fake).chain_error = Some("the checkpoints can't be read (git timed out)".into());
    session.resume(ResumeTarget::Id(old.clone())).await.unwrap().unwrap();
    lock(&store.fake).chain_error = None;
    (session, old, store)
}

#[tokio::test]
async fn resume_into_a_chain_that_cant_be_read_new_turns_go_on_a_chain_of_their_own() {
    let dirs = Dirs::new();
    let (session, old, store) = recovered(&dirs).await;
    assert!(session.checkpoint_problem().failure.unwrap().contains("the turns before this resume can't be undone"));
    collect(go(&session, "again")).await;
    assert!(store.reference().starts_with(&format!("refs/marv/checkpoints/{old}.")), "{}", store.reference());
    let plan = plan_of(&session).await;
    assert_eq!((plan.turn, plan.label.as_str()), (1, "again"));
    // The old chain is untouched: its turn 1 is still "first".
    assert!(git(dirs.root(), &["log", "-1", "--format=%B", &format!("refs/marv/checkpoints/{old}")]).contains("first"));
}

#[tokio::test]
async fn the_replacement_chain_is_saved_a_later_resume_finds_the_turns_made_after_the_recovery() {
    let dirs = Dirs::new();
    let (session, old, _store) = recovered(&dirs).await;
    collect(go(&session, "again")).await;
    session.flush().await;
    // Another process (or a restart) resumes it: the chain it reads is the one "again" went on.
    let later = scripted(&dirs, &Scripted::default(), |init| init.sessions = Some(dirs.sessions()));
    later.resume(ResumeTarget::Id(old)).await.unwrap().unwrap();
    let plan = plan_of(&later).await;
    assert_eq!((plan.turn, plan.label.as_str()), (1, "again"));
}

#[tokio::test]
async fn an_undos_note_for_the_model_survives_a_restart_before_the_next_message() {
    let dirs = Dirs::new();
    let sessions = dirs.sessions();
    let first = Scripted::new(vec![use_tools(vec![write_call("w", "a.txt", "changed\n")]), say("changed it")]);
    let session = scripted(&dirs, &first, |init| init.sessions = Some(sessions.clone()));
    collect(go(&session, "add the parser")).await;
    let plan = plan_of(&session).await;
    session.undo(&plan).await.unwrap();
    session.close().await;
    let saved = sessions.load(dirs.root(), &session.id()).await.unwrap();

    let provider = Scripted::new(vec![say("ok"), say("ok")]);
    let resumed = scripted(&dirs, &provider, |init| init.sessions = Some(sessions.clone()));
    resumed.resume(ResumeTarget::Id(session.id())).await.unwrap().unwrap();
    collect(go(&resumed, "what now?")).await;
    let history = &provider.requests()[0];
    // The saved conversation is sent unchanged (the prompt cache), and the note leads the new message.
    assert_eq!(history[..saved.conversation.len()], saved.conversation[..]);
    let last = last_user_text(history);
    assert!(last.starts_with("Marv: the user undid your changes from an earlier turn (\"add the parser\")"), "{last}");
    assert!(last.ends_with("what now?"));
    // Told once: not saved again after it was sent.
    resumed.flush().await;
    let again = scripted(&dirs, &provider, |init| init.sessions = Some(sessions.clone()));
    again.resume(ResumeTarget::Id(session.id())).await.unwrap().unwrap();
    collect(go(&again, "and now?")).await;
    assert_eq!(last_user_text(provider.requests().last().unwrap()), "and now?");
}

#[tokio::test]
async fn a_resumed_undo_preview_still_names_the_reported_files_it_cant_restore() {
    let dirs = Dirs::new();
    write(dirs.root().join(".gitignore"), "ignored.txt\n");
    git(dirs.root(), &["add", "-A"]);
    git(
        dirs.root(),
        &["-c", "user.email=t@t", "-c", "user.name=t", "-c", "commit.gpgSign=false", "commit", "-q", "-m", "ignore"],
    );
    let sessions = dirs.sessions();
    let provider = Scripted::new(vec![
        use_tools(vec![write_call("w", "a.txt", "two\n"), write_call("w2", "ignored.txt", "x\n")]),
        say("done"),
    ]);
    let session = scripted(&dirs, &provider, |init| init.sessions = Some(sessions.clone()));
    collect(go(&session, "change both")).await;
    assert_eq!(plan_of(&session).await.not_restored, ["ignored.txt"]);
    session.close().await;
    let resumed = scripted(&dirs, &Scripted::default(), |init| init.sessions = Some(sessions.clone()));
    resumed.resume(ResumeTarget::Id(session.id())).await.unwrap().unwrap();
    assert_eq!(plan_of(&resumed).await.not_restored, ["ignored.txt"]);
}

#[tokio::test]
async fn an_old_session_file_without_the_checkpoint_fields_still_resumes() {
    let dirs = Dirs::new();
    let sessions = dirs.sessions();
    let provider = Scripted::new(vec![use_tools(vec![write_call("w", "a.txt", "two\n")]), say("done")]);
    let session = scripted(&dirs, &provider, |init| init.sessions = Some(sessions.clone()));
    collect(go(&session, "change it")).await;
    session.close().await;
    let file: PathBuf = sessions.file(dirs.root(), &session.id());
    let mut old: serde_json::Value = serde_json::from_str(&read(&file)).unwrap();
    let fields = old.as_object_mut().unwrap();
    assert!(fields.remove("checkpoints").is_some());
    fields.remove("pendingNote");
    write(&file, old.to_string());
    let resumed = scripted(&dirs, &Scripted::default(), |init| init.sessions = Some(sessions.clone()));
    assert_eq!(resumed.resume(ResumeTarget::Id(session.id())).await.unwrap().unwrap().id, session.id());
    assert_eq!(plan_of(&resumed).await.label, "change it");
}

#[tokio::test]
async fn a_failing_snapshot_doesnt_stop_the_turn_and_says_so_once() {
    let dirs = Dirs::new();
    let store = Arc::new(CheckpointStore::new(StoreOptions::new(dirs.root(), None, "x")));
    lock(&store.fake).snapshot =
        Some(Arc::new(|_| Some(SnapshotResult::Failed { reason: SnapshotFailure::Failed, message: "disk full".into(), ms: 1 })));
    let provider =
        Scripted::new(vec![use_tools(vec![write_call("w", "a.txt", "two\n"), write_call("w2", "b.txt", "b\n")]), say("done")]);
    let session = scripted(&dirs, &provider, |init| init.checkpoint_store = Some(store.clone()));
    let events = collect(go(&session, "go")).await;
    assert_eq!(read(dirs.root().join("a.txt")), "two\n");
    assert!(dirs.root().join("b.txt").exists());
    let failed: Vec<&str> = events
        .iter()
        .filter_map(|e| if let SessionEvent::CheckpointFailed { message } = e { Some(message.as_str()) } else { None })
        .collect();
    assert_eq!(failed, ["disk full"]);
    assert!(matches!(events.last(), Some(SessionEvent::TurnEnd { reason: TurnEndReason::Done(StopReason::End), .. })));
    assert!(refusal_of(&session).await.contains("disk full"));
}

#[tokio::test]
async fn parallel_shared_folder_subagents_share_one_before() {
    let dirs = Dirs::new();
    let sub = |key: &str, file: &str| {
        Scripted::new(vec![
            use_tools(vec![write_call(&format!("{key}w"), file, &format!("{key}\n"))]),
            say(&format!("{key} done")),
        ])
    };
    let provider = Routed(vec![
        ("task-one".into(), sub("one", "one.txt")),
        ("task-two".into(), sub("two", "two.txt")),
        (
            "do the work".into(),
            Scripted::new(vec![
                use_tools(vec![
                    call("a1", "agent", serde_json::json!({"description": "one", "prompt": "task-one"})),
                    call("a2", "agent", serde_json::json!({"description": "two", "prompt": "task-two"})),
                ]),
                say("both done"),
            ]),
        ),
    ]);
    let session = make(&dirs, Arc::new(provider), |_| {});
    collect(go(&session, "do the work")).await;
    assert_eq!(dirs.subjects(&session), ["marv checkpoint: turn 1 after", "marv checkpoint: turn 1 before"]);
    let mut paths: Vec<String> = plan_of(&session).await.files.into_iter().map(|f| f.path).collect();
    paths.sort();
    assert_eq!(paths, ["one.txt", "two.txt"]);
}

#[tokio::test]
async fn trajectories_get_checkpoint_and_undo_records() {
    let dirs = Dirs::new();
    let trajectories = Arc::new(TrajectoryStore::new(dirs.dir.0.join("trajectories")));
    let provider = Scripted::new(vec![use_tools(vec![write_call("w", "t.txt", "t\n")]), say("ok")]);
    let session = scripted(&dirs, &provider, |init| init.trajectories = Some(trajectories.clone()));
    collect(go(&session, "go")).await;
    let plan = plan_of(&session).await;
    session.undo(&plan).await.unwrap();
    session.flush().await;
    let file = trajectories.open(dirs.root(), &session.id()).path;
    let records: Vec<serde_json::Value> = read(&file).lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    let checkpoints: Vec<(String, bool)> = records
        .iter()
        .filter(|r| r["type"] == "checkpoint")
        .map(|r| (r["kind"].as_str().unwrap().to_string(), r["ok"].as_bool().unwrap()))
        .collect();
    assert_eq!(checkpoints, [("before".into(), true), ("after".into(), true), ("pre-undo".into(), true)]);
    let undo = records.iter().find(|r| r["type"] == "undo").unwrap();
    assert_eq!(
        (undo["turn"].clone(), undo["paths"].clone(), undo["skipped"].clone()),
        (1.into(), serde_json::json!(["t.txt"]), serde_json::json!([]))
    );
}
