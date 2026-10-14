# marv-tools

The `Tool` trait, the registry and `run_tool` (preview, approval, yolo, repair), and Marv's built-in tools: `read_file`, `glob`, `grep`, `web_fetch`, `skill`, `edit_file`, `write_file`, `bash`, `memory` and `agent`. It also holds edit matching, syntax checks, diffs and the web readers. The sandbox and the path rules live in `marv-sandbox`. Tools reach them through `files.rs`.

Spec: the **Tools**, **Web**, **Skills**, **Memory**, **Subagents** and **Yolo mode** bullets in Marv's `CLAUDE.md`, `src/tools/**`, and `tests/{tools,edit-match,tool-call-repair,diff,bash,web-*,write-tools,agent-tool,skills}.test.ts`.

## Modules

| Marv (TS) | Rust module | Status |
|---|---|---|
| `src/tools/types.ts` | `types.rs` | Done: `Tool` (typed `Input`), the type-erased `DynTool`, `ToolContext`, `ToolOutput`, `Preview`, `Scope`, `ApprovalRequest`, `Decision`, `Approval`, `Approver`, `BeforeChange`, `AgentHost`, `Skill`, `MemoryPaths`, `DiffLine`/`DiffShown`; #24: `ProcessHost`, `ProcessForwards`, `ProcessForward`, `ProcessInfo`, `ProcessState`, `StartResult`, `ToolContext::{processes, forwards}` |
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
| `src/tools/bash.ts` | `bash.rs` (+ `output.rs`, `marv_sandbox::{notes,git_guard}`) | **Ported and tested**, bwrap tests included (they skip when bwrap is missing): `run_command` (bwrap or its own process group, a built environment, both pipes at once, timeout/Esc kill the group), `run` (git guard, worktree folders, placeholders removed, `GIT_HINT`/home/network/gh notes); marv-rs (#8): the post-command scan for protected files a sandboxed command creates (reverted, with a `Marv:` note; a new `.git` kept after an approved `git_write`), nested `.git` read-only when confined. #24: a sandboxed command without network gets a relay to each running process's forwarded port (`RunOptions::forwards`, from `ToolContext::forwards`), so `curl localhost:<port>` reaches it |
| `src/tools/process.ts` | `process.rs` | **Ported and tested (#24, Marv #51):** the spec byte for byte, scope `process:[net:]<command>`, door `process` on a start (asks even in yolo: no `auto_safe`), `NO_PROCESS_DOOR` refused in `preview` and `run`, `process_line`; tested with a fake `ProcessHost` (the real one is marv-session's `ProcessManager`) |
| `src/processes/relay.ts` | `relay.rs` | **Ported and tested (#24):** forward and reverse modes, the port-line rule (1024–65535, 16 bytes), bytes before the connection kept. Redesigned as `marv __relay <spec>` (see session.md); `relay_exe`/`set_relay_exe`, `exe_bind`, `sandbox_argv`, `through_socket` (the host side's pipe too) |
| `src/tools/memory.ts` (+ bullet edits from `src/memory.ts`) | `memory.rs` | **Ported and tested:** the tool, and `add_memory`/`find_memory`/`remove_memory` (line-by-line edits, 0600 through `marv_fs::write_private`) |
| `src/tools/agent.ts` | `agent.rs` | Ported (it delegates to `AgentHost`, which marv-session implements) |
| `src/tools/syntax.ts` | `syntax.rs` | **Ported and tested:** `syntax_error` (oxc parser + semantic checks for JS/TS, serde_json, serde_yaml_ng over every document, `python3 -I`), `broken_by`, `syntax_note` |
| `src/tools/web-fetch.ts` | `web_fetch.rs` | **Ported and tested** (#5; `web-fetch-tool.test.ts` through `run_tool`, a local server only): `preview` (the address check fails before anyone is asked), `run` (cache, same-site redirects, GitHub, the "pass an http:// URL" hint), `pasted_scopes` (#11; a fence scanner, Rust's regex has no backreferences), the `offset` `maximum` (2^53 − 1). marv-rs: `Provenance::Web`; invisible characters stripped; `egress_url`/`url_prefix` for tainted sessions. Not ported: the env-var half of Marv's token test (setting env vars needs `unsafe` in Rust 2024) |
| `src/tools/web/address.ts` | `web/address.rs` | **Ported and tested.** `check_address` takes a `Resolve` (tests swap it in, as Marv's spy did) and returns the addresses it approved |
| `src/tools/web/fetch.ts` | `web/fetch.rs` | **Ported and tested.** `get` on reqwest (no redirects, no decompression: gzip/deflate/br inflated by hand under the cap), the connection pinned to `check_address`'s answer; `fetch_page`. marv-rs: HTML comments stripped from Markdown served as it is |
| `src/tools/web/github.ts` | `web/github.rs` | **Ported and tested.** marv-rs: a README's HTML comments are stripped; the rate-limit reset is shown as `HH:MM UTC` (Marv: local time) |
| `src/tools/web/html.ts` | `web/html.rs` | **Ported and tested.** dom_query (html5ever) for the DOM, dom_smoothie for Readability, and a port of turndown 7.2 + turndown-plugin-gfm as the Markdown writer. marv-rs: hiding classes and off-screen styles dropped, invisible characters stripped |
| `src/tools/web/plain.ts` | `web/plain.rs` | **Ported and tested** (`plainText` tests from `web-html-convert.test.ts`). marv-rs: `is_hidden_style` knows the off-screen and zero-size tricks; hiding classes count as hidden |
| `src/tools/web/convert.ts` | `web/convert.rs` | **Ported and tested:** re-execs this binary as `marv __html-convert` (empty environment, temp dir, SIGKILLed and reaped before it returns). Tests use the test binary itself as the converter |
| `src/tools/web/html-convert-cli.ts` | `web/html_convert_cli.rs` | **Ported:** `main` (marv-cli routes `__html-convert` to it) |
| (new, marv-rs) | `web/hidden.rs` | Hiding classes, invisible characters, Markdown comments (docs/SECURITY.md; guards in `tests/guards/tests/web.rs`) |
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
- **grep's regex dialect is Rust's** (`regex`, through `grep-regex`), not JavaScript's: no lookaround and no backreferences. The schema's description keeps Marv's "JavaScript syntax" (byte-identical requests, checked by the oracle, #11); the `Invalid regex` error for such a pattern says to rewrite it without them. `fancy-regex` would add both, but it doesn't plug into ripgrep's searcher.
- **The JS/TS syntax check is oxc**, not `@babel/parser`: parser errors, then the semantic pass's (a redeclared `const`). Messages are oxc's wording. The column is 1-based, in chars, from the primary label.
- **bash outside the sandbox** runs in its own process group (`process_group(0)`) instead of under `setsid`. A timeout or Esc kills the group with `killpg` (the `nix` crate, no `unsafe`). `run_command` returns `[sandbox failed to start: …]` when marv-sandbox refuses a mount, as it does when bwrap fails.
- **Lengths count `char`s.** Marv counted UTF-16 units: line caps, page offsets, the 30k cut. The two differ only outside the BMP.
- **marv-rs behaviour changes** (`docs/SECURITY.md`):
  - In a tainted session, egress (`ToolKind::Egress` or `uses_network`) skips yolo, and its `ApprovalRequest` has `must_ask` set, so the session's "always" wrapper must not answer it.
  - Protected paths do the same, through `protected_target`.
  - A result whose `provenance` taints marks the session.
  - `web_fetch` is `Egress` with `Provenance::Web`. In a tainted session, a URL under a prefix the user approved runs as approved, and "always" on a tainted request approves the URL's prefix (`web_fetch::url_prefix`: origin plus first path segment), not the whole site (the S6 item). `Tool::egress_url` is the hook `run_tool` uses.
  - Hidden text in web pages (#5): the screen-reader and hiding classes of the common CSS frameworks, off-screen and zero-size inline styles, zero-width/tag/variation-selector/bidi characters, and HTML comments in raw Markdown never reach the model, on both conversions.
  - web_fetch connects to the addresses `check_address` approved (reqwest's `resolve_to_addrs`), which closes Marv's DNS-rebinding gap.
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
  - `apply_patch` (#66).
- **Web port notes:**
  - The converter is SIGKILLed on timeout, Esc or completion, and reaped before `convert_html` returns. Marv's exit handler (kill converters still running when Marv exits) is `convert::kill_running()`, for marv-cli's exit path. Dropping the future kills the child too (`kill_on_drop`).
  - Readability is dom_smoothie, not Mozilla's: same patterns, slightly different scoring. Marv's tests pass unchanged.
  - Unconverted tests: the "main thread stays free" timing (a Rust converter runs in another process by design) and the exit-handler test.
