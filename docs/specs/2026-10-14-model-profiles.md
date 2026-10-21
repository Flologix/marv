# Model profiles: apply_patch for GPT models, per-family prompt addenda

- Issue: FlologixAI/marv #66
- Status: **approved 2026-10-20 with amendments.** The amendments are folded into the text below; the decisions are in §9, the PRs in §8.
- Date: 2026-10-14 (draft), revised against `main` 2026-10-20 (after #147, #143, #67 and the 0.7.0 home-folder rules)
- Scope:
  - `crates/marv-provider`: the profile table;
  - `crates/marv-tools`: `apply_patch`, registry;
  - `crates/marv-session`: prompt, offered tools, subagents, config, saved sessions.

The issue was written against the TypeScript build. This spec is grounded in the Rust code on `main`. There is no local opencode checkout (only the `opencode` binary under mise), so opencode's behaviour (`packages/core/src/plugin/patch.ts`, `optimize.ts`, `system-prompt/`) is taken from the issue text and marked as such.

## 1. Problem and evidence

Every model gets the same system prompt and the same edit tools: `edit_file` (exact `old_string` → `new_string`) and `write_file`. Model families are trained on different editing interfaces:
- OpenAI's GPT-5/Codex line: the `*** Begin Patch` format (Codex's `apply_patch`);
- Claude: `str_replace`-style edits.

opencode, per the issue, gives GPT models (other than gpt-4 and `oss`) a patch tool **instead of** edit/write, and has per-family prompt variants.

**Evidence:** per-model trajectory stats and the editing eval tasks are where Marv can see this. This spec doesn't cite a GPT failure rate: none is in the issue, and no runs were made for this spec. The design is set up so that every change is measured (§7), and an addendum that shows no eval gain is removed.

## 2. Current Rust design

