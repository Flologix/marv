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
| new | `switchboard.rs` | **v1 and v2 done, with tests.** `offer`, `plug` (binds the plug's 0600 socket at once, in the 0700 runtime folder), `plug_next` (`worker-N`, never reused), `wire`, `unwire`, `unplug` (closes its socket and connections), `reachable`, `admit` (the policy check, logged as a `MeshRecord` either way), `route` (inbox sends; a late refusal is logged again under the same id), `deliver`, `handle` (`initialize`, `ping`, `tools/list`, `tools/call`), `serve` (every plug's socket, including plugs added later), `drain_inbox` and `set_trajectory`. Shared as `Arc<Switchboard>`. v2: `Service` (what serves a receptacle that isn't an inbox) with `offer_service`, `Resolve` with `add_resolver` and `lookup` (receptacles found on demand), `grant` (asks the far end through `Service::approve_remote` for a cross-process wire), `agents`, `has_plug`; `wire` makes any `peer.*`, peer or remote wire cross-process. `watch` routing answers `NotServed`. |
| new | `tools.rs` | **Done, with tests.** The `mesh_list`, `mesh_call` and `mesh_send` specs, `kind_of`, and their bodies over a `PlugClient` (`target`, `list_text`, `fenced_answer`, `run`). |
| new | `discovery.rs` | **Done, with tests (#16).** `mesh_dir`, `session_socket`, `mesh_name`, `private_folder`, `check_socket`, `list` (`marv mesh ls`), `DoorClient`, `open_door` (`marv/info`, `marv/plug`, `marv/connect`, `marv/wire`) with `PeerApprover`, `PeerResolver` and `PeerLink` (`peer.<session>.agent.<agent>.inbox`), `peer_receptacle`, `parse_peer`, `peer_plug`. |
| `src/herdr.ts` | `herdr.rs` | **Status reporting ported, with tests:** `herdr_target`, `build_request`, `HerdrReporter` (skips repeats, sends only the newest state, a rising seq, one retry) and `SocketSender`. **v2 adapter done, with tests (#16):** `HerdrAdapter` (`agents`, `pane`, `receptacles`, `send_text`, `status`), `HerdrPanes` (resolver), `PaneService`, `pane_segment`/`pane_from_segment`, `pane_receptacle`/`parse_pane_receptacle`. |
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
| v2: a wire to another Marv needs its user too. | `Switchboard::grant` asks the far door (`PeerLink::approve_remote`), which asks its person; `Switchboard::wire` makes every `peer.*` wire cross-process, so a one-ended grant doesn't count. | `discovery::a_cross_process_wire_needs_both_users_then_carries_messages`, `discovery::the_far_user_saying_no_leaves_it_unwired` (proven by reverting the cross-process forcing) |
| v2: a peer is known by its connection. | `marv/wire` checks the caller's pid (`SO_PEERCRED`) against the process listening on `<from>.sock`; its messages then arrive on a plug of its own (`peer-<from>`). | `discovery::the_far_end_checks_the_caller_by_pid` (proven by reverting the check) |
| v2: the door can't hand out another agent's plug. | `marv/plug` takes new names only, never `parent`, `worker-*` or `peer-*`. | `discovery::the_door_plugs_new_names_only_and_unplugs_them_on_leaving` |
| v2: discovery is private. | The folder is 0700 and this user's (else `list` refuses it); sockets must be 0600 and this user's to be connected to; no `/tmp` fallback; sandboxes see an empty `/run` and `/tmp`. | `discovery::ls_lists_live_private_doors_only`, guards' `sandbox.rs::system_is_read_only_and_run_is_empty_but_for_the_mesh_plug` |
| v2: Herdr sends go to one approved pane, to an agent, never into an approval. | One receptacle per pane and action; agent panes only, never Marv's own; sends only to an `idle` or `done` agent; control characters dropped, newlines as spaces. | `herdr::a_wired_send_types_clean_text_into_an_idle_agent_only` (proven by reverting the state check), `herdr::agent_panes_are_offered_but_never_marvs_own_or_a_shell` |

End to end, from inside real bwrap through the mounted socket (`tests/guards/tests/mesh.rs`, a python3 MCP client; both skip without bwrap or python3):
- `an_unwired_call_from_the_sandbox_is_refused_and_logged` (rules 1 and 3);
- `a_sandboxed_subagent_reaches_the_parent_only_over_its_wire` (rule 5: only its wires, the sender is the socket, no wiring from a plug, no other socket reachable).

v2's integration test is `crates/marv-cli/tests/mesh.rs` (#16): two `marv -p` processes, each with the oracle's fake model and real bwrap, wire to each other (`--approve mesh:<r>` on both ends) and exchange a message each way; each arrives fenced from `peer-<name>`. Without the far end's approval nothing arrives and the asking agent reads why.

v1's integration test is `marv_session::mesh::tests::end_to_end`: a `MarvSession` with a scripted model starts a sandboxed subagent; the two exchange messages over wired inboxes, each wire asked for by the agent and granted by a person, each message fenced in the other's next request, both logged to the trajectory.

## Milestones

### v1: one Marv and its subagents

**Status (#15):** done in `marv-mesh` and `marv-session`, except where noted. Left: `watch` routing. (#16 did the rest: `marv-cli` creates the switchboard and passes it as `SessionInit::mesh`, with `HeadlessWires` from `--approve mesh:<r>` in `marv -p`, and `marv mesh wire` reaches a running Marv through its door.)

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

**Status (#16):** done in `marv-mesh`, `marv-session` and `marv-cli`. Left: `watch` (Herdr pane status as notifications), `notifications/tools/list_changed` to plugged-in MCP clients when their wires change, unwiring across processes (a peer's plug and wires last until the far Marv ends), a tmux adapter.

- `discovery`: each switchboard also listens on `$XDG_RUNTIME_DIR/marv/mesh/<name>.sock`, its door. `marv mesh ls` lists the live ones, checking the owner uid and mode 0600. There's no `/tmp` fallback.
- `ReceptacleKind::Peer { session }`: `peer.<session>.agent.<agent>.inbox`, found on demand, routes to the other switchboard through a plug that switchboard gave this one. A cross-process wire needs `Grant.remote`, so it is approved on both ends.
- `HerdrAdapter`:
  - maps pane ids to Marvs (`marv mesh ls` shows each Marv's pane, and which agent panes are Marvs);
  - offers `herdr.pane.<pane>.send` (through `pane send-text` then `pane send-keys enter`, since Herdr 0.8.2 refuses `agent prompt` for Marv) and `herdr.pane.<pane>.status` (call; watch later).
  - Calls go from the switchboard on the host, never from a sandbox.
- A stdio-to-socket shim, `marv mesh plug <marv> [--as <name>]`, so Claude Code, opencode or jcode in a pane can plug in as MCP clients.

#### Try it: two Marvs in two Herdr panes

1. In two Herdr panes, start two Marvs (TUI), each in its own project: `MARV_MESH_NAME=api marv` and `MARV_MESH_NAME=web marv`. Each shows "On the mesh as …".
2. In a third pane: `marv mesh ls`. Both are listed, with their panes, and the Herdr agent panes with their `herdr.pane.*` names.
3. Ask for a wire: `marv mesh wire 'api -> web:inbox'`. `api` asks you to approve `Wire parent -> peer.web.agent.parent.inbox (send)`; then `web` asks about `Wire peer-api -> agent.parent.inbox (send)`. Say yes in both.
   (Or let the agent ask: tell `api` "use mesh_send to tell peer.web.agent.parent.inbox that the API is ready". The same two approvals come up.)
4. In `api`: "tell peer.web.agent.parent.inbox that the API is ready". `web` reads it fenced, from `peer-api`, at its next turn (send `web` any prompt, e.g. "any messages?"), and its session is tainted.
5. Herdr: in `api`, "use mesh_call on herdr.pane.<web's pane>.status" (the name from `marv mesh ls`). Approve the wire (`api -> herdr.pane.….status:call`); the answer is the pane's agent and state. `herdr.pane.….send` types a prompt into an idle agent's pane, as if you typed it.
6. Claude Code in another pane: add an MCP server with command `marv` and args `["mesh", "plug", "web", "--as", "claude"]`, then `marv mesh wire 'web/claude -> web:inbox'` and approve it in `web`. Restart the MCP server so Claude Code lists the new tool (`agent_parent_inbox`; list-changed notifications are still to come).

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

- **Mesh names (v2).** A process is on the mesh as `MARV_MESH_NAME`, else its project folder's name and four hex digits (`api-3f9a`). The name belongs to the process, not the conversation (session ids change with `/clear` and resume, and have uppercase letters), and is at most 43 characters so `peer-<name>` stays one segment.
- **`marv mesh wire` asks, it doesn't approve (differs from MESH.md rule 1).** Any process of the user's can run it, including an unsandboxed agent in another pane, so each end's Marv asks its person (the TUI's queue) or its `--approve mesh:<receptacle>` list. The CLI form is `<marv>[/<agent>] -> <marv>[/<agent>]:inbox`, since a bare `worker-1` can't say which Marv it's in.
- **A peer is the whole other Marv.** The far end sees every message from Marv `a` as from its plug `peer-a`; which of `a`'s agents may use the wire is `a`'s wire table's business. Only inboxes take cross-process wires.
- **Herdr receptacles are per pane** (`herdr.pane.<pane>.send`), not MESH.md's single `herdr.pane.send` with the pane in the message: a grant is decided on names only, so approving it approves typing into that one pane. Pane ids become one segment: `w2T:p1` is `w2_t-p1`.
- **The shim translates names.** MCP clients take tool names of `[A-Za-z0-9_-]` only, so `marv mesh plug` shows `agent.parent.inbox` as `agent_parent_inbox`, and a call to a send-only receptacle goes as a send.
- **`marv -p` is on the mesh only when asked** (`--approve mesh:<r>` or `MARV_MESH_NAME`); the TUI always is, with `XDG_RUNTIME_DIR`.
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

// v2: services, discovery and the Herdr adapter (marv-cli's mesh_cmd::start / Mesh::open_door)
switchboard::{Service { deliver, approve_remote }, Resolve { resolve }}; sb.offer_service(r, svc); sb.add_resolver(res)
  async sb.lookup(&name) -> Option<Receptacle>; async sb.grant(WireRequest, Approver) -> Result; sb.agents(); sb.has_plug(&id)
discovery::{mesh_dir, mesh_name(&env, root), session_socket, list(&dir), open_door(sb, &dir, DoorInfo, Arc<dyn PeerApprover>) -> DoorGuard,
  DoorClient::{connect, request, info}, peer_receptacle, parse_peer, peer_plug, check_socket}
herdr::{HerdrAdapter::new(program, own_pane), HerdrPanes(Arc<HerdrAdapter>), pane_receptacle, PaneAction}
```
