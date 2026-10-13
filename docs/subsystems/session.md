# marv-session

The UI-free engine: Marv's `MarvSession` and everything around it. It covers config, the system prompt, sessions on disk, trajectories, usage, memory, skills, agent types, subagents, worktrees, git, project checks, diagnostics, slash commands (the engine side) and the SDK. The TUI, headless `marv -p` and the SDK all drive it. It never imports the TUI.

Spec: the **Config**, **Session**, **SDK**, **Skills**, **Memory**, **Subagents**, **Worktrees**, **Diagnostics**, **Project checks**, **Slash commands**, **Tokens and cost**, **Project keys**, **Private files**, **Sessions**, **Trajectories** and **Compaction** bullets in Marv's `CLAUDE.md`, and the matching `tests/*.test.ts`.

## Modules

| Marv (TS) | Rust module | Status |
|---|---|---|
| `src/session.ts` | `session.rs` | **Types and API are done:** `SessionEvent`, `TurnStatus`, `TurnEndReason`, `CompactResult`, `SessionInit`, `Configure`, `SendOptions`, `Turn` (a `Stream`; dropping it interrupts), `SessionError`, `check_tool_names`. **Ported and tested:** the constructor, the getters, `send` (starts the turn task), `interrupt`, `clear`, `resume`, `configure` (all or nothing), `rate`, `save`/`snapshot` (`answer_all_calls`), `flush`, `close`, `build_system`, `lookup`, `count`, `nearly_full`, `run_turn` (#11: MCP wait, the turn's settings, trajectory and implicit feedback, compaction before and between steps, the deferred-MCP note and rewrite, the checks hooks, the step-limit question, the subagent sink, `Tainted`), `compact`/`summarize_into`, `save_now`/`schedule_save`/`flush` (writes for a conversation left behind are awaited). Tests: the `session.test.ts` cases whose collaborators exist (`turn_tests`). **Not yet testable:** subagents, worktrees, the MCP wait and deferred mode (needs `McpManager::start`), diagnostics (needs `Checker::turn`) |
| `src/sdk.ts` | `sdk.rs` | **Ported:** `SessionBuilder`, `SessionOptions`, `Store`, `no_yolo_reason`, `create_session` (with the `McpManager`: trust file, logins, the needs-login warning once per server, started in the background), checks tested |
| `src/config/config.ts` (+ `providerFactory` from `src/provider/factory.ts`) | `config.rs` | **Ported and tested:** `preset` (Marv's `PRESETS`), `FileConfig::from_value`/`to_value`, `resolve_config`, `max_steps_from_env`, `ollama_url`, `needs_setup`, `mask_key`, `default_config_dir`, `ConfigStore` (0600), `migrate_legacy_config`, `provider_factory`. The SDK factory's model list is `marv_provider::models::list_models` (#2) |
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
| `src/file-lock.ts`, `src/private-file.ts` | — | Moved to marv-fs (`docs/subsystems/fs.md`): marv-mcp needs them too |
| `src/paths.ts` | `paths.rs` | **Ported and tested** (`project_key`, `legacy_project_key`, `shorten_home`, `home_or_above`) |
| `src/sources.ts` | `sources.rs` | **Ported** (`load_sources`) |
| `src/event-queue.ts` | `event_queue.rs` | **Ported and tested** (an unbounded channel with one reader) |
| `src/frontmatter.ts` | `frontmatter.rs` | **Ported and tested** (`serde_norway`) |
| `src/commands/index.ts`, `sandbox-notice.ts` | `commands.rs` | **Ported and tested:** every command, `run_command`, `help_text`, the `/config`, `/checks`, `/mcp`, `/skills` and `/agents` texts, and the status lines |
| `src/tool-draft.ts` | `tool_draft.rs` | **Ported and tested** |
| `src/types.ts` | `types.rs` | **Done** (`Message`, `Role`, `ToolStatus`, `Notice`): the transcript is saved with sessions |
| (new) | `runner.rs` | **Done and tested:** `SessionToolRunner`, `tool_run_from`, `ToolDisplay`, `approval_record` (`ApprovalRecord` lives in marv-agent, re-exported), `SessionApprover`, `AlwaysScopes`, `step_limit_request`, `SessionAgentHost` (except `preview`) |
| (new) | `mcp_tools.rs` | **Done:** `McpToolAdapter` (its kind follows the taint), `ToolSearchTool` and `McpCallTool` (`run_tool` with the real tool, the closest names, the schema once, the #10 string-arguments repair). Tested without a connected server |
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
// ToolRun::approval is an ApprovalRecord; ToolRun::display downcasts to marv_session::ToolDisplay { diff: Option<DiffShown> }.
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
- **A panicking provider or tool** (Marv: one that throws) is caught around the turn's body (`catch_unwind`): the turn ends with `Error: <message>` and `TurnEnd`, and the session lives on.
- **`working` is a token** cancelled when the turn's task (or `compact()`) has fully ended; `close()` waits on it. `compact()` frees the session from a drop guard, so a caller dropping its future can't leave it busy.
- **Mesh:** the main agent's mesh tools are offered on a plug named `main` when the session has a switchboard; draining the inbox between steps waits for mesh v1 (`Switchboard::drain_inbox`).
- **Trajectory usage** is written with Marv's camelCase field names (`UsageRecord`). `marv_core::Usage` serializes in snake_case, so it isn't used directly.

## marv-rs behaviour changes

- **Fenced project text (fixes a Marv 0.3.0 bug).** The typecheck and project-check notes are now an automatic note in Marv's own words, followed by the project's output inside `<untrusted-data source="project">`. Before, they began `Marv:` and quoted project text verbatim. Results from web, MCP and image tools are fenced the same way in `tool_run_from`. Mesh messages arrive already fenced.
- **"Always" doesn't answer `must_ask` requests** (tainted egress, protected paths): `SessionApprover`. Guard test: `runner::tests::must_ask_requests_are_never_answered_from_always`.
- **Subagents don't inherit network, web, MCP or mesh approvals** (`AlwaysScopes::for_subagent`). Their taint is `TaintState::for_subagent()`. In a worktree, `SubagentApprover` never auto-approves `must_ask`.
- **The config dir comes only from the real environment.** The CLI builds `Env` from `std::env::vars()`. No project file is ever consulted.
- **Skills, agent files and checks are read only after a realpath containment check** (`marv_sandbox::resolve_within`, or `symlink_metadata` for checks). A symlink out of the project is refused and reported.
- **New events and records:** `SessionEvent::Tainted`, plus the `taint` and `mesh` trajectory records.

## Upstream gaps (other crates)

The oracle passes 25 of 28 against marv-rs (#11). What's left elsewhere:

- **marv-mcp:** `McpManager::start`/`close` are todo (the three MCP scenarios panic), and a new manager reports `settled()` before `start()` ran, so `WaitingForMcp` would be skipped (#6).
- **marv-session (other modules):** `Checker::turn` and `combine_checks` for two kinds are todo (a turn with diagnostics or checks on panics into an `Error`), and so are `run_subagent` and `SessionAgentHost::preview` (the agent tool).
- **marv-agent:** nothing fences tool output by provenance in the loop; the session does it in `tool_run_from`. After `on_full` (or anything else) cancels a turn, the loop checks the stop only after `after_step`, so the session always gives it one (a no-op without checks); otherwise it would send one more request.
- **marv-tools:** `memory::{add,find,remove}_memory` are todo.
