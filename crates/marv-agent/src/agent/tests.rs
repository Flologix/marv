use super::*;

fn call(id: &str, path: &str) -> ToolCall {
    ToolCall { id: id.into(), name: "read_file".into(), arguments: format!(r#"{{"path":"{path}"}}"#) }
}

// Ported from tests/agent.test.ts.

#[test]
fn answer_all_calls_gives_waiting_calls_a_not_run_result() {
    let history = vec![
        ChatTurn::User { text: "go".into() },
        ChatTurn::Assistant { text: "".into(), tool_calls: vec![call("c1", "a.ts"), call("c2", "b.ts")], reasoning: None },
        ChatTurn::Tool { call_id: "c1".into(), name: "read_file".into(), text: "a".into() },
    ];
    let answered = answer_all_calls(&history);
    assert_eq!(answered[..3], history[..]);
    assert!(matches!(&answered[3], ChatTurn::Tool { call_id, text, .. } if call_id == "c2" && text.contains("Interrupted")));
    assert_eq!(answered.len(), 4);
    let done = vec![
        ChatTurn::User { text: "hi".into() },
        ChatTurn::Assistant { text: "hello".into(), tool_calls: vec![], reasoning: None },
    ];
    assert_eq!(answer_all_calls(&done), done);
}

// #202: a mid-turn save gives each waiting call its own text, in call order.
#[test]
fn answer_all_calls_with_gives_each_waiting_call_its_own_result() {
    let history = vec![
        ChatTurn::User { text: "go".into() },
        ChatTurn::Assistant { text: "".into(), tool_calls: vec![call("c1", "a.ts"), call("c2", "b.ts")], reasoning: None },
    ];
    let answered = answer_all_calls_with(&history, |c| if c.id == "c1" { "contents".into() } else { not_run::QUIT.into() });
    let texts: Vec<&str> = answered[2..]
        .iter()
        .map(|t| match t {
            ChatTurn::Tool { text, .. } => text.as_str(),
            _ => "",
        })
        .collect();
    assert_eq!(texts, ["contents", not_run::QUIT]);
}

#[test]
fn tool_call_ids_are_made_unique_across_the_conversation() {
    // A resumed session already used call_0_1; Ollama numbers each step's calls from call_0.
    let history = vec![ChatTurn::Assistant { text: "".into(), tool_calls: vec![call("call_0_1", "x")], reasoning: None }];
    let mut used = used_ids(&history);
    let mut step1 = vec![call("call_0", "a.ts"), call("call_0", "b.ts"), call("", "c.ts")];
    make_ids_unique(&mut step1, &mut used);
    let mut step2 = vec![call("call_0", "d.ts")];
    make_ids_unique(&mut step2, &mut used);
    let ids: Vec<_> = step1.iter().chain(&step2).map(|c| c.id.as_str()).collect();
    assert_eq!(ids, ["call_0", "call_0_2", "call_1", "call_0_3"]);
}

#[test]
fn labels_before_a_call_runs() {
    assert_eq!(label_of(&call("c", "src/app.ts")), "src/app.ts");
    let agent = ToolCall {
        id: "c".into(),
        name: "agent".into(),
        arguments: r#"{"type":"explore","description":"Find the config"}"#.into(),
    };
    assert_eq!(label_of(&agent), "explore · Find the config");
    let untyped = ToolCall { arguments: r#"{"description":"x"}"#.into(), ..agent.clone() };
    assert_eq!(label_of(&untyped), "general-purpose · x");
    let bad = ToolCall { arguments: "not json".into(), ..agent };
    assert_eq!(label_of(&bad), "not json");
}

#[test]
fn agent_args_reads_only_objects() {
    assert_eq!(
        agent_args(r#"{"prompt":"p","type":"explore","description":"d","isolation":"worktree"}"#),
        AgentArgs { prompt: "p".into(), kind: "explore".into(), description: "d".into(), isolation: Some("worktree".into()) }
    );
    for junk in ["null", "[]", "3", "nope"] {
        assert_eq!(
            agent_args(junk),
            AgentArgs { prompt: "".into(), kind: "general-purpose".into(), description: "".into(), isolation: None }
        );
    }
}

#[test]
fn concurrent_calls_group_and_anything_else_is_a_barrier() {
    let calls: Vec<ToolCall> = ["read_file", "grep", "write_file", "glob", "read_file", "bash"]
        .iter()
        .map(|n| ToolCall { id: "".into(), name: (*n).into(), arguments: "{}".into() })
        .collect();
    let reads = |c: &ToolCall| matches!(c.name.as_str(), "read_file" | "grep" | "glob");
    assert_eq!(call_groups(&calls, reads), vec![0..2, 2..3, 3..5, 5..6]);
}

#[test]
fn step_limit_arithmetic_and_messages() {
    assert!(!at_step_limit(0, 25));
    assert!(at_step_limit(25, 25) && at_step_limit(50, 25));
    assert!(!at_step_limit(25, 0), "0 turns the limit off");
    assert!(step_limit_message(25, true).contains("as you asked"));
    assert!(step_limit_message(25, false).starts_with("Stopped after 25 steps without a final answer."));
}

#[test]
fn empty_replies_retry_then_nudge_then_stop() {
    assert_eq!([0, 1, 2, 3].map(EmptyNext::for_count), [EmptyNext::Retry, EmptyNext::Nudge, EmptyNext::Stop, EmptyNext::Stop]);
}

fn fold(events: Vec<ProviderEvent>) -> (StepReply, Vec<LoopEvent>) {
    let mut reply = StepReply::default();
    let mut out = Vec::new();
    for event in events {
        out.extend(reply.apply(event));
        if reply.looped {
            break;
        }
    }
    (reply, out)
}

#[test]
fn a_discard_resets_the_steps_text_and_calls_before_the_error() {
    // #96: the second degenerate reply.
    let (reply, events) = fold(vec![
        ProviderEvent::ThinkingDelta("\n\n".into()),
        ProviderEvent::TextDelta("Half".into()),
        ProviderEvent::ToolCall(call("c1", "a")),
        ProviderEvent::Discard { cause: marv_core::RetryCause::Degenerate { discarded: 6 } },
        ProviderEvent::Error("degenerated again".into()),
    ]);
    assert_eq!(reply.text, "");
    assert!(reply.calls.is_empty());
    assert!(events.iter().any(|e| matches!(e, LoopEvent::Discarded { .. })));
    // marv-rs: the loop goes on past it (a bounded number of times); an interrupt is still an error.
    assert_eq!(reply.outcome(false), StepOutcome::Degenerate("degenerated again".into()));
    assert_eq!(reply.outcome(true), StepOutcome::Error("degenerated again".into()));
}

#[test]
fn a_retry_resets_the_steps_text_and_calls() {
    let (reply, events) = fold(vec![
        ProviderEvent::TextDelta("Half".into()),
        ProviderEvent::ToolCall(call("c1", "a")),
        ProviderEvent::Retry { attempt: 1, max: 2, message: "retrying".into(), continued: false, cause: Default::default() },
        ProviderEvent::TextDelta("Whole".into()),
        ProviderEvent::Done { reason: Some("stop".into()) },
    ]);
    assert_eq!(reply.text, "Whole");
    assert!(reply.calls.is_empty());
    assert!(matches!(events[1], LoopEvent::Retry { attempt: 1, continued: false, .. }));
    assert_eq!(reply.outcome(false), StepOutcome::Answer);
}

#[test]
fn a_text_loop_is_stopped_while_streaming() {
    let deltas = (0..100).map(|_| ProviderEvent::TextDelta("Fix the test scripts now. ".into()));
    let (reply, events) = fold(deltas.chain([ProviderEvent::ToolCall(call("c", "a"))]).collect());
    assert!(reply.looped);
    assert!(reply.calls.is_empty(), "the rest of the stream isn't read");
    assert!(events.len() < 100);
    assert_eq!(reply.outcome(false), StepOutcome::TextLoop);
}

#[test]
fn outcomes_empty_error_cut_off_tools() {
    let empty = fold(vec![
        ProviderEvent::ThinkingDelta("hm".into()),
        ProviderEvent::TextDelta("  ".into()),
        ProviderEvent::Done { reason: None },
    ])
    .0;
    assert_eq!(empty.outcome(false), StepOutcome::Empty);
    assert_ne!(empty.outcome(true), StepOutcome::Empty, "not after an interrupt");
    assert_eq!(empty.assistant_turn(), Some(ChatTurn::Assistant { text: "  ".into(), tool_calls: vec![], reasoning: None }));
    let failed = fold(vec![ProviderEvent::Error("boom".into())]).0;
    assert_eq!(failed.outcome(false), StepOutcome::Error("boom".into()));
    let cut = fold(vec![ProviderEvent::ThinkingDelta("hm".into()), ProviderEvent::Done { reason: Some("length".into()) }]).0;
    assert_eq!(cut.outcome(false), StepOutcome::CutOff);
    assert_eq!(cut.cut_off_note(), CUT_OFF_REASONING_NOTE);
    let tools = fold(vec![ProviderEvent::ToolCall(call("c", "a")), ProviderEvent::Done { reason: Some("tool_calls".into()) }]).0;
    assert_eq!(tools.outcome(false), StepOutcome::Tools);
}

#[test]
fn reasoning_is_kept_on_the_assistant_turn() {
    let reasoning = Reasoning(serde_json::json!([{"type": "reasoning.text"}]));
    let (reply, _) = fold(vec![ProviderEvent::TextDelta("ok".into()), ProviderEvent::Reasoning(reasoning.clone())]);
    assert_eq!(
        reply.assistant_turn(),
        Some(ChatTurn::Assistant { text: "ok".into(), tool_calls: vec![], reasoning: Some(reasoning) })
    );
}

#[test]
fn dropping_the_loop_mid_tools_answers_the_rest() {
    struct NoTools;
    #[async_trait]
    impl ToolRunner for NoTools {
        fn kind(&self, _call: &ToolCall) -> ToolKind {
            ToolKind::Read
        }
        async fn run(&self, _call: ToolCall, _cancel: CancellationToken) -> Result<ToolRun, MarvError> {
            unreachable!()
        }
    }
    let history: History = Arc::new(Mutex::new(vec![]));
    let provider = Arc::new(crate::testing::ScriptedProvider::new(vec![]));
    let mut state = LoopState::new(AgentOptions::new(
        provider,
        history.clone(),
        "S".into(),
        vec![],
        Arc::new(NoTools),
        CancellationToken::new(),
    ));
    state.in_flight = InFlight { calls: vec![call("c1", "a"), call("c2", "b")], answered: 1 };
    drop(state);
    let history = history.lock().unwrap();
    assert!(matches!(&history[..], [ChatTurn::Tool { call_id, text, .. }] if call_id == "c2" && text == not_run::ABORTED));
}

#[test]
fn tool_loop_note_counts_the_repeats() {
    assert!(tool_loop_note(4, "read_file", "{}").starts_with("Marv: you have made this exact call 4 times in a row"));
}

#[test]
fn tool_loop_note_shows_the_exact_call_and_what_it_lacks() {
    // TB 0.15.0: the model meant to pass offset=401 and kept sending only the path.
    let note = tool_loop_note(3, "read_file", r#"{"path": "src/app.py"}"#);
    assert!(note.contains(r#"`read_file {"path": "src/app.py"}`"#), "{note}");
    assert!(note.contains("You sent no offset") && note.contains("pass offset"), "{note}");
    // With an offset there's nothing to point out.
    let paged = tool_loop_note(3, "read_file", r#"{"path": "a", "offset": 401}"#);
    assert!(!paged.contains("no offset"), "{paged}");
    assert!(!tool_loop_note(3, "grep", r#"{"pattern": "x"}"#).contains("offset"));
    // Long arguments are cut, on a char boundary.
    let long = format!(r#"{{"command": "{}"}}"#, "é".repeat(1000));
    let note = tool_loop_note(3, "bash", &long);
    assert!(note.contains("…`") && note.len() < 1000, "{}", note.len());
    let last = tool_loop_final_note(5, "read_file", r#"{"path": "a"}"#);
    assert!(last.starts_with("Marv: not run.") && last.contains("final turn") && last.contains("no offset"), "{last}");
}

// The loop itself, against the scripted provider and a fake runner (tests/agent.test.ts).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use crate::testing::ScriptedProvider;
use futures::StreamExt;

type RunFn = Arc<dyn Fn(ToolCall, CancellationToken) -> BoxFuture<'static, Result<ToolRun, MarvError>> + Send + Sync>;

/// A runner that runs `run`; `parallel` says which calls go together (Marv's `isParallel`, default none).
struct FakeRunner {
    run: RunFn,
    parallel: fn(&ToolCall) -> bool,
}

#[async_trait]
impl ToolRunner for FakeRunner {
    fn kind(&self, call: &ToolCall) -> ToolKind {
        if call.name == "write_file" { ToolKind::Write } else { ToolKind::Read }
    }
    fn concurrent(&self, call: &ToolCall) -> bool {
        (self.parallel)(call)
    }
    async fn run(&self, call: ToolCall, cancel: CancellationToken) -> Result<ToolRun, MarvError> {
        (self.run)(call, cancel).await
    }
}

fn specs() -> Vec<ToolSpec> {
    vec![ToolSpec {
        name: "read_file".into(),
        description: "Read a file".into(),
        parameters: serde_json::json!({"type": "object"}),
    }]
}
fn say(text: &str) -> Vec<ProviderEvent> {
    vec![ProviderEvent::TextDelta(text.into()), ProviderEvent::Done { reason: Some("stop".into()) }]
}
fn use_tools(calls: &[ToolCall]) -> Vec<ProviderEvent> {
    let mut events: Vec<_> = calls.iter().cloned().map(ProviderEvent::ToolCall).collect();
    events.push(ProviderEvent::Done { reason: Some("tool_calls".into()) });
    events
}
fn cut_off(mut events: Vec<ProviderEvent>) -> Vec<ProviderEvent> {
    events.push(ProviderEvent::Done { reason: Some("length".into()) });
    events
}
fn arg(call: &ToolCall, key: &str) -> serde_json::Value {
    serde_json::from_str::<serde_json::Value>(&call.arguments).unwrap()[key].clone()
}
fn ok(output: impl Into<String>, label: impl Into<String>) -> ToolRun {
    ToolRun::new(ToolResult { output: output.into(), summary: Some("ok".into()), is_error: false }, label)
}
fn fake_tool() -> RunFn {
    Arc::new(|c, _| {
        let path = arg(&c, "path").as_str().unwrap_or_default().to_string();
        Box::pin(async move { Ok(ok(format!("contents of {path}"), path)) })
    })
}
fn user(text: &str) -> ChatTurn {
    ChatTurn::User { text: text.into() }
}
fn tool(id: &str, text: &str) -> ChatTurn {
    ChatTurn::Tool { call_id: id.into(), name: "read_file".into(), text: text.into() }
}
fn said(text: &str) -> ChatTurn {
    ChatTurn::Assistant { text: text.into(), tool_calls: vec![], reasoning: None }
}
fn history(turns: Vec<ChatTurn>) -> History {
    Arc::new(Mutex::new(turns))
}
fn snapshot(history: &History) -> Vec<ChatTurn> {
    history.lock().unwrap().clone()
}
fn tool_texts(history: &History) -> Vec<String> {
    snapshot(history)
        .into_iter()
        .filter_map(|t| match t {
            ChatTurn::Tool { text, .. } => Some(text),
            _ => None,
        })
        .collect()
}
fn options(provider: Arc<ScriptedProvider>, history: &History, runner: FakeRunner) -> AgentOptions {
    AgentOptions::new(provider, history.clone(), "SYSTEM".into(), specs(), Arc::new(runner), CancellationToken::new())
}
fn plain() -> FakeRunner {
    FakeRunner { run: fake_tool(), parallel: |_| false }
}
async fn collect(options: AgentOptions) -> Vec<LoopEvent> {
    run_agent(options).collect().await
}
fn end(events: &[LoopEvent]) -> Option<StopReason> {
    match events.last() {
        Some(LoopEvent::Done(reason)) => Some(*reason),
        _ => None,
    }
}
fn starts(events: &[LoopEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            LoopEvent::ToolStart { call, .. } => Some(call.id.clone()),
            _ => None,
        })
        .collect()
}
fn count(events: &[LoopEvent], pred: impl Fn(&LoopEvent) -> bool) -> usize {
    events.iter().filter(|e| pred(e)).count()
}

/// What's wrong with a history's tool results (tests/answered.ts): every assistant tool call must
/// be followed by exactly one result each, right after it and in call order, and no result may
/// answer a call that isn't there. Empty: all good.
fn answer_problems(history: &[ChatTurn]) -> Vec<String> {
    let mut problems = Vec::new();
    let mut i = 0;
    while i < history.len() {
        match &history[i] {
            ChatTurn::Tool { call_id, .. } => problems.push(format!("result {call_id} at {i} answers no call just before it")),
            ChatTurn::Assistant { tool_calls, .. } if !tool_calls.is_empty() => {
                let ids: Vec<&str> = tool_calls.iter().map(|c| c.id.as_str()).collect();
                let mut results = Vec::new();
                while let Some(ChatTurn::Tool { call_id, .. }) = history.get(i + 1) {
                    results.push(call_id.as_str());
                    i += 1;
                }
                if results != ids {
                    problems.push(format!("calls {ids:?} got results {results:?}"));
                }
            }
            _ => {}
        }
        i += 1;
    }
    problems
}

/// A provider that cancels the run as its reply starts, then sends `events`.
struct CancelsWhileStreaming {
    cancel: CancellationToken,
    events: Vec<ProviderEvent>,
}

impl Provider for CancelsWhileStreaming {
    fn name(&self) -> &str {
        "p"
    }
    fn context_length(&self) -> Option<u64> {
        None
    }
    fn stream(&self, _history: &[ChatTurn], _options: StreamOptions) -> marv_provider::EventStream {
        self.cancel.cancel();
        Box::pin(futures::stream::iter(self.events.clone()))
    }
}

#[tokio::test]
async fn a_plain_answer_is_one_step() {
    let provider = Arc::new(ScriptedProvider::new(vec![say("Hi!")]));
    let h = history(vec![user("hello")]);
    let events = collect(options(provider, &h, plain())).await;
    assert_eq!(snapshot(&h), [user("hello"), said("Hi!")]);
    assert!(matches!(&events[..], [LoopEvent::TextDelta(_), LoopEvent::Assistant { .. }, LoopEvent::Done(StopReason::End)]));
}

#[tokio::test]
async fn runs_the_tools_the_model_asks_for_then_sends_the_results_back() {
    let provider = Arc::new(ScriptedProvider::new(vec![use_tools(&[call("c1", "a.ts"), call("c2", "b.ts")]), say("Both read.")]));
    let h = history(vec![user("read a and b")]);
    let events = collect(options(provider.clone(), &h, plain())).await;
    assert_eq!(
        snapshot(&h),
        [
            user("read a and b"),
            ChatTurn::Assistant { text: "".into(), tool_calls: vec![call("c1", "a.ts"), call("c2", "b.ts")], reasoning: None },
            tool("c1", "contents of a.ts"),
            tool("c2", "contents of b.ts"),
            said("Both read."),
        ]
    );
    // The second request carried the tool results.
    assert_eq!(provider.requests()[1].0.last(), Some(&tool("c2", "contents of b.ts")));
    let labels: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            LoopEvent::ToolStart { label, .. } => Some(label.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(labels, ["a.ts", "b.ts"]);
    assert_eq!(count(&events, |e| matches!(e, LoopEvent::ToolEnd { .. })), 2);
}

#[tokio::test]
async fn every_request_extends_the_previous_one_exactly() {
    let provider = Arc::new(ScriptedProvider::new(vec![
        use_tools(&[call("c1", "a.ts")]),
        use_tools(&[call("c2", "b.ts")]),
        say("Done."),
        use_tools(&[call("c3", "c.ts")]),
        say("Again."),
    ]));
    let h = history(vec![user("first")]);
    collect(options(provider.clone(), &h, plain())).await;
    h.lock().unwrap().push(user("second"));
    collect(options(provider.clone(), &h, plain())).await;

    let requests = provider.requests();
    assert_eq!(requests.len(), 5);
    for pair in requests.windows(2) {
        let (before, after) = (&pair[0], &pair[1]);
        assert_eq!(after.0[..before.0.len()], before.0[..], "only appended to");
        assert!(after.0.len() > before.0.len());
        assert_eq!(after.1.system, before.1.system, "same system prompt");
        assert_eq!(after.1.tools, before.1.tools, "the same tool definitions");
    }
}

fn endless(n: usize) -> Vec<Vec<ProviderEvent>> {
    (0..n).map(|i| use_tools(&[call(&format!("c{i}"), &format!("file-{i}.ts"))])).collect()
}

fn on_limit(f: impl Fn(u32) -> LimitAnswer + Send + Sync + 'static) -> Option<OnLimit> {
    Some(Arc::new(move |steps| {
        let answer = f(steps);
        Box::pin(async move { answer })
    }))
}

#[tokio::test]
async fn stops_after_max_steps_if_the_model_never_finishes() {
    let provider = Arc::new(ScriptedProvider::new(endless(10)));
    let h = history(vec![user("go")]);
    let events = collect(AgentOptions { max_steps: 3, ..options(provider.clone(), &h, plain()) }).await;
    assert_eq!(provider.requests().len(), 3);
    assert!(events.iter().any(|e| matches!(e, LoopEvent::Error(m) if m.contains("3 steps"))));
    assert_eq!(end(&events), Some(StopReason::MaxSteps));
}

#[tokio::test]
async fn at_the_limit_it_asks_yes_buys_another_max_steps_no_stops() {
    let provider = Arc::new(ScriptedProvider::new(endless(10)));
    let h = history(vec![user("go")]);
    let asked = Arc::new(Mutex::new(Vec::new()));
    let seen = asked.clone();
    let events = collect(AgentOptions {
        max_steps: 2,
        on_limit: on_limit(move |steps| {
            seen.lock().unwrap().push(steps);
            if steps < 4 { LimitAnswer::Continue } else { LimitAnswer::Stop }
        }),
        ..options(provider.clone(), &h, plain())
    })
    .await;
    assert_eq!(*asked.lock().unwrap(), [2, 4]);
    assert_eq!(provider.requests().len(), 4);
    let limits: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            LoopEvent::StepLimit { steps, continued } => Some((*steps, *continued)),
            _ => None,
        })
        .collect();
    assert_eq!(limits, [(2, true), (4, false)]);
    assert!(events.iter().any(|e| matches!(e, LoopEvent::Error(m)
            if m == "Stopped at 4 steps, as you asked. Say \"continue\" to pick up where it left off.")));
    assert_eq!(end(&events), Some(StopReason::MaxSteps));
}

#[tokio::test]
async fn it_asks_only_after_a_steps_tool_results_are_in() {
    let provider = Arc::new(ScriptedProvider::new(vec![use_tools(&[call("c1", "a.ts")]), say("done")]));
    let h = history(vec![user("go")]);
    let seen = h.clone();
    collect(AgentOptions {
        max_steps: 1,
        on_limit: on_limit(move |_| {
            assert!(matches!(seen.lock().unwrap().last(), Some(ChatTurn::Tool { call_id, .. }) if call_id == "c1"));
            LimitAnswer::Continue
        }),
        ..options(provider, &h, plain())
    })
    .await;
    assert_eq!(snapshot(&h).last(), Some(&said("done")));
}

#[tokio::test]
async fn esc_while_it_asks_is_an_interrupt() {
    let provider = Arc::new(ScriptedProvider::new(vec![use_tools(&[call("c1", "a.ts")]), say("never")]));
    let h = history(vec![user("go")]);
    let opts = options(provider.clone(), &h, plain());
    let cancel = opts.cancel.clone();
    let events = collect(AgentOptions {
        max_steps: 1,
        on_limit: on_limit(move |_| {
            cancel.cancel();
            LimitAnswer::Stop
        }),
        ..opts
    })
    .await;
    assert_eq!(provider.requests().len(), 1);
    assert_eq!(end(&events), Some(StopReason::Aborted));
    assert!(!events.iter().any(|e| matches!(e, LoopEvent::Error(_))));
}

#[tokio::test]
async fn tool_call_ids_are_unique_in_the_history() {
    let provider = Arc::new(ScriptedProvider::new(vec![
        use_tools(&[call("call_0", "a.ts"), call("call_0", "b.ts"), call("", "c.ts")]),
        use_tools(&[call("call_0", "d.ts")]), // Ollama numbers each step's calls from call_0
        say("done"),
    ]));
    let h = history(vec![
        user("earlier"),
        ChatTurn::Assistant { text: "".into(), tool_calls: vec![call("call_0_1", "x")], reasoning: None }, // a resumed session
        tool("call_0_1", "x"),
        user("go"),
    ]);
    let events = collect(options(provider, &h, plain())).await;
    let turns = snapshot(&h);
    let ids: Vec<String> = turns
        .iter()
        .flat_map(|t| match t {
            ChatTurn::Assistant { tool_calls, .. } => tool_calls.iter().map(|c| c.id.clone()).collect(),
            _ => vec![],
        })
        .collect();
    assert_eq!(ids.iter().collect::<HashSet<_>>().len(), ids.len());
    assert!(ids.iter().all(|id| !id.is_empty()));
    // Each result answers its own call, and the events carry the same ids.
    let results: Vec<String> = turns
        .iter()
        .filter_map(|t| match t {
            ChatTurn::Tool { call_id, .. } => Some(call_id.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(results, ids);
    assert_eq!(starts(&events), ids[1..]);
}

#[tokio::test]
async fn a_consumer_that_stops_mid_tools_still_leaves_every_call_answered() {
    let provider = Arc::new(ScriptedProvider::new(vec![use_tools(&[call("c1", "a.ts"), call("c2", "b.ts")])]));
    let h = history(vec![user("go")]);
    let mut stream = Box::pin(run_agent(options(provider, &h, plain())));
    while let Some(event) = stream.next().await {
        if matches!(event, LoopEvent::ToolStart { .. }) {
            break; // like the App failing while handling it
        }
    }
    drop(stream);
    let turns = snapshot(&h);
    assert_eq!(turns[turns.len() - 2..], [tool("c1", not_run::ABORTED), tool("c2", not_run::ABORTED)]);
}

#[tokio::test]
async fn calls_answered_without_running_are_reported_too() {
    let provider =
        Arc::new(ScriptedProvider::new(vec![use_tools(&[call("c1", "a.ts"), call("c2", "b.ts"), call("c3", "c.ts")])]));
    let h = history(vec![user("go")]);
    let decline_first: RunFn =
        Arc::new(|c, _| Box::pin(async move { Ok(ToolRun { declined: c.id == "c1", ..ok("x", c.id.clone()) }) }));
    let events = collect(options(provider, &h, FakeRunner { run: decline_first, parallel: |_| false })).await;
    let skipped: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            LoopEvent::ToolSkipped { call, output } => Some((call.clone(), output.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(skipped, [(call("c2", "b.ts"), not_run::DECLINED.into()), (call("c3", "c.ts"), not_run::DECLINED.into())]);
}

#[tokio::test]
async fn an_interrupt_mid_tools_still_answers_every_call() {
    let provider = Arc::new(ScriptedProvider::new(vec![use_tools(&[call("c1", "a.ts"), call("c2", "b.ts")])]));
    let h = history(vec![user("go")]);
    // The user presses ctrl+c while the first tool runs.
    let aborting: RunFn = Arc::new(|c, cancel| {
        cancel.cancel();
        fake_tool()(c, cancel)
    });
    let events = collect(options(provider, &h, FakeRunner { run: aborting, parallel: |_| false })).await;
    assert_eq!(tool_texts(&h), ["contents of a.ts", not_run::ABORTED]);
    assert_eq!(end(&events), Some(StopReason::Aborted));
}

#[tokio::test]
async fn a_declined_action_stops_the_run_and_later_calls_are_answered_too() {
    let provider =
        Arc::new(ScriptedProvider::new(vec![use_tools(&[call("c1", "a.ts"), call("c2", "b.ts")]), say("never asked")]));
    let h = history(vec![user("change things")]);
    let declining: RunFn =
        Arc::new(|c, _| Box::pin(async move { Ok(ToolRun { declined: true, ..ok("The user declined this.", c.id.clone()) }) }));
    let events = collect(options(provider.clone(), &h, FakeRunner { run: declining, parallel: |_| false })).await;
    assert_eq!(provider.requests().len(), 1, "no further request: the user decides what's next");
    assert_eq!(tool_texts(&h), ["The user declined this.", not_run::DECLINED]);
    assert_eq!(end(&events), Some(StopReason::Declined));
}

#[tokio::test]
async fn a_provider_error_ends_the_run_and_is_reported() {
    let provider = Arc::new(ScriptedProvider::new(vec![vec![ProviderEvent::Error("Rate limited".into())]]));
    let h = history(vec![user("go")]);
    let events = collect(options(provider, &h, plain())).await;
    assert!(matches!(
        &events[..],
        [LoopEvent::Error(m), LoopEvent::Done(StopReason::Error)] if m == "Rate limited"
    ));
}

#[tokio::test]
async fn tool_call_fragments_are_passed_on_as_they_stream() {
    let provider = Arc::new(ScriptedProvider::new(vec![
        vec![
            ProviderEvent::ToolCallDelta { index: 0, name: "read_file".into(), text: r#"{"path":"#.into() },
            ProviderEvent::ToolCall(call("c1", "a.ts")),
            ProviderEvent::Done { reason: None },
        ],
        say("Done."),
    ]));
    let h = history(vec![user("go")]);
    let events = collect(options(provider, &h, plain())).await;
    assert!(events.iter().any(|e| matches!(e, LoopEvent::ToolCallDelta { index: 0, name, text }
            if name == "read_file" && text == r#"{"path":"#)));
}

#[tokio::test]
async fn a_retry_resets_this_steps_text_and_calls() {
    let c = call("c1", "a.ts");
    let provider = Arc::new(ScriptedProvider::new(vec![vec![
        ProviderEvent::TextDelta("Hel".into()),
        ProviderEvent::ToolCall(c.clone()),
        ProviderEvent::Retry { attempt: 1, max: 2, message: "lost".into(), continued: false, cause: Default::default() },
        ProviderEvent::TextDelta("Hello".into()),
        ProviderEvent::ToolCall(c),
        ProviderEvent::Done { reason: Some("tool_calls".into()) },
    ]]));
    let h = history(vec![user("go")]);
    let events = collect(options(provider, &h, plain())).await;
    assert!(events.iter().any(|e| matches!(e, LoopEvent::Assistant { text } if text == "Hello")));
    assert_eq!(starts(&events).len(), 1);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, LoopEvent::Retry { attempt: 1, max: 2, message, continued: false, .. } if message == "lost"))
    );
}

// An empty reply (no text, no tool calls). Upstream hosts sometimes bill for a reply and send back
// nothing (OpenRouter's Novita and SiliconFlow for qwen3-coder, gpt-oss-120b): the run ended as if
// the model had finished. A third of the failed eval runs.

fn empty() -> Vec<ProviderEvent> {
    vec![ProviderEvent::Done { reason: Some("stop".into()) }]
}
fn empties(events: &[LoopEvent]) -> Vec<EmptyNext> {
    events
        .iter()
        .filter_map(|e| match e {
            LoopEvent::EmptyReply { next } => Some(*next),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn an_empty_reply_is_asked_for_again_unchanged() {
    let provider = Arc::new(ScriptedProvider::new(vec![empty(), use_tools(&[call("c1", "a.ts")]), say("Done.")]));
    let h = history(vec![user("go")]);
    let events = collect(options(provider.clone(), &h, plain())).await;
    let requests = provider.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[1].0, requests[0].0, "the prompt cache still hits");
    assert_eq!(empties(&events), [EmptyNext::Retry]);
    assert_eq!(end(&events), Some(StopReason::End));
}

#[tokio::test]
async fn empty_again_the_model_is_told_once_a_third_time_the_run_ends() {
    let provider = Arc::new(ScriptedProvider::new(vec![empty(), empty(), empty(), say("never asked")]));
    let h = history(vec![user("go")]);
    let events = collect(options(provider.clone(), &h, plain())).await;
    let requests = provider.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[2].0.last(), Some(&user(&format!("go\n\n{EMPTY_REPLY_NOTE}"))));
    assert_eq!(empties(&events), [EmptyNext::Retry, EmptyNext::Nudge, EmptyNext::Stop]);
    assert_eq!(end(&events), Some(StopReason::End));
}

#[tokio::test]
async fn whitespace_or_reasoning_only_is_empty_too_and_isnt_put_in_the_history() {
    let provider = Arc::new(ScriptedProvider::new(vec![
        vec![ProviderEvent::TextDelta("\n\n".into()), ProviderEvent::Done { reason: Some("stop".into()) }],
        vec![ProviderEvent::ThinkingDelta("Hmm.".into()), ProviderEvent::Done { reason: Some("stop".into()) }],
        say("Done."),
    ]));
    let h = history(vec![user("go")]);
    collect(options(provider, &h, plain())).await;
    assert_eq!(snapshot(&h), [user(&format!("go\n\n{EMPTY_REPLY_NOTE}")), said("Done.")]);
}

#[tokio::test]
async fn each_step_gets_its_own_empty_retries() {
    let provider = Arc::new(ScriptedProvider::new(vec![empty(), use_tools(&[call("c1", "a.ts")]), empty(), say("Done.")]));
    let h = history(vec![user("go")]);
    let events = collect(options(provider, &h, plain())).await;
    assert_eq!(empties(&events), [EmptyNext::Retry, EmptyNext::Retry]);
    assert_eq!(end(&events), Some(StopReason::End));
}

#[tokio::test]
async fn not_empty_after_an_interrupt_or_an_error() {
    let h = history(vec![user("go")]);
    let cancel = CancellationToken::new();
    let provider = Arc::new(CancelsWhileStreaming { cancel: cancel.clone(), events: empty() });
    let events = collect(AgentOptions::new(provider, h.clone(), "S".into(), specs(), Arc::new(plain()), cancel)).await;
    assert!(matches!(&events[..], [LoopEvent::Done(StopReason::Aborted)]));

    let failing = Arc::new(ScriptedProvider::new(vec![vec![ProviderEvent::Error("Rate limited".into())], say("never asked")]));
    let events = collect(options(failing.clone(), &history(vec![user("go")]), plain())).await;
    assert!(empties(&events).is_empty());
    assert_eq!(failing.requests().len(), 1);
}

// A reply that announces its next step ("Now checking issue #7 before commenting:") but
// makes no tool call ends the run halfway (#90). The loop says "continue" once per turn,
// like a user would.

fn announced(text: &str) -> Vec<ProviderEvent> {
    vec![ProviderEvent::TextDelta(text.into()), ProviderEvent::Done { reason: Some("stop".into()) }]
}
fn announced_steps(events: &[LoopEvent]) -> Vec<bool> {
    events
        .iter()
        .filter_map(|e| match e {
            LoopEvent::AnnouncedStep { continued } => Some(*continued),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn an_announced_step_with_no_tool_call_gets_one_continue() {
    let provider = Arc::new(ScriptedProvider::new(vec![
        announced("Both modes work. Now let me look at issue #7 and comment:"),
        use_tools(&[call("c1", "a.ts")]),
        say("Done."),
    ]));
    let h = history(vec![user("go")]);
    let events = collect(options(provider.clone(), &h, plain())).await;
    let requests = provider.requests();
    assert_eq!(requests.len(), 3, "the run did not stop at the announcement");
    assert_eq!(requests[1].0.last(), Some(&user(ANNOUNCED_STEP_NOTE)));
    // The announced reply stays in the history before the note (a valid, ordinary turn).
    assert_eq!(
        snapshot(&h)[1..3],
        [said("Both modes work. Now let me look at issue #7 and comment:"), user(ANNOUNCED_STEP_NOTE)]
    );
    assert_eq!(announced_steps(&events), [true]);
    assert_eq!(end(&events), Some(StopReason::End));
}

#[tokio::test]
async fn a_self_summary_with_no_tool_call_is_continued_up_to_three_times() {
    let summary = "<analysis>\nLet me work through the conversation chronologically.";
    let provider = Arc::new(ScriptedProvider::new(vec![
        announced(summary),
        use_tools(&[call("c1", "a.ts")]),
        announced(summary),
        announced(summary),
        announced(summary),
        say("never asked"),
    ]));
    let h = history(vec![user("go")]);
    let events = collect(options(provider.clone(), &h, plain())).await;
    let requests = provider.requests();
    assert_eq!(requests.len(), 5, "three continues, then the fourth summary ends the run");
    assert_eq!(requests[1].0.last(), Some(&user(SELF_SUMMARY_NOTE)));
    assert_eq!(announced_steps(&events), [true, true, true, false]);
    assert_eq!(end(&events), Some(StopReason::End));
}

#[tokio::test]
async fn a_second_announced_step_after_the_rescue_only_ends_the_run() {
    let provider = Arc::new(ScriptedProvider::new(vec![
        announced("Now checking issue #7 before commenting:"),
        announced("Now also summarizing the diff:"),
        say("never asked"),
    ]));
    let h = history(vec![user("go")]);
    let events = collect(options(provider.clone(), &h, plain())).await;
    assert_eq!(provider.requests().len(), 2, "one continue per turn: the second announcement stops the run");
    assert_eq!(announced_steps(&events), [true, false]);
    assert_eq!(end(&events), Some(StopReason::End));
    let notes = snapshot(&h).iter().filter(|t| matches!(t, ChatTurn::User { text } if text == ANNOUNCED_STEP_NOTE)).count();
    assert_eq!(notes, 1);
}

#[tokio::test]
async fn a_question_or_a_summary_ending_is_never_rescued() {
    // A plain answer (a question to the user, a summary) ends the run as before: no note,
    // no continue, no second request.
    let provider = Arc::new(ScriptedProvider::new(vec![announced("Should I also update the docs?"), say("never asked")]));
    let h = history(vec![user("go")]);
    let events = collect(options(provider.clone(), &h, plain())).await;
    assert_eq!(provider.requests().len(), 1, "the question ended the run as before");
    assert_eq!(announced_steps(&events), Vec::<bool>::new());
    assert!(snapshot(&h).iter().all(|t| !matches!(t, ChatTurn::User { text } if text == ANNOUNCED_STEP_NOTE)));
    assert_eq!(end(&events), Some(StopReason::End));

    let provider = Arc::new(ScriptedProvider::new(vec![announced("Done. Both modes work as expected."), say("never asked")]));
    let events = collect(options(provider, &history(vec![user("go")]), plain())).await;
    assert_eq!(announced_steps(&events), Vec::<bool>::new());
    assert_eq!(end(&events), Some(StopReason::End));
}

#[tokio::test]
async fn an_announced_step_counts_toward_the_step_limit_and_the_wrap_up_is_never_continued() {
    // #90 + #59: the continue is an ordinary step, so it can reach the limit; the wrap-up reply
    // that follows is tools-off and final even when it announces a next step itself.
    let mut script = steps(1);
    script.push(announced("Read a. Now let me fix b:"));
    script.push(announced("Read a; b is left. Next, let me fix b:"));
    script.push(say("never asked"));
    let provider = Arc::new(ScriptedProvider::new(script));
    let h = history(vec![user("go")]);
    let (_state, runner) = tracked(None);
    let events = collect(AgentOptions { max_steps: 2, wrap_up_at_limit: true, ..options(provider.clone(), &h, runner) }).await;
    let requests = provider.requests();
    assert_eq!(requests.len(), 3, "the continue used the last step; then only the wrap-up");
    assert!(matches!(requests[2].0.last(), Some(ChatTurn::User { text }) if text.ends_with(STEP_LIMIT_NOTE)));
    assert!(crate::compact::alternates(&requests[2].0));
    assert_eq!(requests[2].1.tool_choice, ToolChoice::None);
    assert_eq!(announced_steps(&events), [true]);
    assert_eq!(end(&events), Some(StopReason::MaxSteps));
}

#[tokio::test]
async fn an_announced_step_with_tool_calls_is_an_ordinary_step() {
    let provider = Arc::new(ScriptedProvider::new(vec![
        // The same reply announced its step and called the tool: the calls run, nothing to rescue.
        vec![
            ProviderEvent::TextDelta("Now checking issue #7:".into()),
            ProviderEvent::ToolCall(call("c1", "a.ts")),
            ProviderEvent::Done { reason: Some("tool_calls".into()) },
        ],
        say("Done."),
    ]));
    let h = history(vec![user("go")]);
    let events = collect(options(provider.clone(), &h, plain())).await;
    assert_eq!(provider.requests().len(), 2);
    assert_eq!(announced_steps(&events), Vec::<bool>::new());
    assert_eq!(end(&events), Some(StopReason::End));
}

// #90: debug recording is a plain option on its way to the provider (which collects the
// raw payloads for the trajectory's request records when it's on).
#[tokio::test]
async fn debug_recording_reaches_the_provider() {
    let provider = Arc::new(ScriptedProvider::new(vec![say("Done.")]));
    let h = history(vec![user("go")]);
    let mut flagged = options(provider.clone(), &h, plain());
    flagged.debug_raw = true;
    collect(flagged).await;
    assert!(provider.requests()[0].1.debug_raw);

    let provider = Arc::new(ScriptedProvider::new(vec![say("Done.")]));
    collect(options(provider.clone(), &history(vec![user("go")]), plain())).await;
    assert!(!provider.requests()[0].1.debug_raw, "off by default");
}

// A reply cut off at the output limit.

fn cut_offs(events: &[LoopEvent]) -> Vec<bool> {
    events
        .iter()
        .filter_map(|e| match e {
            LoopEvent::CutOff { continued } => Some(*continued),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn cut_off_calls_arent_run_each_gets_a_result_and_the_run_goes_on() {
    let provider = Arc::new(ScriptedProvider::new(vec![
        cut_off(vec![ProviderEvent::ToolCall(call("c1", "a.ts"))]),
        say("Smaller steps, then."),
    ]));
    let h = history(vec![user("go")]);
    let ran = Arc::new(AtomicUsize::new(0));
    let counted = ran.clone();
    let counting: RunFn = Arc::new(move |c, cancel| {
        counted.fetch_add(1, Ordering::SeqCst);
        fake_tool()(c, cancel)
    });
    let events = collect(options(provider.clone(), &h, FakeRunner { run: counting, parallel: |_| false })).await;
    assert_eq!(ran.load(Ordering::SeqCst), 0);
    assert_eq!(cut_offs(&events), [true]);
    assert!(events.iter().any(|e| matches!(e, LoopEvent::ToolSkipped { call: c, output }
            if *c == call("c1", "a.ts") && output == not_run::CUT_OFF)));
    assert_eq!(provider.requests()[1].0.last(), Some(&tool("c1", not_run::CUT_OFF)));
    assert_eq!(end(&events), Some(StopReason::End));
}

#[tokio::test]
async fn cut_off_with_nothing_to_run_the_model_is_told_once_a_second_ends_the_run() {
    let provider =
        Arc::new(ScriptedProvider::new(vec![cut_off(vec![ProviderEvent::TextDelta("Here is the fi".into())]), cut_off(vec![])]));
    let h = history(vec![user("go")]);
    let events = collect(options(provider.clone(), &h, plain())).await;
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[1].0[requests[1].0.len() - 2..], [said("Here is the fi"), user(CUT_OFF_NOTE)]);
    assert_eq!(cut_offs(&events), [true, false]);
    assert_eq!(end(&events), Some(StopReason::Length));
}

#[tokio::test]
async fn only_two_cut_offs_in_a_row_end_the_run() {
    let provider =
        Arc::new(ScriptedProvider::new(vec![cut_off(vec![]), use_tools(&[call("c1", "a.ts")]), cut_off(vec![]), say("Done.")]));
    let h = history(vec![user("go")]);
    let events = collect(options(provider, &h, plain())).await;
    assert_eq!(cut_offs(&events), [true, true]);
    assert_eq!(end(&events), Some(StopReason::End));
}

#[tokio::test]
async fn after_a_reply_cut_off_while_only_reasoning_the_retry_reasons_less_until_a_step_finishes() {
    // TB 2026-11-06 dna-insert: three 64k-token reasoning-only replies in a row, about 400 s each.
    let thinking = || cut_off(vec![ProviderEvent::ThinkingDelta("Let me reconsider the primers…".into())]);
    let provider = Arc::new(ScriptedProvider::new(vec![
        thinking(),
        thinking(),
        use_tools(&[call("c1", "a.ts")]),
        cut_off(vec![ProviderEvent::TextDelta("Here is the fi".into())]),
        say("Done."),
    ]));
    let events = collect(options(provider.clone(), &history(vec![user("go")]), plain())).await;
    assert_eq!(end(&events), Some(StopReason::End));
    let caps: Vec<_> = provider.requests().iter().map(|(_, o)| o.effort_cap).collect();
    let (high, low) = (Some(CUT_OFF_RETRY_EFFORT), Some(CUT_OFF_LAST_RETRY_EFFORT));
    // A second in a row goes lower still; a cut-off that wrote text isn't reasoning too much: no cap after it.
    assert_eq!(caps, [None, high, low, None, None]);
    assert_eq!(notes(&events, NoteKind::CutOff).len(), 3);
}

#[tokio::test]
async fn replies_cut_off_while_only_reasoning_get_three_tries_then_the_run_ends() {
    let thinking = || cut_off(vec![ProviderEvent::ThinkingDelta("Let me consider every case…".into())]);
    let provider = Arc::new(ScriptedProvider::new(vec![thinking(), thinking(), thinking(), thinking(), say("never")]));
    let h = history(vec![user("go")]);
    let events = collect(options(provider.clone(), &h, plain())).await;
    assert_eq!(provider.requests().len(), 4, "three more tries after the first cut-off, then the run ends");
    assert_eq!(cut_offs(&events), [true, true, true, false]);
    assert_eq!(end(&events), Some(StopReason::Length));
    // A second reasoning-only cut-off that then acts: the run goes on.
    let provider = Arc::new(ScriptedProvider::new(vec![thinking(), thinking(), say("Done.")]));
    let h = history(vec![user("go")]);
    let events = collect(options(provider, &h, plain())).await;
    assert_eq!(cut_offs(&events), [true, true]);
    assert_eq!(end(&events), Some(StopReason::End));
}

#[tokio::test]
async fn a_cut_off_reply_that_was_all_reasoning_is_told_to_reason_briefly() {
    let provider = Arc::new(ScriptedProvider::new(vec![
        cut_off(vec![ProviderEvent::ThinkingDelta("Let me think about boids at length…".into())]),
        say("Done."),
    ]));
    let h = history(vec![user("go")]);
    collect(options(provider.clone(), &h, plain())).await;
    assert_eq!(
        provider.requests()[1].0.last(),
        Some(&user(&format!("go\n\n{CUT_OFF_REASONING_NOTE}"))),
        "joined, not a second user turn"
    );
}

#[tokio::test]
async fn a_consumer_that_stops_at_the_cut_off_event_still_leaves_every_call_answered() {
    let provider = Arc::new(ScriptedProvider::new(vec![cut_off(vec![ProviderEvent::ToolCall(call("c1", "a.ts"))])]));
    let h = history(vec![user("go")]);
    let mut stream = Box::pin(run_agent(options(provider, &h, plain())));
    while let Some(event) = stream.next().await {
        if matches!(event, LoopEvent::CutOff { .. }) {
            break;
        }
    }
    drop(stream);
    let turns = snapshot(&h);
    assert_eq!(answer_all_calls(&turns), turns);
    assert_eq!(turns.last(), Some(&tool("c1", not_run::CUT_OFF)));
}

#[tokio::test]
async fn cut_off_and_interrupted_reads_as_interrupted() {
    let cancel = CancellationToken::new();
    let provider = Arc::new(CancelsWhileStreaming {
        cancel: cancel.clone(),
        events: cut_off(vec![ProviderEvent::ToolCall(call("c1", "a.ts"))]),
    });
    let h = history(vec![user("go")]);
    let events = collect(AgentOptions::new(provider, h.clone(), "S".into(), specs(), Arc::new(plain()), cancel)).await;
    assert!(cut_offs(&events).is_empty());
    assert_eq!(snapshot(&h).last(), Some(&tool("c1", not_run::ABORTED)));
    assert_eq!(end(&events), Some(StopReason::Aborted));
}

// Parallel calls. Marv runs consecutive `agent` calls together; here the runner's `concurrent` decides (#58).

fn sub(id: &str, ms: u64) -> ToolCall {
    ToolCall { id: id.into(), name: "agent".into(), arguments: serde_json::json!({"description": id, "ms": ms}).to_string() }
}
fn is_agent(call: &ToolCall) -> bool {
    call.name == "agent"
}

#[derive(Default)]
struct Tracked {
    active: usize,
    max: usize,
    started: Vec<String>,
}

/// A runner that sleeps `ms` (from the arguments) and records how many run at once.
fn tracked(decline: Option<&'static str>) -> (Arc<Mutex<Tracked>>, FakeRunner) {
    let state = Arc::new(Mutex::new(Tracked::default()));
    let shared = state.clone();
    let run: RunFn = Arc::new(move |c, _| {
        let state = shared.clone();
        {
            let mut s = state.lock().unwrap();
            s.started.push(c.id.clone());
            s.active += 1;
            s.max = s.max.max(s.active);
        }
        Box::pin(async move {
            tokio::time::sleep(Duration::from_millis(arg(&c, "ms").as_u64().unwrap_or(0))).await;
            state.lock().unwrap().active -= 1;
            Ok(ToolRun { declined: Some(c.id.as_str()) == decline, ..ok(format!("out {}", c.id), c.id.clone()) })
        })
    });
    (state, FakeRunner { run, parallel: is_agent })
}

fn ends(events: &[LoopEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            LoopEvent::ToolEnd { call, .. } => Some(call.id.clone()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn consecutive_parallel_calls_run_at_once_results_go_in_call_order() {
    let provider = Arc::new(ScriptedProvider::new(vec![use_tools(&[sub("a", 60), sub("b", 10)]), say("Both done.")]));
    let h = history(vec![user("go")]);
    let (state, runner) = tracked(None);
    let events = collect(options(provider, &h, runner)).await;
    assert_eq!(state.lock().unwrap().max, 2);
    // b finished first…
    assert_eq!(ends(&events), ["b", "a"]);
    // …but the history is in call order, so every request is deterministic (prompt cache).
    assert_eq!(tool_texts(&h), ["out a", "out b"]);
    assert_eq!(end(&events), Some(StopReason::End));
}

#[tokio::test]
async fn at_most_max_parallel_run_at_once() {
    let calls: Vec<_> = (0..6).map(|i| sub(&format!("s{i}"), 20)).collect();
    let provider = Arc::new(ScriptedProvider::new(vec![use_tools(&calls), say("ok")]));
    let (state, runner) = tracked(None);
    collect(options(provider, &history(vec![user("go")]), runner)).await;
    let state = state.lock().unwrap();
    assert_eq!(state.max, MAX_PARALLEL);
    assert_eq!(state.started.len(), 6);
}

#[tokio::test]
async fn a_non_concurrent_call_between_them_splits_the_group() {
    let provider = Arc::new(ScriptedProvider::new(vec![use_tools(&[sub("a", 20), call("r", "x.ts"), sub("b", 20)]), say("ok")]));
    let (state, runner) = tracked(None);
    collect(options(provider, &history(vec![user("go")]), runner)).await;
    let state = state.lock().unwrap();
    assert_eq!(state.max, 1);
    assert_eq!(state.started, ["a", "r", "b"]);
}

#[tokio::test]
async fn by_default_reads_run_together_and_a_write_is_a_barrier() {
    // #58: no `concurrent` override, so the runner's kinds decide.
    struct Kinds(FakeRunner);
    #[async_trait]
    impl ToolRunner for Kinds {
        fn kind(&self, call: &ToolCall) -> ToolKind {
            self.0.kind(call)
        }
        async fn run(&self, call: ToolCall, cancel: CancellationToken) -> Result<ToolRun, MarvError> {
            self.0.run(call, cancel).await
        }
    }
    let read = |id: &str| ToolCall { name: "read_file".into(), ..sub(id, 20) };
    let write = ToolCall { name: "write_file".into(), ..sub("w", 0) };
    let provider = Arc::new(ScriptedProvider::new(vec![use_tools(&[read("a"), read("b"), write, read("c")]), say("ok")]));
    let (state, runner) = tracked(None);
    let h = history(vec![user("go")]);
    let opts = AgentOptions::new(provider, h.clone(), "S".into(), specs(), Arc::new(Kinds(runner)), CancellationToken::new());
    collect(opts).await;
    assert_eq!(state.lock().unwrap().max, 2);
    assert_eq!(state.lock().unwrap().started, ["a", "b", "w", "c"]);
    assert_eq!(tool_texts(&h), ["out a", "out b", "out w", "out c"]);
}

#[tokio::test]
async fn an_interrupt_answers_queued_calls_without_running_them() {
    let provider = Arc::new(ScriptedProvider::new(vec![use_tools(&[sub("a", 10), sub("b", 10), sub("c", 10)])]));
    let h = history(vec![user("go")]);
    let aborting: RunFn = Arc::new(|c, cancel| {
        cancel.cancel();
        Box::pin(async move { Ok(ok(format!("out {}", c.id), c.id.clone())) })
    });
    let events =
        collect(AgentOptions { max_parallel: 1, ..options(provider, &h, FakeRunner { run: aborting, parallel: is_agent }) })
            .await;
    assert_eq!(tool_texts(&h), ["out a", not_run::ABORTED, not_run::ABORTED]);
    assert_eq!(end(&events), Some(StopReason::Aborted));
}

#[tokio::test]
async fn a_no_in_a_parallel_group_lets_the_running_ones_finish_then_stops() {
    let provider = Arc::new(ScriptedProvider::new(vec![use_tools(&[sub("a", 5), sub("b", 30)]), say("never")]));
    let h = history(vec![user("go")]);
    let (_, runner) = tracked(Some("a"));
    let events = collect(options(provider.clone(), &h, runner)).await;
    assert_eq!(tool_texts(&h), ["out a", "out b"]);
    assert_eq!(provider.requests().len(), 1);
    assert_eq!(end(&events), Some(StopReason::Declined));
}

/// Pulls until the first ToolStart, then stops (as a consumer whose handler fails would).
async fn stop_at_first_tool_start(provider: Arc<ScriptedProvider>, runner: FakeRunner) {
    let mut stream = Box::pin(run_agent(options(provider, &history(vec![user("go")]), runner)));
    while let Some(event) = stream.next().await {
        if matches!(event, LoopEvent::ToolStart { .. }) {
            break;
        }
    }
    drop(stream);
    tokio::time::sleep(Duration::from_millis(20)).await; // anything already launched would have started by now
}

#[tokio::test]
async fn a_tool_doesnt_start_until_its_tool_start_was_delivered_single_call() {
    let (state, runner) = tracked(None);
    stop_at_first_tool_start(Arc::new(ScriptedProvider::new(vec![use_tools(&[call("r", "x.ts")]), say("ok")])), runner).await;
    assert!(state.lock().unwrap().started.is_empty());
}

#[tokio::test]
async fn a_tool_doesnt_start_until_its_tool_start_was_delivered_parallel_group() {
    let (state, runner) = tracked(None);
    stop_at_first_tool_start(Arc::new(ScriptedProvider::new(vec![use_tools(&[sub("a", 5), sub("b", 5)]), say("ok")])), runner)
        .await;
    assert!(state.lock().unwrap().started.is_empty());
}

#[tokio::test]
async fn a_no_in_a_parallel_group_answers_its_queued_calls_without_running_them() {
    let provider = Arc::new(ScriptedProvider::new(vec![use_tools(&[sub("a", 5), sub("b", 5), sub("c", 5)]), say("never")]));
    let h = history(vec![user("go")]);
    let (state, runner) = tracked(Some("a"));
    let events = collect(AgentOptions { max_parallel: 1, ..options(provider, &h, runner) }).await;
    assert_eq!(state.lock().unwrap().started, ["a"]);
    assert_eq!(tool_texts(&h), ["out a", not_run::DECLINED, not_run::DECLINED]);
    assert_eq!(end(&events), Some(StopReason::Declined));
}

#[tokio::test]
async fn a_tool_that_fails_in_a_group_becomes_an_error_result_and_the_run_goes_on() {
    let provider = Arc::new(ScriptedProvider::new(vec![use_tools(&[sub("a", 0), sub("b", 0), sub("c", 0)]), say("Recovered.")]));
    let h = history(vec![user("go")]);
    let failing: RunFn = Arc::new(|c, _| {
        Box::pin(async move {
            match c.id.as_str() {
                "a" => Err(MarvError::Tool("boom".into())),
                "b" => Err(MarvError::Provider("not a tool error".into())),
                _ => Ok(ok("out c", "c")),
            }
        })
    });
    let events = collect(options(provider.clone(), &h, FakeRunner { run: failing, parallel: is_agent })).await;
    assert_eq!(tool_texts(&h), ["agent failed: boom", "agent failed: provider: not a tool error", "out c"]);
    assert!(events.iter().any(|e| matches!(e, LoopEvent::ToolEnd { run, .. } if run.result.is_error)));
    assert_eq!(provider.requests().len(), 2);
    assert_eq!(end(&events), Some(StopReason::End));
}

#[tokio::test]
async fn after_an_interrupt_reserved_and_queued_calls_are_answered_without_a_tool_start() {
    let provider = Arc::new(ScriptedProvider::new(vec![use_tools(&[sub("a", 10), sub("b", 10), sub("c", 10)])]));
    let h = history(vec![user("go")]);
    let started = Arc::new(Mutex::new(Vec::new()));
    let seen = started.clone();
    let aborting: RunFn = Arc::new(move |c, cancel| {
        seen.lock().unwrap().push(c.id.clone());
        cancel.cancel();
        Box::pin(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            Ok(ok(format!("out {}", c.id), c.id.clone()))
        })
    });
    let events =
        collect(AgentOptions { max_parallel: 2, ..options(provider, &h, FakeRunner { run: aborting, parallel: is_agent }) })
            .await;
    // b had a slot too, but the interrupt came before its ToolStart was delivered.
    assert_eq!(*started.lock().unwrap(), ["a"]);
    assert_eq!(starts(&events), ["a"]);
    assert_eq!(tool_texts(&h), ["out a", not_run::ABORTED, not_run::ABORTED]);
    assert_eq!(end(&events), Some(StopReason::Aborted));
}

#[tokio::test]
async fn a_call_after_a_declined_parallel_group_isnt_run() {
    let provider = Arc::new(ScriptedProvider::new(vec![use_tools(&[sub("a", 5), sub("b", 5), call("r", "x.ts")]), say("never")]));
    let h = history(vec![user("go")]);
    let (state, runner) = tracked(Some("a"));
    let events = collect(options(provider, &h, runner)).await;
    assert_eq!(state.lock().unwrap().started, ["a", "b"]);
    assert_eq!(tool_texts(&h), ["out a", "out b", not_run::DECLINED]);
    assert_eq!(end(&events), Some(StopReason::Declined));
}

/// Esc at an approval declines what's waiting and aborts the run: the model must read that as an
/// interrupt, not as the user saying no to those calls.
fn esc_on(id: &'static str) -> FakeRunner {
    let run: RunFn = Arc::new(move |c, cancel| {
        if c.id == id {
            cancel.cancel();
        }
        Box::pin(async move {
            tokio::time::sleep(Duration::from_millis(5)).await;
            Ok(ToolRun { declined: c.id == id, ..ok(format!("out {}", c.id), c.id.clone()) })
        })
    });
    FakeRunner { run, parallel: is_agent }
}

#[tokio::test]
async fn after_esc_a_groups_queued_calls_read_as_interrupted() {
    let provider = Arc::new(ScriptedProvider::new(vec![use_tools(&[sub("a", 5), sub("b", 5), sub("c", 5)]), say("never")]));
    let h = history(vec![user("go")]);
    let events = collect(AgentOptions { max_parallel: 1, ..options(provider, &h, esc_on("a")) }).await;
    assert_eq!(tool_texts(&h), ["out a", not_run::ABORTED, not_run::ABORTED]);
    assert_eq!(end(&events), Some(StopReason::Aborted));
}

#[tokio::test]
async fn after_esc_the_calls_after_the_group_read_as_interrupted_too() {
    let provider = Arc::new(ScriptedProvider::new(vec![use_tools(&[sub("a", 5), call("r", "x.ts")]), say("never")]));
    let h = history(vec![user("go")]);
    collect(options(provider, &h, esc_on("a"))).await;
    assert_eq!(tool_texts(&h), ["out a", not_run::ABORTED]);
}

#[tokio::test]
async fn an_agent_call_is_labeled_with_its_type_and_description_while_it_runs() {
    let typed = ToolCall {
        id: "t".into(),
        name: "agent".into(),
        arguments: serde_json::json!({"type": "explore", "description": "Find the config"}).to_string(),
    };
    let provider = Arc::new(ScriptedProvider::new(vec![use_tools(&[sub("Read notes", 0), typed]), say("ok")]));
    let (_, runner) = tracked(None);
    let events = collect(options(provider, &history(vec![user("go")]), runner)).await;
    let labels: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            LoopEvent::ToolStart { label, .. } => Some(label.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(labels, ["general-purpose · Read notes", "explore · Find the config"]);
}

#[tokio::test]
async fn a_tools_display_payload_is_for_the_transcript_the_model_gets_only_the_output() {
    let provider = Arc::new(ScriptedProvider::new(vec![use_tools(&[call("c1", "a.ts")]), say("Done.")]));
    let with_diff: RunFn = Arc::new(|c, cancel| {
        Box::pin(async move {
            let run = fake_tool()(c, cancel).await?;
            Ok(ToolRun { display: Some(Arc::new("SECRET".to_string())), ..run })
        })
    });
    collect(options(provider.clone(), &history(vec![user("go")]), FakeRunner { run: with_diff, parallel: |_| false })).await;
    assert!(!format!("{:?}", provider.requests()[1].0).contains("SECRET"));
}

// after_step.

fn after_step(f: impl Fn(Vec<ToolRun>) -> Option<String> + Send + Sync + 'static) -> Option<AfterStep> {
    Some(Arc::new(move |runs| {
        let note = f(runs);
        Box::pin(async move { note })
    }))
}

#[tokio::test]
async fn after_steps_note_goes_in_after_all_of_the_steps_results_before_the_next_request() {
    let provider = Arc::new(ScriptedProvider::new(vec![use_tools(&[call("c1", "a.ts"), call("c2", "b.ts")]), say("Fixed.")]));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let record = seen.clone();
    collect(AgentOptions {
        after_step: after_step(move |runs| {
            record.lock().unwrap().push(runs.iter().map(|r| r.result.output.clone()).collect::<Vec<_>>());
            Some("Marv: 1 new error".into())
        }),
        ..options(provider.clone(), &history(vec![user("go")]), plain())
    })
    .await;
    assert_eq!(*seen.lock().unwrap(), [["contents of a.ts", "contents of b.ts"]]);
    let requests = provider.requests();
    let second = &requests[1].0;
    assert!(matches!(&second[second.len() - 3..], [ChatTurn::Tool { .. }, ChatTurn::Tool { .. }, ChatTurn::User { .. }]));
    assert_eq!(second.last(), Some(&user("Marv: 1 new error")));
    // Appended, not inserted: the second request still starts with the first one (prompt cache).
    assert_eq!(second[..requests[0].0.len()], requests[0].0[..]);
}

#[tokio::test]
async fn after_step_none_adds_nothing() {
    let provider = Arc::new(ScriptedProvider::new(vec![use_tools(&[call("c1", "a.ts")]), say("Done.")]));
    let calls = Arc::new(AtomicUsize::new(0));
    let counted = calls.clone();
    collect(AgentOptions {
        after_step: after_step(move |_| {
            counted.fetch_add(1, Ordering::SeqCst);
            None
        }),
        ..options(provider.clone(), &history(vec![user("go")]), plain())
    })
    .await;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(matches!(provider.requests()[1].0.last(), Some(ChatTurn::Tool { .. })));
}

/// How many times `after_step` ran in a run of `script` with `runner`.
async fn after_step_calls(script: Vec<Vec<ProviderEvent>>, runner: FakeRunner) -> (usize, Vec<LoopEvent>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let counted = calls.clone();
    let events = collect(AgentOptions {
        after_step: after_step(move |_| {
            counted.fetch_add(1, Ordering::SeqCst);
            None
        }),
        ..options(Arc::new(ScriptedProvider::new(script)), &history(vec![user("go")]), runner)
    })
    .await;
    (calls.load(Ordering::SeqCst), events)
}

#[tokio::test]
async fn after_step_is_not_called_after_a_plain_answer() {
    assert_eq!(after_step_calls(vec![say("Hi.")], plain()).await.0, 0);
}

#[tokio::test]
async fn after_step_stopped_during_it_ends_the_run_with_every_call_answered_and_no_note() {
    let provider = Arc::new(ScriptedProvider::new(vec![use_tools(&[call("c1", "a.ts")]), say("never asked")]));
    let h = history(vec![user("go")]);
    let opts = options(provider.clone(), &h, plain());
    let cancel = opts.cancel.clone();
    let events = collect(AgentOptions {
        after_step: after_step(move |_| {
            cancel.cancel();
            Some("Marv: too late".into())
        }),
        ..opts
    })
    .await;
    assert_eq!(end(&events), Some(StopReason::Aborted));
    assert_eq!(provider.requests().len(), 1);
    assert!(matches!(snapshot(&h).last(), Some(ChatTurn::Tool { .. })));
}

#[tokio::test]
async fn after_step_is_not_called_after_a_declined_call() {
    let declining: RunFn =
        Arc::new(|c, cancel| Box::pin(async move { Ok(ToolRun { declined: true, ..fake_tool()(c, cancel).await? }) }));
    let (calls, events) = after_step_calls(
        vec![use_tools(&[call("c1", "a.ts")]), say("never asked")],
        FakeRunner { run: declining, parallel: |_| false },
    )
    .await;
    assert_eq!(end(&events), Some(StopReason::Declined));
    assert_eq!(calls, 0);
}

#[tokio::test]
async fn after_step_is_not_called_after_a_cut_off_reply() {
    let script = vec![cut_off(vec![ProviderEvent::ToolCall(call("c1", "a.ts"))]), say("Smaller steps, then.")];
    assert_eq!(after_step_calls(script, plain()).await.0, 0);
}

#[tokio::test]
async fn after_step_gets_a_parallel_groups_results_in_call_order() {
    let provider = Arc::new(ScriptedProvider::new(vec![use_tools(&[call("a", "a.ts"), call("b", "b.ts")]), say("Done.")]));
    let slow_a: RunFn = Arc::new(|c, cancel| {
        Box::pin(async move {
            if c.id == "a" {
                tokio::time::sleep(Duration::from_millis(40)).await;
            }
            fake_tool()(c, cancel).await
        })
    });
    let seen = Arc::new(Mutex::new(Vec::new()));
    let record = seen.clone();
    collect(AgentOptions {
        after_step: after_step(move |runs| {
            record.lock().unwrap().push(runs.iter().map(|r| r.result.output.clone()).collect::<Vec<_>>());
            None
        }),
        ..options(provider, &history(vec![user("go")]), FakeRunner { run: slow_a, parallel: |_| true })
    })
    .await;
    assert_eq!(*seen.lock().unwrap(), [["contents of a.ts", "contents of b.ts"]]);
}

#[tokio::test]
async fn at_the_step_limit_the_note_is_already_in_the_history_when_on_limit_is_asked() {
    let provider = Arc::new(ScriptedProvider::new(vec![use_tools(&[call("c1", "a.ts")]), say("never")]));
    let h = history(vec![user("go")]);
    let seen = h.clone();
    let last = Arc::new(Mutex::new(None));
    let record = last.clone();
    collect(AgentOptions {
        max_steps: 1,
        after_step: after_step(|_| Some("Marv: note".into())),
        on_limit: on_limit(move |_| {
            *record.lock().unwrap() = seen.lock().unwrap().last().cloned();
            LimitAnswer::Stop
        }),
        ..options(provider, &h, plain())
    })
    .await;
    assert_eq!(*last.lock().unwrap(), Some(user("Marv: note")));
}

#[tokio::test]
async fn on_full_compacts_mid_turn_and_the_next_request_starts_from_the_summary() {
    let provider = Arc::new(ScriptedProvider::new(vec![use_tools(&[call("c1", "a.ts")]), say("Done.")]));
    let h = history(vec![user("go")]);
    let swapped = h.clone();
    let on_full: OnFull = Arc::new(move || {
        *swapped.lock().unwrap() = vec![user("summary"), user(crate::compact::MID_TURN_NOTE)];
        Box::pin(async { true })
    });
    let events = collect(AgentOptions { on_full: Some(on_full), ..options(provider.clone(), &h, plain()) }).await;
    assert!(events.iter().any(|e| matches!(e, LoopEvent::Compacted)));
    assert_eq!(provider.requests()[1].0, [user("summary"), user(crate::compact::MID_TURN_NOTE)]);
    assert_eq!(end(&events), Some(StopReason::End));
}

// ---- Marv #63: overflow recovery ----

const OVERFLOW: &str = "OpenRouter returned 400.\nThis endpoint's maximum context length is 1000 tokens. However, you requested about 1200 tokens (1200 of text input, 0 in the output).";

fn overflow() -> Vec<ProviderEvent> {
    vec![ProviderEvent::TextDelta("partial".into()), ProviderEvent::Error(OVERFLOW.into())]
}

/// A hook answering from a list, recording each attempt; it swaps the history for `[user("fitted")]`.
fn on_overflow(h: &History, answers: Vec<OverflowOutcome>) -> (OnOverflow, Arc<Mutex<Vec<u32>>>) {
    let attempts = Arc::new(Mutex::new(Vec::new()));
    let (seen, h, answers) = (attempts.clone(), h.clone(), Arc::new(Mutex::new(VecDeque::from(answers))));
    let hook: OnOverflow = Arc::new(move |error, attempt| {
        assert!(error.contains("maximum context length"));
        seen.lock().unwrap().push(attempt);
        *h.lock().unwrap() = vec![user("fitted")];
        let answer = answers.lock().unwrap().pop_front().unwrap_or(OverflowOutcome::Failed("gave up".into()));
        Box::pin(async move { answer })
    });
    (hook, attempts)
}

#[tokio::test]
async fn an_overflow_with_the_hook_retries_the_same_step_with_nothing_of_the_failed_reply() {
    let provider = Arc::new(ScriptedProvider::new(vec![overflow(), say("Done.")]));
    let h = history(vec![user("go")]);
    let (hook, attempts) = on_overflow(&h, vec![OverflowOutcome::Recovered]);
    let events = collect(AgentOptions { on_overflow: Some(hook), ..options(provider.clone(), &h, plain()) }).await;
    assert!(events.iter().any(|e| matches!(e, LoopEvent::Overflow { attempt: 0 })));
    assert!(!events.iter().any(|e| matches!(e, LoopEvent::Error(_))));
    assert_eq!(*attempts.lock().unwrap(), [0]);
    assert_eq!(provider.requests()[1].0, [user("fitted")], "the failed reply's partial text never entered it");
    assert_eq!(end(&events), Some(StopReason::End));
}

#[tokio::test]
async fn an_overflow_hook_that_fails_ends_with_its_text_and_none_ends_as_before() {
    let provider = Arc::new(ScriptedProvider::new(vec![overflow()]));
    let h = history(vec![user("go")]);
    let (hook, _) = on_overflow(&h, vec![OverflowOutcome::Failed("too big".into())]);
    let events = collect(AgentOptions { on_overflow: Some(hook), ..options(provider, &h, plain()) }).await;
    assert!(matches!(&events[events.len() - 2..], [LoopEvent::Error(m), LoopEvent::Done(StopReason::Error)] if m == "too big"));

    let provider = Arc::new(ScriptedProvider::new(vec![overflow()]));
    let h = history(vec![user("go")]);
    let events = collect(options(provider, &h, plain())).await;
    assert!(!events.iter().any(|e| matches!(e, LoopEvent::Overflow { .. })));
    assert!(matches!(&events[events.len() - 2..], [LoopEvent::Error(m), LoopEvent::Done(StopReason::Error)] if m == OVERFLOW));
}

#[tokio::test]
async fn at_most_four_recoveries_a_step_and_the_count_resets_after_a_good_step() {
    let mut script = vec![overflow(), use_tools(&[call("c1", "a.ts")])];
    script.extend((0..5).map(|_| overflow()));
    let provider = Arc::new(ScriptedProvider::new(script));
    let h = history(vec![user("go")]);
    let (hook, attempts) = on_overflow(&h, vec![OverflowOutcome::Recovered; 10]);
    let events = collect(AgentOptions { on_overflow: Some(hook), ..options(provider.clone(), &h, plain()) }).await;
    // Step 1: one recovery; step 2: four, then the fifth overflow gives up.
    assert_eq!(*attempts.lock().unwrap(), [0, 0, 1, 2, 3, 4]);
    assert_eq!(provider.requests().len(), 2 + 5);
    assert!(matches!(&events[events.len() - 2..], [LoopEvent::Error(m), LoopEvent::Done(StopReason::Error)] if m == OVERFLOW));
}

#[tokio::test]
async fn the_wrap_up_request_is_recovered_too_and_sent_again_as_the_wrap_up() {
    // max_steps 1, no on_limit: the step limit's tools-off wrap-up (#59) overflows, recovers, and is sent again.
    let provider = Arc::new(ScriptedProvider::new(vec![use_tools(&[call("c1", "a.ts")]), overflow(), say("Wrapped up.")]));
    let h = history(vec![user("go")]);
    let (hook, attempts) = on_overflow(&h, vec![OverflowOutcome::Recovered]);
    let opts =
        AgentOptions { max_steps: 1, wrap_up_at_limit: true, on_overflow: Some(hook), ..options(provider.clone(), &h, plain()) };
    let events = collect(opts).await;
    assert_eq!(*attempts.lock().unwrap(), [0]);
    let requests = provider.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[2].1.tool_choice, ToolChoice::None, "still the wrap-up");
    assert_eq!(requests[2].0.last(), Some(&user(&format!("fitted\n\n{STEP_LIMIT_NOTE}"))), "its note again");
    assert_eq!(end(&events), Some(StopReason::MaxSteps));
}

#[tokio::test]
async fn esc_during_the_overflow_hook_ends_aborted() {
    let provider = Arc::new(ScriptedProvider::new(vec![overflow(), say("never")]));
    let h = history(vec![user("go")]);
    let opts = options(provider.clone(), &h, plain());
    let cancel = opts.cancel.clone();
    let hook: OnOverflow = Arc::new(move |_, _| {
        cancel.cancel();
        Box::pin(async { OverflowOutcome::Recovered })
    });
    let events = collect(AgentOptions { on_overflow: Some(hook), ..opts }).await;
    assert_eq!(end(&events), Some(StopReason::Aborted));
    assert!(!events.iter().any(|e| matches!(e, LoopEvent::Error(_))));
    assert_eq!(provider.requests().len(), 1);

    let provider = Arc::new(ScriptedProvider::new(vec![overflow()]));
    let h = history(vec![user("go")]);
    let hook: OnOverflow = Arc::new(|_, _| Box::pin(async { OverflowOutcome::Stopped }));
    let events = collect(AgentOptions { on_overflow: Some(hook), ..options(provider, &h, plain()) }).await;
    assert_eq!(end(&events), Some(StopReason::Aborted));
    assert!(!events.iter().any(|e| matches!(e, LoopEvent::Error(_))));
}

#[tokio::test]
async fn a_note_after_a_mid_turn_compaction_joins_its_note_and_every_request_alternates() {
    let provider = Arc::new(ScriptedProvider::new(vec![use_tools(&[call("c1", "a.ts")]), say("Done.")]));
    let h = history(vec![user("go")]);
    let swapped = h.clone();
    let on_full: OnFull = Arc::new(move || {
        let ack = ChatTurn::Assistant { text: crate::compact::COMPACTED_ACK.into(), tool_calls: vec![], reasoning: None };
        *swapped.lock().unwrap() = vec![user("summary"), ack, user(crate::compact::MID_TURN_NOTE)];
        Box::pin(async { true })
    });
    let opts = AgentOptions {
        on_full: Some(on_full),
        after_step: after_step(|_| Some("Marv: the typecheck found 1 new error".into())),
        ..options(provider.clone(), &h, plain())
    };
    collect(opts).await;
    for (sent, _) in provider.requests() {
        assert!(crate::compact::alternates(&sent), "{sent:?}");
    }
    let last = provider.requests()[1].0.last().cloned().unwrap();
    assert_eq!(last, user(&format!("{}\n\nMarv: the typecheck found 1 new error", crate::compact::MID_TURN_NOTE)));
}

#[tokio::test]
async fn esc_during_on_full_ends_the_run() {
    let provider = Arc::new(ScriptedProvider::new(vec![use_tools(&[call("c1", "a.ts")]), say("never")]));
    let h = history(vec![user("go")]);
    let opts = options(provider.clone(), &h, plain());
    let cancel = opts.cancel.clone();
    let on_full: OnFull = Arc::new(move || {
        cancel.cancel();
        Box::pin(async { true })
    });
    let events = collect(AgentOptions { on_full: Some(on_full), ..opts }).await;
    assert_eq!(end(&events), Some(StopReason::Aborted));
    assert_eq!(provider.requests().len(), 1);
}

// Issue #5: whatever ends a run, the history it leaves must be one the API accepts. Each step mixes
// plain calls with a parallel group of agent calls (a slow one in the middle), so the groups are
// covered too.

fn mixed(n: &str) -> Vec<ToolCall> {
    vec![
        call(&format!("{n}r1"), "a.ts"),
        sub(&format!("{n}a"), 5),
        sub(&format!("{n}b"), 30),
        sub(&format!("{n}c"), 5),
        call(&format!("{n}r2"), "b.ts"),
    ]
}
fn steps(n: usize) -> Vec<Vec<ProviderEvent>> {
    (1..=n).map(|i| use_tools(&mixed(&format!("s{i}")))).collect()
}
fn cut_off_calls(calls: Vec<ToolCall>) -> Vec<ProviderEvent> {
    cut_off(calls.into_iter().map(ProviderEvent::ToolCall).collect())
}

#[derive(Default)]
struct Drive {
    /// The call to cancel on (ctrl+c while it runs), and to answer "no" (both: Esc at an approval).
    abort_on: Option<&'static str>,
    decline_on: Option<&'static str>,
    /// Where the consumer stops (it drops the stream: a break or a failure, alike here).
    stop_at: Option<StopAt>,
}

type StopAt = Box<dyn Fn(&LoopEvent) -> bool + Send>;
/// The call of a tool event of one kind, for where a consumer stops.
type PickCall = fn(&LoopEvent) -> Option<&ToolCall>;

struct Driven {
    history: Vec<ChatTurn>,
    events: Vec<LoopEvent>,
    stopped: bool,
    calls: usize,
}

impl Driven {
    fn end(&self) -> Option<StopReason> {
        end(&self.events)
    }
}

async fn drive(script: Vec<Vec<ProviderEvent>>, how: Drive, configure: impl FnOnce(AgentOptions) -> AgentOptions) -> Driven {
    let Drive { abort_on, decline_on, stop_at } = how;
    let h = history(vec![user("go")]);
    let run: RunFn = Arc::new(move |c, cancel| {
        if Some(c.id.as_str()) == abort_on {
            cancel.cancel();
        }
        Box::pin(async move {
            tokio::time::sleep(Duration::from_millis(arg(&c, "ms").as_u64().unwrap_or(0))).await;
            Ok(ToolRun { declined: Some(c.id.as_str()) == decline_on, ..ok(format!("out {}", c.id), c.id.clone()) })
        })
    });
    let opts = configure(AgentOptions {
        max_parallel: 2,
        ..options(Arc::new(ScriptedProvider::new(script)), &h, FakeRunner { run, parallel: is_agent })
    });
    let mut stream = Box::pin(run_agent(opts));
    let mut events = Vec::new();
    let mut stopped = false;
    while let Some(event) = stream.next().await {
        let stop = stop_at.as_ref().is_some_and(|f| f(&event));
        events.push(event);
        if stop {
            stopped = true;
            break;
        }
    }
    drop(stream);
    tokio::time::sleep(Duration::from_millis(50)).await; // tools still running after the consumer left must not add anything
    let history = snapshot(&h);
    let calls = history
        .iter()
        .map(|t| match t {
            ChatTurn::Assistant { tool_calls, .. } => tool_calls.len(),
            _ => 0,
        })
        .sum();
    Driven { history, events, stopped, calls }
}

fn same(o: AgentOptions) -> AgentOptions {
    o
}

#[tokio::test]
async fn every_call_answered_a_run_that_answers() {
    let mut script = steps(2);
    script.push(say("Done."));
    let d = drive(script, Drive::default(), same).await;
    assert_eq!(d.calls, 10);
    assert_eq!(answer_problems(&d.history), Vec::<String>::new());
    assert_eq!(d.end(), Some(StopReason::End));
}

fn two_steps_then_never() -> Vec<Vec<ProviderEvent>> {
    let mut script = steps(2);
    script.push(say("never"));
    script
}

#[tokio::test]
async fn every_call_answered_ctrl_c_mid_tools() {
    for id in ["s2r1", "s2a", "s2b", "s2r2"] {
        let d = drive(two_steps_then_never(), Drive { abort_on: Some(id), ..Default::default() }, same).await;
        assert_eq!(answer_problems(&d.history), Vec::<String>::new(), "while {id} runs");
        assert_eq!(d.end(), Some(StopReason::Aborted), "while {id} runs");
    }
}

#[tokio::test]
async fn every_call_answered_a_no_from_the_approver() {
    for id in ["s2r1", "s2a", "s2b", "s2r2"] {
        let d = drive(two_steps_then_never(), Drive { decline_on: Some(id), ..Default::default() }, same).await;
        assert_eq!(answer_problems(&d.history), Vec::<String>::new(), "to {id}");
        assert_eq!(d.end(), Some(StopReason::Declined), "to {id}");
    }
}

#[tokio::test]
async fn every_call_answered_esc_at_an_approval_in_the_group() {
    let d =
        drive(two_steps_then_never(), Drive { decline_on: Some("s2b"), abort_on: Some("s2b"), ..Default::default() }, same).await;
    assert_eq!(answer_problems(&d.history), Vec::<String>::new());
    assert_eq!(d.end(), Some(StopReason::Aborted));
}

#[tokio::test]
async fn every_call_answered_a_consumer_that_stops() {
    let cases: [(&str, PickCall, &str); 4] = [
        ("at a plain call's tool_start", |e| if let LoopEvent::ToolStart { call, .. } = e { Some(call) } else { None }, "s2r1"),
        (
            "at a group's first tool_start (the other has a slot)",
            |e| if let LoopEvent::ToolStart { call, .. } = e { Some(call) } else { None },
            "s2a",
        ),
        (
            "at a group's tool_end, with a call still running",
            |e| if let LoopEvent::ToolEnd { call, .. } = e { Some(call) } else { None },
            "s2a",
        ),
        ("at the last call's tool_end", |e| if let LoopEvent::ToolEnd { call, .. } = e { Some(call) } else { None }, "s2r2"),
    ];
    for (name, pick, id) in cases {
        let stop_at = Box::new(move |e: &LoopEvent| pick(e).is_some_and(|c| c.id == id));
        let d = drive(two_steps_then_never(), Drive { stop_at: Some(stop_at), ..Default::default() }, same).await;
        assert!(d.stopped, "{name}: never stopped");
        assert_eq!(pick(d.events.last().unwrap()).map(|c| c.id.as_str()), Some(id), "{name}: nothing came after it");
        assert_eq!(answer_problems(&d.history), Vec::<String>::new(), "{name}");
    }
}

#[tokio::test]
async fn every_call_answered_a_consumer_that_stops_at_a_tool_skipped_after_a_no() {
    let d = drive(
        two_steps_then_never(),
        Drive {
            decline_on: Some("s2r1"),
            stop_at: Some(Box::new(|e| matches!(e, LoopEvent::ToolSkipped { .. }))),
            ..Default::default()
        },
        same,
    )
    .await;
    assert!(matches!(d.events.last(), Some(LoopEvent::ToolSkipped { call, .. }) if call.id == "s2a"));
    assert_eq!(answer_problems(&d.history), Vec::<String>::new());
}

#[tokio::test]
async fn every_call_answered_a_cut_off_once() {
    let mut script = steps(1);
    script.push(cut_off_calls(mixed("s2")));
    script.push(say("Smaller steps."));
    let d = drive(script, Drive::default(), same).await;
    assert_eq!(answer_problems(&d.history), Vec::<String>::new());
    assert_eq!(d.end(), Some(StopReason::End));
}

#[tokio::test]
async fn every_call_answered_cut_off_twice_in_a_row() {
    let mut script = steps(1);
    script.push(cut_off_calls(mixed("s2")));
    script.push(cut_off_calls(mixed("s3")));
    let d = drive(script, Drive::default(), same).await;
    assert_eq!(d.calls, 15);
    assert_eq!(answer_problems(&d.history), Vec::<String>::new());
    assert_eq!(d.end(), Some(StopReason::Length));
}

#[tokio::test]
async fn every_call_answered_a_consumer_that_stops_at_a_cut_off_calls_tool_skipped() {
    let d = drive(
        vec![cut_off_calls(mixed("s1"))],
        Drive { stop_at: Some(Box::new(|e| matches!(e, LoopEvent::ToolSkipped { .. }))), ..Default::default() },
        same,
    )
    .await;
    assert!(matches!(d.events.last(), Some(LoopEvent::ToolSkipped { call, .. }) if call.id == "s1r1"));
    assert_eq!(answer_problems(&d.history), Vec::<String>::new());
}

#[tokio::test]
async fn every_call_answered_max_steps_0_turns_the_limit_off() {
    let mut script = steps(3);
    script.push(say("Done."));
    let d = drive(script, Drive::default(), |o| AgentOptions { max_steps: 0, ..o }).await;
    assert_eq!(d.calls, 15, "3 steps × 5 calls, none cut off by a limit");
    assert_eq!(answer_problems(&d.history), Vec::<String>::new());
    assert_eq!(d.end(), Some(StopReason::End));
}

#[tokio::test]
async fn every_call_answered_without_on_limit_a_hard_stop() {
    let d = drive(steps(5), Drive::default(), |o| AgentOptions { max_steps: 2, ..o }).await;
    assert_eq!(d.calls, 10);
    assert_eq!(answer_problems(&d.history), Vec::<String>::new());
    assert_eq!(d.end(), Some(StopReason::MaxSteps));
}

#[tokio::test]
async fn every_call_answered_with_on_limit_a_no_stops_it() {
    let d = drive(steps(5), Drive::default(), |o| AgentOptions { max_steps: 2, on_limit: on_limit(|_| LimitAnswer::Stop), ..o })
        .await;
    assert_eq!(answer_problems(&d.history), Vec::<String>::new());
    assert_eq!(d.end(), Some(StopReason::MaxSteps));
}

#[tokio::test]
async fn every_call_answered_with_on_limit_unavailable_stops_like_a_hard_limit() {
    let d = drive(steps(5), Drive::default(), |o| AgentOptions {
        max_steps: 2,
        on_limit: on_limit(|_| LimitAnswer::Unavailable),
        ..o
    })
    .await;
    assert_eq!(answer_problems(&d.history), Vec::<String>::new());
    assert!(d.events.iter().any(|e| matches!(e, LoopEvent::StepLimit { steps: 2, continued: false })));
    assert!(d.events.iter().any(|e| matches!(e, LoopEvent::Error(m) if m.contains("without a final answer"))));
    assert_eq!(d.end(), Some(StopReason::MaxSteps));
}

#[tokio::test]
async fn every_call_answered_with_on_limit_a_yes_goes_on_to_the_next_limit() {
    let d = drive(steps(5), Drive::default(), |o| AgentOptions {
        max_steps: 2,
        on_limit: on_limit(|n| if n < 4 { LimitAnswer::Continue } else { LimitAnswer::Stop }),
        ..o
    })
    .await;
    assert_eq!(d.calls, 20);
    assert_eq!(answer_problems(&d.history), Vec::<String>::new());
    assert_eq!(d.end(), Some(StopReason::MaxSteps));
}

#[tokio::test]
async fn every_call_answered_with_on_limit_esc_while_it_asks() {
    let d = drive(steps(5), Drive::default(), |o| {
        let cancel = o.cancel.clone();
        AgentOptions {
            max_steps: 2,
            on_limit: on_limit(move |_| {
                cancel.cancel();
                LimitAnswer::Stop
            }),
            ..o
        }
    })
    .await;
    assert_eq!(answer_problems(&d.history), Vec::<String>::new());
    assert_eq!(d.end(), Some(StopReason::Aborted));
}

#[tokio::test]
async fn every_call_answered_a_consumer_that_stops_at_the_step_limit_event() {
    let d = drive(
        steps(5),
        Drive { stop_at: Some(Box::new(|e| matches!(e, LoopEvent::StepLimit { .. }))), ..Default::default() },
        |o| AgentOptions { max_steps: 2, on_limit: on_limit(|_| LimitAnswer::Continue), ..o },
    )
    .await;
    assert!(matches!(d.events.last(), Some(LoopEvent::StepLimit { steps: 2, continued: true })));
    assert_eq!(answer_problems(&d.history), Vec::<String>::new());
}

#[tokio::test]
async fn wrap_up_at_the_step_limit_asks_for_one_tools_off_reply() {
    // #59: tools still sent (the cache holds), any call answered without running, still MaxSteps.
    let mut script = steps(2);
    script.push(vec![
        ProviderEvent::TextDelta("Read both; next: fix b.".into()),
        ProviderEvent::ToolCall(call("late", "c.ts")),
        ProviderEvent::Done { reason: Some("tool_calls".into()) },
    ]);
    let provider = Arc::new(ScriptedProvider::new(script));
    let h = history(vec![user("go")]);
    let (state, runner) = tracked(None);
    let events = collect(AgentOptions { max_steps: 2, wrap_up_at_limit: true, ..options(provider.clone(), &h, runner) }).await;
    let requests = provider.requests();
    assert_eq!(requests.len(), 3);
    let (wrap_history, wrap_options) = &requests[2];
    assert_eq!(wrap_history.last(), Some(&user(STEP_LIMIT_NOTE)));
    assert_eq!(wrap_options.tool_choice, ToolChoice::None);
    assert_eq!(wrap_options.tools, requests[0].1.tools);
    assert_eq!(requests[0].1.tool_choice, ToolChoice::Auto);
    assert!(!state.lock().unwrap().started.contains(&"late".to_string()));
    let turns = snapshot(&h);
    assert_eq!(turns.last(), Some(&tool("late", not_run::STEP_LIMIT)));
    assert_eq!(answer_problems(&turns), Vec::<String>::new());
    assert!(events.iter().any(|e| matches!(e, LoopEvent::Assistant { text } if text == "Read both; next: fix b.")));
    assert!(events.iter().any(|e| matches!(e, LoopEvent::Error(m) if m.contains("without a final answer"))));
    assert_eq!(end(&events), Some(StopReason::MaxSteps));
}

#[tokio::test]
async fn no_wrap_up_when_the_user_said_stop() {
    let provider = Arc::new(ScriptedProvider::new(steps(5)));
    let events = collect(AgentOptions {
        max_steps: 2,
        wrap_up_at_limit: true,
        on_limit: on_limit(|_| LimitAnswer::Stop),
        ..options(provider.clone(), &history(vec![user("go")]), tracked(None).1)
    })
    .await;
    assert_eq!(provider.requests().len(), 2);
    assert_eq!(end(&events), Some(StopReason::MaxSteps));
}

// Text loop detection.

fn repeat(sentence: &str, n: usize) -> Vec<ProviderEvent> {
    vec![ProviderEvent::TextDelta(vec![sentence; n].join("\n")), ProviderEvent::Done { reason: Some("stop".into()) }]
}
fn text_loops(events: &[LoopEvent]) -> Vec<bool> {
    events
        .iter()
        .filter_map(|e| match e {
            LoopEvent::TextLoop { continued } => Some(*continued),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn a_sentence_repeated_8_plus_times_is_caught_and_the_reply_dropped() {
    let provider = Arc::new(ScriptedProvider::new(vec![repeat("Fix the test scripts now.", 10), say("I'll do something else.")]));
    let events = collect(options(provider.clone(), &history(vec![user("go")]), plain())).await;
    assert_eq!(text_loops(&events), [true]);
    assert!(matches!(provider.requests()[1].0.last(), Some(ChatTurn::User { text }) if text.contains("stuck repeating")));
    assert_eq!(end(&events), Some(StopReason::End));
}

#[tokio::test]
async fn a_text_loop_is_stopped_while_streaming_and_none_of_it_reaches_the_next_request() {
    let sentence = "Fix the test scripts now. ";
    let mut looping: Vec<_> = (0..60).map(|_| ProviderEvent::TextDelta(sentence.into())).collect();
    looping.push(ProviderEvent::ToolCall(call("c9", "a.ts")));
    looping.push(ProviderEvent::Done { reason: Some("stop".into()) });
    let provider = Arc::new(ScriptedProvider::new(vec![looping, say("I'll do something else.")]));
    let events = collect(options(provider.clone(), &history(vec![user("go")]), plain())).await;
    assert_eq!(text_loops(&events), [true]);
    assert!(starts(&events).is_empty());
    // Stopped early: far fewer than 60 deltas were passed on.
    assert!(count(&events, |e| matches!(e, LoopEvent::TextDelta(_))) < 40);
    let next = &provider.requests()[1].0;
    assert!(!format!("{next:?}").contains(sentence.trim()));
    assert!(matches!(next.last(), Some(ChatTurn::User { text }) if text.contains("stuck repeating")));
}

#[tokio::test]
async fn a_second_text_loop_in_a_row_ends_the_run() {
    let provider =
        Arc::new(ScriptedProvider::new(vec![repeat("Fix the test scripts now.", 10), repeat("Fix the test scripts now.", 10)]));
    let events = collect(options(provider, &history(vec![user("go")]), plain())).await;
    assert_eq!(text_loops(&events), [true, false]);
    assert_eq!(end(&events), Some(StopReason::Loop));
}

#[tokio::test]
async fn a_step_that_answered_resets_the_text_loop_count() {
    let provider = Arc::new(ScriptedProvider::new(vec![
        repeat("Fix the test scripts now.", 10),
        use_tools(&[call("c1", "a.ts")]),
        repeat("Fix the test scripts now.", 10),
        say("Done."),
    ]));
    let events = collect(options(provider, &history(vec![user("go")]), plain())).await;
    assert_eq!(text_loops(&events), [true, true]);
    assert_eq!(end(&events), Some(StopReason::End));
}

#[tokio::test]
async fn varied_text_doesnt_trigger_a_text_loop() {
    let text = [
        "- Install dependencies with `bun install`.",
        "- Run tests with `bun test`.",
        "- Check types with `bun run typecheck`.",
        "- Done at http://localhost:3000.",
    ]
    .join("\n");
    let provider = Arc::new(ScriptedProvider::new(vec![say(&text), say("Done.")]));
    let events = collect(options(provider, &history(vec![user("go")]), plain())).await;
    assert!(text_loops(&events).is_empty());
    assert_eq!(end(&events), Some(StopReason::End));
}

// marv-rs: a reply the provider gave up on for degenerating twice (#96).

fn degenerated() -> Vec<ProviderEvent> {
    vec![
        ProviderEvent::ThinkingDelta("####\n####\n".into()),
        ProviderEvent::Discard { cause: marv_core::RetryCause::Degenerate { discarded: 16_384 } },
        ProviderEvent::Error("openrouter: the model's reply degenerated again".into()),
    ]
}
fn degenerate_retries(events: &[LoopEvent]) -> Vec<(u32, u32)> {
    events
        .iter()
        .filter_map(|e| match e {
            LoopEvent::Retry { attempt, max, cause: marv_core::RetryCause::Degenerate { .. }, .. } => Some((*attempt, *max)),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn a_reply_the_provider_gave_up_on_for_degenerating_is_not_the_end_of_the_run() {
    // TB 0.15.0 gcode-to-text: rendered '#' art in the reasoning, cut twice, and the run ended with 8 minutes left.
    let provider = Arc::new(ScriptedProvider::new(vec![
        use_tools(&[call("c1", "a.ts")]),
        degenerated(),
        use_tools(&[call("c2", "out.txt")]),
        say("Done."),
    ]));
    let history = history(vec![user("go")]);
    let events = collect(options(provider.clone(), &history, plain())).await;
    assert!(!events.iter().any(|e| matches!(e, LoopEvent::Error(_))), "{events:?}");
    assert_eq!(degenerate_retries(&events), [(1, MAX_DEGENERATE_RECOVERIES)]);
    assert_eq!(end(&events), Some(StopReason::End));
    // Nothing of the degenerate reply reached the history; the model was told after the last tool result.
    let requests = provider.requests();
    let after = &requests[2].0;
    assert_eq!(after[after.len() - 2..], [tool("c1", "contents of a.ts"), user(DEGENERATE_NOTE)]);
    assert!(!format!("{:?}", snapshot(&history)).contains("####"));
    assert_eq!(count(&events, |e| matches!(e, LoopEvent::ToolEnd { .. })), 2);
}

#[tokio::test]
async fn degenerate_replies_past_the_recoveries_end_the_run_with_the_providers_error() {
    let script = (0..=MAX_DEGENERATE_RECOVERIES).map(|_| degenerated()).collect();
    let provider = Arc::new(ScriptedProvider::new(script));
    let events = collect(options(provider.clone(), &history(vec![user("go")]), plain())).await;
    assert_eq!(degenerate_retries(&events), [(1, 2), (2, 2)]);
    assert_eq!(provider.requests().len() as u32, MAX_DEGENERATE_RECOVERIES + 1);
    assert!(events.iter().any(|e| matches!(e, LoopEvent::Error(m) if m.contains("degenerated again"))));
    assert_eq!(end(&events), Some(StopReason::Error));
}

#[tokio::test]
async fn another_provider_error_still_ends_the_run() {
    let provider = Arc::new(ScriptedProvider::new(vec![vec![ProviderEvent::Error("401 unauthorized".into())]]));
    let events = collect(options(provider, &history(vec![user("go")]), plain())).await;
    assert!(degenerate_retries(&events).is_empty());
    assert_eq!(end(&events), Some(StopReason::Error));
}

// Tool loop detection.

fn read_call(id: &str, path: &str, extra: serde_json::Value) -> ToolCall {
    let mut args = serde_json::json!({ "path": path });
    if let (Some(args), serde_json::Value::Object(extra)) = (args.as_object_mut(), extra) {
        args.extend(extra);
    }
    ToolCall { id: id.into(), name: "read_file".into(), arguments: args.to_string() }
}
fn tool_loops(events: &[LoopEvent]) -> Vec<bool> {
    events
        .iter()
        .filter_map(|e| match e {
            LoopEvent::ToolLoop { continued } => Some(*continued),
            _ => None,
        })
        .collect()
}

/// A call with side effects (not one of `repetition::READ_ONLY_TOOLS`, not a write tool that resets the history).
fn side_call(id: &str, path: &str) -> ToolCall {
    ToolCall { id: id.into(), name: "process".into(), arguments: serde_json::json!({ "path": path }).to_string() }
}

#[tokio::test]
async fn period_1_the_same_call_with_the_same_result_warns_on_the_4th_final_turn_on_the_5th_stops_on_the_6th() {
    // A call counts as a repeat when it also got the same result as last time, which the first call
    // can't have: calls 2, 3 and 4 are the three identical entries.
    let script = (1..=6).map(|i| use_tools(&[side_call(&format!("c{i}"), "a.ts")])).collect();
    let history = history(vec![user("go")]);
    let events = collect(options(Arc::new(ScriptedProvider::new(script)), &history, plain())).await;
    assert_eq!(tool_loops(&events), [true, true, false]);
    assert_eq!(end(&events), Some(StopReason::Loop));
    let texts = tool_texts(&history);
    assert!(
        texts[3].starts_with("Marv: you have made this exact call") && texts[3].contains(r#"process {"path":"a.ts"}"#),
        "{}",
        texts[3]
    );
    assert!(texts[4].contains("final turn"), "{}", texts[4]);
    // Marv answered these with the cut-off text ("your reply hit the output limit"), which wasn't true.
    assert_eq!(texts[5], not_run::TOOL_LOOP);
    assert!(!texts.iter().any(|t| t == not_run::CUT_OFF));
}

#[tokio::test]
async fn the_final_turn_after_a_loop_can_save_the_work_and_answer() {
    // TB 0.15.0: the run had the fix but not yet the report the task asked for when the guard ended it.
    let read = |id: &str| use_tools(&[read_call(id, "a.py", serde_json::json!({}))]);
    let write = ToolCall {
        id: "w".into(),
        name: "write_file".into(),
        arguments: serde_json::json!({"path": "report.txt", "content": "fixed"}).to_string(),
    };
    let script = vec![read("c1"), read("c2"), read("c3"), read("c4"), read("c5"), use_tools(&[write]), say("Done: report.txt.")];
    let ran = Arc::new(Mutex::new(Vec::new()));
    let record = ran.clone();
    let run: RunFn = Arc::new(move |c, _| {
        record.lock().unwrap().push(c.name.clone());
        Box::pin(async move { Ok(ok("same text", c.id.clone())) })
    });
    let events = collect(options(
        Arc::new(ScriptedProvider::new(script)),
        &history(vec![user("go")]),
        FakeRunner { run, parallel: |_| false },
    ))
    .await;
    assert_eq!(tool_loops(&events), [true, true]);
    assert_eq!(ran.lock().unwrap().last().map(String::as_str), Some("write_file"));
    assert_eq!(end(&events), Some(StopReason::End));
}

#[tokio::test]
async fn progress_after_a_warning_means_a_later_unrelated_loop_is_warned_about_again() {
    // Marv never reset `warned`: one early warning made any later loop end the run without a word.
    let read = |id: String, path: &str| use_tools(&[read_call(&id, path, serde_json::json!({}))]);
    let mut script: Vec<_> = (1..=4).map(|i| read(format!("a{i}"), "a.ts")).collect();
    // Genuine progress: distinct calls that ran and succeeded.
    script.extend((0..crate::repetition::TOOL_LOOP_PROGRESS).map(|i| read(format!("p{i}"), &format!("f{i}.ts"))));
    script.extend((1..=4).map(|i| read(format!("b{i}"), "b.ts")));
    script.push(say("Done."));
    let events = collect(options(Arc::new(ScriptedProvider::new(script)), &history(vec![user("go")]), plain())).await;
    // Two warnings, never a stop or a final turn.
    assert_eq!(tool_loops(&events), [true, true]);
    assert_eq!(end(&events), Some(StopReason::End));
}

#[tokio::test]
async fn without_progress_a_later_loop_gets_the_final_turn_not_another_warning() {
    let side = |id: String, path: &str| use_tools(&[side_call(&id, path)]);
    let mut script: Vec<_> = (1..=4).map(|i| side(format!("a{i}"), "a.ts")).collect();
    script.extend((1..=4).map(|i| side(format!("b{i}"), "b.ts")));
    script.push(say("Done."));
    let history = history(vec![user("go")]);
    let events = collect(options(Arc::new(ScriptedProvider::new(script)), &history, plain())).await;
    assert_eq!(tool_loops(&events), [true, true]);
    assert!(tool_texts(&history)[7].contains("final turn"));
}

fn notes(events: &[LoopEvent], kind: NoteKind) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            LoopEvent::Note { kind: k, text } if *k == kind => Some(text.clone()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn a_read_only_loop_is_warned_about_five_times_and_only_a_truly_stuck_one_ends_the_run() {
    // TB 2026-11-06 schemelike-metacircular-eval: warn, final turn, stop in three steps, 22 s into 40 minutes.
    let script = (1..=10).map(|i| use_tools(&[read_call(&format!("c{i}"), "a.ts", serde_json::json!({}))])).collect();
    let history = history(vec![user("go")]);
    let events = collect(options(Arc::new(ScriptedProvider::new(script)), &history, plain())).await;
    assert_eq!(tool_loops(&events), [true, true, true, true, true, true, false]);
    assert_eq!(end(&events), Some(StopReason::Loop));
    let texts = tool_texts(&history);
    assert!(texts[3..8].iter().all(|t| t.starts_with("Marv: you have made this exact call") && !t.contains("final turn")));
    assert!(texts[8].contains("final turn"), "{}", texts[8]);
    // Each guard note is logged as it was given.
    assert_eq!(notes(&events, NoteKind::ToolLoop), texts[3..10]);
}

#[tokio::test]
async fn a_read_only_loop_the_model_breaks_out_of_never_ends_the_run() {
    let read = |i: usize| use_tools(&[read_call(&format!("c{i}"), "a.ts", serde_json::json!({}))]);
    let mut script: Vec<_> = (1..=8).map(read).collect();
    script.push(use_tools(&[read_call("d", "b.ts", serde_json::json!({}))]));
    script.push(say("Done."));
    let events = collect(options(Arc::new(ScriptedProvider::new(script)), &history(vec![user("go")]), plain())).await;
    assert_eq!(tool_loops(&events), [true; 5]);
    assert_eq!(end(&events), Some(StopReason::End));
}

/// `read_file` on a 586-line file, as marv-tools renders its pages (Marv's text).
fn interp_page(call: &ToolCall) -> String {
    let offset = arg(call, "offset").as_u64().unwrap_or(1);
    if offset == 1 {
        "    1→#!/usr/bin/env python3\n  400→    return env\n\n(Showing lines 1-400 of 586. Use offset=401 to read more.)".into()
    } else {
        format!("  {offset}→def main():\n  586→    main()\n\n(Showing lines {offset}-586 of 586.)")
    }
}

#[tokio::test]
async fn the_schemelike_sequence_a_repeated_truncated_read_gets_the_next_page_and_the_run_goes_on() {
    // TB 2026-11-06 schemelike-metacircular-eval: `ls` and `read_file interp.py` (lines 1-400 of 586), then the
    // same `read_file {"path": "interp.py"}` four more times, the reasoning saying "I need offset=401" each time.
    let read = |id: &str| ToolCall { id: id.into(), name: "read_file".into(), arguments: r#"{"path": "interp.py"}"#.into() };
    let ls = ToolCall { id: "ls".into(), name: "bash".into(), arguments: r#"{"command": "ls -la /app"}"#.into() };
    let write = ToolCall {
        id: "w".into(),
        name: "write_file".into(),
        arguments: r#"{"path": "eval.scm", "content": "(define (eval e) e)"}"#.into(),
    };
    let script = vec![
        use_tools(&[ls, read("r1")]),
        use_tools(&[read("r2")]),
        use_tools(&[read("r3")]),
        use_tools(&[read("r4")]),
        use_tools(&[read("r5")]),
        use_tools(&[write]),
        say("Wrote eval.scm."),
    ];
    let ran = Arc::new(Mutex::new(Vec::new()));
    let record = ran.clone();
    let run: RunFn = Arc::new(move |c, _| {
        record.lock().unwrap().push((c.name.clone(), c.arguments.clone()));
        let output = if c.name == "read_file" { interp_page(&c) } else { "ok".to_string() };
        Box::pin(async move { Ok(ok(output, c.id.clone())) })
    });
    let provider = Arc::new(ScriptedProvider::new(script));
    let history = history(vec![user("Write eval.scm, an evaluator for interp.py's language.")]);
    let events = collect(options(provider.clone(), &history, FakeRunner { run, parallel: |_| false })).await;
    // Never stopped: the deliverable got written and the run ended on its answer.
    assert_eq!(end(&events), Some(StopReason::End), "{events:?}");
    assert!(tool_loops(&events).iter().all(|c| *c));
    let ran = ran.lock().unwrap().clone();
    assert_eq!(ran.last().map(|(n, _)| n.as_str()), Some("write_file"));
    // The first repeat read on from line 401, and its result says so (and where to go from there).
    let texts = tool_texts(&history);
    assert!(texts[1].ends_with("Use offset=401 to read more.)"));
    assert!(texts[2].starts_with(&read_on_note(401)) && texts[2].contains("(Showing lines 401-586 of 586.)"), "{}", texts[2]);
    assert_eq!(serde_json::from_str::<serde_json::Value>(&ran[2].1).unwrap()["offset"], 401);
    assert_eq!(notes(&events, NoteKind::ReadOn).len(), 2, "pages 401+ twice: r2 and r4");
    // The history keeps the call exactly as the model sent it (the prompt cache, and the record of what it did).
    let sent: Vec<String> = snapshot(&history)
        .into_iter()
        .flat_map(|t| match t {
            ChatTurn::Assistant { tool_calls, .. } => tool_calls.into_iter().map(|c| c.arguments).collect(),
            _ => vec![],
        })
        .filter(|a| a.contains("interp.py"))
        .collect();
    assert_eq!(sent, vec![r#"{"path": "interp.py"}"#; 5]);
    // Every request starts with the one before.
    let requests = provider.requests();
    assert!(requests.windows(2).all(|w| w[1].0.starts_with(&w[0].0)));
}

#[tokio::test]
async fn a_read_after_a_change_is_not_moved_on() {
    let read = |id: &str| ToolCall { id: id.into(), name: "read_file".into(), arguments: r#"{"path": "interp.py"}"#.into() };
    let edit = ToolCall { id: "e".into(), name: "edit_file".into(), arguments: r#"{"path": "interp.py"}"#.into() };
    let script = vec![use_tools(&[read("r1")]), use_tools(&[edit]), use_tools(&[read("r2")]), say("Done.")];
    let run: RunFn = Arc::new(move |c, _| {
        let output = if c.name == "read_file" { interp_page(&c) } else { "ok".to_string() };
        Box::pin(async move { Ok(ok(output, c.id.clone())) })
    });
    let history = history(vec![user("go")]);
    let events =
        collect(options(Arc::new(ScriptedProvider::new(script)), &history, FakeRunner { run, parallel: |_| false })).await;
    assert!(notes(&events, NoteKind::ReadOn).is_empty());
    assert!(tool_texts(&history)[2].starts_with("    1→"));
}

// #267: repeated calls that failed validation.

const HERDR_REASON: &str = "branch, base and task: only for action spawn";

fn herdr_call(id: &str, args: serde_json::Value) -> ToolCall {
    ToolCall { id: id.into(), name: "herdr".into(), arguments: args.to_string() }
}
/// Refuses `branch` outside `spawn` before running, as the herdr tool's `check` does (marked `invalid`, the way
/// the session's runner passes marv-tools' `ToolOutput::invalid` on); anything else runs.
fn validating_runner(ran: Arc<AtomicUsize>) -> FakeRunner {
    let run: RunFn = Arc::new(move |c, _| {
        let ran = ran.clone();
        Box::pin(async move {
            if arg(&c, "branch").is_string() && arg(&c, "action") != "spawn" {
                let output = format!("Invalid input for herdr:\n{HERDR_REASON}");
                let result = ToolResult { output, summary: Some("error".into()), is_error: true };
                return Ok(ToolRun { invalid: Some(HERDR_REASON.into()), ..ToolRun::new(result, "herdr") });
            }
            ran.fetch_add(1, Ordering::SeqCst);
            Ok(ok("agents: none", "list agents"))
        })
    });
    FakeRunner { run, parallel: |_| false }
}

#[tokio::test]
async fn an_identical_invalid_call_is_refused_on_the_2nd_try_and_a_corrected_call_goes_through() {
    let bad = serde_json::json!({"action": "list", "branch": "x"});
    let script = vec![
        use_tools(&[herdr_call("c1", bad.clone())]),
        use_tools(&[herdr_call("c2", bad.clone())]),
        use_tools(&[herdr_call("c3", serde_json::json!({"action": "list"}))]),
        say("No agents."),
    ];
    let ran = Arc::new(AtomicUsize::new(0));
    let history = history(vec![user("go")]);
    let events = collect(options(Arc::new(ScriptedProvider::new(script)), &history, validating_runner(ran.clone()))).await;
    let refused = format!(
        "Marv: not run. This exact call was refused for: {HERDR_REASON}. Change the arguments (only send the fields this action uses), or tell the user it can't be done."
    );
    assert_eq!(refused_call_note(HERDR_REASON), refused);
    assert_eq!(tool_texts(&history), [format!("Invalid input for herdr:\n{HERDR_REASON}"), refused, "agents: none".into()]);
    assert_eq!(ran.load(Ordering::SeqCst), 1);
    assert!(tool_loops(&events).is_empty());
    assert_eq!(end(&events), Some(StopReason::End));
    // The model read the refusal as sent: its arguments untouched (prompt cache).
    let ChatTurn::Assistant { tool_calls, .. } = &snapshot(&history)[3] else { panic!("the 2nd call's turn") };
    assert_eq!(tool_calls[0].arguments, bad.to_string());
}

#[tokio::test]
async fn an_invalid_call_with_other_arguments_is_not_refused_as_a_repeat() {
    // Only the identical call is refused: a different invalid call gets its own validation error first.
    let script = vec![
        use_tools(&[herdr_call("c1", serde_json::json!({"action": "list", "branch": "x"}))]),
        use_tools(&[herdr_call("c2", serde_json::json!({"action": "list", "branch": "y"}))]),
        say("Stuck."),
    ];
    let history = history(vec![user("go")]);
    collect(options(Arc::new(ScriptedProvider::new(script)), &history, validating_runner(Arc::default()))).await;
    let invalid = format!("Invalid input for herdr:\n{HERDR_REASON}");
    assert_eq!(tool_texts(&history), [invalid.clone(), invalid]);
}

#[tokio::test]
async fn a_model_that_keeps_sending_the_refused_call_is_still_stopped_by_the_loop_guard() {
    // The refusals are identical results, so #37's guard backs this one up: it warns once the refusal has come
    // back three times in a row (the 5th call) and ends the run on the 6th.
    let bad = serde_json::json!({"action": "list", "branch": "x"});
    let script = (1..=6).map(|i| use_tools(&[herdr_call(&format!("c{i}"), bad.clone())])).collect();
    let history = history(vec![user("go")]);
    let events = collect(options(Arc::new(ScriptedProvider::new(script)), &history, validating_runner(Arc::default()))).await;
    assert_eq!(tool_loops(&events), [true, true]);
    assert_eq!(end(&events), Some(StopReason::End)); // the script ran out during the final turn
    let texts = tool_texts(&history);
    assert!(texts[1..4].iter().all(|t| t.starts_with("Marv: not run. This exact call was refused")), "{texts:?}");
    assert!(texts[4].starts_with("Marv: you have made this exact call"), "{}", texts[4]);
}

#[tokio::test]
async fn a_call_that_ran_and_failed_is_still_left_to_the_4_call_rule() {
    // A failure from a tool that ran (not marked invalid) is never refused early: warned on the 4th, stopped on
    // the 5th, as #37.
    let failing: RunFn = Arc::new(|c, _| {
        Box::pin(async move {
            let result = ToolResult { output: "ENOENT: a.ts".into(), summary: Some("error".into()), is_error: true };
            Ok(ToolRun::new(result, c.id.clone()))
        })
    });
    let script = (1..=5).map(|i| use_tools(&[read_call(&format!("c{i}"), "a.ts", serde_json::json!({}))])).collect();
    let history = history(vec![user("go")]);
    let events =
        collect(options(Arc::new(ScriptedProvider::new(script)), &history, FakeRunner { run: failing, parallel: |_| false }))
            .await;
    assert_eq!(tool_loops(&events), [true, true]);
    let texts = tool_texts(&history);
    assert_eq!(texts[..3], ["ENOENT: a.ts", "ENOENT: a.ts", "ENOENT: a.ts"]);
    assert!(texts[3].starts_with("Marv: you have made this exact call 4 times in a row"), "{}", texts[3]);
    assert!(!texts.iter().any(|t| t.contains("This exact call was refused")));
}

#[tokio::test]
async fn period_2_alternating_offsets_are_caught() {
    // The exact pattern from issue #37: read_file with offset 95, then 98, alternating, each getting
    // the same result as the last time it ran.
    let script = [95, 98, 95, 98, 95, 98, 95]
        .iter()
        .enumerate()
        .map(|(i, offset)| {
            use_tools(&[read_call(&format!("c{i}"), "src/tools/index.ts", serde_json::json!({"offset": offset, "limit": 40}))])
        })
        .collect();
    let events = collect(options(Arc::new(ScriptedProvider::new(script)), &history(vec![user("go")]), plain())).await;
    assert_eq!(tool_loops(&events), [true]);
    assert_eq!(end(&events), Some(StopReason::End)); // no more scripted steps after the warning
}

#[tokio::test]
async fn the_same_call_getting_a_new_result_each_time_is_new_work_not_a_loop() {
    // A tool that hands out the next queued item on each identical call.
    let n = Arc::new(AtomicUsize::new(0));
    let queue: RunFn = Arc::new(move |c, _| {
        let item = n.fetch_add(1, Ordering::SeqCst) + 1;
        Box::pin(async move { Ok(ok(format!("item {item}"), c.id.clone())) })
    });
    let script = (0..6).map(|i| use_tools(&[read_call(&format!("c{i}"), "queue", serde_json::json!({}))])).collect();
    let events = collect(options(
        Arc::new(ScriptedProvider::new(script)),
        &history(vec![user("go")]),
        FakeRunner { run: queue, parallel: |_| false },
    ))
    .await;
    assert!(tool_loops(&events).is_empty());
    assert_eq!(count(&events, |e| matches!(e, LoopEvent::ToolEnd { .. })), 6);
}

#[tokio::test]
async fn a_write_tool_between_calls_resets_the_loop_counter() {
    let read = |id: &str| use_tools(&[read_call(id, "a.ts", serde_json::json!({}))]);
    let write = ToolCall {
        id: "c3".into(),
        name: "write_file".into(),
        arguments: serde_json::json!({"path": "a.ts", "content": "edited"}).to_string(),
    };
    let script = vec![read("c1"), read("c2"), use_tools(&[write]), read("c4"), read("c5"), say("Done.")];
    let events = collect(options(Arc::new(ScriptedProvider::new(script)), &history(vec![user("go")]), plain())).await;
    // The write_file reset, so 4 and 5 are fresh: history stays too short to trigger.
    assert!(tool_loops(&events).is_empty());
    assert_eq!(end(&events), Some(StopReason::End));
}

// Issue #81: a clone that can't work without network, re-sent with another tail each time.
fn bash_call(id: &str, command: &str) -> ToolCall {
    ToolCall { id: id.into(), name: "bash".into(), arguments: serde_json::json!({ "command": command }).to_string() }
}
/// Every bash command fails like a clone without network, counting how many ran.
fn offline(ran: Arc<AtomicUsize>) -> FakeRunner {
    let run: RunFn = Arc::new(move |c, _| {
        ran.fetch_add(1, Ordering::SeqCst);
        let out = "fatal: unable to access 'https://github.com/x/y/': Could not resolve host: github.com\n\n[exit code 128]";
        Box::pin(async move { Ok(ok(out, c.id.clone())) })
    });
    FakeRunner { run, parallel: |_| false }
}
const CLONE: &str = "git clone https://github.com/x/y";

#[tokio::test]
async fn a_command_failing_the_same_way_with_another_tail_each_time_warns_then_stops() {
    let tails = ["2>&1 | tail -2", "&& du -sh y", "|| true", "&& command -v tokei", "; ls", "; pwd"];
    let mut script: Vec<_> =
        tails.iter().enumerate().map(|(i, t)| use_tools(&[bash_call(&format!("c{i}"), &format!("{CLONE} {t}"))])).collect();
    // A probe in between doesn't hide it.
    script.insert(1, use_tools(&[bash_call("p", "echo test")]));
    let ran = Arc::new(AtomicUsize::new(0));
    let history = history(vec![user("go")]);
    let events = collect(options(Arc::new(ScriptedProvider::new(script)), &history, offline(ran.clone()))).await;
    assert_eq!(tool_loops(&events), [true, true, false]);
    assert_eq!(end(&events), Some(StopReason::Loop));
    // The two clones and the probe ran; the third clone got the note, the fourth the final turn, the fifth
    // stopped the run.
    assert_eq!(ran.load(Ordering::SeqCst), 3);
    let turns = history.lock().unwrap();
    let noted = turns.iter().any(
        |t| matches!(t, ChatTurn::Tool { text, .. } if text.starts_with("Marv: not run. This command already failed 2 times")),
    );
    assert!(noted, "{turns:?}");
    let final_turn = turns.iter().any(|t| matches!(t, ChatTurn::Tool { text, .. } if text.contains("final turn")));
    assert!(final_turn, "{turns:?}");
}

// Greptile on #111: each call is checked when it's about to run, after the calls before it in the reply.
#[tokio::test]
async fn an_edit_earlier_in_the_same_reply_resets_the_failures_before_the_retry_runs() {
    let edit = ToolCall {
        id: "e".into(),
        name: "edit_file".into(),
        arguments: serde_json::json!({"path": "a.rs", "old": "x", "new": "y"}).to_string(),
    };
    let script = vec![
        use_tools(&[bash_call("c1", "cargo test")]),
        use_tools(&[bash_call("c2", "cargo test")]),
        use_tools(&[edit, bash_call("c3", "cargo test")]),
        say("Done."),
    ];
    let (bash_runs, edits) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let (b, e) = (bash_runs.clone(), edits.clone());
    let run: RunFn = Arc::new(move |c, _| {
        let out = if c.name == "bash" {
            b.fetch_add(1, Ordering::SeqCst);
            "error[E0308]: mismatched types\n\n[exit code 101]"
        } else {
            e.fetch_add(1, Ordering::SeqCst);
            "Edited a.rs"
        };
        Box::pin(async move { Ok(ok(out, c.id.clone())) })
    });
    let runner = FakeRunner { run, parallel: |_| false };
    let events = collect(options(Arc::new(ScriptedProvider::new(script)), &history(vec![user("go")]), runner)).await;
    assert!(tool_loops(&events).is_empty());
    assert_eq!((edits.load(Ordering::SeqCst), bash_runs.load(Ordering::SeqCst)), (1, 3));
    assert_eq!(end(&events), Some(StopReason::End));
}

#[tokio::test]
async fn re_running_a_failing_command_after_an_edit_isnt_a_loop() {
    let write = ToolCall {
        id: "w".into(),
        name: "write_file".into(),
        arguments: serde_json::json!({"path": "a.rs", "content": "fixed"}).to_string(),
    };
    let script = vec![
        use_tools(&[bash_call("c1", CLONE)]),
        use_tools(&[bash_call("c2", CLONE)]),
        use_tools(&[write]),
        use_tools(&[bash_call("c3", CLONE)]),
        say("Done."),
    ];
    let ran = Arc::new(AtomicUsize::new(0));
    let events =
        collect(options(Arc::new(ScriptedProvider::new(script)), &history(vec![user("go")]), offline(ran.clone()))).await;
    assert!(tool_loops(&events).is_empty());
    assert_eq!(end(&events), Some(StopReason::End));
    assert_eq!(ran.load(Ordering::SeqCst), 4);
}

#[tokio::test]
async fn rewrite_call_changes_a_call_before_it_enters_the_history_and_it_runs_as_rewritten() {
    let provider = Arc::new(ScriptedProvider::new(vec![
        use_tools(&[ToolCall { id: "c1".into(), name: "mcp__x__ping".into(), arguments: r#"{"a":1}"#.into() }]),
        say("Done."),
    ]));
    let ran = Arc::new(Mutex::new(Vec::new()));
    let record = ran.clone();
    let run: RunFn = Arc::new(move |c, _| {
        record.lock().unwrap().push(c.name.clone());
        Box::pin(async move { Ok(ok("ok", c.name.clone())) })
    });
    let rewrite: RewriteCall = Arc::new(|c| {
        (c.name == "mcp__x__ping").then(|| ToolCall {
            name: "mcp_call".into(),
            arguments: format!(r#"{{"tool":"mcp__x__ping","arguments":{}}}"#, c.arguments),
            ..c.clone()
        })
    });
    collect(AgentOptions {
        rewrite_call: Some(rewrite),
        tools: vec![],
        ..options(provider.clone(), &history(vec![user("ping")]), FakeRunner { run, parallel: |_| false })
    })
    .await;
    assert_eq!(*ran.lock().unwrap(), ["mcp_call"]);
    assert_eq!(
        provider.requests()[1].0[1],
        ChatTurn::Assistant {
            text: "".into(),
            tool_calls: vec![ToolCall {
                id: "c1".into(),
                name: "mcp_call".into(),
                arguments: r#"{"tool":"mcp__x__ping","arguments":{"a":1}}"#.into()
            }],
            reasoning: None
        }
    );
}

// "Send now": the person's message at the next step boundary.

/// A runner whose calls each push `text` into `q` while they run (the person typed it then).
fn sending(q: &SendNow, text: &'static str) -> FakeRunner {
    let q = q.clone();
    let run: RunFn = Arc::new(move |c, cancel| {
        let _ = q.push(text.to_string());
        fake_tool()(c, cancel)
    });
    FakeRunner { run, parallel: |_| true }
}

#[tokio::test]
async fn sent_now_while_tools_run_goes_in_after_every_result_of_the_step_before_the_next_request() {
    let provider = Arc::new(ScriptedProvider::new(vec![use_tools(&[call("c1", "a.ts"), call("c2", "b.ts")]), say("Done.")]));
    let q = SendNow::new();
    let h = history(vec![user("go")]);
    let events =
        collect(AgentOptions { send_now: Some(q.clone()), ..options(provider.clone(), &h, sending(&q, "also b")) }).await;
    let requests = provider.requests();
    let second = &requests[1].0;
    // Both calls ran (nothing was stopped for it), their results first, in call order, then the person's words.
    // Each call pushed it once: two messages, one user turn (never two in a row), in order.
    assert!(matches!(
        &second[second.len() - 3..],
        [ChatTurn::Tool { call_id: a, .. }, ChatTurn::Tool { call_id: b, .. }, ChatTurn::User { text }]
            if a == "c1" && b == "c2" && text == "also b\n\nalso b"
    ));
    // Appended, not inserted: the second request starts with the first one (prompt cache).
    assert_eq!(second[..requests[0].0.len()], requests[0].0[..]);
    // The event comes once it's in the history, after the step's tool ends.
    let last_end = events.iter().rposition(|e| matches!(e, LoopEvent::ToolEnd { .. })).unwrap();
    let first_sent = events.iter().position(|e| matches!(e, LoopEvent::SentNow { .. })).unwrap();
    assert!(first_sent > last_end);
    assert_eq!(count(&events, |e| matches!(e, LoopEvent::SentNow { text } if text == "also b")), 2);
    assert_eq!(end(&events), Some(StopReason::End));
    assert!(q.close().is_empty());
}

#[tokio::test]
async fn sent_now_follows_after_steps_note() {
    let provider = Arc::new(ScriptedProvider::new(vec![use_tools(&[call("c1", "a.ts")]), say("Fixed.")]));
    let q = SendNow::new();
    let mut runner = sending(&q, "and the tests");
    runner.parallel = |_| false;
    collect(AgentOptions {
        send_now: Some(q.clone()),
        after_step: after_step(|_| Some("Marv: 1 new error".into())),
        ..options(provider.clone(), &history(vec![user("go")]), runner)
    })
    .await;
    let second = &provider.requests()[1].0;
    // The note was never sent: the person's words join it, after a blank line.
    assert_eq!(second.last(), Some(&user("Marv: 1 new error\n\nand the tests")));
    assert!(!matches!(&second[second.len() - 2], ChatTurn::User { .. }));
}

#[tokio::test]
async fn sent_now_during_a_plain_answer_is_read_by_one_more_step() {
    let provider = Arc::new(ScriptedProvider::new(vec![say("Done."), say("Sure, b too.")]));
    let q = SendNow::new();
    let h = history(vec![user("go")]);
    let mut events = Vec::new();
    let mut stream =
        std::pin::pin!(run_agent(AgentOptions { send_now: Some(q.clone()), ..options(provider.clone(), &h, plain()) }));
    while let Some(event) = stream.next().await {
        // Typed while the reply streams: the run would end after it.
        if matches!(event, LoopEvent::TextDelta(_)) && events.is_empty() {
            q.push("and b".into()).unwrap();
        }
        events.push(event);
    }
    assert_eq!(provider.requests().len(), 2);
    assert_eq!(snapshot(&h), [user("go"), said("Done."), user("and b"), said("Sure, b too.")]);
    assert_eq!(count(&events, |e| matches!(e, LoopEvent::SentNow { .. })), 1);
    // Ahead of #90's "continue": no announced-step event either way here, and the run ends normally.
    assert_eq!(end(&events), Some(StopReason::End));
}

#[tokio::test]
async fn sent_now_before_the_first_request_goes_right_after_the_turns_message() {
    let provider = Arc::new(ScriptedProvider::new(vec![say("Ok.")]));
    let q = SendNow::new();
    q.push("in Rust".into()).unwrap();
    collect(AgentOptions { send_now: Some(q.clone()), ..options(provider.clone(), &history(vec![user("go")]), plain()) }).await;
    assert_eq!(provider.requests()[0].0, [user("go\n\nin Rust")]);
}

#[tokio::test]
async fn sent_now_is_never_delivered_after_a_stop_or_a_decline_it_stays_for_the_session() {
    // A decline ends the run: the message isn't read, and the session gets it back.
    let provider = Arc::new(ScriptedProvider::new(vec![use_tools(&[call("c1", "a.ts")]), say("never asked")]));
    let q = SendNow::new();
    let q2 = q.clone();
    let run: RunFn = Arc::new(move |c, cancel| {
        let _ = q2.push("do it this way".into());
        Box::pin(async move { Ok(ToolRun { declined: true, ..fake_tool()(c, cancel).await? }) })
    });
    let h = history(vec![user("go")]);
    let events = collect(AgentOptions {
        send_now: Some(q.clone()),
        ..options(provider.clone(), &h, FakeRunner { run, parallel: |_| false })
    })
    .await;
    assert_eq!(end(&events), Some(StopReason::Declined));
    assert_eq!(count(&events, |e| matches!(e, LoopEvent::SentNow { .. })), 0);
    assert!(!snapshot(&h).contains(&user("do it this way")));
    assert_eq!(q.close(), ["do it this way"]);

    // A stop while a tool runs: the call is answered "interrupted", the message stays waiting.
    let provider = Arc::new(ScriptedProvider::new(vec![use_tools(&[call("c1", "a.ts")]), say("never asked")]));
    let q = SendNow::new();
    let (q2, stop) = (q.clone(), CancellationToken::new());
    let stopper = stop.clone();
    let run: RunFn = Arc::new(move |c, cancel| {
        let _ = q2.push("wait".into());
        stopper.cancel();
        fake_tool()(c, cancel)
    });
    let h = history(vec![user("go")]);
    let events = collect(AgentOptions {
        send_now: Some(q.clone()),
        cancel: stop,
        ..options(provider.clone(), &h, FakeRunner { run, parallel: |_| false })
    })
    .await;
    assert_eq!(end(&events), Some(StopReason::Aborted));
    assert_eq!(provider.requests().len(), 1);
    assert!(!snapshot(&h).contains(&user("wait")));
    assert_eq!(q.close(), ["wait"]);
}

#[tokio::test]
async fn a_history_that_isnt_at_a_step_boundary_keeps_the_message_waiting() {
    // Whatever the phase: with a call still unanswered at the end of the history, nothing is appended.
    let h = history(vec![
        user("go"),
        ChatTurn::Assistant { text: String::new(), tool_calls: vec![call("c1", "a")], reasoning: None },
    ]);
    let q = SendNow::new();
    q.push("now".into()).unwrap();
    let provider = Arc::new(ScriptedProvider::new(vec![]));
    let mut state = LoopState::new(AgentOptions { send_now: Some(q.clone()), ..options(provider, &h, plain()) });
    assert!(!state.deliver_sent_now());
    assert!(state.queue.is_empty());
    assert_eq!(snapshot(&h).len(), 2);
    h.lock().unwrap().push(tool("c1", "ok"));
    assert!(state.deliver_sent_now());
    assert_eq!(snapshot(&h).last(), Some(&user("now")));
    assert!(matches!(state.queue.pop_front(), Some(LoopEvent::SentNow { text }) if text == "now"));
}

#[tokio::test]
async fn sent_now_after_an_empty_reply_waits_rather_than_follow_or_rewrite_the_sent_message() {
    let empty = vec![ProviderEvent::Done { reason: Some("stop".into()) }];
    let provider = Arc::new(ScriptedProvider::new(vec![empty, say("Ok."), say("And b.")]));
    let q = SendNow::new();
    let h = history(vec![user("go")]);
    let mut stream =
        std::pin::pin!(run_agent(AgentOptions { send_now: Some(q.clone()), ..options(provider.clone(), &h, plain()) }));
    while let Some(event) = stream.next().await {
        if matches!(event, LoopEvent::EmptyReply { .. }) {
            q.push("and b".into()).unwrap();
        }
    }
    let requests = provider.requests();
    // The retry is the same request (cached): "go" was sent, so it isn't rewritten and nothing follows it.
    assert_eq!(requests[1].0, requests[0].0);
    assert_eq!(requests[2].0, [user("go"), said("Ok."), user("and b")]);
}

/// A run with `max_steps` 1 whose only step's call runs while the person sends "and b"; `on_limit` answers.
async fn at_the_limit(answer: Option<LimitAnswer>) -> (Arc<ScriptedProvider>, SendNow, Vec<LoopEvent>) {
    let provider = Arc::new(ScriptedProvider::new(vec![use_tools(&[call("c1", "a.ts")]), say("Wrapped up.")]));
    let q = SendNow::new();
    let mut runner = sending(&q, "and b");
    runner.parallel = |_| false;
    let on_limit: Option<OnLimit> = answer.map(|a| -> OnLimit { Arc::new(move |_| Box::pin(async move { a })) });
    let events = collect(AgentOptions {
        send_now: Some(q.clone()),
        max_steps: 1,
        on_limit,
        wrap_up_at_limit: true,
        ..options(provider.clone(), &history(vec![user("go")]), runner)
    })
    .await;
    (provider, q, events)
}

#[tokio::test]
async fn sent_now_waits_at_the_step_limit_goes_in_on_continue_and_comes_back_on_a_stop() {
    let mentions = |p: &ScriptedProvider| {
        p.requests().iter().any(|(h, _)| h.iter().any(|t| matches!(t, ChatTurn::User { text } if text.contains("and b"))))
    };
    // "Continue": read by the next request.
    let (provider, q, _) = at_the_limit(Some(LimitAnswer::Continue)).await;
    assert!(mentions(&provider));
    assert!(q.close().is_empty());
    // "Stop": not read, and back to the person.
    let (provider, q, events) = at_the_limit(Some(LimitAnswer::Stop)).await;
    assert!(!mentions(&provider));
    assert_eq!(end(&events), Some(StopReason::MaxSteps));
    assert_eq!(q.close(), ["and b"]);
    // A hard stop's wrap-up reply doesn't get it either.
    let (provider, q, events) = at_the_limit(None).await;
    assert_eq!(provider.requests().len(), 2);
    assert!(!mentions(&provider));
    assert_eq!(end(&events), Some(StopReason::MaxSteps));
    assert_eq!(q.close(), ["and b"]);
}

#[tokio::test]
async fn sent_now_during_a_plain_answer_at_the_limit_ends_the_turn_and_comes_back() {
    let provider = Arc::new(ScriptedProvider::new(vec![say("Done.")]));
    let q = SendNow::new();
    let h = history(vec![user("go")]);
    let mut events = Vec::new();
    let mut stream = std::pin::pin!(run_agent(AgentOptions {
        send_now: Some(q.clone()),
        max_steps: 1,
        ..options(provider.clone(), &h, plain())
    }));
    while let Some(event) = stream.next().await {
        if matches!(event, LoopEvent::TextDelta(_)) && events.is_empty() {
            q.push("and b".into()).unwrap();
        }
        events.push(event);
    }
    assert_eq!(provider.requests().len(), 1);
    assert_eq!(end(&events), Some(StopReason::End));
    assert_eq!(q.close(), ["and b"]);
}

#[tokio::test]
async fn sent_now_during_an_overflow_recovery_goes_in_once_and_requests_keep_alternating() {
    let provider =
        Arc::new(ScriptedProvider::new(vec![use_tools(&[call("c1", "a.ts")]), overflow(), say("Done."), say("And b.")]));
    let h = history(vec![user("go")]);
    let q = SendNow::new();
    // The person types while the history is being made to fit.
    let (hook, attempts) = on_overflow(&h, vec![OverflowOutcome::Recovered]);
    let typed = q.clone();
    let hook: OnOverflow = Arc::new(move |error, attempt| {
        typed.push("and b".into()).unwrap();
        hook(error, attempt)
    });
    let events =
        collect(AgentOptions { on_overflow: Some(hook), send_now: Some(q.clone()), ..options(provider.clone(), &h, plain()) })
            .await;
    assert_eq!(*attempts.lock().unwrap(), [0]);
    let requests = provider.requests();
    // The resend is the fitted history as the hook left it: the message waits for the next boundary.
    assert_eq!(requests[2].0, [user("fitted")]);
    // Then it goes in once, as its own turn after the reply: never merged into "fitted", which the resend carried.
    assert_eq!(requests[3].0, [user("fitted"), said("Done."), user("and b")]);
    for (n, (sent, _)) in requests.iter().enumerate() {
        let roles: Vec<bool> =
            sent.iter().filter(|t| !matches!(t, ChatTurn::Tool { .. })).map(|t| matches!(t, ChatTurn::User { .. })).collect();
        assert!(roles.windows(2).all(|w| !(w[0] && w[1])), "request {n}: {sent:?}");
    }
    assert_eq!(count(&events, |e| matches!(e, LoopEvent::SentNow { .. })), 1);
    assert_eq!(end(&events), Some(StopReason::End));
    assert!(q.close().is_empty());
}

// marv-rs: the run's deadline (`marv -p --time-limit`), the module doc's "Run deadline".

#[test]
fn the_deadline_margin_is_a_quarter_of_the_budget_at_most_90_s() {
    assert_eq!(deadline_margin(Duration::from_secs(120)), Duration::from_secs(30));
    assert_eq!(deadline_margin(Duration::from_secs(600)), DEADLINE_WRAP_UP_MAX);
    assert_eq!(deadline_margin(Duration::ZERO), Duration::ZERO);
    let soon = std::time::Instant::now() + Duration::from_secs(300);
    assert!(time_left_note(Some(soon)).starts_with("\n[time left in this run: 4m 5"), "{}", time_left_note(Some(soon)));
    assert_eq!(time_left_note(None), "");
    assert!(deadline_note(Duration::from_secs(42)).contains("About 42 s are left"));
}

fn notes_in(turns: &[ChatTurn]) -> usize {
    turns.iter().filter(|t| matches!(t, ChatTurn::User { text } if text.contains("before the time limit stops this run"))).count()
}

#[tokio::test]
async fn near_the_deadline_the_model_is_told_once_and_every_result_says_the_time_left() {
    let provider =
        Arc::new(ScriptedProvider::new(vec![use_tools(&[call("c1", "a.ts")]), use_tools(&[call("c2", "b.ts")]), say("Saved.")]));
    let h = history(vec![user("go")]);
    let mut o = options(provider.clone(), &h, plain());
    o.deadline = Some(std::time::Instant::now() + Duration::from_secs(60));
    let mut state = LoopState::new(o);
    // A margin wider than the time left: the first step is already within it (a real run reaches it later).
    state.margin = Duration::from_secs(120);
    let mut events = Vec::new();
    while let Some(event) = state.next_event().await {
        events.push(event);
    }
    assert_eq!(end(&events), Some(StopReason::End));
    let requests = provider.requests();
    assert_eq!(requests.len(), 3);
    // In from the first request on, once, appended (each request starts with the one before: the cache holds).
    assert!(requests.iter().all(|(turns, _)| notes_in(turns) == 1));
    assert!(requests[1].0.starts_with(&requests[0].0) && requests[2].0.starts_with(&requests[1].0));
    assert!(requests.iter().all(|(_, o)| o.tool_choice == ToolChoice::Auto), "tools stay on: saving takes a call");
    // Every tool's result (read_file here, not only bash) ends with the time left.
    let texts = tool_texts(&h);
    assert_eq!(texts.len(), 2);
    for (text, path) in texts.iter().zip(["a.ts", "b.ts"]) {
        assert!(text.starts_with(&format!("contents of {path}\n[time left in this run: 0m 5")), "{text}");
    }
}

#[tokio::test]
async fn without_a_deadline_there_is_no_note_and_no_footer() {
    let provider = Arc::new(ScriptedProvider::new(vec![use_tools(&[call("c1", "a.ts")]), say("Done.")]));
    let h = history(vec![user("go")]);
    let events = collect(options(provider.clone(), &h, plain())).await;
    assert_eq!(end(&events), Some(StopReason::End));
    assert_eq!(tool_texts(&h), ["contents of a.ts"]);
    assert!(provider.requests().iter().all(|(turns, _)| notes_in(turns) == 0));
}

#[tokio::test]
async fn the_deadline_note_and_the_time_left_footers_are_logged_as_the_model_got_them() {
    let provider = Arc::new(ScriptedProvider::new(vec![use_tools(&[call("c1", "a.ts")]), say("Saved.")]));
    let h = history(vec![user("go")]);
    let mut o = options(provider, &h, plain());
    o.deadline = Some(std::time::Instant::now() + Duration::from_secs(60));
    let mut state = LoopState::new(o);
    state.margin = Duration::from_secs(120);
    let mut events = Vec::new();
    while let Some(event) = state.next_event().await {
        events.push(event);
    }
    let deadline = notes(&events, NoteKind::Deadline);
    assert_eq!(deadline.len(), 1);
    assert!(matches!(snapshot(&h)[0].clone(), ChatTurn::User { text } if text.ends_with(&deadline[0])));
    let footers = notes(&events, NoteKind::TimeLeft);
    assert_eq!(footers.len(), 1);
    assert!(tool_texts(&h)[0].ends_with(&footers[0]), "{footers:?}");
}

/// The first request reasons and never ends (ignoring its stop, as a slow provider might); later ones answer.
struct ReasonsForever {
    requests: Mutex<Vec<(Vec<ChatTurn>, CancellationToken)>>,
    first: Vec<ProviderEvent>,
}
impl Provider for ReasonsForever {
    fn name(&self) -> &str {
        "reasons-forever"
    }
    fn context_length(&self) -> Option<u64> {
        None
    }
    fn stream(&self, history: &[ChatTurn], options: StreamOptions) -> EventStream {
        let mut requests = self.requests.lock().unwrap();
        requests.push((history.to_vec(), options.cancel));
        if requests.len() == 1 {
            Box::pin(futures::stream::iter(self.first.clone()).chain(futures::stream::pending()))
        } else {
            Box::pin(futures::stream::iter(say("Saved; done.")))
        }
    }
}

async fn run_to_the_margin(first: Vec<ProviderEvent>) -> (Arc<ReasonsForever>, History, Vec<LoopEvent>) {
    let provider = Arc::new(ReasonsForever { requests: Mutex::default(), first });
    let h = history(vec![user("go")]);
    let mut o = AgentOptions::new(provider.clone(), h.clone(), "S".into(), specs(), Arc::new(plain()), CancellationToken::new());
    o.deadline = Some(std::time::Instant::now() + Duration::from_millis(1500));
    let mut state = LoopState::new(o);
    // The margin starts 0.2 s in.
    state.margin = Duration::from_millis(1300);
    let events = tokio::time::timeout(Duration::from_secs(10), async {
        let mut events = Vec::new();
        while let Some(event) = state.next_event().await {
            events.push(event);
        }
        events
    })
    .await
    .expect("the run ended");
    (provider, h, events)
}

#[tokio::test]
async fn a_reply_still_only_reasoning_when_the_margin_starts_is_dropped_and_asked_again_with_the_note() {
    // TB 2026-11-06 dna-insert: the last request started 317 s before the deadline and reasoned until it.
    let (provider, h, events) = run_to_the_margin(vec![ProviderEvent::ThinkingDelta("Let me reconsider…".into())]).await;
    assert_eq!(end(&events), Some(StopReason::End));
    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(requests[0].1.is_cancelled(), "only the dropped request was stopped");
    assert!(!requests[1].1.is_cancelled());
    // The second request is the first plus the note (nothing of the dropped reply), and the note is logged.
    assert_eq!(notes_in(&requests[0].0), 0);
    assert_eq!(notes_in(&requests[1].0), 1);
    assert_eq!(notes(&events, NoteKind::Deadline).len(), 1);
    assert!(!format!("{:?}", snapshot(&h)).contains("reconsider"));
    assert!(events.iter().any(|e| matches!(e, LoopEvent::Assistant { text } if text == "Saved; done.")));
}

#[tokio::test]
async fn a_reply_already_writing_when_the_margin_starts_is_left_to_finish() {
    // Its text (or the call saving the deliverable) would be lost: the deadline itself still stops it.
    let (provider, _h, events) = run_to_the_margin(vec![ProviderEvent::TextDelta("Writing the report".into())]).await;
    assert_eq!(end(&events), Some(StopReason::TimeLimit));
    assert_eq!(provider.requests.lock().unwrap().len(), 1);
    assert!(notes(&events, NoteKind::Deadline).is_empty());
}

/// A reply that streams a little, then never ends (and ignores the stop), as a model deep in reasoning would.
struct Hanging;
impl Provider for Hanging {
    fn name(&self) -> &str {
        "hanging"
    }
    fn context_length(&self) -> Option<u64> {
        None
    }
    fn stream(&self, _history: &[ChatTurn], _options: StreamOptions) -> EventStream {
        let start = futures::stream::iter(vec![ProviderEvent::TextDelta("Writing the report".into())]);
        Box::pin(start.chain(futures::stream::pending()))
    }
}

#[tokio::test]
async fn at_the_deadline_a_reply_in_flight_is_dropped_and_the_run_ends_time_limit() {
    let h = history(vec![user("go")]);
    let mut o = AgentOptions::new(Arc::new(Hanging), h.clone(), "S".into(), specs(), Arc::new(plain()), CancellationToken::new());
    o.deadline = Some(std::time::Instant::now() + Duration::from_millis(300));
    let events = tokio::time::timeout(Duration::from_secs(10), collect(o)).await.expect("the run ended at its deadline");
    assert_eq!(end(&events), Some(StopReason::TimeLimit));
    assert!(events.iter().any(|e| matches!(e, LoopEvent::Error(m) if m == TIME_LIMIT_MESSAGE)));
    // What streamed so far is kept, so `marv -p` has a reply to print.
    assert!(events.iter().any(|e| matches!(e, LoopEvent::Assistant { text } if text == "Writing the report")));
    assert_eq!(snapshot(&h).last(), Some(&said("Writing the report")));
}

#[tokio::test]
async fn at_the_deadline_running_tools_are_stopped_and_the_rest_not_run() {
    let provider = Arc::new(ScriptedProvider::new(vec![use_tools(&[call("c1", "slow"), call("c2", "next")])]));
    let h = history(vec![user("go")]);
    // The first call runs until it's stopped, as a long command would.
    let run: RunFn = Arc::new(|_, cancel| {
        Box::pin(async move {
            cancel.cancelled().await;
            Ok(ok("stopped", "slow"))
        })
    });
    let mut o = options(provider, &h, FakeRunner { run, parallel: |_| false });
    o.deadline = Some(std::time::Instant::now() + Duration::from_millis(300));
    let events = tokio::time::timeout(Duration::from_secs(10), collect(o)).await.expect("the run ended at its deadline");
    assert_eq!(end(&events), Some(StopReason::TimeLimit));
    let texts = tool_texts(&h);
    assert!(texts[0].starts_with("stopped\n[time left in this run: 0m 00s]"), "{}", texts[0]);
    assert_eq!(texts[1], not_run::TIME_LIMIT);
}

#[tokio::test]
async fn a_persons_stop_under_a_deadline_is_still_aborted() {
    let provider = Arc::new(ScriptedProvider::new(vec![say("hi")]));
    let h = history(vec![user("go")]);
    let mut o = options(provider, &h, plain());
    o.deadline = Some(std::time::Instant::now() + Duration::from_secs(60));
    o.cancel.cancel();
    let events = collect(o).await;
    assert_eq!(end(&events), Some(StopReason::Aborted));
    assert!(!events.iter().any(|e| matches!(e, LoopEvent::Error(m) if m == TIME_LIMIT_MESSAGE)));
}

#[tokio::test]
async fn a_run_that_starts_past_its_deadline_ends_without_a_request() {
    let provider = Arc::new(ScriptedProvider::new(vec![say("hi")]));
    let h = history(vec![user("go")]);
    let mut o = options(provider.clone(), &h, plain());
    o.deadline = Some(std::time::Instant::now());
    let events = collect(o).await;
    assert_eq!(end(&events), Some(StopReason::TimeLimit));
    assert!(provider.requests().is_empty());
}
