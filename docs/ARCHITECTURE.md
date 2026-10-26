# marv-rs architecture

marv-rs is a from-scratch Rust rebuild of [Marv](https://github.com/Flologix/marv), the TypeScript terminal coding agent. It keeps Marv's shape, its behaviour and its hard-won edge cases, and drops Bun and Ink. The TypeScript repo is the **spec**:
- its `CLAUDE.md`, one bullet per subsystem;
- its tests and its `evals/`.

`docs/PORTING.md` maps every TypeScript file to its Rust home.

## Goals, in order
1. **Safe by construction.** Every command runs in a sandbox, with protected paths, provenance and taint (`docs/SECURITY.md`). A single static binary: no runtime that reads a project's `bunfig.toml`/`.env`.
2. **Same behaviour as Marv.** This includes:
   - the cache-stable prompt and append-only history;
   - edit matching;
   - loop and cut-off guards;
   - MCP trust and OAuth;
   - headless exit codes.
   The oracle in `tests/oracle/` proves it.
3. **Light.** It should idle near 0% CPU and use tens of MB per session; Marv's Ink TUI uses 40–60% CPU per session (Marv #68).
4. **Sandboxed agents that can talk:** the mesh (`docs/MESH.md`). Simple first: one switchboard per Marv, explicit wiring.

## Crates

```
                 marv-core   (types only, no I/O)
        ┌────────────┼──────────────┬──────────────┐
  marv-provider  marv-sandbox ──────┤              │
        │         │      │          │              │
        │     marv-tools marv-mcp marv-mesh        │
        │         │      │          │              │
     marv-agent ──┘      │          │              │
        └────────────────┴── marv-session ─────────┘   (the engine: UI-free, like Marv's MarvSession)
                                  │
                              marv-tui                (ratatui + crossterm; the martian lives here)
                                  │
                              marv-cli                (the `marv` binary: TUI, `-p` headless, `mesh` commands)
```

| Crate | What it owns | Marv (TS) source |
|---|---|---|
| `marv-core` | `ChatTurn`, `ToolCall`, `ProviderEvent`, `Usage`, `ToolKind`, `Provenance`, `StopReason` | `src/provider/types.ts`, `src/types.ts` |
| `marv-fs` | Private (0600, atomic) writes and the cross-process file lock, below marv-mcp and marv-session | `src/private-file.ts`, `src/file-lock.ts` |
| `marv-provider` | `Provider` trait, OpenAI-compatible (OpenRouter) and Ollama adapters, SSE/NDJSON, retry, errors, model list, cache breakpoints | `src/provider/*` |
| `marv-agent` | The loop (`run_agent`), repetition guards, cut-off handling, compaction | `src/agent.ts`, `src/repetition.ts`, `src/compact.ts` |
| `marv-tools` | `Tool` trait, registry and `run_tool` (approval, yolo, repair), built-in tools, edit matching, syntax checks, diffs | `src/tools/*` |
| `marv-sandbox` | bubblewrap command builder, stand-in home, protected paths, taint state | `src/sandbox.ts`, `src/tools/files.ts` (path rules) |
| `marv-mcp` | MCP config, trust, connection manager, OAuth, deferred tools, `/mcp add` installers | `src/mcp/*` |
| `marv-mesh` | Switchboard, plugs and receptacles, grants, Herdr adapter, AgentMesh bridge | new (see `docs/MESH.md`), `src/herdr.ts` |
| `marv-session` | `MarvSession` engine, config, prompt, sessions on disk, trajectories, usage, memory, skills, agent types, subagents, worktrees, git, checks, diagnostics | `src/session.ts`, `src/sdk.ts`, `src/config/*`, `src/prompt.ts`, … |
| `marv-tui` | App state, transcript, Markdown, diff view, approvals, prompt input, pickers, status bar, splash and martian, mouse/selection/clipboard | `src/app.tsx`, `src/ui/*`, `src/mouse.ts`, `src/selection.ts`, `src/clipboard.ts` |
| `marv-cli` | Argument parsing, headless (`marv -p`), launching the TUI | `src/cli.tsx`, `src/headless.ts`, `bin/marv` |

## Runtime shape
- **One process for now.** The TUI and engine share a Tokio runtime. The engine (`marv-session`) never imports the TUI, so headless mode, the SDK and the oracle drive it directly.
- **The agent loop yields events**, as in Marv: a `Stream<Item = LoopEvent>`. The UI renders them; headless prints them.
- **Later, not now:** a daemon plus protocol, so many sessions share one process (jcode-style, about 3 MB per extra session). The engine and event types are already UI-free, so adding a socket protocol on top won't need a rewrite.

## Conventions
- **Comments explain *why*.** Port Marv's comments with the code: they record bugs that were really hit.
- **Every module starts with a `//!` header** saying what it owns and which Marv file it ports.
- `todo!("port: <what>")` marks unported bodies. `grep -rn 'todo!("port' crates` is the remaining work.
- **No `unsafe`** (workspace lint, `deny`), but for one audited function: `marv-cli`'s `signals::ignored`, a read-only `sigaction` query (#196), the only `allow(unsafe_code)` (a test checks).
- **Each crate's tests port the matching TS tests.** The oracle covers behaviour end to end.
