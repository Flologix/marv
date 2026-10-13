//! The whole TUI driven by a scripted fake engine: type, stream, approve, pick, set up.
//!
//! ```sh
//! cargo run -p marv-tui --example fake_session                 # splash first
//! cargo run -p marv-tui --example fake_session -- --no-splash
//! cargo run -p marv-tui --example fake_session -- --stream     # streams a long reply at once (CPU check)
//! ```
//!
//! Try: any message (thinking, then a streamed reply); a message with "file" in it (a tool call
//! streams, then asks for approval); "agents" (two subagents ask at once: the approval queue,
//! click an agent entry to open its view); "long" (a long reply, for scrolling and the CPU
//! numbers); `/help`, `/clear`, `/resume`, `/model`, `/setup`, `/busy`, `/exit`. Esc interrupts,
//! ctrl+c twice exits, drag to select and copy, the wheel and PgUp/PgDn scroll.
//!
//! The fake implements `SessionPort` the way marv-session will: every method returns at once and
//! the results come back as `UiEvent`s on the channel handed to `App::new`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::BoxFuture;
use marv_core::{StopReason, Usage};
use marv_tui::app::{App, AppOptions};
use marv_tui::view::{
    AgentProgress, ApprovalId, ApprovalRequest, CommandInfo, Decision, DiffKind, DiffLine, DiffShown, Message, ModelInfo, Notice,
    NoticeItem, NoticeTone, Preview, ProviderId, ProviderPreset, Resumed, Role, Scope, SessionPort, SessionSummary, SetupAnswer,
    SetupContext, SetupStart, StatusInfo, ToolState, ToolStatus, Totals, TurnEndReason, TurnStatus, UiEvent, WelcomeInfo,
};
use tokio::sync::{mpsc::UnboundedSender, oneshot};
use tokio::time::sleep;

const REPLY: &str = "Here's what I found. The **event loop** waits in one `select!` over the terminal's input, \
the engine's events and the next deadline, so an idle session uses no CPU at all.\n\n\
- Streamed text is batched every 33 ms.\n- Only the rows in the viewport are drawn.\n- The martian sleeps when it's scrolled away.\n\n\
```rust\nloop {\n    tokio::select! { /* … */ }\n}\n```\n\nAnything else?";

struct Fake {
    tx: UnboundedSender<UiEvent>,
    /// Set by Esc / ctrl+c; the running script checks it between events.
    aborted: Arc<AtomicBool>,
    busy: Arc<AtomicBool>,
    waiting: Arc<Mutex<HashMap<u64, oneshot::Sender<Decision>>>>,
    next_approval: Arc<AtomicU64>,
    totals: Arc<Mutex<Totals>>,
}

impl Fake {
    fn emit(&self, event: UiEvent) {
        let _ = self.tx.send(event);
    }

    fn handle(&self) -> Handle {
        Handle {
            tx: self.tx.clone(),
            aborted: self.aborted.clone(),
            waiting: self.waiting.clone(),
            next_approval: self.next_approval.clone(),
            totals: self.totals.clone(),
        }
    }

    fn system(&self, text: &str) {
        self.emit(UiEvent::Message(message(Role::System, text)));
    }
}

/// What a running script needs: the channel, the abort flag, and the approval waiters.
#[derive(Clone)]
struct Handle {
    tx: UnboundedSender<UiEvent>,
    aborted: Arc<AtomicBool>,
    waiting: Arc<Mutex<HashMap<u64, oneshot::Sender<Decision>>>>,
    next_approval: Arc<AtomicU64>,
    totals: Arc<Mutex<Totals>>,
}

/// The run was interrupted.
struct Stopped;

impl Handle {
    fn emit(&self, event: UiEvent) -> Result<(), Stopped> {
        if self.aborted.load(Ordering::SeqCst) {
            return Err(Stopped);
        }
        let _ = self.tx.send(event);
        Ok(())
    }

