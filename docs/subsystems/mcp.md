# marv-mcp

Marv's MCP client:
- config in Claude Code's format, and trust for project servers;
- the connection manager (stdio and Streamable HTTP);
- MCP tools as Marv tools, and deferred tools (`tool_search`/`mcp_call`);
- OAuth logins, with a shared token store protected by a file lock;
- the `/mcp add|search|remove` installers.

It doesn't depend on `marv-tools`. An `McpTool` is plain data plus a call handle, and `marv-session` wraps it in the `Tool` trait.

Spec: the **MCP servers** bullet in Marv's `CLAUDE.md`, `src/mcp/**`, and the tests in `tests/mcp*.test.ts`.

## Modules

| Marv (TS) | Rust module | Status |
|---|---|---|
| `src/mcp/config.ts` | `config.rs` | **Ported, with tests.** Covers `${VAR}`/`${VAR:-d}` expansion, `reads_of`, `display_of`, `parse_server` (stdio, http, a strict `oauth` block, `sse` refused, name rules), `project_files` (the trust-key input), `load_mcp_config` (file order kept, so a project server can't take your server's name), `check_entry` and `parse_mcp_servers`. |
| `src/mcp/trust.ts` | `trust.rs` | `read` and `is_trusted` are ported and tested. `trust()` (the write) is todo: it needs the private-file writer. |
| `src/mcp/tool.ts` | `tool.rs` | **Ported, with tests.** Covers `mcp_tool_name`, `format_mcp_result` (30k cap; images and audio named, not sent), `acceptable_schema`, `label_of`, `McpTool` (kind/scope/preview/run) and `make_mcp_tool`. |
| `src/mcp/deferred.ts` | `deferred.rs` | **Ported, with tests:** `one_line`, `announcement`, `choose_mode`, `search_tools`, `tool_search_output`, `unknown_tool_error`, `schema_hint`, `arguments_as_object`, `as_mcp_call`, `deferred_specs`. The `mcp_call` runner belongs in `marv-session`, which owns `run_tool`. |
| `src/mcp/manager.ts` | `manager.rs` | Types and pure helpers are ported and tested: `cwd_for`, `server_env`, `own_authorization`, `login_status`, `state_error`, `login_error`, `status`, `rebuild`, `forget_removed`, `loginable`. Still todo: `start`, `ready`, `add`, `remove`, `trust_all`, `login`, `logout`, `close` and `kill`. |
| (the MCP SDK) | `jsonrpc.rs` | Our own JSON-RPC 2.0 and MCP types (initialize, tools/list, tools/call) with line framing, plus a `Transport` trait. Tested. marv-mesh shares it. |
| `src/mcp/oauth.ts` | `oauth.rs` | Ported and tested: `LoginNeeded`, `redirect_url_for`, `authorization_url_ok`, `CallbackExpectation` (state compared in constant time, used once), `is_callback`, `callback_page`, `can_open_browser`, and the provider's metadata and redirect. Still todo: the protocol (`new_state`, the store-backed provider methods, `listen`, `open_browser`, `probe_login`, `revoke`). |
| `src/mcp/oauth-store.ts` | `oauth_store.rs` | Ported and tested: `login_key`, `Login` (extra fields kept), and reads (unreadable or wrong shape means no logins). Still todo: `update` and `remove`, which need the file lock. |
| `src/mcp/personal-file.ts` | `personal_file.rs` | `read` and `personal_server_names` are ported and tested. Add and remove are todo (they need the file lock and the private-file writer). |
| `src/mcp/install/index.ts` | `install/mod.rs` | `fits` is ported. Still todo: `resolve_add` and `with_login_fact`. |
| `src/mcp/install/source.ts` | `install/source.rs` | **Ported, with tests:** `parse_source`, `server_name`, `split_words`, `parse_add_args`. |
| `src/mcp/install/proposal.ts` | `install/proposal.rs` | **Ported, with tests:** `McpEntry`, `fact`, `env_entries`, `repo_of`, `http_proposal`, `approval_for` (returns `AddApproval`; the session maps it to `ApprovalRequest { once: true }`), `added_notice`. `ResolveDeps` is a trait. |
| `src/mcp/install/npm.ts` | `install/npm.rs` | `packument_url` is ported. `resolve_npm` is todo. |
| `src/mcp/install/pypi.ts` | `install/pypi.rs` | `project_url` is ported. `resolve_pypi` is todo. |
| `src/mcp/install/registry.ts` | `install/registry.rs` | `namespace_proof` is ported. Still todo: search, resolve and `search_text`. |
| `src/mcp/install/github.ts` | `install/github.rs` | `Candidate` and `source_text` are ported. `readme_candidates` and `resolve_github` are todo. |
| (none) | `text.rs` | `sha256_hex`, plus a local `printable()` and `truncate`. Replace `printable()` with the shared one once it exists. |

Run `grep -rn 'todo!("port' crates/marv-mcp` to see what's left.

## Security rules baked in

