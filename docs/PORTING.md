# Porting plan

The skeleton maps the TypeScript Marv (`FlologixAI/marv` at `7df450f`, 0.3.0) onto the crates. Each module's `//!` header names the Marv file it ports. Each `docs/subsystems/<crate>.md` has the file-by-file table, status and port notes.

```sh
grep -rn 'todo!("port' crates | wc -l             # what's left
grep -rn 'todo!("port' crates/<crate>             # in one crate
```

## Status (skeleton, 2026-10-13)

| Crate | Doc | Pure logic already ported, with tests | Left (`todo!`) |
|---|---|---|---|
| marv-core | — | All: types, `printable`, `estimate_tokens` (#63: the one copy) | — |
| marv-fs | [fs.md](subsystems/fs.md) | All: `write_private`, `with_file_lock` (cross-process) | — |
| marv-provider | [provider.md](subsystems/provider.md) | All of Marv's provider: SSE/NDJSON, the OpenRouter and Ollama streaming loops with retries, `create_provider`, `ConfigFactory`, `list_models`, errors; cache breakpoints (#56), reasoning round-trip (#57), continue after a dropped stream and jitter (#64), context-overflow detection (#63) | — |
| marv-agent | [agent.md](subsystems/agent.md) | The loop (`run_agent`, all of `agent.test.ts`), repetition guards, parallel reads (#58), wrap-up at the step limit (#59), compaction v1, compaction v2 (#63: template, tail, rolling, overflow recovery; default v1 until the eval) | Continue after a dropped stream (#64) |
| marv-sandbox | [sandbox.md](subsystems/sandbox.md) | All: bwrap args, stand-in home, protected paths, taint, realpath containment; the Seatbelt backend on macOS (#164, #166) | — |
| marv-tools | [tools.md](subsystems/tools.md) | `run_tool` approval flow with taint and protected paths, edit matching with the unicode tier (#60), repair, diff, `read_file`, `skill`, the file and shell tools (`edit_file`, `write_file`, `glob`, `grep` on ripgrep's crates, `bash` with bwrap, `memory`, `syntax_error`, `list_project_files`), saved pages (`swap`) and `recall` (after #63) | — (`apply_patch` #66 and the formatter #67 are later Sprint 5 items) |
| marv-mcp | [mcp.md](subsystems/mcp.md) | Config, trust key, deferred tools, tool wrapping, JSON-RPC, install proposals | Manager lifecycle, OAuth flow, locked writes, package resolvers |
| marv-mesh | [mesh.md](subsystems/mesh.md) | Wire table (default deny), envelopes, limits, approvals parsing, admission and logging, Herdr reporter | Socket serving, routing, discovery, Herdr adapter, AgentMesh bridge |
| marv-session | [session.md](subsystems/session.md) | Config and presets, system prompt with AGENTS.md cut, session store, trajectories, usage, memory, skills/agent loaders (symlink-safe), frontmatter, slash commands, `SessionToolRunner` (always ≠ must_ask), fenced check notes, `run_turn`, compaction (v2, overflow recovery and saved pages, #63), saving, deferred MCP tools, the SDK's MCP manager (#11) | subagents and worktrees, `Checker::turn`, checks runner (`marv __run-checks`), mesh tool bodies |
| marv-tui | [tui.md](subsystems/tui.md) | The martian (byte-exact frames, deadline animation, widget), theme, splash, welcome, terminal guard, `SessionPort`/`UiEvent` boundary; the shell (#10): App loop, keys, mouse, selection + OSC 52 copy, approval queue, prompt input with history and `/` menu, session/model pickers, setup, status bar; rendering (#9): Markdown + stream lexer, message/diff/notice/thinking/draft views, transcript layout cache, scroll view with culling and glide, agent log and view | — (engine-side gaps: see tui.md) |
| marv-cli | [cli.md](subsystems/cli.md) | clap args, approvals list (incl. `mesh:<r>`), exit codes, headless collector, printable output | `tui::run` (the session ↔ TUI bridge), `mesh ls`/`mesh wire` |
| marv-guards (dev) | [guards.md](subsystems/guards.md) | One test per `docs/SECURITY.md` rule (#8), each proven by reverting its guard; real-bwrap tests; the post-command scan for created protected files | Ignored until their code lands: headless output (#11), web prefixes (#5), secret reads and masks, the network approval's script list, images |
| marv-oracle (dev) | [oracle.md](subsystems/oracle.md) | All: fake OpenAI-compatible server, scenario runner, normalization, canonical request diff, the prefix-rule check, the tracker MCP fixture; 28 scenarios recorded from TypeScript Marv (which passes them): all 17 eval tasks plus 11 loop edge cases | — |

Try the martian: `cargo run -p marv-tui --example martian` (q to quit). Try the whole shell on a scripted fake engine: `cargo run -p marv-tui --example fake_session`.

## Cross-crate API gaps to fix early
None open. Issue #1 closed the skeleton's: marv-fs (`private_file`, `file_lock`) below marv-mcp and marv-session, a self-synchronized `McpManager` (`Arc`, `&self`, `set_on_change`) and `Switchboard` (`serve`/`route` on `Arc<Self>`), an owning `list_models(kind, base_url, api_key)`, and `ToolRun::approval`.

## Cross-crate seams (by design)
- **The session ↔ TUI bridge:** marv-cli maps `SessionEvent` to `UiEvent` and implements `SessionPort` on `MarvSession`. See cli.md.
- **Draft labels:** `tool_draft` lives in marv-session; the TUI's draft view uses it through `UiEvent::ToolCallDelta`.

## Sprint 4 features (Marv 0.4.0): ported
Marv's Sprint 4 (FlologixAI/marv#71, 9bb4a34) is in, and the oracle is recorded from it (30/30):
- the plan tool (#15, marv-rs #21);
- checkpoints and `/undo` (#16, #22);
- the doors (#23, #24): the `github` tool (#47), host bash `sandbox: false` (#52) and the `process` tool (#51), with Marv's door warnings, `once` approvals and guards for each. They are tools as in Marv; exposing them as mesh receptacles (`github.read`, `process.<name>`) is later work.

## Build order (milestones)

Each milestone ends with something you can run.

1. **M1 Talk.** These calls do one real round-trip with OpenRouter:
   - `marv-provider` streaming loops;
   - `create_provider`;
   - the session config (presets, `resolveConfig`).

   Test it with a tiny `examples/chat.rs`.
2. **M2 Act.** Port the agent loop body, then the edit/write/glob/grep/bash `run` bodies, then `SessionToolRunner`. Then `marv -p "…"` with `--output text|json|stream-json` edits files, sandboxed, and the exit codes match Marv. ✅ Done (#3 loop, #4 tools, #11 session): `marv -p` edits files sandboxed with Marv's exit codes.
3. **M3 Oracle.** ✅ The harness and 30 scenarios (#7, `crates/marv-oracle`, `tests/oracle/`). **marv-rs passes 30/30**, and so does TypeScript Marv against its own recordings. Deliberate marv-rs differences are declared per scenario (oracle.md): #64 dropped-stream continue, #58 parallel reads, and fenced MCP results and check notes (SECURITY.md). **Everything after this is checked against the oracle.**
4. **M4 Guards.** ✅ One test per `docs/SECURITY.md` rule in `tests/guards/` (#8, guards.md), each proven by reverting its guard, plus the post-command scan for protected files a command creates (sandbox.md). Rules whose code isn't written yet have ignored guards that name what they need.
5. **M5 TUI.** Transcript, prompt input, approvals, diff view, status bar, pickers and the martian. **Measure CPU while idle and while streaming:** the target is under 2% idle and under 15% streaming (Marv #68). Shell (#10), `examples/fake_session` in a pty, `top -b -n 3 -d 2`: release idle 0.0%, streaming 1–5%; debug idle ≤0.5%, streaming 10–16%. Re-measure once #9's renderers replace the stand-ins. With #9's renderers: release idle 0.0%, streaming 0–5%.
6. **M6 Context.** Compaction v1, then v2 (#63), plus sessions on disk, resume and trajectories.
7. **M7 MCP.** Manager lifecycle, OAuth, deferred tools, `/mcp add`. ✅ Done (#6).
8. **M8 Mesh v1.** One Marv and its subagents over the switchboard socket, with wired inboxes. ✅ Done (#15): per-plug Unix sockets, wired inboxes, a plug per sandboxed subagent, wire approvals in the TUI.
9. **M9 Mesh v2.** Several Marvs on one machine: discovery, cross-process wires, the Herdr adapter. ✅ Done (#16): discovery sockets, `marv mesh ls|wire|plug`, cross-process wires approved on both ends, the Herdr pane adapter, an MCP shim for other agents.
10. **M10 Release.** One static binary (musl), install script, and a `0.1.0` tag. Workflow and `install.sh` done (#19); no tag yet.

Later:
- the mesh radio (#124, `docs/specs/2026-10-14-mesh-radio.md`): phase 1, presence, ✅ done; phase 2, messages (`/tx`, `radio_tx`, radio wakes), ✅ done; phase 3, requests (`/ask`, `radio_tx kind: request|reply`, automatic acks and reports, hops, no pane send to a Marv on the radio), ✅ done; phase 4 (non-Marv agents) later;
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
- **Situational prompt:** a marv-rs `# Your situation` section (what the model can't see or do, short replies). The oracle skips it when it compares requests.
- **TUI streaming:** text is revealed at a pace (a burst drains over ~120 ms) instead of in 33 ms clumps, and the reasoning preview is wrapped with a fixed height.
- **Serialization:**
  - schema keys are sorted. This is still byte-stable, but differs from Marv's key order; the oracle compares requests with TypeScript Marv's as canonical JSON and checks only each run's own requests byte for byte (the prefix rule), so this passes without `preserve_order` (oracle.md);
  - lengths are counted in `char`s.
- **MCP trust:** the key is canonical JSON, so project servers need trusting once more.
