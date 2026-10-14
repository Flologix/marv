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
| `src/app.tsx` | `app.rs` | **Ported (#10; renderers wired in #9).** The event loop (`select!` over `EventStream`, the `UiEvent` channel and `sleep_until(Deadlines)`; draws only when dirty, capped at 60 fps, in synchronized output), 33 ms stream batching, the turn's event switch, the approval queue (head + "N more waiting", Always covers the same scope, Esc/ctrl+c decline all), keys (Esc, ctrl+c ladder, ctrl+o, PgUp/PgDn), mouse (wheel glide, drag-select, release copies, click opens a subagent's view), setup/picker/approval/prompt panels, status bar. App tests from `app.test.tsx` that are UI-side are ported against a fake `SessionPort`; the engine-side ones (commands, config, MCP, memory, sessions on disk) belong to marv-session. The transcript, the live reply, the waiting line, drafts and the subagent view are drawn by the #9 renderers below (the #10 stand-ins are gone); the subagent view's header now sits above the view, as in Marv. |
| `src/mouse.ts` | `mouse.rs` | **Ported.** `from_crossterm`; the SGR cases of `mouse.test.ts` as tests over crossterm events (`filterMouseInput`'s split-code tests have no counterpart: crossterm parses). |
| `src/selection.ts` | `selection.rs` | **Ported.** `SelectionStore` over ratatui `Buffer` cells (`transform_buffer` remembers rows and sets `REVERSED`), `whole_cells`, auto-scroll as a deadline. All `selection.test.ts` cases ported (the repaint-batching one is the loop's drain-then-draw). |
| `src/clipboard.ts` | `clipboard.rs` | **Ported.** Platform tools via `tokio::process`, OSC 52 fallback. Tests ported. |
| `src/ui/Markdown.tsx` | `ui/markdown.rs` | **Ported (#9).** pulldown-cmark → owned `Block`s (styles resolved into `Run`s), `layout` with Ink's wrapping (wrap-ansi `hard`), `StreamLexer` (Marv's boundary scan; also keeps each finished block's lines per width). All `markdown.test.tsx` cases, including the random streamed-prefix property (blocks *and* laid-out lines equal parsing whole), plus wrapping, autolink and escape tests. |
| `src/ui/MessageView.tsx` | `ui/message_view.rs` | **Ported (#9).** 2-column gutter, tool lines, steps, collapsed diffs, notices, Markdown system messages; `has_details` keys the ctrl+o cache. All `messageview.test.tsx` cases through ratatui's `TestBackend`. |
| `src/ui/DiffView.tsx` | `ui/diff_view.rs` | **Ported (#9).** With local copies of `previewLines`/`fileLines` (see notes). All `diffview.test.tsx` cases. Also draws the approval prompt's diff. |
| `src/ui/NoticeView.tsx` | `ui/notice_view.rs` | **Ported (#9).** Bar on every row, hanging indents. All `noticeview.test.tsx` cases (colours checked on spans). |
| `src/ui/ThinkingView.tsx` | `ui/thinking_view.rs` | **Ported (#9).** `ThinkingView`, `DraftView`, `next_frame` (the spinner's deadline), and a copy of `tool-draft.ts` (see notes). `thinking-view.test.tsx` and `tool-draft.test.ts` cases. |
| `src/ui/ScrollView.tsx` | `ui/scroll_view.rs` | **Ported (#9).** Follow/pin, pages, the glide as a deadline (Marv's `ceil` third), hidden keeps its place and stops a glide, `visible` culls entries to the viewport, `draw` for laid-out lines. All `scrollview.test.tsx` cases. |
| `src/ui/Transcript.tsx` | `ui/transcript.rs` | **Ported (#9).** `Transcript` (items + per-entry line cache keyed by width and ctrl+o), `agent_entries`/`agent_entry_at`, `welcome_lines`. The `render-performance.test.tsx` keystroke case (600 entries, 120×45). |
| `src/ui/AgentView.tsx` + `src/agent-log.ts` | `ui/agent_view.rs` | **Ported (#9).** `AgentLog::new/apply`, `AgentViewHeader`, `AgentView` (its own stream lexer, waiting line clock, entry cache, spinner deadline). All `agent-log.test.ts` cases plus view and header tests. |
| `src/ui/Approval.tsx` | `ui/approval.rs` | **Ported.** Full payload, wrapped; the diff through `diff_view::render` (#9). All `approval.test.tsx` cases ported. |
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
- **Paced streaming (deliberate difference).** Text deltas collect in a buffer. The reply and the reasoning are revealed from it one share per frame (1/7 of what's hidden, at least `REVEAL_MIN_CHARS`), so each provider burst drains over about `REVEAL_MS` (120 ms) instead of landing at once. Marv batched at a fixed 33 ms, so text came in jerks. A backlog over `REVEAL_CATCH_UP` shows at once. Step ends show everything at once. The loop drains all queued events before it draws.
- **What Marv's Ink patches did, and what replaces them:**
  - *Viewport culling* (`patches/ink@8.0.0.patch`). Ink drew every scrolled-away message on every frame: ~270 ms per keystroke on 60 entries, ~30 ms patched. Here each entry caches its wrapped lines per width (Marv's `memo`), `ScrollView` keeps prefix sums of the heights, and only the entries that intersect the viewport are rendered.
  - *Selection overlay* (`transformOutput` + `repaint()`). Marv drew the highlight into Ink's output string, so moving it cost ~3 ms instead of a ~30 ms React render. Here `SelectionStore::transform_buffer` reads and highlights the drawn `Buffer` after the widgets render. Because frames are culled and ratatui diffs cells, moving the highlight is an ordinary cheap draw.
  - *Render speed* (the `string-width` patch, and skipping `findStaticNode`). `string-width` was ~36% of CPU while scrolling, because of its per-character emoji regex. `unicode-width` is a table lookup, and there is no React tree to walk.
  - *Incremental rendering* (`incrementalRendering: true`, guarded by `.marv/checks/render-options.yaml`). ratatui diffs at the cell level natively; the App wraps each frame in synchronized output.
- **Mouse.** `crossterm::EnableMouseCapture` also turns on mode 1003 (every motion), which would wake the loop on each mouse move. `TerminalGuard` writes Marv's `MOUSE_ON` (1002 + 1006) raw instead. crossterm parses SGR codes, including split ones, so Marv's `filterMouseInput` stdin wrapper has no counterpart.
- **Clipboard.** It uses platform tools, then falls back to OSC 52 (`base64` crate). We chose this over `arboard`: no X or Wayland libraries in the static binary, and it works over ssh.
- **printable().** View-model text is raw. The function that draws it calls `marv_core::printable` (`markdown::render`, `message_view::render`, `splash`, `welcome`, approvals, the status bar, agent titles), so the model's conversation is never modified.
- **Martian timing.** Marv re-armed `setTimeout` after each render, so it drifted. `MartianAnimation` adds each step's ms to the previous deadline, so it doesn't drift, and it resyncs instead of replaying when woken more than one loop late.
- **Not in this crate:** `src/commands/*` (the engine runs them and sends the UI-side results) and Herdr reporting (marv-cli).
- **Two kept copies (#9).** marv-tui depends only on marv-core, so it can't call marv-session's `tool_draft` or marv-tools' `preview_lines`/`file_lines`. `thinking_view` keeps `draft_label`/`draft_preview` (no `regex`) and `diff_view` keeps `preview_lines`/`file_lines` over the view's `DiffLine`, each with the original's tests. To drop them: have the engine send the draft's label and preview in `UiEvent::ToolCallDelta`, and move the two diff helpers to marv-core.
- **Rendering differences from Ink (#9).** Markdown is parsed by pulldown-cmark, not marked: it has no GFM bare-URL autolinks, so `markdown::autolink` finds `http(s)://` URLs in text (`www.` and emails aren't linked). Wrapping follows wrap-ansi as Ink called it, except that the row after an exactly full row doesn't start with the separating space. Tabs are drawn as two spaces (a buffer cell is one column). The quote bar `▎` marks a quote's first row only, as Ink drew it. A diff line that serde can't read fails the whole message's parse (Marv filtered bad lines while drawing); the engine's session loader handles that.

## The shell (#10): measured CPU and known gaps

**CPU** (`examples/fake_session` in a pty via `script`, `top -b -n 3 -d 2 -p <pid>`, 120×40; the fake streams ~4 chars every 10 ms):

| build | idle (martian on screen) | streaming, first seconds | streaming, 20 s in |
|---|---|---|---|
| release | 0.0 / 0.0 / 0.0 % | 0.0 / 1.5 / 2.0 % | 5.0 / 1.5 / 1.0 % |
| debug | 0.0 / 0.5 / 0.0 % | 9.9 / 16.0 / 14.5 % | 9.9 / 14.0 / 14.5 % |

The streaming reply's finished lines are laid out once (only the unfinished last line is re-wrapped each frame); without that, debug climbed past 30% as the reply grew.

**Re-measured with #9's renderers** (same method; release): idle 0.0 / 0.0 / 0.0 %; streaming, first seconds 0.0 / 2.0 / 1.5 %; 20 s in 5.0 / 2.0 / 2.0 %. The stream lexer keeps each finished Markdown block's lines, so a frame parses and wraps only the unfinished end.

**No `terminal.clear()` at start:** it asks the terminal for the cursor position, and a terminal that never answers (a bare pty) would stop the TUI starting. The alternate screen starts blank.

**Gaps for the engine / other issues:**
- `UiEvent::ToolStart` carries no arguments, so a subagent's view can't show its task (Marv's first user message in `AgentLog`). The engine should send it (e.g. a `prompt` on `ToolStart` for `agent`, or a first `Subagent` event).
- Marv routes a pasted OAuth callback address (`isCallback`) to the pending login *before* the busy check and keeps it out of the prompt history. The App sends `/…` lines and messages as typed; the callback check needs a `SessionPort` hook (or the engine catches it in `send`), and the history should skip it.
- `marv -c`'s "Loading your last session…" (a message typed while the session loads waits) is the engine's to sequence: it should hold `send` until `Resumed`.
- Herdr state reporting (blocked / working / idle) is marv-cli's; the App exposes `busy()` and the approval queue but no hook yet.
- Engine messages get the App's own ids (`UiEvent::Message`'s id is replaced), so restored and new ids never collide.
