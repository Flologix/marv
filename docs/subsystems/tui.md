# marv-tui

Marv's terminal UI, rebuilt on **ratatui 0.30 + crossterm 0.29** (Marv used Ink/React). It renders what the engine reports and sends back what the user asks for. It never imports `marv-session`: the boundary is `view.rs`.

Spec: Marv `CLAUDE.md` bullets **Rendering**, **Mouse**, **Selection**, **Replies are Markdown**, **Setup**, **Waiting line**, **Render budget**, **Yolo mode** (status bar), **Tokens and cost** (status bar); `src/app.tsx`, `src/ui/*.tsx`, `src/mouse.ts`, `src/selection.ts`, `src/clipboard.ts`, `src/render-options.ts`; tests `tests/{app,approval,diffview,markdown,martian,messageview,mouse,noticeview,prompt,render-options,render-performance,scrollview,selection,setup,clipboard}.test.ts(x)`.

## Modules

| Marv (TS) | Rust module | Status |
|---|---|---|
| — (new) | `view.rs` | **Done.** `UiEvent`, `SessionPort`, the view-model types (`Message`, `Notice`, `ApprovalRequest`, `DiffLine`, `ModelInfo`, `SessionSummary`, `Totals`, `SetupContext`…). Test: `Message` keeps Marv's JSON names. |
| `src/ui/Martian.tsx` | `ui/martian.rs` | **Ported.** All 8 frames byte-exact; `GREETING`, `IDLE_LOOP` with exact ms; `MartianAnimation` (deadline-driven state machine, drift-free, resyncs after a stall); `MartianWidget` (accent foreground only). All tests ported, plus extras. |
| `src/ui/Splash.tsx` | `ui/splash.rs` | **Ported.** Logo, gradient, narrow fallback, deadline instead of a timer. Tests. |
| `src/ui/Welcome.tsx` | `ui/welcome.rs` | **Ported.** Bordered banner, martian beside centred text, loaded line. Test from `martian.test.tsx`. |
| `src/ui/theme.ts` | `ui/theme.rs` | **Ported** (all colours). |
| `src/render-options.ts` | `render_options.rs` | **Ported.** `TerminalGuard` (raw mode, alternate screen, 1002+1006 mouse, restores on drop and on panic), `MAX_FPS`. |
| `src/app.tsx` | `app.rs` | **Ported (#10).** The event loop (`select!` over `EventStream`, the `UiEvent` channel and `sleep_until(Deadlines)`; draws only when dirty, capped at 60 fps, in synchronized output), 33 ms stream batching, the turn's event switch, the approval queue (head + "N more waiting", Always covers the same scope, Esc/ctrl+c decline all), keys (Esc, ctrl+c ladder, ctrl+o, PgUp/PgDn), mouse (wheel glide, drag-select, release copies, click opens a subagent's view), setup/picker/approval/prompt panels, status bar. App tests from `app.test.tsx` that are UI-side are ported against a fake `SessionPort`; the engine-side ones (commands, config, MCP, memory, sessions on disk) belong to marv-session. Until #9 lands, a private `standin` module draws entries, the waiting line, drafts and the agent view as plain wrapped text, and the scroll/glide arithmetic lives in `app.rs`; each stand-in names its replacement (`TODO(#9)`). |
| `src/mouse.ts` | `mouse.rs` | **Ported.** `from_crossterm`; the SGR cases of `mouse.test.ts` as tests over crossterm events (`filterMouseInput`'s split-code tests have no counterpart: crossterm parses). |
| `src/selection.ts` | `selection.rs` | **Ported.** `SelectionStore` over ratatui `Buffer` cells (`transform_buffer` remembers rows and sets `REVERSED`), `whole_cells`, auto-scroll as a deadline. All `selection.test.ts` cases ported (the repaint-batching one is the loop's drain-then-draw). |
| `src/clipboard.ts` | `clipboard.rs` | **Ported.** Platform tools via `tokio::process`, OSC 52 fallback. Tests ported. |
| `src/ui/Markdown.tsx` | `ui/markdown.rs` | `render` (runs `printable()` and dispatches to the stream lexer) done; `lex`, `StreamLexer::parse`, `layout` todo |
| `src/ui/MessageView.tsx` | `ui/message_view.rs` | user entry done; assistant/tool/system todo |
| `src/ui/DiffView.tsx` | `ui/diff_view.rs` | `mark` done; `render` todo |
| `src/ui/NoticeView.tsx` | `ui/notice_view.rs` | `tone_color`, `worst_tone` done; `render` todo |
| `src/ui/ThinkingView.tsx` | `ui/thinking_view.rs` | `elapsed` done; `ThinkingView`/`DraftView` todo |
| `src/ui/ScrollView.tsx` | `ui/scroll_view.rs` | `scroll_top`/`follow` done; scrolling, glide, culling todo |
| `src/ui/Transcript.tsx` | `ui/transcript.rs` | `agent_entry_at` done; layout cache todo |
| `src/ui/AgentView.tsx` + `src/agent-log.ts` | `ui/agent_view.rs` | todo |
| `src/ui/Approval.tsx` | `ui/approval.rs` | **Ported.** Full payload, wrapped; the diff through a stand-in until `diff_view::render` (#9). All `approval.test.tsx` cases ported. |
| `src/ui/Select.tsx` | `ui/select.rs` | **Ported**, plus the panels' shared `wrap`/`truncate_end` (Ink's word wrap and `truncate-end`). |
| `src/ui/SessionPicker.tsx` | `ui/session_picker.rs` | **Ported**, with `time_ago`. Tests. |
| `src/ui/ModelPicker.tsx` | `ui/model_picker.rs` | **Ported**, with `format_price`. `setup.test.tsx` picker cases ported. |
| `src/ui/Setup.tsx` | `ui/setup.rs` | **Ported.** Answers with `SetupAnswer` (`api_key: None` keeps the saved key; the engine owns the file, the env and the endpoint). All `setup.test.tsx` cases ported. |
| `src/ui/PromptInput.tsx` | `ui/prompt_input.rs` | **Ported.** Owns its value (the App clears it on an accepted submit). All `prompt.test.tsx` cases ported. |
| `src/ui/StatusBar.tsx` | `ui/status_bar.rs` | **Ported**, with `tokens`, `format_usage` (cache-hit rate), `cost_text` (integer rounding to match JS's halves-up). Tests. |

`grep -rn 'todo!("port' crates/marv-tui` lists what's left. `cargo run -p marv-tui --example martian` shows the animated martian (q to quit); `cargo run -p marv-tui --example fake_session` runs the whole shell on a scripted fake engine (see its header for what to try; `--stream` streams a long reply at once, `--no-splash`).

## The boundary the engine implements

```rust
pub trait SessionPort: Send + Sync {          // every method returns at once; results come back as UiEvents
    fn commands(&self) -> Vec<CommandInfo>;   // the `/` menu
    fn send(&self, text: String);             // start a turn
    fn run_command(&self, line: String);      // a slash command; UI-side results as events
    fn answer(&self, id: ApprovalId, decision: Decision);
    fn abort(&self);                          // Esc / ctrl+c
    fn load_models(&self, provider: ProviderId);     // → UiEvent::Models
    fn complete_setup(&self, answer: SetupAnswer);
    fn resume(&self, id: Option<String>);            // → UiEvent::Resumed
    fn flush(&self) -> BoxFuture<'static, ()>;       // before exit
}
// App::new(port: Arc<dyn SessionPort>, events: UnboundedReceiver<UiEvent>, options: AppOptions)
```

`UiEvent` mirrors Marv's `SessionEvent` (the turn and its `LoopEvent`s, `Subagent { call_id, title, event }`, `SubagentProgress`, `Compaction`, `TurnEnd`) and adds `Approval { id, request }` plus the results of port calls (`Message`, `StatusInfo`, `Welcome`, `Clear`, `OpenSetup`, `Sessions`, `Models`, `Resumed`, `Flash`, `Busy`, `Exit`). The engine maps project checks and diagnostics to `Message`s with a `Notice`. The approval queue lives in the TUI as in Marv: each request has an `ApprovalId`, the head is shown with "N more waiting", and Esc answers `No` to the whole queue and then calls `abort()`.

## Port notes

- **Idle CPU (Marv #68).** Ink used 40–60% CPU per session. The App waits in one `tokio::select!` over crossterm's `EventStream`, the `UiEvent` channel and `sleep_until(earliest deadline)`, and draws only when something changed. Every animation exposes a deadline instead of owning a timer: the martian (only while the welcome banner is in the viewport), the 33 ms stream flush, the 20 ms wheel glide, the selection auto-scroll, the spinner (only while shown), and the notice expiries. With nothing animating, the loop sleeps until input arrives. `examples/martian.rs` is a miniature of this loop.
- **Stream batching.** Text deltas collect in a buffer and reach the transcript at most every `STREAM_FLUSH_MS` (33 ms, ~30 fps). Step ends flush at once. The loop drains all queued events before it draws.
- **What Marv's Ink patches did, and what replaces them:**
  - *Viewport culling* (`patches/ink@8.0.0.patch`). Ink drew every scrolled-away message on every frame: ~270 ms per keystroke on 60 entries, ~30 ms patched. Here each entry caches its wrapped lines per width (Marv's `memo`), `ScrollView` keeps prefix sums of the heights, and only the entries that intersect the viewport are rendered.
  - *Selection overlay* (`transformOutput` + `repaint()`). Marv drew the highlight into Ink's output string, so moving it cost ~3 ms instead of a ~30 ms React render. Here `SelectionStore::transform_buffer` reads and highlights the drawn `Buffer` after the widgets render. Because frames are culled and ratatui diffs cells, moving the highlight is an ordinary cheap draw.
  - *Render speed* (the `string-width` patch, and skipping `findStaticNode`). `string-width` was ~36% of CPU while scrolling, because of its per-character emoji regex. `unicode-width` is a table lookup, and there is no React tree to walk.
  - *Incremental rendering* (`incrementalRendering: true`, guarded by `.marv/checks/render-options.yaml`). ratatui diffs at the cell level natively; the App wraps each frame in synchronized output.
- **Mouse.** `crossterm::EnableMouseCapture` also turns on mode 1003 (every motion), which would wake the loop on each mouse move. `TerminalGuard` writes Marv's `MOUSE_ON` (1002 + 1006) raw instead. crossterm parses SGR codes, including split ones, so Marv's `filterMouseInput` stdin wrapper has no counterpart.
- **Clipboard.** It uses platform tools, then falls back to OSC 52 (`base64` crate). We chose this over `arboard`: no X or Wayland libraries in the static binary, and it works over ssh.
- **printable().** View-model text is raw. The function that draws it calls `marv_core::printable` (`markdown::render`, `message_view::render`, `splash`, `welcome`, approvals, the status bar, agent titles), so the model's conversation is never modified.
- **Martian timing.** Marv re-armed `setTimeout` after each render, so it drifted. `MartianAnimation` adds each step's ms to the previous deadline, so it doesn't drift, and it resyncs instead of replaying when woken more than one loop late.
- **Not in this crate:** `src/commands/*` (the engine runs them and sends the UI-side results), `src/tool-draft.ts` (`draftLabel`/`draftPreview`, which `DraftView` needs: to be decided between engine and TUI), and Herdr reporting (marv-cli).

## The shell (#10): measured CPU and known gaps

**CPU** (`examples/fake_session` in a pty via `script`, `top -b -n 3 -d 2 -p <pid>`, 120×40; the fake streams ~4 chars every 10 ms):

| build | idle (martian on screen) | streaming, first seconds | streaming, 20 s in |
|---|---|---|---|
| release | 0.0 / 0.0 / 0.0 % | 0.0 / 1.5 / 2.0 % | 5.0 / 1.5 / 1.0 % |
| debug | 0.0 / 0.5 / 0.0 % | 9.9 / 16.0 / 14.5 % | 9.9 / 14.0 / 14.5 % |

The streaming reply's finished lines are laid out once (only the unfinished last line is re-wrapped each frame); without that, debug climbed past 30% as the reply grew. Re-measure when #9's Markdown renderer replaces the stand-ins.

**No `terminal.clear()` at start:** it asks the terminal for the cursor position, and a terminal that never answers (a bare pty) would stop the TUI starting. The alternate screen starts blank.

**Gaps for the engine / other issues:**
- `UiEvent::ToolStart` carries no arguments, so a subagent's view can't show its task (Marv's first user message in `AgentLog`). The engine should send it (e.g. a `prompt` on `ToolStart` for `agent`, or a first `Subagent` event).
- Marv routes a pasted OAuth callback address (`isCallback`) to the pending login *before* the busy check and keeps it out of the prompt history. The App sends `/…` lines and messages as typed; the callback check needs a `SessionPort` hook (or the engine catches it in `send`), and the history should skip it.
- `marv -c`'s "Loading your last session…" (a message typed while the session loads waits) is the engine's to sequence: it should hold `send` until `Resumed`.
- Herdr state reporting (blocked / working / idle) is marv-cli's; the App exposes `busy()` and the approval queue but no hook yet.
- Engine messages get the App's own ids (`UiEvent::Message`'s id is replaced), so restored and new ids never collide.
