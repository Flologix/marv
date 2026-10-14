//! Marv's `tests/process-tool.test.ts`: the process tool in a session, with a real sandbox (skipped where bwrap
//! doesn't work). The `--approve process` case is marv-cli's (`headless.rs`).

mod support;

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::StreamExt;
use marv_agent::{ApprovalRecord, LoopEvent, ToolRun};
use marv_core::{ChatTurn, Door, ProviderEvent, ToolCall};
use marv_provider::{EventStream, Provider, ProviderFactory, ProviderOption, StreamOptions};
use marv_session::processes::ProcessManager;
use marv_session::{MarvSession, ProviderChoice, SendOptions, SessionBuilder, SessionEvent, SessionInit};
use marv_tools::{ApprovalRequest, Approver, Decision, ProcessState};
use serde_json::{Value, json};
use support::{Dirs, free_port, get, pgrep};

/// Replays one scripted reply per request and records the requests (Marv's `ScriptedProvider`).
#[derive(Default)]
struct Scripted {
    replies: Mutex<std::collections::VecDeque<Vec<ProviderEvent>>>,
    requests: Mutex<Vec<(Vec<ChatTurn>, StreamOptions)>>,
}

impl Scripted {
    fn new(replies: Vec<Vec<ProviderEvent>>) -> Arc<Self> {
        Arc::new(Self { replies: Mutex::new(replies.into()), requests: Mutex::default() })
    }
    fn tools(&self, request: usize) -> Vec<String> {
        self.requests.lock().unwrap()[request].1.tools.iter().map(|t| t.name.clone()).collect()
    }
    fn system(&self, request: usize) -> String {
        self.requests.lock().unwrap()[request].1.system.clone().unwrap_or_default()
    }
}