    async fn pause(&self, ms: u64) -> Result<(), Stopped> {
        sleep(Duration::from_millis(ms)).await;
        if self.aborted.load(Ordering::SeqCst) { Err(Stopped) } else { Ok(()) }
    }

    async fn ask(&self, request: ApprovalRequest) -> Result<Decision, Stopped> {
        let id = self.next_approval.fetch_add(1, Ordering::SeqCst);
        let (send, recv) = oneshot::channel();
        self.waiting.lock().unwrap().insert(id, send);
        self.emit(UiEvent::Approval { id: ApprovalId(id), request })?;
        recv.await.map_err(|_| Stopped)
    }

    /// Streams `text` a few characters at a time, `ms` apart, wrapped by `wrap`.
    async fn stream(&self, text: &str, chunk: usize, ms: u64, wrap: impl Fn(String) -> UiEvent) -> Result<(), Stopped> {
        let chars: Vec<char> = text.chars().collect();
        for piece in chars.chunks(chunk) {
            self.emit(wrap(piece.iter().collect()))?;
            self.pause(ms).await?;
        }
        Ok(())
    }

    fn usage(&self, prompt: u64, completion: u64) -> Result<(), Stopped> {
        let mut totals = self.totals.lock().unwrap();
        totals.requests += 1;
        totals.prompt_tokens += prompt;
        totals.cached_tokens += prompt * 4 / 5;
        totals.completion_tokens += completion;
        totals.cost = Some(totals.cost.unwrap_or(0.0) + 0.0042);
        let last =
            Usage { prompt_tokens: prompt, completion_tokens: completion, cached_tokens: Some(prompt * 4 / 5), cost: None };
        let totals = *totals;
        self.emit(UiEvent::Usage { last, totals })
    }
}

fn message(role: Role, text: &str) -> Message {
    Message { id: 0, role, text: text.into(), is_error: false, markdown: false, notice: None, tool: None }
}

fn tool_status(label: &str, status: ToolState, summary: &str) -> ToolStatus {
    ToolStatus { label: label.into(), status, summary: Some(summary.into()), steps: None, diff: None }
}

fn write_request(path: &str, agent: Option<String>) -> ApprovalRequest {
    let diff = (1..=4)
        .map(|i| DiffLine { kind: DiffKind::Add, text: format!("line {i} of {path}"), old_line: None, new_line: Some(i) })
        .collect();
    ApprovalRequest {
        tool: "write_file".into(),
        label: path.into(),
        preview: Preview { title: format!("Create {path}"), diff: Some(diff), ..Default::default() },
        scope: Scope { key: "files".into(), description: "file changes".into() },
        agent,
        network: false,
        once: false,
    }
}

