# marv-session

The UI-free engine: Marv's `MarvSession` and everything around it. It covers config, the system prompt, sessions on disk, trajectories, usage, memory, skills, agent types, subagents, worktrees, git, project checks, diagnostics, slash commands (the engine side) and the SDK. The TUI, headless `marv -p` and the SDK all drive it. It never imports the TUI.

Spec: the **Config**, **Session**, **SDK**, **Skills**, **Memory**, **Subagents**, **Worktrees**, **Diagnostics**, **Project checks**, **Slash commands**, **Tokens and cost**, **Project keys**, **Private files**, **Sessions**, **Trajectories** and **Compaction** bullets in Marv's `CLAUDE.md`, and the matching `tests/*.test.ts`.

## Modules

| Marv (TS) | Rust module | Status |
|---|---|---|
| `src/session.ts` | `session.rs` | **Types and API are done:** `SessionEvent`, `TurnStatus`, `TurnEndReason`, `CompactResult`, `SessionInit`, `Configure`, `SendOptions`, `Turn` (a `Stream`; dropping it interrupts), `SessionError`, `check_tool_names`. **Ported and tested:** the constructor, the getters, `send` (starts the turn task), `interrupt`, `clear`, `resume`, `configure` (all or nothing), `rate`, `save`/`snapshot` (`answer_all_calls`), `flush`, `close`, `build_system`, `lookup`, `count`, `nearly_full`. **Todo:** `run_turn` (its order is documented on the function), `compact`, `save_now` |
| `src/sdk.ts` | `sdk.rs` | `SessionBuilder`, `SessionOptions`, `Store`, `no_yolo_reason` and `create_session` are ported, with checks tested. Todo: making and starting the `McpManager` |
| `src/config/config.ts` (+ `providerFactory` from `src/provider/factory.ts`) | `config.rs` | **Ported and tested:** `preset` (Marv's `PRESETS`), `FileConfig::from_value`/`to_value`, `resolve_config`, `max_steps_from_env`, `ollama_url`, `needs_setup`, `mask_key`, `default_config_dir`, `ConfigStore` (0600), `migrate_legacy_config`, `provider_factory`. Todo: the SDK factory's model list (`load`) |
| `src/prompt.ts` | `prompt.rs` | **Ported and tested:** `system_prompt`, `subagent_prompt`, `cut_instructions`/`read_instructions` (the 20k cut, headings after the cut, fences skipped), `instructions_cut_notice`, `instructions_cut_item`. Plan #62 `instruction_files` is a stub |
| `src/sessions.ts` | `sessions.rs` | **Ported and tested:** `SavedSession` (Marv's field names), `new_session`, `title_of`, `time_ago`, `SessionStore` (save, load, list, latest, the legacy folder) |
| `src/trajectory.ts` | `trajectory.rs` | **Ported and tested:** `TrajectoryRecord` (Marv's JSON shapes, plus `mesh` and `taint`), `TrajectoryStore`, `Trajectory` (an ordered writer thread, fresh line after a crash, the first error reported once), `AgentRecorder`, `MeshSink` |
| `src/trajectory-stats.ts` | `trajectory_stats.rs` | **Ported and tested** (`summarize`, `format_stats`) |
| `src/usage.ts` | `usage.rs` | **Ported and tested** (`Totals`, `add_usage`, `estimate_cost`, `tokens`, `cost_text`, `usage_report`) |
| `src/memory.ts` | `memory.rs` | **Ported and tested:** paths, `entries_of`, `load_memory` (moves the legacy file), `memory_section`. `add/find/remove_memory` are re-exported from `marv_tools::memory`, where they're still todo |
| `src/skills.ts` | `skills.rs` | **Ported and tested** (`load_skills`, `read_skill`, `skill_message`), with the realpath fix |
| `src/agents.ts` | `agents.rs` | **Ported and tested** (`load_agents`, `parse_agent`, `parse_tools`, `find_agent`, `general_purpose`) |
| `src/subagent.ts` | `subagent.rs` | **Ported and tested:** `stopped_text`, `resolve_type`, `SubagentApprover`, `subagent_context`, `subagent_report`. **Todo:** `run_subagent` |
| `src/worktree.ts` | `worktree.rs` | `Worktree`, `worktree_env` and `branch_name` are ported and tested. **Todo:** `inspect_repo`, `create_worktree`, `finish_worktree` |
| `src/git.ts` | `git.rs` | **Ported and tested** (`run_git`, async with a timeout; `git_environment`; `git_head`) |
| `src/checks/load.ts` | `checks/load.rs` | **Ported and tested:** `parse_pattern_check`, `parse_script_check`, `split_patterns`, `load_checks`. **Todo:** `concerns` (glob matching) |
| `src/checks/kind.ts` | `checks/kind.rs` | **Ported and tested:** `parse_runner`, `violation_key`, `format_violation`, `checks_note` (fenced), `project_check_notice`, `ProjectChecksKind` (except `pass`). **Todo:** `runner_command` (needs a redesign) |
| `src/checks/runner.ts` | `checks/runner.rs` | Types only. Todo: `run_checks` |
| `src/diagnostics/tsc.ts` | `diagnostics/tsc.rs` | **Ported and tested** (`parse_tsc`, `new_errors` via `new_problems`, `format_error`, `missing_interpreter`, `diagnostics_note` (fenced), `detect_checker`, `check_command`) |
| `src/diagnostics/turn.ts` | `diagnostics/turn.rs` | **Ported:** `CheckOutcome`, `CheckKind`, `Typecheck`, `new_problems`, `changed_files`, `TurnChecks`, `Checker::reset`, `check_notice`. **Todo:** `Checker::turn`, and `combine_checks` for two or more kinds |
| `src/feedback.ts` | `feedback.rs` | **Ported and tested** (`classify_reply`; the lookarounds are emulated) |
| `src/file-lock.ts` | `file_lock.rs` | Types only. Todo: `with_file_lock` |
| `src/private-file.ts` | `private_file.rs` | **Ported and tested** (`write_private`: 0600 in a 0700 folder, unique temp file, queued per path) |
| `src/paths.ts` | `paths.rs` | **Ported and tested** (`project_key`, `legacy_project_key`, `shorten_home`, `home_or_above`) |
| `src/sources.ts` | `sources.rs` | **Ported** (`load_sources`) |
| `src/event-queue.ts` | `event_queue.rs` | **Ported and tested** (an unbounded channel with one reader) |
| `src/frontmatter.ts` | `frontmatter.rs` | **Ported and tested** (`serde_norway`) |
| `src/commands/index.ts`, `sandbox-notice.ts` | `commands.rs` | **Ported and tested:** every command, `run_command`, `help_text`, the `/config`, `/checks`, `/mcp`, `/skills` and `/agents` texts, and the status lines |
| `src/tool-draft.ts` | `tool_draft.rs` | **Ported and tested** |
| `src/types.ts` | `types.rs` | **Done** (`Message`, `Role`, `ToolStatus`, `Notice`): the transcript is saved with sessions |
| (new) | `runner.rs` | **Done and tested:** `SessionToolRunner`, `tool_run_from`, `ToolDisplay`, `ApprovalRecord`, `SessionApprover`, `AlwaysScopes`, `step_limit_request`, `SessionAgentHost` (except `preview`) |
| (new) | `mcp_tools.rs` | `McpToolAdapter` is done (its kind follows the taint). `ToolSearchTool` and `McpCallTool` are skeletons with `run` todo |
| (new) | `mesh.rs` | `MeshHandle`, `WireApprover`, `headless_grants` (tested), and `MeshTool` with `run` todo |
| (new) | `provenance.rs` | **Done and tested:** `fence`, `project_note` |
| (new) | `time.rs` | **Done and tested:** `now_ms`, `iso`, `iso_date` |

`grep -rn 'todo!("port' crates/marv-session` lists what's left.

## The API clients use

```rust
// Making one: the TUI builds a SessionInit itself; the SDK and `marv -p` use the builder.
SessionInit::new(root, ProviderChoice::Factory(factory) | ProviderChoice::Option(option)) // then set fields
MarvSession::new(init) -> Result<MarvSession, SessionError>         // Clone (an Arc inside)
SessionBuilder::new(cwd, ProviderOption).sources(&[..]).approve(a).yolo(b).persist(Store::Default)
    .trajectories(..).resume(ResumeTarget::Latest).max_steps(n).build().await -> Result<MarvSession, SessionError>

// Turns. send() must run inside a Tokio runtime. A Turn is a Stream; dropping it interrupts the turn.
session.send(&str, SendOptions { for_model, cancel }) -> Result<Turn, SessionError>
turn.next().await -> Option<SessionEvent>;  turn.interrupt();  turn.finish().await  // interrupt + wait for cleanup
session.interrupt()

pub enum SessionEvent {
    TurnStart { turn: String },                          // always first
    Status(TurnStatus),                                  // WaitingForMcp | Compacting | Checking | Running
    Loop(marv_agent::LoopEvent),                         // the main agent
    Subagent { call_id: String, event: LoopEvent },
    SubagentProgress { call_id: String, progress: marv_tools::AgentProgress },
    Compaction(CompactResult),                           // Compacted { summary, tokens_before } | NotCompacted { reason, error }
    Check(CheckResult),                                  // the typecheck: CheckOutcome<TsError>
    Checks(ProjectCheckResult),                          // project checks: CheckOutcome<Violation>
    Tainted { source: Provenance },                      // marv-rs: show it in the status bar
    TurnEnd { turn: String, reason: TurnEndReason },     // always last; Done(StopReason) | Interrupted
}
// ToolRun::display downcasts to marv_session::ToolDisplay { diff: Option<DiffShown>, approval: ApprovalRecord }.
// Notices: diagnostics::check_notice(&CheckResult), checks::project_check_notice(&ProjectCheckResult) (both printable).

// Between turns
session.compact(Option<&str>).await -> Result<CompactResult, SessionError>
session.clear().await -> Result<(), SessionError>
session.resume(ResumeTarget::Latest | ResumeTarget::Id(String)).await -> Result<Option<Resumed>, SessionError>
session.configure(Configure { provider, thinking, sandbox, yolo, trajectories, diagnostics, checks, max_steps, mcp_tools }) -> Result<(), SessionError>
session.rate(Feedback { score, note, labels }) -> RateOutcome
session.save().await -> Result<(), SessionError>;  session.flush().await;  session.close().await

// Reads
session.id() -> String; busy() -> bool; provider_name() -> String; context_length() -> Option<u64>
model_info() -> Option<ModelInfo>; memory() -> Option<Memories>; problems() -> &[String]
usage() -> UsageSnapshot { last, totals, context_length }; tainted() -> Vec<Provenance>
mcp() -> Option<McpHandle>; mesh() -> Option<MeshHandle>; system_prompt() -> String; conversation() -> Vec<ChatTurn>

// Commands (pure): commands::run_command(line, &CommandContext) -> CommandAction; commands::commands() for the / menu
```

Approvals: the client supplies an `Arc<dyn marv_tools::Approver>` (`SessionInit::approve`). The TUI answers it from its approval queue. The session wraps it in `SessionApprover`.

## Port notes

- **Shared state.** `MarvSession` is an `Arc` around one `Mutex<State>`. The lock is never held across an `.await`. The conversation is a `marv_agent::History` (`Arc<Mutex<Vec<ChatTurn>>>`), so a mid-turn compaction swaps it in place, as Marv's `splice` did.
- **A turn runs on its own Tokio task** and pushes into an `EventQueue`. `Turn` reads the queue. Dropping a `Turn` cancels its token, which is Marv's `break`. To wait for the cleanup, use `Turn::finish().await`: `Drop` can't await.
- **Lengths count `char`s** (Marv counted UTF-16 units). This affects the AGENTS.md cut, titles and the 500-character memory entries.
- **Token formatting** (`usage::tokens`) uses integer half-up rounding, to match JavaScript's `toFixed` on ties.
- **Frontmatter YAML** uses `serde_norway`. The YAML error wording differs from Bun's.
- **Project-check patterns** are JavaScript regexes that run in the sandbox. They aren't compiled in Marv's process to validate them (Rust's regex dialect differs), so a bad pattern shows up as that check's `failed` violation.
- **The check runner needs a redesign.** Marv sent TypeScript source to `bun -e` inside the sandbox. marv-rs has no TypeScript of its own. Plan: bind-mount the `marv` binary read-only and run a hidden `marv __run-checks`, which takes the spec on stdin and matches the patterns itself. `.ts` scripts still run with the project's bun.
- **Trajectory usage** is written with Marv's camelCase field names (`UsageRecord`). `marv_core::Usage` serializes in snake_case, so it isn't used directly.

## marv-rs behaviour changes

- **Fenced project text (fixes a Marv 0.3.0 bug).** The typecheck and project-check notes are now an automatic note in Marv's own words, followed by the project's output inside `<untrusted-data source="project">`. Before, they began `Marv:` and quoted project text verbatim. Results from web, MCP and image tools are fenced the same way in `tool_run_from`. Mesh messages arrive already fenced.
- **"Always" doesn't answer `must_ask` requests** (tainted egress, protected paths): `SessionApprover`. Guard test: `runner::tests::must_ask_requests_are_never_answered_from_always`.
- **Subagents don't inherit network, web, MCP or mesh approvals** (`AlwaysScopes::for_subagent`). Their taint is `TaintState::for_subagent()`. In a worktree, `SubagentApprover` never auto-approves `must_ask`.
- **The config dir comes only from the real environment.** The CLI builds `Env` from `std::env::vars()`. No project file is ever consulted.
- **Skills, agent files and checks are read only after a realpath containment check** (`marv_sandbox::resolve_within`, or `symlink_metadata` for checks). A symlink out of the project is refused and reported.
- **New events and records:** `SessionEvent::Tainted`, plus the `taint` and `mesh` trajectory records.

## Upstream gaps (other crates)

- **marv-mcp:** `McpManager`'s lifecycle methods take `&mut self` and `ready()` is async. The session shares it as `Arc<tokio::sync::RwLock<McpManager>>` (`McpHandle`), so `/mcp status` waits while a turn waits on `ready()`. An internally synchronized manager (`&self` methods) would fix that. There is also no `onChange` hook yet, which the SDK's "needs a login" warnings need.
- **marv-mesh:** `Switchboard::serve(&mut self)` holds the switchboard for the whole socket loop, and `route` takes `&mut self` too. `serve` should take `Arc<Self>` with interior mutability.
- **marv-provider:** `models::list_models` needs a `reqwest::Client` from the caller. `ConfigFactory::load` (the SDK path) can't make one without a reqwest dependency in marv-session. Suggest `list_models(kind, base_url)` with the provider crate owning the client.
- **marv-agent:** `ToolRun` has no `approval` field. The session carries it in `display` (`ToolDisplay`) so trajectories and headless can read it. A first-class `approval: Option<…>` would be cleaner. Nothing fences tool output by provenance in the loop; the session does it in `tool_run_from`.
- **marv-tools:** `web_fetch::pasted_scopes` and `memory::{add,find,remove}_memory` are todo, and `send()` calls `pasted_scopes`.
- **Shared file helpers:** `private_file` and `file_lock` are needed by marv-mcp (trust, token store, personal file), which can't depend on marv-session. They belong in a small crate below both (or in marv-sandbox).