| What | Where |
|---|---|
| The built-in tool list, in a fixed order (`read_file`, `glob`, `grep`, `web_fetch`, `github`, `skill`, `edit_file`, `write_file`, `bash`, `process`, `memory`, `plan`, `agent`) | `crates/marv-tools/src/registry.rs::builtin_tools` |
| Session tool specs: `tool_specs_for(has_skills, plan, github)` (minus `process` without its door), then the caller's tools, then `herdr` (the Herdr door, #74), then the mesh's `mesh_list`, `mesh_call`, `mesh_send`; built **once** in `MarvSession::new` and held in `Inner::specs` (immutable). `offered()` adds `radio_tx` (#124) after the mesh tools in a conversation that started with the radio on, then the MCP specs | `registry.rs`, `crates/marv-session/src/session.rs` (`MarvSession::new`, `offered`) |
| Callable tools filtered the same way: the model can't call a tool that isn't offered | `session.rs::offered` |
| System prompt: `system_prompt(&PromptInput)`. `BASE_REST_TEXT` names `edit_file` and `write_file` in fixed text. The `# Fixing bugs` section (`FIXING_BUGS`, #147) is added only when `input.tools` contains `edit_file`; `# Your situation` (`situation_section`, with `HERDR_SITUATION`/`HERDR_SITUATION_READS` in Herdr) follows. Built by `MarvSession::build_system`, rebuilt only by `clear()` | `crates/marv-session/src/prompt.rs` (`system_prompt`, the `fixing` line), `session.rs` |
| Subagents: `SUBAGENT_TOOLS` includes `edit_file` and `write_file`; Claude Code's `Edit`/`MultiEdit`/`Write` map to them; `subagent_prompt` adds an `edit_file` sentence when it is offered; a subagent type can name its own model | `crates/marv-session/src/agents.rs`, `subagent.rs`, `prompt.rs::subagent_prompt` |
| `edit_file`: plan in preview and again under the file lock; `edit_match::apply_edit` (exact → Unicode (#60) → trailing whitespace → indentation; CRLF kept); `closest_lines` error; `syntax::broken_by` note; `before_change` (typecheck baseline); `auto_safe` unless `touches_git`; `protected_target` (protected paths, the `package.json` lifecycle-scripts rule, and in the home folder `marv_sandbox::protected::home_rule`: dotfiles and git hooks folders); scope `files` | `crates/marv-tools/src/edit_file.rs` (`protected_target(path, ctx)`), `edit_match.rs` |
| #67 formatter: `formatter::after_write(ctx, shown, absolute, wrote)` runs under the file lock right after the write; it skips protected paths and `package.json` itself, and the result's diff is the formatted file | `crates/marv-tools/src/formatter.rs`, `edit_file.rs::run` |
| Home-folder rules (0.7.0): a project that is `~` or holds it makes every `ToolKind::Write` call with scope `files` ask `once` (`registry::home_file_write`): no "always", no config seed, not in yolo | `registry.rs::run_tool`, guard `tests/guards/tests/sandbox.rs::in_the_home_folder_dotfiles_and_hooks_ask_even_in_yolo` |
| `write_file`: create/overwrite/append (the long-file-in-parts workaround), same rules | `crates/marv-tools/src/write_file.rs` |
| The approval flow: protected → `must_ask` (asks even in yolo); taint; the turn's checkpoint (#16) before any write; yolo + `auto_safe` | `registry.rs::run_tool` |
| One changed path per call: `ToolOutput::changed: Option<String>`, read by the undo report (`session.rs::note_changed`), the typecheck (`diagnostics/turn.rs`) and the project checks | `crates/marv-tools/src/types.rs` |
| Multi-file preview already exists: `Preview::files: Option<Vec<PreviewFile>>` (built for `/undo`), rendered by the TUI approval panel | `types.rs`, `crates/marv-tui/src/ui/approval.rs` |
| The apply_patch plan and types, not registered: `PatchOp`, `Hunk`, `parse_patch` (todo). Its module doc predates this spec and is superseded by §3.3: hunks are found with `edit_match::find_from` (first match after the cursor), not `apply_edit`, and files are locked together with `with_file_locks`, not `with_file_lock` per file. The implementation PR (M3) rewrites the doc | `crates/marv-tools/src/apply_patch.rs` |
| Model quirks that exist today, scattered: `needs_cache_breakpoints(model)` (`anthropic/`, `google/gemini-`, #56); `reasoning_for(thinking, reasoningEffort, model list's reasoning)` (#143: medium effort for always-reasoning models, from the model list, not the id); Ollama `presence_penalty: 0` and `num_ctx` (all Ollama models); the duplicate tool-call ids (`make_ids_unique`, every provider) | `openai_compat.rs`, `crates/marv-provider/src/lib.rs`, `ollama.rs`, `crates/marv-agent/src/agent.rs` |
| `/model` mid-session: `configure()` remakes the provider; the system prompt and `Inner::specs` stay as they were | `session.rs::configure` |

## 3. Proposed design

### 3.1 The profile table (`marv-provider/src/profile.rs`)

marv-provider already owns per-model knowledge (`needs_cache_breakpoints`), and marv-session depends on it. The table is pure data plus one function.

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EditTools { Edit, Patch }          // Edit: edit_file + write_file; Patch: apply_patch

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family { Claude, Gpt, Gemini, Glm, Qwen, Kimi, DeepSeek, Other }

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelProfile {
    pub family: Family,
    pub edit_tools: EditTools,
    /// Appended to the system prompt; None for every family at first (§3.4).
    pub addendum: Option<&'static str>,
    /// #56, moved here from `needs_cache_breakpoints`.
    pub cache_breakpoints: bool,
}

/// First match wins. `model` is the id as configured: "openai/gpt-5.2-codex", "gpt-oss:20b" (Ollama), …
pub fn profile_for(model: &str) -> ModelProfile;
```

Matching works on the lowercased id with any `vendor/` prefix stripped. Rules, in order:

| Pattern (after stripping the prefix) | Family | Edit tools | Cache breakpoints |
|---|---|---|---|
| contains `gpt-oss` or `-oss` | Gpt | Edit | no |
| starts with `gpt-4` | Gpt | Edit | no |
| starts with `gpt-` or `codex`, or matches `^o[1-9]` | Gpt | **Patch** | no |
| `claude` | Claude | Edit | yes when the id starts `anthropic/` (today's rule) |
| `gemini` | Gemini | Edit | yes when the id starts `google/gemini-` (today's rule) |
| `glm` / `qwen` / `kimi` / `deepseek` | as named | Edit | no |
| anything else | Other | Edit | no |

**Assumption:** the GPT row follows opencode's rule as the issue states it ("GPT models other than gpt-4 and oss"). The exact current OpenRouter ids (`openai/gpt-5.x`, `openai/gpt-5.x-codex`, `openai/o4-mini`) should be checked against the live model list when M1 is built.

`cache_breakpoints` keeps today's exact prefix rule, so moving it changes nothing on the wire. `needs_cache_breakpoints` becomes `profile_for(model).cache_breakpoints`.

**Not moved into profiles:** reasoning support comes from the model list (`ModelInfo::reasoning`), not the id, and that stays; #143's `reasoningEffort` is consistent with it (a config level, mapped per model by the list). The Ollama options apply to every Ollama model, so they stay in the adapter. Tool-id de-duplication applies to every provider, so it stays in the loop. The profile is for things keyed by model family; forcing provider-wide behaviour into it would hide it.

**Override:** config `editTools: "auto" | "edit" | "patch"` (default `"auto"` = the profile's), and the same in the SDK options. It lets the eval A/B the same GPT model, and lets a user opt out. **Until M7, `auto` resolves to `Edit` for every model**; the table's Patch column takes effect only when M7 flips it.

### 3.2 When the profile is chosen (cache)

- **Once per conversation:** in `MarvSession::new`, and again on `clear()` and on resume, which already start a new system prompt or a new cache. `Inner::specs` moves into `State` (`State::specs`) so `clear()` can rebuild it.
- **`/model` mid-conversation keeps the conversation's profile.** The history holds calls to the current edit tools. Swapping the tools under it would leave the model staring at calls to tools it no longer has, and would restart the cache. The TUI shows one notice when the new model's profile differs: "`<model>` works best with apply_patch; it applies from the next /clear." From `/clear` on, the new profile applies.
- **Saved sessions** record `edit_tools` (`SavedSession::edit_tools: Option<EditTools>`, serde default `Edit`). On resume the saved value wins over the model's profile, for the same reason. The trajectory's `session` record already lists `tools`; it also gains `profile: "<family>/<edit_tools>"` so `trajectory_stats` can group by it.
- **Subagents:** profile by **their** model (a subagent type can name another one), at spawn. Each subagent is its own fresh conversation, so there is no cache concern. Under `Patch`, `SUBAGENT_TOOLS`' `edit_file` and `write_file` become `apply_patch`; an agent type listing `Edit`/`Write` gets `apply_patch` too. `subagent_prompt` gets the matching sentence (§3.4).

### 3.3 The `apply_patch` tool (`marv-tools/src/apply_patch.rs`)

**Input:** `{ "patch": string }` (`deny_unknown_fields`).

**The description (byte-stable)** gives the format in brief:

```text
Change files with a patch: add, update, delete or move several files in one call. The user may be asked to approve the change.

*** Begin Patch
*** Add File: path/new.ts
+every line of the new file, each starting with +
*** Update File: path/old.ts
*** Move to: path/renamed.ts        (optional)
@@ a line near the change, e.g. the function's signature (optional)
 context line (unchanged, starts with a space)
-removed line
+added line
*** Delete File: path/gone.ts
*** End Patch

Paths are relative to the project root. Give about 3 lines of context above and below each change, copied exactly from read_file. Use one @@ header per change site; if the same context appears more than once, add an @@ line that makes it unique.
```

**Grammar** (Codex's, per the issue). `parse_patch(&str) -> Result<Vec<PatchOp>, PatchError { line, message }>`:
- Leading and trailing whitespace is trimmed. A heredoc wrapper (`apply_patch <<'EOF'` … `EOF`), which GPT models trained on Codex's shell form sometimes emit, is accepted and stripped. A missing `*** End Patch` at the very end is accepted with a note; anything else malformed is an error naming the line.
- `*** Add File: p`: every following line must start with `+`.
- `*** Delete File: p`: no body.
- `*** Update File: p`, optional `*** Move to: q`, then one or more hunks. Each hunk is an optional `@@ <context>` header, then lines starting with ` `, `-` or `+`. An optional `*** End of File` anchors the last hunk at the end of the file.
- **Every path is the target of at most one operation, and this is checked before anything is written** (all or nothing):
  - A target is an Add path, an Update path (or its Move destination when it has one), or a Delete path. A Move's *source* counts as touched too.
  - Two operations touching the same path is an error. That includes:
    - a Move destination that is also another op's path;
    - two Moves to one destination;
    - Add plus Update of one path.
  - **The duplicate key is `(realpath(parent folder), file name)`**, not the path as written. That way a destination that doesn't exist yet is covered.
    - When the parent folder doesn't exist yet either (an Add that creates folders): take the realpath of the deepest existing ancestor, then the remaining components appended as written. `..` is refused there (`resolve_in_project` already refuses an escape).
    - So `a.ts`, `./src/../a.ts`, and `link/a.ts` where `link → src`, all key to the same file, whether or not it exists.
    - The file name itself is compared as is. A final-component symlink is followed by `resolve_in_project`'s own rules, as for `edit_file`.
- **A Move onto an existing file is an error** ("`*** Move to: <dest>`: <dest> already exists; delete it first or update it instead"). The patch never overwrites by moving. This is checked in the plan (preview) and again under the locks.

**Applying a hunk.** Per file, a cursor starts at 0:
1. If the hunk has an `@@ <context>` header, find the first line at or after the cursor that matches it, using `edit_match`'s tiers line by line; the search starts after that line.
2. `old` = the context and `-` lines, `new` = the context and `+` lines (each joined with `\n`). Find `old` at or after the search start using the same tiers as `edit_file`, through a new `edit_match::find_from(text, old, from) -> Option<(Range, Fuzzy)>`. It reuses the tier code; `apply_edit`'s uniqueness rule is not used here.
3. **Codex semantics: the first match at or after the cursor wins** (the order of hunks and `@@` headers disambiguate). This differs from `edit_file`'s "must be unique", deliberately: it is how the format is trained and written. See open question 3.
4. A pure addition (no `old` lines) inserts at the `@@` position, or at the end of the file with `*** End of File`, or else is an error asking for context.
5. CRLF is kept (`edit_match::line_ending`). Indentation re-flow from the indentation tier applies, as in `edit_file`, and is reported with `fuzzy_note`.
6. Not found → an error naming the file and hunk number, with `closest_lines` for that hunk's `old` text, in the same shape as `edit_file::not_found`.

**All or nothing at plan time.** `plan(input, ctx)` parses, resolves every path and computes every file's new text before anything is written. Any failure fails the whole call with nothing written. It runs twice, like `edit_file`: once in `preview` (so a bad patch fails before anyone is asked) and again in `run` under the locks (the file may have changed while the user decided).

**Writing.** The file locks for all touched paths (sources and move targets) are taken in sorted path order, through a new `file_lock::with_file_locks(root, paths, f)`, so two patches can't deadlock. Then:
- adds and updates are written with `tokio::fs::write`, as `edit_file` does;
- a move writes the destination (which the plan checked doesn't exist, and checks again under its lock), then removes the source;
- deletes go last;
- **#67:** `formatter::after_write` runs for each added, updated or moved-to file, still under the locks, as `edit_file` does; it skips protected paths and `package.json` itself. Each file's diff and syntax note describe the formatted file.

An I/O error part-way is reported per file ("applied: a, b; failed: c: <err>") with `is_error`. The turn's checkpoint (#16) was taken before the run, so `/undo` restores the lot. **To verify in M3:** the checkpoint store restores a file the turn *deleted* (a test).

**Result to the model:** one line per file (`Updated src/a.ts (+3 −1)`, `Added …`, `Deleted …`, `Moved a → b`), plus `fuzzy_note`s and the syntax notes from `syntax::broken_by`, as `edit_file` does.

### 3.4 System prompt and addenda (`marv-session/src/prompt.rs`)

- `PromptInput` gains `edit_tools: EditTools` and `addendum: Option<&'static str>`.
- **The `# Fixing bugs` gate (#147) becomes "can change files":** `input.tools` contains `edit_file` **or** `apply_patch`. Today it checks `edit_file` only, so a Patch conversation would silently lose the section.
- The first sentence of `BASE_REST_TEXT` is split out:
  - `EditTools::Edit`: **exactly today's text** ("You can change files with edit_file (replace exact text; …) and write_file (new files or full rewrites), never through bash …"), and the long-file `append: true` sentence. The default prompt stays byte-identical to today; a test asserts it.
  - `EditTools::Patch`: "You can change files with apply_patch (add, update, delete or move files; copy context lines exactly from read_file), never through bash (no heredocs, echo >, sed -i or an apply_patch command), and run shell commands with bash, …". The `append` sentence is dropped. The rest of the paragraph is unchanged.
- `subagent_prompt`: under `Patch`, " apply_patch changes files: copy context lines exactly from read_file." takes the place of the `edit_file` sentence (its `has("edit_file")` check gets an `apply_patch` twin).
- **Addenda start empty for every family.** The edit-tool sentence above is a necessity, not an addendum. An addendum is added only with an eval delta (§7) that shows a gain on its family and no loss elsewhere. When one exists, it is appended as a final `# Notes for <family> models` section, fixed per conversation. Candidates to test, not to ship blind:
  - GPT: "Prefer one apply_patch call that covers all the changes to a file";
  - GLM and Qwen: whatever their tool-error stats point to.
- `SystemPrompt::Replace` gets no addendum and no edit sentence (the caller owns the prompt); `Append` keeps both.
- **A bash `apply_patch` command:** under `Patch`, a bash command whose first word is `apply_patch` is refused before it runs (in `bash.rs`'s preview, like the git guard), with "Use the apply_patch tool, not a shell command." It would fail in the sandbox anyway; this tells the model why. It is not rerouted to the tool: rerouting would change what the user approves.

### 3.5 Registry and tool order

- `ApplyPatch` goes into `builtin_tools` right after `WriteFile`.
- `tool_specs_for(has_skills, plan, github, edit_tools)` keeps `edit_file` + `write_file` and drops `apply_patch` for `Edit`, and the reverse for `Patch`. The order is stable within each set.
- `offered()` filters the callable tools the same way, so a GPT conversation can't call `edit_file` and a Claude one can't call `apply_patch`.
- `ToolOutput::changed` becomes `changed: Vec<String>` (empty = none). Its three readers iterate:
  - `session.rs::note_changed` (undo report);
  - `diagnostics/turn.rs` (typecheck files);
  - the project checks.

  `edit_file`/`write_file` set one entry. This is a mechanical change in its own PR (M2).

### 3.6 Config keys

| Key (`~/.marv/config.json`, SDK option) | Default | Meaning |
|---|---|---|
| `editTools` | `"auto"` | `"auto"` (from the profile), `"edit"` or `"patch"`. Applies from the next conversation (`/clear`, new session). |

No per-profile prompt config. Addenda live in code, with their eval evidence in the PR that adds them.

## 4. What stays byte-stable for prompt caching

- **The system prompt and tool specs are chosen once per conversation** (§3.2) and never change within it. `/model` doesn't change them; `/clear` and resume rebuild them, as `/clear` already does.
- **For every model whose profile is `Edit` (everything but the GPT row), the system prompt and tool specs are byte-identical to today.** Guard: a snapshot test in `prompt.rs` and `session.rs`, written against today's prompt (`FIXING_BUGS`, the situation section, the Herdr text) and covering the **full offered list**: the built-ins, the caller's tools, `herdr`, the mesh tools and `radio_tx` where they apply, in their order.
- The `apply_patch` schema and description are constants (`serde_json` key order is stable). The tool order is fixed.
- The request wire: `cache_control` placement is unchanged (same prefix rule, now read from the profile). `ToolCall::arguments` for `apply_patch` (often large) is stored and re-sent as received, never re-serialized.

## 5. Security, sandbox and taint

`apply_patch` is a write tool, and it must be gated **exactly** like `edit_file`/`write_file`. Every rule below has a test, and the starred ones are guards in `tests/guards/tests/sandbox.rs`.

- **Every path** (Add, Update, Delete, the Move source **and** target) goes through:
  - `resolve_in_project` (realpath; no symlink out of the project; a dangling symlink refused);
  - `require_regular_file` for Update and Delete;
  - `refuse_git` when confined;
  - `ctx.read_only` (subagent worktrees).

  The checks run in `preview` and again in `run` under the locks.
- **Protected paths ask even in yolo** \*: `protected_target` returns the first protected rule among *all* the patch's paths (sorted). That covers:
  - `.git/` and `core.hooksPath`;
  - `.env*`, `.envrc`, `bunfig.toml`, `.mcp.json`;
  - `AGENTS.md`/`CLAUDE.md` at any level, `.marv/**`;
  - `.vscode/`, `.idea/`.

  Any one of them makes the whole call `must_ask`. A Move *into* a protected path, and a Delete of one, count.
- **The home folder** \*: `protected_target` applies `home_rule` to **every** path, Move destinations included, by calling `edit_file::protected_target` per path (it already chains `protected_rule` and `home_rule`). And since `apply_patch` is `ToolKind::Write` with scope `files`, `registry::home_file_write` makes every call there `once`, as for the edit tools, with no change to the registry. A Move of a harmless file to `~/.bashrc` or into a hooks folder asks even in yolo.
- **`package.json` lifecycle scripts** \*: the content rule (`marv_sandbox::protected::lifecycle_scripts_changed`) runs on every **resulting** `package.json`, whether added, updated, or the **destination of a Move**.
  - It compares the destination path's current contents (none, since Moves onto existing files are refused) with the new contents written there.
  - Updating `template.json` and moving it to `package.json` with a `postinstall` script triggers the rule. The source's old contents never stand in for "before".
- **Every protected-path and content rule is judged on destinations.** `protected_target` and the content rules look at each path as it will be after the patch (a Move destination), not only at the paths as written in the op headers. Moving a harmless file to `.envrc`, `AGENTS.md` or `.git/hooks/x` is a protected write. Moving a protected file away (source `.envrc`) is protected too: it deletes the original.
- **Yolo:** `auto_safe` only if every path is `auto_safe` for `edit_file` (`!touches_git`).
- **Taint:** `apply_patch` is `ToolKind::Write`, not egress; a tainted session doesn't change that, as for `edit_file`. Protected writes in a tainted session are `must_ask` and "always" never answers them (the existing rule) \*.
- **Approval:** one approval for the whole patch, scope `files` (same key as the edit tools, so "always" for file changes covers it, as it covers both edit tools today). The preview fills `Preview::files` with one `PreviewFile` per file (its diff, or "delete", or "move a → b"), so the user sees every file before saying yes. A Delete shows the removed content as the diff.
- **Checkpoint (#16):** `Tool::checkpoint` is true (the default for Write), so the turn's checkpoint is taken before the patch runs.
- **`before_change`:** awaited once before the first write (the typecheck baseline), as `edit_file` does.
- **After the write:** the formatter (#67) and the syntax check per file. The typecheck and project checks run on `changed` (all files). The formatter never runs on a protected path or `package.json`, so what was approved there is what lands.
- **Size:** a patch over 1 MB (input) is refused before parsing (a guard against pathological input; the arguments are already bounded by the model's output limit).
- New guards (each proven by reverting it):
  - \* `apply_patch_protected_paths_ask_even_in_yolo`: every pattern, as Add, Update, Delete and Move target;
  - \* `apply_patch_package_json_lifecycle_scripts_ask_even_in_yolo`, including **Update `template.json` + Move to `package.json`** with a new `postinstall` (must ask; reverting the destination check makes it pass silently);
  - \* `apply_patch_moves_are_judged_by_destination`: a harmless file moved to each protected pattern (`.envrc`, `AGENTS.md`, `.marv/x`, `.vscode/settings.json`, `.git/hooks/pre-commit`) asks even in yolo, and so does a protected file moved away;
  - \* `apply_patch_no_path_out_of_the_project`: `../`, absolute outside, a symlink out, a dangling symlink, a Move target out;
  - \* `in_the_home_folder_dotfiles_and_hooks_ask_even_in_yolo` extended to `apply_patch`: Add, Update and Move-to of a dotfile and of a hooks-folder file, and that each call is `once` (no "always");
  - \* a bad hunk in file 2 of 3 writes nothing (all-or-nothing): a test in `apply_patch.rs`, not a SECURITY rule.

## 6. Test plan

Parser (`apply_patch.rs`):
- every op; several hunks; `@@` headers; `*** End of File`; the heredoc wrapper; trailing whitespace;
- a missing `End Patch` at EOF;
- errors with line numbers: a bad prefix, an Add line without `+`, an empty Update, duplicate paths, unknown `***` lines.
- Fixtures: real patches from GPT trajectories once M5 has them (as `tests/fixtures/patch/*.txt`).

Apply:
- first-after-cursor semantics; the `@@` narrowing;
- each `edit_match` tier, CRLF preserved, the indentation re-flow;
- a pure addition at `@@` and at EOF;
- not found shows the closest lines; two hunks in order; a hunk whose context is before the previous hunk's (error);
- Move plus Update; Delete of a missing file (error); Add of an existing file (error, "use Update File").
- A Move onto an existing file → error, and nothing is written, not even the patch's other files. The same when the destination appears between preview and run (the check under the lock).
- Two operations targeting one path → error with nothing written. Cases:
  - Update A with Move to B, plus Add B;
  - two Moves to B;
  - Update A with Move to B, plus Delete B;
  - Update A plus Update `./A` (the same file after realpath);
  - **through a symlinked folder** (`link → src`):
    - Update `src/a.ts` plus Update `link/a.ts` (an existing file);
    - Add `src/new.ts` plus Add `link/new.ts` (a file that doesn't exist yet: the parent-realpath key catches it);
    - Update `src/a.ts` with Move to `link/b.ts`, plus Add `src/b.ts`.

    All three are rejected as duplicates with nothing written.

Through `run_tool`, end to end, like `edit_file`'s:
- the formatter runs on each written file (not on a protected path or `package.json`), and the result's diffs are the formatted files;
- in the home folder, the call is asked `once`, even with "always" for files remembered;
- an approval is asked once, with `Preview::files`, one per file;
- a decline writes nothing;
- the yolo `auto_safe` path;
- `before_change` is awaited;
- the checkpoint is taken;
- `changed` lists every file;
- the syntax note.

Profile (`profile.rs`): a table test of ids → profile (OpenRouter and Ollama spellings, the gpt-4 and oss exclusions, case, the vendor prefix), and `cache_breakpoints` == today's `needs_cache_breakpoints` for a list of ids.

Prompt and registry:
- the `Edit` profile prompt and the full offered list are byte-identical to today (snapshot, §4);
- the `Patch` prompt has the apply_patch sentence and no `edit_file`/`write_file` mention, and still has `# Fixing bugs`; a prompt with neither edit tool has no `# Fixing bugs`;
- `offered()` refuses `edit_file` under Patch;
- the subagent tools map `Edit`/`Write` → `apply_patch` under Patch.

Session:
- `/model` to a GPT model mid-conversation keeps the specs and shows the notice once;
- `/clear` switches;
- resume keeps the saved `edit_tools`;
- the trajectory `session` record has `profile`.

Guards: the ones listed in §5 (protected paths, lifecycle scripts including the move-to-package.json case, moves judged by destination, no path out of the project, plus the all-or-nothing test).

## 7. Eval plan (no runs without approval: they cost money)

**What to measure, per model, with `editTools` as the A/B switch:**
- pass rate;
- edit tool errors (not found, ambiguous, parse errors);
- edit calls per task;
- requests, tokens and cost per task;
- runs ending in `error` or `loop`.

`results.jsonl` already has tool calls by name and tool errors. Grouping by `profile` comes from the trajectory.

**Tasks:**
- all 21 in `evals/tasks/`;
- the editing stress set is the core:
  - `crlf-line-endings`, `trailing-whitespace`, `tabs-off-by-one`, `duplicate-blocks`, `long-file`, `json-edit`;
  - `rename-across-files`, `refactor-signature`, `callback-to-promise`;
  - `ts-make-async`, `ts-rename-export`, `ts-required-field`.
- `duplicate-blocks` specifically tests the first-after-cursor rule (repeated code), so watch it.

**Runs:**
1. **GPT, edit vs patch.** One current GPT-5-line model on OpenRouter (exact id to pick from the live list), × 21 tasks × 2 × `--repeat 5`. This is the "apply_patch passes all editing tasks on a GPT model" gate.
2. **No-regression check.** `anthropic/claude-haiku-5.5` (bench default), a GLM model (`z-ai/glm-5.x`) and a Qwen model, `auto` only, before vs after the change, × `--repeat 5`. Their prompts and tools are byte-identical, so any delta is noise; this run confirms that.
3. **Addenda (later, each its own run).** Family X with vs without the candidate addendum. It ships only with a gain and no loss.

**Bench change:** `bench.py` has no `--marv-config` yet. M6 adds `--marv-config key=value`, written into the throwaway `MARV_CONFIG_DIR` config (shared with the #63 spec). Ask for a budget before running (`--budget`).

## 8. Implementation plan

One PR each, in this order. Only M7 and the addenda need paid runs.

- **(a) M1 Profiles:** `marv-provider/src/profile.rs`, the table and its tests; `needs_cache_breakpoints` delegates to `profile_for(model).cache_breakpoints` (no wire change).
- **(b) M2 `changed` → `Vec<String>`:** in `ToolOutput`, and its readers. Mechanical, no behaviour change.
- **(c) M3 apply_patch parser and apply:** `parse_patch`, `edit_match::find_from`, `plan()`, `file_lock::with_file_locks`; the unit tests and the checkpoint-restores-a-deletion test; the module doc rewritten (§2). Not registered.
- **(d) M4 apply_patch as a tool, with its guards:** the `Tool` impl (preview with `Preview::files`, `protected_target` with `home_rule` on every path, `auto_safe`, run with the formatter); the `run_tool` end-to-end tests and the §5 guards, the home-folder one included. Registered in `builtin_tools`, but offered only with `editTools: "patch"`.
- **(e) M5 Session wiring, with `auto` → Edit for every model:** `State::specs`, the edit tools chosen on new/clear/resume; `PromptInput::edit_tools`, the prompt variants, the `FIXING_BUGS` gate on "can change files", and the snapshot test over the full offered list; subagents, `SavedSession::edit_tools`, the trajectory `profile`; the `/model` notice, the bash `apply_patch` refusal.
- **(f) M6 Bench flag** (shared with #63): `bench.py --marv-config`. Then **ask** to run §7 runs 1–2.
- **M7 Turn on `auto`** (GPT → patch, paid runs) if run 1 shows a gain and run 2 no regression. Update `docs/subsystems/tools.md`, `session.md` and `provider.md`.
- **Later, per addendum** (paid runs): a PR with the eval delta in its description.

## 9. Decisions (2026-10-20)

1. **Under `Patch`, apply_patch replaces both `edit_file` and `write_file`** (as opencode does). Long new files lose the `append` workaround; the cut-off guard still protects them.
2. **A `/model` switch mid-conversation doesn't change the edit tools**; they change from the next `/clear`, with a one-line notice.
3. **Hunks match the first occurrence after the cursor** (Codex's semantics, with `@@` headers), not `edit_file`'s must-be-unique. `duplicate-blocks` in the eval watches it.
4. **gpt-4 and `gpt-oss` stay on Edit**, per opencode's rule. Revisit if `gpt-oss` users show edit failures.
5. **No prompt addenda ship without an eval delta.** The first version has only the edit-tool sentence, which is required.
6. **The profile table lives in marv-provider**, next to `needs_cache_breakpoints`; marv-core stays types-only.
7. **A bash `apply_patch …` command is refused with a hint**, never rerouted: rerouting would make a bash approval apply a patch the user didn't see as one.