/// One turn, scripted from what was typed.
async fn turn(h: Handle, text: String) -> Result<(), Stopped> {
    h.emit(UiEvent::TurnStart { turn: "t1".into() })?;
    h.emit(UiEvent::Status(TurnStatus::Running))?;
    h.stream(
        "Let me think about this. The user wants a demo of the shell.\nI should show streaming, then answer briefly.",
        6,
        40,
        UiEvent::ThinkingDelta,
    )
    .await?;

    if text.contains("file") {
        h.stream("I'll write it:", 3, 20, UiEvent::TextDelta).await?;
        let args = r#"{"path": "demo.txt", "content": "line 1 of demo.txt\nline 2 of demo.txt\n  indented line 3\nline 4"}"#;
        let chars: Vec<char> = args.chars().collect();
        for piece in chars.chunks(4) {
            h.emit(UiEvent::ToolCallDelta { index: 0, name: "write_file".into(), text: piece.iter().collect() })?;
            h.pause(40).await?;
        }
        h.emit(UiEvent::Assistant { text: "I'll write it:".into() })?;
        h.emit(UiEvent::ToolStart { call_id: "w1".into(), name: "write_file".into(), label: "demo.txt".into() })?;
        let decision = h.ask(write_request("demo.txt", None)).await?;
        if decision == Decision::No {
            h.emit(UiEvent::ToolEnd {
                call_id: "w1".into(),
                name: "write_file".into(),
                status: tool_status("demo.txt", ToolState::Declined, "declined"),
            })?;
            h.emit(UiEvent::Done(StopReason::Declined))?;
            return Ok(());
        }
        let diff = (1..=4)
            .map(|i| DiffLine { kind: DiffKind::Add, text: format!("line {i}"), old_line: None, new_line: Some(i) })
            .collect();
        let status = ToolStatus {
            diff: Some(DiffShown { lines: diff, more: 0 }),
            ..tool_status("demo.txt", ToolState::Done, "created · 4 lines")
        };
        h.emit(UiEvent::ToolEnd { call_id: "w1".into(), name: "write_file".into(), status })?;
        h.usage(1200, 60)?;
    }

    if text.contains("agents") {
        for (id, what) in [("a1", "one"), ("a2", "two")] {
            h.emit(UiEvent::ToolStart {
                call_id: id.into(),
                name: "agent".into(),
                label: format!("general-purpose · write {what}.txt"),
            })?;
        }
        let mut asks = Vec::new();
        for (id, what) in [("a1", "one"), ("a2", "two")] {
            let title = format!("general-purpose · write {what}.txt");
            let sub = |event: UiEvent| UiEvent::Subagent { call_id: id.into(), title: title.clone(), event: Box::new(event) };
            h.emit(sub(UiEvent::TextDelta(format!("Writing {what}.txt now."))))?;
            h.emit(sub(UiEvent::Assistant { text: format!("Writing {what}.txt now.") }))?;
            h.emit(sub(UiEvent::ToolStart {
                call_id: format!("w-{id}"),
                name: "write_file".into(),
                label: format!("{what}.txt"),
            }))?;
            h.emit(UiEvent::SubagentProgress {
                call_id: id.into(),
                progress: AgentProgress { line: format!("shared folder · 1 tool · write_file {what}.txt"), steps: vec![] },
            })?;
            let h2 = h.clone();
            let title2 = title.clone();
            asks.push(tokio::spawn(async move { h2.ask(write_request(&format!("{what}.txt"), Some(title2))).await.ok() }));
        }
        for (i, ask) in asks.into_iter().enumerate() {
            let (id, what) = [("a1", "one"), ("a2", "two")][i];
            let decision = ask.await.ok().flatten().ok_or(Stopped)?;
            let title = format!("general-purpose · write {what}.txt");
            let sub = |event: UiEvent| UiEvent::Subagent { call_id: id.into(), title: title.clone(), event: Box::new(event) };
            let (state, summary) = if decision == Decision::No {
                (ToolState::Declined, "declined")
            } else {
                (ToolState::Done, "created · 4 lines")
            };
            h.emit(sub(UiEvent::ToolEnd {
                call_id: format!("w-{id}"),
                name: "write_file".into(),
                status: tool_status(&format!("{what}.txt"), state, summary),
            }))?;
            h.emit(sub(UiEvent::Done(StopReason::End)))?;
            let mut status = tool_status(&title, ToolState::Done, "done · 1 tool");
            status.steps = Some(vec![format!("write_file {what}.txt · {summary}")]);
            h.emit(UiEvent::ToolEnd { call_id: id.into(), name: "agent".into(), status })?;
        }
    }

    let reply = if text.contains("long") {
        (1..=60)
            .map(|i| format!("Paragraph {i}: {}", "the quick brown fox jumps over the lazy dog. ".repeat(4)))
            .collect::<Vec<_>>()
            .join("\n\n")
    } else {
        REPLY.to_string()
    };
    // ~4 chars a token, a token every 10 ms: a fast provider.
    h.stream(&reply, 4, 10, UiEvent::TextDelta).await?;
    h.emit(UiEvent::Assistant { text: reply.clone() })?;
    h.usage(1500, reply.len() as u64 / 4)?;
    h.emit(UiEvent::Done(StopReason::End))?;
    Ok(())
}

