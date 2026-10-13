# marv-mesh

The mesh gives sandboxed agents deliberate, wired and auditable ways to reach each other:
- a switchboard;
- receptacles;
- plugs;
- wires in three modes: `call`, `send` and `watch`.

The design is in `docs/MESH.md`, and the security rules are in `docs/SECURITY.md`. Only `herdr.rs` has a TypeScript original, Marv's `src/herdr.ts` (spec: the **Herdr** bullet in Marv's `CLAUDE.md` and `tests/herdr.test.tsx`).

## Modules

| Source | Rust module | Status |
|---|---|---|
| new | `wire.rs` | **Done, with tests.** `WireMode`, `Approver`, `Grant`, `Wire`, and `WireTable` (`add`, `remove`, `unplug`, `find`, `allows`, `check`, `from_plug`). |
| new | `receptacle.rs` | **Done, with tests.** `ReceptacleName` (a checked name, compared exactly), `ReceptacleKind`, and `Receptacle::inbox`. |
| new | `plug.rs` | **Done, with tests.** `PlugId`, `Plug`, `SocketMount` (bound at `/run/marv/mesh.sock`), `Reachable`, and `PlugClient` (MCP over the socket: `connect` as `marv-plug`, `list`, `call`, `send`; the mode rides in `tools/call`'s `_meta` as `marv/mode`). |
| new | `message.rs` | **Done, with tests.** `Envelope` (sender, receptacle, mode, `Provenance::Mesh`, size) and `fenced()`. |
| new | `limits.rs` | **Done, with tests.** `WireLimits` (size) and `RateLimiter` (a token bucket per wire, fed the time in ms). |
| new | `approval.rs` | **Done, with tests.** `WireRequest`, `parse_approve_flag` (`--approve mesh:<receptacle>`), and `parse_wire_spec` (`a -> b:inbox`). |
| new | `switchboard.rs` | **v1 done, with tests.** `offer`, `plug` (binds the plug's 0600 socket at once, in the 0700 runtime folder), `plug_next` (`worker-N`, never reused), `wire`, `unwire`, `unplug` (closes its socket and connections), `reachable`, `admit` (the policy check, logged as a `MeshRecord` either way), `route` (inbox sends; a late refusal is logged again under the same id), `deliver`, `handle` (`initialize`, `ping`, `tools/list`, `tools/call`), `serve` (every plug's socket, including plugs added later), `drain_inbox` and `set_trajectory`. Shared as `Arc<Switchboard>`. `watch` routing and the v2/v3 kinds answer `NotServed`. |
| new | `tools.rs` | **Done, with tests.** The `mesh_list`, `mesh_call` and `mesh_send` specs, `kind_of`, and their bodies over a `PlugClient` (`target`, `list_text`, `fenced_answer`, `run`). |
| new | `discovery.rs` | `mesh_dir` and `session_socket` are done and tested. Still todo: `list` (`marv mesh ls`). |
| `src/herdr.ts` | `herdr.rs` | **Status reporting ported, with tests:** `herdr_target`, `build_request`, `HerdrReporter` (skips repeats, sends only the newest state, a rising seq, one retry) and `SocketSender`. Still todo: `HerdrAdapter`, for v2. |
| new | `agentmesh_bridge.rs` | `endpoint_ok` (loopback only), `remote_receptacle` and `BridgeConfig` are done and tested. Still todo: `connect`, `call` and `publish`, for v3. |

Run `grep -rn 'todo!("port' crates/marv-mesh` to see what's left. Each mesh todo names its milestone (`port: mesh v1: …`).

## How the rules are enforced

| Rule (MESH.md) | Where | Guard test (in-crate) |
|---|---|---|
| 1. Nothing is wired by default. Only the user wires, and yolo never does. | `WireTable::allows` returns false on an empty table. `Approver` (`Tui`, `Cli` or `Headless`) has no variant for an agent or for yolo. | `wire::default_deny`, `switchboard::unwired_messages_are_refused_and_logged` |
| A grant goes by name only. | Exact match on `PlugId`, `ReceptacleName` and `WireMode`, with no wildcards. Names are validated (lowercase, dot-separated). | `wire::a_wire_allows_exactly_its_plug_receptacle_and_mode`, `approval::approve_flags` |
| 2. Mesh text is data. | `Envelope.provenance` is always `Provenance::Mesh`, which taints. `fenced()` defuses a `</mesh-message` inside the body. | `message::*` |
| 3. Every message is logged. | `Switchboard::admit` writes a `MeshRecord` (sender, receptacle, mode, size, outcome; never the body) for each message, delivered or refused. | `switchboard::unwired_messages_are_refused_and_logged` |
| 4. Size and rate limits per wire. | `WireLimits::check_size` refuses oversized messages rather than cutting them. `RateLimiter` is keyed by (plug, receptacle, mode). | `limits::*`, `switchboard::per_wire_limits_apply` |
| 5. No ambient authority and no self-wiring. | `WireTable::add` refuses a wire to a receptacle that its own plug owns. The sender is the socket the message came in on, never a field in the message. | `wire::no_self_wiring`, `switchboard::agents_cant_wire_their_own_inbox` |
| Cross-process wires are approved on both ends. | A wire with `cross_process` counts only when `Grant.remote` is set. | `wire::cross_process_wires_need_both_ends` |
| Mesh calls are Egress. | `tools::kind_of`: `mesh_call` and `mesh_send` are `Egress`, `mesh_list` is `Read`. | `tools::specs_are_constant_and_egress_where_data_leaves` |

End to end, from inside real bwrap through the mounted socket (`tests/guards/tests/mesh.rs`, a python3 MCP client; both skip without bwrap or python3):
- `an_unwired_call_from_the_sandbox_is_refused_and_logged` (rules 1 and 3);
- `a_sandboxed_subagent_reaches_the_parent_only_over_its_wire` (rule 5: only its wires, the sender is the socket, no wiring from a plug, no other socket reachable).

The issue's integration test is `marv_session::mesh::tests::end_to_end`: a `MarvSession` with a scripted model starts a sandboxed subagent; the two exchange messages over wired inboxes, each wire asked for by the agent and granted by a person, each message fenced in the other's next request, both logged to the trajectory.

## Milestones

### v1: one Marv and its subagents

**Status (#15):** done in `marv-mesh` and `marv-session`, except where noted. Left for other issues: `marv-cli` must create the switchboard (`marv_session::mesh::start`) and pass it as `SessionInit::mesh` (with `HeadlessWires` from `--approve mesh:<r>` in `marv -p`); `marv mesh wire` needs a way to reach the running Marv's switchboard (a control socket, v2's discovery); `watch` routing.

- `Switchboard::serve`: one Unix socket per plug, mode 0600, in a private 0700 runtime folder, with newline-delimited JSON-RPC (`marv_mcp::jsonrpc`).
- `Switchboard::handle`:
  - `initialize`;
  - `tools/list`, which returns the plug's wired receptacles as tools;
  - `tools/call`, which runs `admit` and then `route`.
- Inboxes: a queue per agent, `DEFAULT_INBOX_DEPTH`. `drain_inbox` feeds the next turn. Each message reaches the model fenced and tagged `Provenance::Mesh`.
- `PlugClient` and the three model tools, in `marv-session`. Subagents get a plug with no wires. Typical wires, each approved: `worker-N -> agent.parent.inbox` (send) and `parent -> agent.worker-N.inbox` (send).
- Other crates:
  - `marv-sandbox` bind-mounts `Plug::mount()` at `/run/marv/mesh.sock`;
  - `marv-tui` shows `WireRequest` approvals;
  - `marv-cli` adds `marv mesh wire` and `--approve mesh:<r>`;
  - `marv-session` sends `MeshRecord`s to the trajectory.
- `watch`: notifications, for example "agent B changed a file you read". The API exists, and the routing comes last in v1.

### v2: several Marvs on one machine (Herdr or tmux)
- `discovery`: each switchboard also listens on `$XDG_RUNTIME_DIR/marv/mesh/<session>.sock`. `marv mesh ls` lists the live ones, checking the owner uid and mode 0600. There's no `/tmp` fallback.
- `ReceptacleKind::Peer { session }`: routes to the other switchboard. A cross-process wire needs `Grant.remote`, so it is approved on both ends.
- `HerdrAdapter`:
  - maps pane ids to sessions;
  - offers `herdr.pane.send` (through `pane send-text`, since Herdr 0.8.2 refuses `agent prompt` for Marv) and `herdr.pane.status` (call, or watch).
  - Calls go from the switchboard on the host, never from a sandbox.
- A stdio-to-socket shim, so Claude Code, opencode or jcode in a pane can plug in as MCP clients.

### v3: across machines (AgentMesh)
- `AgentMeshBridge::connect` connects to a local `agentmesh-node` (MCP on `127.0.0.1:8080`, loopback only). It republishes the services listed in `config.import` as `mesh.remote.<service>`, and `publish` exports the receptacles listed in `config.export`.
- AgentMesh adds identity (OIDC), relays and mesh-wide policy. Marv's wire approvals still apply on top: a remote receptacle is `cross_process`, so it needs both ends.
- Remote descriptions and results are stranger-written. They are `Provenance::Mesh`, fenced like any other mesh message.

## Port notes

- **Plug sockets** live in `$XDG_RUNTIME_DIR/marv/plugs/<session>/<plug>.sock` (`marv_session::mesh::start`). Without `XDG_RUNTIME_DIR` there's no mesh: no `/tmp` fallback, as for discovery.
- **The mode rides in `_meta`.** `tools/call` carries `"_meta": {"marv/mode": "send"}` (default `call`), and `tools/list` gives each receptacle's wired modes as `"_meta": {"marv/modes": [...]}`. A refusal is a tool result with `isError`, so the agent reads it.
- **The model's tools use the socket too.** `MeshTool` runs in Marv, but it connects to its agent's own plug socket like anything in the sandbox would, so every message from a model goes through the same checks, identity and log.
- **Asking for a wire.** A `mesh_send`/`mesh_call` on a receptacle that exists, takes the mode and isn't the agent's own asks the session's `WireApprover` first (a person: `once`, `must_ask`, tool `mesh_wire`); on a no, the message still goes to the switchboard, which refuses and logs it. A wire to an agent's own inbox is never asked for. Yolo may *use* a granted wire (`auto_safe` when wired; tainted sessions still ask), never grant one.
- **Inboxes are read** before the turn's request (ahead of the user's message) and after each step (`with_inbox`, after the checks' note). Messages still unread when a subagent ends go with its plug. A message that arrives after a final answer waits for the next turn.

- `herdr.rs` replaces Marv's single-flight promise with a `watch::Sender<bool>` for "sending" plus one drain task. The effect is the same: only the newest queued state is sent, and `settled()` waits for the drain. `state()` needs a Tokio runtime.
- `HerdrReporter::release` drops any queued state, waits for the drain, then sends `pane.release_agent`. The CLI bounds it to 1 s, as in Marv.
- `marv-mesh` depends on `marv-mcp`, but only for `jsonrpc` and `Env`.

## Interfaces other crates use

```rust
// switchboard (marv-session owns one per process: marv_session::mesh::start(session, &env, cancel) -> Option<MeshHandle>)
// Shared as Arc<Switchboard>: every method takes &self; serve/route/deliver/handle take the Arc.
Switchboard::new(session, runtime_dir, Arc<dyn TrajectorySink>)
  .table() -> WireTable (snapshot); .receptacle(&name) -> Option<Receptacle> (clone)
  .plug(PlugId) -> Result<Plug>; .plug_next("worker") -> Result<Plug>   // Plug::mount() for marv-sandbox
  .offer(Receptacle); .wire(Wire) -> Result; .unwire(..); .unplug(&PlugId)
  .reachable(&PlugId) -> Vec<Reachable>    // mesh_list
  .admit(&from, &receptacle, mode, body, now_ms) -> Result<Envelope>
  async Arc::clone(&sb).route(Envelope); async sb.deliver(&from, name, mode, body); async sb.handle(&from, Request)
  async Arc::clone(&sb).serve(cancel); .drain_inbox(&PlugId) -> Vec<Envelope>; .set_trajectory(Arc<dyn TrajectorySink>)
trait TrajectorySink { fn mesh(&self, MeshRecord); }

// approvals (marv-tui / marv-cli)
approval::{WireRequest, parse_approve_flag, parse_wire_spec}; wire::{Approver, Grant, Wire, WireMode, WireTable}
Envelope::fenced() -> String              // what the receiving model sees; tag the turn Provenance::Mesh

// model tools (marv-session wraps them in mesh::MeshTool)
tools::{mesh_specs, kind_of, target, run, MESH_LIST, MESH_CALL, MESH_SEND}; plug::PlugClient::{connect, list, call, send}

// marv-session::mesh (for marv-cli and the TUI)
start, log_to, PARENT, parent_id, WireApprover, PersonWires { inner, by }, HeadlessWires(Vec<ReceptacleName>), wire_approval,
wire_approver, headless_grants, inbox_note, with_inbox, MeshAccess::plug_subagent -> SubagentPlug { tools, sandbox, after_step }

// Herdr (marv-cli makes one, the App reports)
herdr::{herdr_target(&env), HerdrReporter::new(target, None), .state(s, msg), async .session(id, path), async .release()}
```
