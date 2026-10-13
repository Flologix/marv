# Porting plan

The skeleton maps the TypeScript Marv (`FlologixAI/marv` at `7df450f`, 0.3.0) onto the crates. Each module's `//!` header names the Marv file it ports. Each `docs/subsystems/<crate>.md` has the file-by-file table, status and port notes.

```sh
grep -rn 'todo!("port' crates | wc -l             # what's left
grep -rn 'todo!("port' crates/<crate>             # in one crate
```

## Status (skeleton, 2026-10-13)

| Crate | Doc | Pure logic already ported, with tests | Left (`todo!`) |
|---|---|---|---|
| marv-core | — | All: types, `printable` | — |
| marv-fs | [fs.md](subsystems/fs.md) | All: `write_private`, `with_file_lock` (cross-process) | — |
| marv-provider | [provider.md](subsystems/provider.md) | SSE/NDJSON, retry helpers, errors, model list, request bodies, cache breakpoints (#56), reasoning round-trip (#57) | HTTP streaming loops, `create_provider`, model lookup |
| marv-agent | [agent.md](subsystems/agent.md) | Repetition guards, per-step stream fold, call grouping for parallel reads (#58), compaction v1 | The loop body (`LoopState::advance`), compaction v2 (#63) |
| marv-sandbox | [sandbox.md](subsystems/sandbox.md) | All: bwrap args, stand-in home, protected paths, taint, realpath containment | — |
| marv-tools | [tools.md](subsystems/tools.md) | `run_tool` approval flow with taint and protected paths, edit matching, repair, diff, `read_file`, `skill` | `run` for edit/write/glob/grep/bash, web_fetch, unicode tier (#60), spill (#61), `apply_patch` (#66) |
| marv-mcp | [mcp.md](subsystems/mcp.md) | Config, trust key, deferred tools, tool wrapping, JSON-RPC, install proposals | Manager lifecycle, OAuth flow, locked writes, package resolvers |
| marv-mesh | [mesh.md](subsystems/mesh.md) | Wire table (default deny), envelopes, limits, approvals parsing, admission and logging, Herdr reporter | Socket serving, routing, discovery, Herdr adapter, AgentMesh bridge |
| marv-session | [session.md](subsystems/session.md) | Config and presets, system prompt with AGENTS.md cut, session store, trajectories, usage, memory, skills/agent loaders (symlink-safe), frontmatter, slash commands, `SessionToolRunner` (always ≠ must_ask), fenced check notes | `run_turn`, compaction call, subagents and worktrees, checks runner (`marv __run-checks`), MCP/mesh tool bodies |
| marv-tui | [tui.md](subsystems/tui.md) | The martian (byte-exact frames, deadline animation, widget), theme, splash, welcome, terminal guard, `SessionPort`/`UiEvent` boundary | App loop and drawing, Markdown, transcript, approvals, prompt input, pickers, selection, clipboard |
| marv-cli | [cli.md](subsystems/cli.md) | clap args, approvals list (incl. `mesh:<r>`), exit codes, headless collector, printable output | `tui::run` (the session ↔ TUI bridge), `mesh ls`/`mesh wire` |
| marv-oracle (dev) | [oracle.md](subsystems/oracle.md) | All: fake OpenAI-compatible server, scenario runner, normalization, canonical request diff, the prefix-rule check, the tracker MCP fixture; 23 scenarios recorded from TypeScript Marv (which passes them) | Five more eval tasks, the `SECURITY.md` guards |

Try the martian: `cargo run -p marv-tui --example martian` (q to quit).

## Cross-crate API gaps to fix early
None open. Issue #1 closed the skeleton's: marv-fs (`private_file`, `file_lock`) below marv-mcp and marv-session, a self-synchronized `McpManager` (`Arc`, `&self`, `set_on_change`) and `Switchboard` (`serve`/`route` on `Arc<Self>`), an owning `list_models(kind, base_url, api_key)`, and `ToolRun::approval`.

## Cross-crate seams (by design)
- **The session ↔ TUI bridge:** marv-cli maps `SessionEvent` to `UiEvent` and implements `SessionPort` on `MarvSession`. See cli.md.
- **Draft labels:** `tool_draft` lives in marv-session; the TUI's draft view uses it through `UiEvent::ToolCallDelta`.

## Sprint 4 features (Marv 0.4.0) still to add to the skeleton
These features merge in Marv after this skeleton was cut at 0.3.0:
- the plan tool (#15);
- checkpoints and `/undo` (#16);
- the doors: `github` tool (#47), background processes (#51), `sandbox: false` (#52).

Add modules for them once Marv's sprint-4 PR merges. In marv-rs, the doors become mesh receptacles (`github.read`, `process.<name>`).

## Build order (milestones)

Each milestone ends with something you can run.

1. **M1 Talk.** These calls do one real round-trip with OpenRouter:
   - `marv-provider` streaming loops;
   - `create_provider`;
   - the session config (presets, `resolveConfig`).

   Test it with a tiny `examples/chat.rs`.
2. **M2 Act.** Port the agent loop body, then the edit/write/glob/grep/bash `run` bodies, then `SessionToolRunner`. Then `marv -p "…"` with `--output text|json|stream-json` edits files, sandboxed, and the exit codes match Marv.
3. **M3 Oracle.** The harness and the first 23 scenarios, recorded from TypeScript Marv's evals plus the loop edge cases, are done (#7: `crates/marv-oracle`, `tests/oracle/`, see its README). Left: make them pass once `marv -p` works (#11), and add more. **Everything after this is checked against the oracle.**
4. **M4 Guards.** One test per `docs/SECURITY.md` rule in `tests/guards/`, each proven by reverting its guard. Includes the placeholder or post-command scan for protected files a command creates (see sandbox.md).
5. **M5 TUI.** Transcript, prompt input, approvals, diff view, status bar, pickers and the martian. **Measure CPU while idle and while streaming:** the target is under 2% idle and under 15% streaming (Marv #68).
6. **M6 Context.** Compaction v1, then v2 (#63), plus sessions on disk, resume and trajectories.
7. **M7 MCP.** Manager lifecycle, OAuth, deferred tools, `/mcp add`.
8. **M8 Mesh v1.** One Marv and its subagents over the switchboard socket, with wired inboxes.
9. **M9 Mesh v2.** Several Marvs on one machine: discovery, cross-process wires, the Herdr adapter.
10. **M10 Release.** One static binary (musl), install script, and a `0.1.0` tag.

Later:
- mesh v3 (AgentMesh bridge);
- the Sprint 5 items not yet in: formatter (#67), schema repair (#65), more instruction files (#62);
- LSP diagnostics;
- the daemon/protocol split.

## Deliberate differences from Marv
Each one is recorded in the module doc that makes it:
- **Security fixes:**
  - protected paths;
  - taint and `must_ask`;
  - realpath containment for skills;
  - fenced project-check text;
  - read-only MCP tools become Egress when tainted;
  - the trust hash covers the files a server's command runs;
  - `printable()` in headless output;
  - no runtime reading the project's bunfig/.env.
- **Sprint 5 items built in from the start:** #56, #57, #58, #59, #64, plus the #63 structure.
- **Serialization:**
  - schema keys are sorted. This is still byte-stable, but differs from Marv's key order; the oracle compares requests with TypeScript Marv's as canonical JSON and checks only each run's own requests byte for byte (the prefix rule), so this passes without `preserve_order` (oracle.md);
  - lengths are counted in `char`s.
- **MCP trust:** the key is canonical JSON, so project servers need trusting once more.