impl SessionPort for Fake {
    fn commands(&self) -> Vec<CommandInfo> {
        [
            ("help", "Show available commands"),
            ("clear", "Clear the conversation"),
            ("resume", "Resume an earlier session"),
            ("model", "Switch model"),
            ("setup", "Change provider"),
            ("busy", "Work outside a turn for 2 s"),
            ("exit", "Quit"),
        ]
        .into_iter()
        .map(|(name, description)| CommandInfo { name: name.into(), description: description.into() })
        .collect()
    }

    fn send(&self, text: String) {
        self.aborted.store(false, Ordering::SeqCst);
        self.busy.store(true, Ordering::SeqCst);
        let h = self.handle();
        let busy = self.busy.clone();
        tokio::spawn(async move {
            let reason = match turn(h.clone(), text).await {
                Ok(()) => TurnEndReason::Loop(StopReason::End),
                Err(Stopped) => {
                    let _ = h.tx.send(UiEvent::Done(StopReason::Aborted));
                    TurnEndReason::Loop(StopReason::Aborted)
                }
            };
            h.waiting.lock().unwrap().clear();
            busy.store(false, Ordering::SeqCst);
            let _ = h.tx.send(UiEvent::TurnEnd { turn: "t1".into(), reason });
        });
    }

    fn run_command(&self, line: String) {
        let name = line.trim_start_matches('/').split_whitespace().next().unwrap_or_default().to_string();
        match name.as_str() {
            "help" => {
                let list =
                    self.commands().iter().map(|c| format!("/{:<8} {}", c.name, c.description)).collect::<Vec<_>>().join("\n");
                self.system(&list);
            }
            "clear" => self.emit(UiEvent::Clear),
            "resume" => {
                let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64;
                let sessions = (0..20)
                    .map(|i| SessionSummary {
                        id: format!("s{i}"),
                        title: format!("Session about thing {i}"),
                        updated_at: now - i * 3_600_000,
                        messages: 2 + i as usize,
                        model: "ollama/qwen3.5:9b".into(),
                    })
                    .collect();
                self.emit(UiEvent::Sessions(sessions));
            }
            "model" | "setup" => self.emit(UiEvent::OpenSetup(setup_context(if name == "model" {
                SetupStart::Model
            } else {
                SetupStart::Reconfigure
            }))),
            "busy" => {
                let tx = self.tx.clone();
                self.emit(UiEvent::Busy(Some("Searching the MCP Registry…".into())));
                tokio::spawn(async move {
                    sleep(Duration::from_secs(2)).await;
                    let _ = tx.send(UiEvent::Busy(None));
                    let _ = tx.send(UiEvent::Message(message(Role::System, "Found 3 servers.")));
                });
            }
            "exit" => self.emit(UiEvent::Exit),
            _ => self
                .emit(UiEvent::Message(Message { is_error: true, ..message(Role::System, &format!("Unknown command: {line}")) })),
        }
    }

    fn answer(&self, id: ApprovalId, decision: Decision) {
        if let Some(waiter) = self.waiting.lock().unwrap().remove(&id.0) {
            let _ = waiter.send(decision);
        }
    }

    fn abort(&self) {
        self.aborted.store(true, Ordering::SeqCst);
        // Waiters dropped: their asks end as stopped.
        self.waiting.lock().unwrap().clear();
    }

    fn load_models(&self, provider: ProviderId) {
        let tx = self.tx.clone();
        tokio::spawn(async move {
            sleep(Duration::from_millis(300)).await;
            let m = |id: &str, price: Option<(f64, f64)>, local: bool, tools: bool| ModelInfo {
                id: id.into(),
                tools: Some(tools),
                price_in: price.map(|p| p.0),
                price_out: price.map(|p| p.1),
                local,
                ..Default::default()
            };
            let models = match provider {
                ProviderId::OpenRouter => vec![
                    m("anthropic/claude-sonnet-5.5", Some((2.0, 10.0)), false, true),
                    m("deepseek/deepseek-v4.1-flash", Some((0.3, 1.2)), false, true),
                    m("z-ai/glm-5.3", Some((0.22, 3.39)), false, true),
                    m("meta/llama-free", Some((0.0, 0.0)), false, false),
                ],
                ProviderId::Ollama => vec![m("qwen3.5:9b", None, true, true), m("tiny:1b", None, true, false)],
            };
            let _ = tx.send(UiEvent::Models(Ok(models)));
        });
    }

