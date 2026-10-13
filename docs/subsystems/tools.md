# marv-tools

The `Tool` trait, the registry and `run_tool` (preview, approval, yolo, repair), and Marv's built-in tools: `read_file`, `glob`, `grep`, `web_fetch`, `skill`, `edit_file`, `write_file`, `bash`, `memory` and `agent`. It also holds edit matching, syntax checks, diffs and the web readers. The sandbox and the path rules live in `marv-sandbox`. Tools reach them through `files.rs`.

Spec: the **Tools**, **Web**, **Skills**, **Memory**, **Subagents** and **Yolo mode** bullets in Marv's `CLAUDE.md`, `src/tools/**`, and `tests/{tools,edit-match,tool-call-repair,diff,bash,web-*,write-tools,agent-tool,skills}.test.ts`.

## Modules

| Marv (TS) | Rust module | Status |
|---|---|---|
| `src/tools/types.ts` | `types.rs` | Done: `Tool` (typed `Input`), the type-erased `DynTool`, `ToolContext`, `ToolOutput`, `Preview`, `Scope`, `ApprovalRequest`, `Decision`, `Approval`, `Approver`, `BeforeChange`, `AgentHost`, `Skill`, `MemoryPaths`, `DiffLine`/`DiffShown` |
| `src/tools/index.ts` | `registry.rs` | **Ported and tested:** `run_tool` (the whole approval flow, plus taint and protected paths), `builtin_tools` (Marv's order), `tool_specs`/`tool_specs_for`, `is_parallel_call` |
| `src/tools/repair.ts` | `repair.rs` | **Ported and tested:** JSON control-character escaping, timeout from ms to s, the leading-slash path. Todo: schema-guided repair (#65) |
| `src/tools/edit-match.ts` | `edit_match.rs` | **Ported and tested:** exact, then the Unicode tier (#60: typographic quotes, dashes, `…` and Unicode spaces folded to ASCII, unique only, the file's original span replaced), then trailing whitespace, then indentation; CRLF handling, `closest_lines` |
| `src/tools/diff.ts` | `diff.rs` | **Ported and tested.** Uses `similar` in place of jsdiff |
| `src/tools/file-lock.ts` | `file_lock.rs` | **Ported and tested** (a per-path tokio mutex; an entry is dropped once nothing waits on it) |
| `src/tools/files.ts` | `files.rs` (+ `marv_sandbox::paths`) | **Ported and tested.** The path rules live in marv-sandbox. Here: regular-file checks, `looks_binary`, `list_project_files` (git ls-files through a local `run_git` with `NO_PROGRAMS`, no inherited `GIT_*`, `git_env` pinned, a timeout that errors; the `ignore` walker outside git) |
| `src/tools/read-file.ts` | `read_file.rs` | **Ported and tested** (end to end through `run_tool`) |
| `src/tools/skill.ts` | `skill.rs` | **Ported and tested**, including the realpath fix (symlink test) |
| `src/tools/edit-file.ts` | `edit_file.rs` | **Ported and tested** (end to end through `run_tool`): `run` awaits `before_change` (or Esc), plans again and writes under the file lock, syntax note, fuzzy notes. marv-rs: the `package.json` lifecycle-scripts content rule. Todo: formatter (#67) |
| `src/tools/write-file.ts` | `write_file.rs` | **Ported and tested** (end to end, `write-tools.test.ts`): create/overwrite/append, CRLF, folders, parallel appends, syntax note, `before_change`. marv-rs: the `package.json` lifecycle-scripts content rule. Todo: formatter (#67) |
| `src/tools/glob.ts` | `glob.rs` | **Ported and tested.** `globset` with `literal_separator`, checked against Bun.Glob's answers |
| `src/tools/grep.ts` | `grep.rs` | **Ported and tested.** ripgrep's `grep-regex` + `grep-searcher`; Marv's caps (100 matches, 300 chars a line, 1 MB files, binary skipped). Regex dialect: Rust's (see port notes) |
| `src/tools/bash.ts` | `bash.rs` (+ `output.rs`, `marv_sandbox::{notes,git_guard}`) | **Ported and tested**, bwrap tests included (they skip when bwrap is missing): `run_command` (bwrap or its own process group, a built environment, both pipes at once, timeout/Esc kill the group), `run` (git guard, worktree folders, placeholders removed, `GIT_HINT`/home/network/gh notes); marv-rs (#8): the post-command scan for protected files a sandboxed command creates (reverted, with a `Marv:` note; a new `.git` kept after an approved `git_write`), nested `.git` read-only when confined |
| `src/tools/memory.ts` (+ bullet edits from `src/memory.ts`) | `memory.rs` | **Ported and tested:** the tool, and `add_memory`/`find_memory`/`remove_memory` (line-by-line edits, 0600 through `marv_fs::write_private`) |
| `src/tools/agent.ts` | `agent.rs` | Ported (it delegates to `AgentHost`, which marv-session implements) |
| `src/tools/syntax.ts` | `syntax.rs` | **Ported and tested:** `syntax_error` (oxc parser + semantic checks for JS/TS, serde_json, serde_yaml_ng over every document, `python3 -I`), `broken_by`, `syntax_note` |
| `src/tools/web-fetch.ts` | `web_fetch.rs` | Ported and tested: `domain_of`, `web_scope`, `paged`, the page cache, `needs_approval`. Todo: `pasted_scopes`, `preview`, `run` |
| `src/tools/web/address.ts` | `web/address.rs` | Ported and tested: `is_private_address`. Todo: `check_address` |
| `src/tools/web/fetch.ts` | `web/fetch.rs` | Ported and tested: `parse_url`, `sends_headers`, `redirect_note`. Todo: `get`, `fetch_page` (using reqwest with a pinned resolver) |
| `src/tools/web/github.ts` | `web/github.rs` | Constants and `safe_segment`. Todo: `parse_github`, `fetch_github`, `fetch_readme` |
| `src/tools/web/html.ts` | `web/html.rs` | Types and limits. Todo: `html_to_markdown` |
| `src/tools/web/plain.ts` | `web/plain.rs` | Ported: `one_line`. Todo: `is_hidden_style`, `plain_text` |
| `src/tools/web/convert.ts` | `web/convert.rs` | Todo: re-exec `marv __html-convert` |
| `src/tools/web/html-convert-cli.ts` | `web/html_convert_cli.rs` | Types. Todo: `main` (marv-cli wires the subcommand) |
| (new) | `output.rs` | Ported: the 30k cut (10k head + 20k tail). Todo: spill the cut to a file (#61) |
| (new, #66) | `apply_patch.rs` | The plan and its types only. Not registered |

`grep -rn 'todo!("port' crates/marv-tools` lists what's left.

## Interfaces other crates use

```rust
#[async_trait]
pub trait Tool: Send + Sync + 'static {
    type Input: DeserializeOwned + Send + Sync + 'static;
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn schema(&self) -> serde_json::Value;                       // byte-stable (prompt cache)
    fn kind(&self) -> ToolKind { ToolKind::Read }
    fn provenance(&self) -> Provenance { Provenance::Project }
    fn parallel(&self) -> bool { false }
    fn check(&self, input: &Self::Input) -> Result<(), String> { Ok(()) }   // zod's min/max
    fn label(&self, input: &Self::Input) -> String;
    fn needs_approval(&self, input: &Self::Input) -> Option<bool> { None }
    fn uses_network(&self, input: &Self::Input) -> bool { false }
    async fn preview(&self, input: &Self::Input, ctx: &ToolContext) -> Result<Preview, ToolError>;  // default: title = name
    fn scope(&self, input: &Self::Input) -> Option<Scope> { None }          // None: the tool's name
    fn auto_safe(&self, input: &Self::Input, ctx: &ToolContext) -> bool { false }
    fn protected_target(&self, input: &Self::Input, ctx: &ToolContext) -> Option<ProtectedRule> { None }
    fn repair(&self, raw: &Value, ctx: &ToolContext) -> Option<Repaired> { None }
    async fn run(&self, input: &Self::Input, ctx: &ToolContext) -> Result<ToolOutput, ToolError>;
}
// Every T: Tool is also a DynTool (object-safe). The registry holds ToolRef = Arc<dyn DynTool>.

#[async_trait] pub trait Approver: Send + Sync { async fn approve(&self, request: ApprovalRequest) -> Decision; }
#[async_trait] pub trait BeforeChange: Send + Sync { async fn before_change(&self); }
#[async_trait] pub trait AgentHost: Send + Sync {
    fn agent_types(&self) -> Vec<(String, String)>;
    fn worktrees(&self) -> bool;
    async fn preview(&self, input: &AgentInput, ctx: &ToolContext) -> Result<Preview, ToolError>;
    async fn run(&self, input: &AgentInput, ctx: &ToolContext) -> Result<ToolOutput, ToolError>;
}

pub async fn run_tool(call: &ToolCall, ctx: &ToolContext, available: &[ToolRef]) -> ToolOutcome; // never fails
pub struct ToolOutcome { pub result: ToolOutput, pub label: String }
impl ToolOutput { pub fn into_core(self) -> marv_core::ToolResult }
pub fn builtin_tools() -> &'static [ToolRef];
pub fn tool_specs_for(has_skills: bool) -> Vec<ToolSpec>;
pub fn is_parallel_call(call: &ToolCall, available: &[ToolRef]) -> bool;
pub const INTERRUPTED: &str;  // must equal marv-agent's NOT_RUN.aborted
```

`ToolContext` fields: `root`, `home`, `cancel: CancellationToken`, `approver: Option<Arc<dyn Approver>>`, `sandbox: Arc<marv_sandbox::Sandbox>`, `yolo`, `confined`, `skills: Arc<Vec<Skill>>`, `memory: Option<MemoryPaths>`, `call_id`, `read_only`, `agent_host`, `git_env`, `before_change`, `taint: Arc<TaintState>`. `ToolContext::new(root)` gives Marv's defaults.

## Port notes

- **Typed input in place of zod.** Each tool deserializes into its own `Input` struct (`deny_unknown_fields`), plus `check()` for ranges. The error text is serde's, after `Invalid input for <tool>:`. Marv's text came from `z.prettifyError`, so error wording can differ.
- **Schema key order.** `serde_json` without `preserve_order` sorts object keys. The specs are still byte-stable, which is what the prompt cache needs, but the key order differs from Marv's. Turn on `preserve_order` workspace-wide if the oracle compares bytes.
- **grep's regex dialect is Rust's** (`regex`, through `grep-regex`), not JavaScript's: no lookaround and no backreferences. The schema's description says so, and the `Invalid regex` error for such a pattern says to rewrite it without them. `fancy-regex` would add both, but it doesn't plug into ripgrep's searcher.
- **The JS/TS syntax check is oxc**, not `@babel/parser`: parser errors, then the semantic pass's (a redeclared `const`). Messages are oxc's wording. The column is 1-based, in chars, from the primary label.
- **bash outside the sandbox** runs in its own process group (`process_group(0)`) instead of under `setsid`. A timeout or Esc kills the group with `killpg` (the `nix` crate, no `unsafe`). `run_command` returns `[sandbox failed to start: …]` when marv-sandbox refuses a mount, as it does when bwrap fails.
- **Lengths count `char`s.** Marv counted UTF-16 units: line caps, page offsets, the 30k cut. The two differ only outside the BMP.
- **marv-rs behaviour changes** (`docs/SECURITY.md`):
  - In a tainted session, egress (`ToolKind::Egress` or `uses_network`) skips yolo, and its `ApprovalRequest` has `must_ask` set, so the session's "always" wrapper must not answer it.
  - Protected paths do the same, through `protected_target`.
  - A result whose `provenance` taints marks the session.
  - `web_fetch` is `Egress` with `Provenance::Web`.
  - Skill files are read only after a realpath check.
  - The Unicode edit tier (Marv #60, still open there): exact, then Unicode, then trailing whitespace, then indentation.
  - `edit_file` and `write_file` ask even in yolo for a `package.json` change that adds, removes or changes a lifecycle script (`ProtectedRule::PackageScripts`).
- **Not here, by design:**
  - Skill and agent-type loading, the memory paths, and subagents: all in marv-session.
  - Marv's cross-process `withFileLock` (`src/file-lock.ts`): in marv-fs (`marv_fs::with_file_lock`).
- **The `agent` tool** reaches subagents only through `AgentHost`, because marv-tools can't depend on marv-agent.
- **Planned:**
  - spill cut output to a file (#61, `output::spill_cut`);
  - schema-guided arg repair (#65, `repair`);
  - a formatter after edits (#67, `edit_file`/`write_file`);
  - `apply_patch` (#66);
  - a pinned DNS resolver for web fetches (closes Marv's DNS-rebinding limit).