These come from `docs/SECURITY.md` ("MCP") and the 2026-10-12 audit.
- **readOnlyHint tools are Egress once the session is tainted.** In Marv, `readOnlyHint` meant `kind: "read"`, so the tool ran without asking. But a tool's arguments leave the sandbox, so an injected model could leak data through a "read-only" search query. `McpTool::kind(tainted)` returns `Read` (with the hint) or `Execute` while the session is clean, and `Egress` once it's tainted. `auto_safe()` is always false, so yolo never skips the approval.
- **The trust hash covers the files the command runs.** The key is SHA-256 over the canonical entry plus the content of every project file named on the command line, and each file's symlink target too. Unlike Marv, the value of a `--flag=path` word is also checked. Files those files load aren't covered, and the trust notice says the server runs project files (`runs_project_files`).
- **npx/uvx run outside the project folder.** `cwd_for` starts package runners (`npx`, `uvx`, `bunx`, `pnpx`, `uv`) in your home folder even for a *trusted project* server. Marv did this only for personal servers: `npx -y pkg` would resolve a `pkg` planted in the project's `node_modules`.
- **Server `instructions` are ignored.** `InitializeResult::instructions` is parsed and then dropped. It never reaches the prompt.
- **Stdio servers get no API keys.** `server_env` passes only `SAFE_ENV` (HOME, LOGNAME, PATH, SHELL, TERM, USER) plus the entry's own `env`.
- **OAuth callbacks need the login's `state`.** It is compared in constant time and used once. An `error=` value is shown only as a short code, and the browser page echoes nothing.

## Port notes

- **Trust keys don't carry over from Marv.** The key hashes the entry as canonical JSON (sorted keys, compact), while Marv used `JSON.stringify` in file order. Each project server is trusted once more after switching, and reordering keys no longer drops trust.
- **Server order is file order.** `OrderedEntries` deserializes `mcpServers` with its order kept: serde_json's default map sorts its keys, and the server order sets the tool order.
- **`env` and `headers` show in key order** in `display`, not file order.
- **No `rmcp` yet.** Our own `jsonrpc` types keep the skeleton quick to compile. `rmcp` (and its auth module for OAuth) can replace the transports behind `jsonrpc::Transport` without changing the manager's API.
- **File lock and private files.** `oauth_store::update`, `personal_file::{add,remove}` and `trust::trust` need Marv's `withFileLock` and `writePrivate` (0600 files in 0700 folders, an O_EXCL lock with takeover after 10 s). They live with `marv-session` until they get a shared home. Those bodies stay todo until then.
- **`as_mcp_call` and JSON repair.** It accepts only text that is exactly one JSON object. Marv also accepted text that #10's `repairJsonArgs` turns into one; pass that in once `marv-tools` exposes it.
- **Char counts.** Lengths and the 30k cap count Unicode scalar values. Marv counted UTF-16 units, so results differ only for astral-plane text.

## Interfaces other crates use

```rust
// config
pub fn load_mcp_config(root: &Path, config_dir: &Path, env: &Env, include: Include) -> LoadedConfig; // {servers, problems}
pub fn parse_mcp_servers(entries: &[(String, Value)], env: &Env) -> LoadedConfig;                  // SDK mcpServers
pub struct McpServerConfig { name, source: ServerSource, transport: McpTransport, timeout, key, display, reads, runs_project_files }
pub type Env = BTreeMap<String, String>;

// manager (session owns one; the TUI's /mcp reads status())
McpManager::new(servers, ManagerOptions { root, project_key, home, version, trust, logins, env, connect_timeout, login_timeout })
async start(); async ready(); tools() -> Arc<Vec<McpTool>>; specs() -> Arc<Vec<ToolSpec>>; settled();
async add(cfg); async remove(name); forget_removed(); untrusted(); async trust_all(); loginable();
login(name, authorize) -> McpLogin { paste, done }; async logout(name) -> Revoked; status() -> Vec<McpServerStatus>;
async close(); kill();

// tool (marv-session wraps in Tool)
McpTool { name, description, spec, server, source, info, .. }
  .kind(tainted) -> ToolKind  // Egress when tainted
  .scope() -> (key, description); .preview(args) -> McpPreview; .label(args); .auto_safe() == false
  async .run(args, cancel) -> Result<ToolResult, String>

// deferred (session: decide once per conversation, save ModeChoice.mode)
choose_mode(setting, &specs, context_len) -> ModeChoice; deferred_specs(); announcement(&added, &gone);
tool_search_output(..); unknown_tool_error(..); schema_hint(spec); as_mcp_call(call, &names); arguments_as_object(raw)

// oauth / store (TUI: is_callback() before sending a message to the model)
oauth::is_callback(text); oauth::LoginNeeded; oauth_store::{McpTokenStore, login_key}

// install (/mcp add)
install::parse_add_args(text) -> AddOptions; install::resolve_add(&opts, &dyn ResolveDeps) -> Resolved;
install::proposal::approval_for(&proposal, is_set) -> (AddApproval, json); added_notice(&status)
personal_file::{add_personal_server, remove_personal_server, personal_server_names}

// jsonrpc (shared with marv-mesh)
jsonrpc::{Message, Request, Response, Notification, encode_line, decode_line, Transport, method, PROTOCOL_VERSION}
```