    fn complete_setup(&self, answer: SetupAnswer) {
        let label = match answer.provider {
            ProviderId::OpenRouter => "OpenRouter",
            ProviderId::Ollama => "Ollama",
        };
        self.system(&format!("Saved to ~/.marv/config.json · {label} · {}", answer.model));
        self.emit(UiEvent::StatusInfo(status(&format!("{}/{}", label.to_lowercase(), answer.model))));
    }

    fn resume(&self, id: Option<String>) {
        let id = id.unwrap_or_else(|| "s0".into());
        let transcript = vec![
            Message { id: 1, ..message(Role::User, &format!("What was {id} about?")) },
            Message { id: 2, ..message(Role::Assistant, "It was about the fake session example.") },
        ];
        let updated_at =
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64 - 7_200_000;
        self.emit(UiEvent::Resumed(Resumed {
            id,
            updated_at,
            model: "ollama/qwen3.5:9b".into(),
            transcript,
            totals: Totals::default(),
        }));
    }

    fn flush(&self) -> BoxFuture<'static, ()> {
        Box::pin(async {})
    }
}

fn setup_context(start: SetupStart) -> SetupContext {
    SetupContext {
        start,
        presets: vec![
            ProviderPreset {
                id: ProviderId::OpenRouter,
                label: "OpenRouter".into(),
                key_env: Some("OPENROUTER_API_KEY".into()),
                key_url: Some("openrouter.ai/settings/keys".into()),
                default_model: "anthropic/claude-sonnet-5.5".into(),
            },
            ProviderPreset {
                id: ProviderId::Ollama,
                label: "Ollama".into(),
                key_env: None,
                key_url: None,
                default_model: String::new(),
            },
        ],
        current: Some((ProviderId::Ollama, "qwen3.5:9b".into())),
        saved_key: None,
        env_keys: vec![],
    }
}

fn status(model: &str) -> StatusInfo {
    StatusInfo { model: model.into(), cwd: "~/Projects/demo".into(), yolo: true, context_length: Some(32_768) }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let no_splash = args.iter().any(|a| a == "--no-splash");
    let stream = args.iter().any(|a| a == "--stream");

    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let fake = Arc::new(Fake {
        tx: tx.clone(),
        aborted: Arc::default(),
        busy: Arc::default(),
        waiting: Arc::default(),
        next_approval: Arc::new(AtomicU64::new(1)),
        totals: Arc::default(),
    });

    // A startup notice, as the engine collates them.
    let notice = Notice {
        title: "Startup · 1 note".into(),
        tone: NoticeTone::Info,
        items: vec![NoticeItem { text: "This is a fake engine: nothing runs, nothing is saved.".into(), detail: None }],
    };
    fake.emit(UiEvent::Message(Message { notice: Some(notice), ..message(Role::System, "") }));
    if stream {
        // As if typed: the user's line, then the turn (the App starts it on TurnStart).
        fake.emit(UiEvent::Message(message(Role::User, "write something long")));
        fake.send("write something long".into());
    }

    let options = AppOptions {
        welcome: WelcomeInfo {
            version: "0.0.1".into(),
            cwd: "~/Projects/demo".into(),
            instructions: true,
            skills: 2,
            memories: 1,
            mcp_servers: 0,
        },
        status: status("ollama/qwen3.5:9b"),
        splash_ms: if no_splash || stream { 0 } else { 1200 },
        render: Default::default(),
    };
    App::new(fake, rx, options).run().await
}