impl Provider for Scripted {
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

struct Fixed(Arc<Scripted>);

impl ProviderFactory for Fixed {
    fn id(&self) -> &str {
        "test"
    }
    fn model(&self) -> &str {
        "test-model"
    }
    fn make(&self, _: Option<&str>, _: Option<&marv_provider::ModelInfo>) -> Arc<dyn Provider> {
        self.0.clone()
    }
}

/// Answers every request with `decision` and keeps them.
struct Asked(Mutex<Vec<ApprovalRequest>>, Decision);

impl Asked {
    fn new(decision: Decision) -> Arc<Self> {
        Arc::new(Self(Mutex::default(), decision))
    }
    fn all(&self) -> Vec<ApprovalRequest> {
        self.0.lock().unwrap().clone()
    }
}

#[async_trait]
impl Approver for Asked {
    async fn approve(&self, request: ApprovalRequest) -> Decision {
        self.0.lock().unwrap().push(request);
        self.1
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
fn call(name: &str, args: Value) -> ToolCall {
    static IDS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let id = IDS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    ToolCall { id: format!("c{id}"), name: name.into(), arguments: args.to_string() }
}
fn process(args: Value) -> Vec<ProviderEvent> {
    use_tools(vec![call("process", args)])
}

/// A command that serves `text` on `port` from inside the process's sandbox.
fn serve(port: u16, text: &str) -> String {
    support::serve(&[port], text)
}

fn session(dirs: &Dirs, provider: &Arc<Scripted>, manager: &ProcessManager, edit: impl FnOnce(&mut SessionInit)) -> MarvSession {
    let mut init = SessionInit::new(&dirs.root, ProviderChoice::Factory(Arc::new(Fixed(provider.clone()))));
    init.diagnostics = false;
    init.checks = false;
    init.yolo = true;
    init.process_manager = Some(manager.clone());
    edit(&mut init);
    MarvSession::new(init).unwrap()
}

async fn turn(session: &MarvSession, text: &str) -> Vec<SessionEvent> {
    let mut turn = session.send(text, SendOptions::default()).unwrap();
    let mut events = Vec::new();
    while let Some(event) = turn.next().await {
        events.push(event);
    }
    events
}

fn results(events: &[SessionEvent]) -> Vec<ToolRun> {
    events
        .iter()
        .filter_map(|e| match e {
            SessionEvent::Loop(LoopEvent::ToolEnd { run, .. }) => Some(run.clone()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn start_asks_in_yolo_always_covers_that_command_only_logs_list_stop_never_ask() {
    require_bwrap!();
    let dirs = Dirs::new("ptool-ask");
    let manager = support::manager(&dirs, 3000);
    let (a, b) = (free_port(), free_port());
    let provider = Scripted::new(vec![
        process(json!({"action": "start", "command": serve(a, "served"), "name": "a"})),
        process(json!({"action": "stop", "name": "a"})),
        process(json!({"action": "start", "command": serve(a, "served"), "name": "a"})),
        process(json!({"action": "start", "command": serve(b, "served"), "name": "b"})),
        use_tools(vec![call("process", json!({"action": "logs", "name": "a"})), call("process", json!({"action": "list"}))]),
        process(json!({"action": "start", "command": "sleep 100", "network": true, "name": "net"})),
        say("Done."),
    ]);
    let asked = Asked::new(Decision::Always);
    let session = session(&dirs, &provider, &manager, |i| i.approve = Some(asked.clone()));
    let runs = results(&turn(&session, "go").await);
    let [started, stopped, again, other, logs, list, net] = &runs[..] else { panic!("{} results", runs.len()) };
    let keys: Vec<_> = asked.all().iter().map(|r| (r.door, r.scope.key.clone(), r.once, r.network)).collect();
    assert_eq!(
        keys,
        [
            (Some(Door::Process), format!("process:{}", serve(a, "served")), false, false),
            (Some(Door::Process), format!("process:{}", serve(b, "served")), false, false),
            (Some(Door::Process), "process:net:sleep 100".to_string(), false, true),
        ]
    );
    assert_eq!(started.approval, ApprovalRecord::Always);
    assert!(
        started.result.output.contains(&format!("Forwarded to the user's machine: http://localhost:{a}")),
        "{}",
        started.result.output
    );
    assert!(stopped.result.output.contains("Stopped a"));
    assert_eq!(stopped.approval, ApprovalRecord::None);
    assert_eq!(again.approval, ApprovalRecord::Yes); // from "always"
    assert!(other.result.output.contains(&format!("http://localhost:{b}")));
    assert_eq!(logs.approval, ApprovalRecord::None);
    assert!(logs.result.output.starts_with("Logs of a (running"), "{}", logs.result.output);
    assert!(list.result.output.contains("a: running"));
    assert!(list.result.output.contains("b: running"));
    assert!(net.result.output.contains("net: running"), "{}", net.result.output);
    assert_eq!(get(b).await.unwrap(), "served");
    session.close().await;
}

#[tokio::test]
async fn a_sandboxed_bash_command_reaches_a_started_server_on_localhost_without_network_or_an_approval_in_yolo() {
    require_bwrap!();
    let dirs = Dirs::new("ptool-curl");
    let manager = support::manager(&dirs, 3000);
    let port = free_port();
    let provider = Scripted::new(vec![
        process(json!({"action": "start", "command": serve(port, "PAGE-FROM-PROCESS"), "name": "web"})),
        use_tools(vec![call("bash", json!({"command": format!("curl -s --max-time 5 localhost:{port}")}))]),
        say("Done."),
    ]);
    let asked = Asked::new(Decision::Yes);
    let session = session(&dirs, &provider, &manager, |i| i.approve = Some(asked.clone()));
    let runs = results(&turn(&session, "go").await);
    let tools: Vec<String> = asked.all().iter().map(|r| r.tool.clone()).collect();
    assert_eq!(tools, ["process"]);
    assert_eq!(runs[1].approval, ApprovalRecord::Auto);
    assert!(runs[1].result.output.contains("PAGE-FROM-PROCESS"), "{}", runs[1].result.output);
    session.close().await;
}

#[tokio::test]
async fn processes_survive_clear_and_close_stops_them() {
    require_bwrap!();
    let dirs = Dirs::new("ptool-clear");
    let manager = support::manager(&dirs, 3000);
    let port = free_port();
    let provider =
        Scripted::new(vec![process(json!({"action": "start", "command": serve(port, "served"), "name": "web"})), say("Up.")]);
    let session = session(&dirs, &provider, &manager, |i| i.approve = Some(Asked::new(Decision::Yes)));
    turn(&session, "go").await;
    session.clear().await.unwrap();
    let listed: Vec<_> = session.processes().into_iter().map(|p| (p.name, p.state, p.ports)).collect();
    assert_eq!(listed, [("web".to_string(), ProcessState::Running, vec![port])]);
    assert_eq!(get(port).await.unwrap(), "served");
    session.close().await;
    assert!(session.processes().is_empty());
    assert!(get(port).await.is_err());
}

#[tokio::test]
async fn a_subagent_has_no_process_tool_naming_it_fails_as_unknown() {
    require_bwrap!();
    let dirs = Dirs::new("ptool-sub");
    let manager = support::manager(&dirs, 3000);
    let provider = Scripted::new(vec![
        use_tools(vec![call("agent", json!({"type": "general-purpose", "description": "try", "prompt": "start a server"}))]),
        process(json!({"action": "start", "command": "sleep 100"})),
        say("Couldn't."),
        say("Done."),
    ]);
    let asked = Asked::new(Decision::Yes);
    let session = session(&dirs, &provider, &manager, |i| i.approve = Some(asked.clone()));
    let events = turn(&session, "go").await;
    let sub: Vec<String> = events
        .iter()
        .filter_map(|e| match e {
            SessionEvent::Subagent { event: LoopEvent::ToolEnd { run, .. }, .. } => Some(run.result.output.clone()),
            _ => None,
        })
        .collect();
    assert!(sub[0].starts_with("Unknown tool \"process\""), "{}", sub[0]);
    assert!(asked.all().is_empty());
    assert!(manager.list().is_empty());
    // Its tools never included it either.
    assert!(!provider.tools(1).contains(&"process".to_string()));
    session.close().await;
}

#[tokio::test]
async fn processes_false_and_no_prompt_paragraph_or_tool_without_the_door() {
    require_bwrap!();
    let dirs = Dirs::new("ptool-off");
    let manager = support::manager(&dirs, 3000);
    let off_provider = Scripted::new(vec![say("Hi.")]);
    let off = session(&dirs, &off_provider, &manager, |i| i.processes = false);
    turn(&off, "hi").await;
    assert!(!off_provider.tools(0).contains(&"process".to_string()));
    assert!(!off_provider.system(0).contains("use process (start, logs, stop, list)"));
    assert!(off_provider.system(0).contains("# Sandbox limits"));
    assert!(!off.processes_offered());
    let on_provider = Scripted::new(vec![say("Hi.")]);
    let on = session(&dirs, &on_provider, &manager, |_| {});
    turn(&on, "hi").await;
    let tools = on_provider.tools(0);
    assert!(tools.contains(&"process".to_string()));
    // In Marv's order: right after bash.
    assert_eq!(tools.iter().position(|t| t == "process"), tools.iter().position(|t| t == "bash").map(|i| i + 1));
    assert!(on_provider.system(0).contains("use process (start, logs, stop, list)"));
    assert!(!on_provider.system(0).contains("# Sandbox limits"));
    off.close().await;
    on.close().await;
}

#[tokio::test]
async fn an_sdk_session_that_started_a_server_leaves_nothing_running_after_close() {
    // As marv -p runs.
    require_bwrap!();
    let dirs = Dirs::new("ptool-sdk");
    support::options(&dirs, 0); // the relay fixture
    let port = free_port();
    let marker = format!("marv-sdk-proc-{}", std::process::id());
    let provider = Scripted::new(vec![
        process(json!({"action": "start", "command": format!("{}; echo {marker}", serve(port, "served")), "name": "web"})),
        say("Started."),
    ]);
    let session = SessionBuilder::new(&dirs.root, ProviderOption::Custom(provider.clone()))
        .approve(Asked::new(Decision::Yes))
        .build()
        .await
        .unwrap();
    let runs = results(&turn(&session, "start it").await);
    assert!(runs[0].result.output.contains(&format!("http://localhost:{port}")), "{}", runs[0].result.output);
    assert!(!pgrep(&marker).is_empty());
    session.close().await;
    assert!(pgrep(&marker).is_empty());
    assert!(get(port).await.is_err());
}
