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
| `src/app.tsx` | `app.rs` | Shape: constants, `STEP_ENDS`, `Deadlines` (done and tested), `App` state, `run`/`apply`/`on_key`/`on_mouse`/`draw` todo. The event loop design is in the module header. |
| `src/mouse.ts` | `mouse.rs` | Constants done; `from_crossterm` todo. |
| `src/selection.ts` | `selection.rs` | Types; `SelectionStore` todo. |
| `src/clipboard.ts` | `clipboard.rs` | todo |
| `src/ui/Markdown.tsx` | `ui/markdown.rs` | `render` (runs `printable()` and dispatches to the stream lexer) done; `lex`, `StreamLexer::parse`, `layout` todo |
| `src/ui/MessageView.tsx` | `ui/message_view.rs` | user entry done; assistant/tool/system todo |
| `src/ui/DiffView.tsx` | `ui/diff_view.rs` | `mark` done; `render` todo |
| `src/ui/NoticeView.tsx` | `ui/notice_view.rs` | `tone_color`, `worst_tone` done; `render` todo |
| `src/ui/ThinkingView.tsx` | `ui/thinking_view.rs` | `elapsed` done; `ThinkingView`/`DraftView` todo |
| `src/ui/ScrollView.tsx` | `ui/scroll_view.rs` | `scroll_top`/`follow` done; scrolling, glide, culling todo |
| `src/ui/Transcript.tsx` | `ui/transcript.rs` | `agent_entry_at` done; layout cache todo |
| `src/ui/AgentView.tsx` + `src/agent-log.ts` | `ui/agent_view.rs` | todo |
| `src/ui/Approval.tsx` | `ui/approval.rs` | todo |
| `src/ui/Select.tsx` | `ui/select.rs` | `new` done; rest todo |
| `src/ui/SessionPicker.tsx` | `ui/session_picker.rs` | todo |
| `src/ui/ModelPicker.tsx` | `ui/model_picker.rs` | todo |
| `src/ui/Setup.tsx` | `ui/setup.rs` | todo |
| `src/ui/PromptInput.tsx` | `ui/prompt_input.rs` | todo |
| `src/ui/StatusBar.tsx` | `ui/status_bar.rs` | todo |

`grep -rn 'todo!("port' crates/marv-tui` lists what's left. `cargo run -p marv-tui --example martian` shows the animated martian (q to quit).

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
  - *Incremental rendering* (`incrementalRendering: true`, guarded by `.marv/checks/render-options.yaml`). ratatui diffs at the cell level natively; the App (todo) wraps each frame in synchronized output.
- **Mouse.** `crossterm::EnableMouseCapture` also turns on mode 1003 (every motion), which would wake the loop on each mouse move. `TerminalGuard` writes Marv's `MOUSE_ON` (1002 + 1006) raw instead. crossterm parses SGR codes, including split ones, so Marv's `filterMouseInput` stdin wrapper has no counterpart.
- **Clipboard.** It uses platform tools, then falls back to OSC 52 (`base64` crate). We chose this over `arboard`: no X or Wayland libraries in the static binary, and it works over ssh.
- **printable().** View-model text is raw. The function that draws it calls `marv_core::printable` (`markdown::render`, `message_view::render`, `splash`, `welcome`, approvals, the status bar, agent titles), so the model's conversation is never modified.
- **Martian timing.** Marv re-armed `setTimeout` after each render, so it drifted. `MartianAnimation` adds each step's ms to the previous deadline, so it doesn't drift, and it resyncs instead of replaying when woken more than one loop late.
- **Not in this crate:** `src/commands/*` (the engine runs them and sends the UI-side results), `src/tool-draft.ts` (`draftLabel`/`draftPreview`, which `DraftView` needs: to be decided between engine and TUI), and Herdr reporting (marv-cli).
