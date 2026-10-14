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
| `src/mcp/trust.ts` | `trust.rs` | **Ported, with tests:** `read`, `is_trusted`, and `trust()` (async: merged as a set, written 0600 through `marv_fs::write_private`). |
| `src/mcp/tool.ts` | `tool.rs` | **Ported, with tests.** Covers `mcp_tool_name`, `format_mcp_result` (30k cap; images and audio named, not sent), `acceptable_schema` (and `acceptable_schema_ordered`, the same in the server's key order), `label_of`, `McpTool` (kind/scope/preview/run) and `make_mcp_tool`. |
| `src/mcp/deferred.ts` | `deferred.rs` | **Ported, with tests:** `one_line`, `announcement`, `choose_mode`, `search_tools`, `tool_search_output` (schemas in the server's key order, as Marv printed them), `unknown_tool_error`, `schema_hint`, `arguments_as_object`, `as_mcp_call`, `deferred_specs`. The `mcp_call` runner belongs in `marv-session`, which owns `run_tool`. |
| `src/mcp/manager.ts` | `manager.rs` | **Ported, with tests** (`tests/mcp.rs`, `tests/mcp_login.rs`): `start`, `ready`, `add`, `remove`, `forget_removed`, `trust_all`, `login` (listener, interactive provider, callback, code exchange, reconnect), `logout`, `status`, `close`, `kill`, and the pure helpers. Shared as `Arc<McpManager>` with `&self` methods. |
| (the MCP SDK) | `jsonrpc.rs` | Our own JSON-RPC 2.0 and MCP types (initialize, tools/list, tools/call) with line framing, plus a `Transport` trait. Tested. marv-mesh shares it. |
| (the MCP SDK) | `client.rs` | The client side of one connection: requests with timeouts that progress restarts, cancellation (`notifications/cancelled`), stdio (child process, piped stderr, close then kill) and Streamable HTTP (JSON or SSE answers, session id, a 401 runs the login's refresh and retries once). |
| `src/mcp/oauth.ts` | `oauth.rs` | **Ported, with tests:** `LoginNeeded`, `new_state`/`pkce` (OS RNG), the store-backed provider (client, tokens, discovery, invalidation that keeps a newer refresh token), `auth` (the SDK's flow: RFC 9728/8414 discovery cached in the store, dynamic registration, PKCE S256, code exchange, refresh), `CallbackExpectation`, `listen`, `open_browser`, `probe_login`, `revoke`. |
| `src/mcp/oauth-store.ts` | `oauth_store.rs` | **Ported, with tests:** `login_key`, `Login` (extra fields kept), reads, and `update`/`remove` under `marv_fs::with_file_lock` (refuses a file it can't read; the update closure may refuse too: a cancelled login). |
| `src/mcp/personal-file.ts` | `personal_file.rs` | **Ported, with tests:** `read`, `add_personal_server`, `remove_personal_server` (locked, 0600, the file's key order kept), `personal_server_names`. |
| `src/mcp/install/index.ts` | `install/mod.rs` | **Ported, with tests:** `resolve_add`, `with_login_fact`, `fits`. The live `ResolveDeps` (Marv's `liveDeps`) is the session's `mcp_install::LiveDeps` (#27). |
| `src/mcp/install/source.ts` | `install/source.rs` | **Ported, with tests:** `parse_source`, `server_name`, `split_words`, `parse_add_args`. |
| `src/mcp/install/proposal.ts` | `install/proposal.rs` | **Ported, with tests:** `McpEntry`, `fact`, `env_entries`, `repo_of`, `http_proposal`, `approval_for` (returns `AddApproval`; the session maps it to `ApprovalRequest { once: true }`), `added_notice`. `ResolveDeps` is a trait. |
| `src/mcp/install/npm.ts` | `install/npm.rs` | **Ported, with tests:** `packument_url`, `resolve_npm`. |
| `src/mcp/install/pypi.ts` | `install/pypi.rs` | **Ported, with tests:** `project_url`, `resolve_pypi`. |
| `src/mcp/install/registry.ts` | `install/registry.rs` | **Ported, with tests:** `search_registry`, `resolve_registry`, `search_text`, `namespace_proof`. |
| `src/mcp/install/github.ts` | `install/github.rs` | **Ported, with tests:** `readme_candidates` (a small fence reader instead of `marked`), `source_text`, `resolve_github`. |
| (none) | `text.rs` | `sha256_hex`, `truncate`, and the shared `printable()` re-exported. |
| (none) | `ordered.rs` | JSON with key order kept (`Ordered`): your `mcp.json` isn't reordered by a write, README servers keep their order, schemas show as the server wrote them. |
| `tests/fixtures/mcp-server.ts` & co. | `src/bin/marv-mcp-fixture.rs` | Test fixture only: the stdio test server, the paging server, and the multi-process store/file writers. The 25-tool tracker is the oracle's (`marv_oracle::tracker`, a dev-dependency), served over HTTP in `tests/mcp.rs`. |

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
- **No `rmcp`.** `client.rs` and `oauth::auth` are our own (the slice of the SDK Marv used): quick to compile, no SDK churn.
- **File lock and private files** come from `marv-fs` (`with_file_lock`, `write_private`). `McpTrust::trust` is async now (it writes through `write_private`).
- **Settled before start** (marv-rs): a manager with servers is unsettled from `new()`, and `start()`/`add()` mark it unsettled when called, not when first polled. Marv's `start()` did that synchronously; the session spawns it, and would otherwise skip `waiting_for_mcp`.
- **No standalone GET stream** on Streamable HTTP: Marv reacted to no server-initiated message (list changes, sampling), so only POSTs are made. A server's request on a POST's stream gets "method not found" (a ping is answered).
- **Passive providers refuse before registering.** The SDK sent the registration request and then failed to save it in a background connection; marv-rs raises `LoginNeeded` before any request.
- **Timeouts.** `connecting timed out after Ns` / `listing tools timed out after Ns` wrap the handshake; a call's timeout (`timeout` in the config, 120 s) restarts on progress and ends with the SDK's `MCP error -32001: Request timed out`.
- **Schema key order.** `ToolSpec::parameters` sorts keys (serde_json); `tool_search` prints a tool's schema in the server's order, as Marv did. The manager keeps the server's text in `McpTool::parameters_text` (`with_schema_order`), and `tool_search_output` takes the turn's `McpTool`s.
- **Logins and tests.** OAuth is tested against a fake authorization server (`tests/support`), never a real one.
- **Schema-guided repair (#65, new in marv-rs).** An MCP tool's arguments are repaired against the server's schema by `run_tool` (`marv_tools::schema_repair`, `RepairMode::Mcp`), on a direct call and through `mcp_call` alike (it runs the real tool through `run_tool`): type coercions only (`{"number": "12"}` → `12` where the schema says integer; a JSON string → object/array; a lone value → a one-item array), each with a `Marv:` note. Unknown keys are dropped only under `additionalProperties: false`, and `null` passes through: a server may accept what its schema doesn't list. Before the server is called, `McpToolAdapter::check` (`check_required`) refuses a missing required field or a string shorter than its `minLength`, naming the field (bench #73: an `add_comment` without `body` posted an empty comment). The repair touches the value sent to the server, never the stored call.
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
// Every method takes &self: no lock around it (/mcp reads status() while a turn waits on ready()).
McpManager::new(servers, ManagerOptions { root, project_key, home, version, trust, logins, env, connect_timeout, login_timeout }) -> Arc<McpManager>
start() -> impl Future (unsettled at once); async ready(); tools() -> Arc<Vec<McpTool>>; specs() -> Arc<Vec<ToolSpec>>; settled();
set_on_change(Fn() + Send + Sync)   // Marv's onChange: the UI's notices, the SDK's "needs login" warnings
add(cfg) -> impl Future<Result<McpServerStatus>>; async remove(name); forget_removed(); untrusted() -> Vec<McpServerConfig>; async trust_all(); loginable();
login(self: &Arc<Self>, name, authorize: oauth::Authorize /* Arc<dyn Fn(Url)> */) -> McpLogin { paste, done }; async logout(name) -> Revoked; status() -> Vec<McpServerStatus>;
async close(); kill();

// tool (marv-session wraps in Tool)
McpTool { name, description, spec, server, source, info, .. }
  .kind(tainted) -> ToolKind  // Egress when tainted
  .scope() -> (key, description); .preview(args) -> McpPreview; .label(args); .auto_safe() == false
  async .run(args, cancel) -> Result<ToolResult, String>

// deferred (session: decide once per conversation, save ModeChoice.mode)
choose_mode(setting, &specs, context_len) -> ModeChoice; deferred_specs(); announcement(&added, &gone);
tool_search_output(&tools, input, &mut loaded); unknown_tool_error(..); schema_hint(spec); as_mcp_call(call, &names); arguments_as_object(raw)

// oauth / store (TUI: is_callback() before sending a message to the model)
oauth::is_callback(text); oauth::LoginNeeded; oauth_store::{McpTokenStore, login_key}

// install (/mcp add)
install::parse_add_args(text) -> AddOptions; install::resolve_add(&opts, &dyn ResolveDeps) -> Resolved;
install::proposal::approval_for(&proposal, is_set) -> (AddApproval, json); added_notice(&status)
personal_file::{add_personal_server, remove_personal_server, personal_server_names}   // async add/remove
oauth::{probe_login, revoke, open_browser, can_open_browser, is_callback}; trust::McpTrust::trust (async)

// jsonrpc (shared with marv-mesh)
jsonrpc::{Message, Request, Response, Notification, encode_line, decode_line, Transport, method, PROTOCOL_VERSION}
```
