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
| new | `message.rs` | **Done, with tests.** `Envelope` (sender, receptacle, mode, `Provenance::Mesh`, size), `fenced()`, and `send_text()`/`text_body` (a JSON string or `{"text": "..."}` alone; anything else is `expected {"text": "..."}`, #123). |
| new | `limits.rs` | **Done, with tests.** `WireLimits` (size) and `RateLimiter` (a token bucket per wire, fed the time in ms). |
| new | `approval.rs` | **Done, with tests.** `WireRequest`, `parse_approve_flag` (`--approve mesh:<receptacle>`), and `parse_wire_spec` (`a -> b:inbox`). |
| new | `switchboard.rs` | **v1 and v2 done, with tests.** `offer`, `plug` (binds the plug's 0600 socket at once, in the 0700 runtime folder), `plug_next` (`worker-N`, never reused), `wire`, `unwire`, `unplug` (closes its socket and connections), `reachable`, `admit` (the policy check, logged as a `MeshRecord` either way), `route` (inbox sends; a late refusal is logged again under the same id), `deliver`, `handle` (`initialize`, `ping`, `tools/list`, `tools/call`), `serve` (every plug's socket, including plugs added later), `drain_inbox` and `set_trajectory`, `set_herdr`/`herdr` (the Herdr adapter `mesh_list` lists panes from, also added as a resolver). Shared as `Arc<Switchboard>`. v2: `Service` (what serves a receptacle that isn't an inbox) with `offer_service`, `Resolve` with `add_resolver` and `lookup` (receptacles found on demand), `grant` (asks the far end through `Service::approve_remote` for a cross-process wire), `agents`, `has_plug`; `wire` makes any `peer.*`, peer or remote wire cross-process. `watch` routing answers `NotServed`. |
| new | `tools.rs` | **Done, with tests.** The `mesh_list`, `mesh_call` and `mesh_send` specs, `kind_of`, and their bodies over a `PlugClient` (`target`, `Wireable`, `list_answer`, `fenced_answer`, `run`, which says whether its `Answer` carries words from the mesh). `mesh_list` shows the wires (or, with none, the names a first `mesh_send`/`mesh_call` can use: `peer.<marv>.agent.parent.inbox`, `herdr.pane.<pane>.send|status`), the caller's own inbox and its `peer.<session>.…` name, and what can be wired (#76): the other Marvs on the machine (names and inboxes) and, in Herdr, the agent panes (receptacles, agent kind and state; plain identifiers only, else `?`; never a pane's given name or folder), less what's already wired. The hint says `.status` with `lines: N` reads the pane; a pane whose Marv is listed by name offers `.status` only and names that Marv's inbox for a reply (#123; `.send` still resolves when named). Only a list of wired receptacles, whose descriptions can come from another Marv, is `Provenance::Mesh`. `MeshTool::wireable` looks for the Marvs and the panes at once, bounded (DOOR_TIMEOUT per door, `herdr::LIST_TIMEOUT` for Herdr) and cut short by a stopped turn. |
| new | `discovery.rs` | **Done, with tests (#16).** `mesh_dir`, `session_socket`, `mesh_name`, `private_folder`, `check_socket`, `list` (`marv mesh ls`), `DoorClient`, `open_door` (first removes the sockets nothing listens on: a killed Marv's door outlives it; records the folder for `mesh_list`, `Switchboard::peer_dir`) (`marv/info`, `marv/plug`, `marv/connect`, `marv/wire`) with `PeerApprover`, `PeerResolver` and `PeerLink` (`peer.<session>.agent.<agent>.inbox`), `peer_receptacle`, `parse_peer`, `peer_plug`. |
| `src/herdr.ts` | `herdr.rs` | **Status reporting ported, with tests:** `herdr_target`, `build_request`, `HerdrReporter` (skips repeats, sends only the newest state, a rising seq, one retry) and `SocketSender`. **v2 adapter done, with tests (#16):** `HerdrAdapter` (`agents`, `pane`, `receptacles`, `wireable_panes` for `mesh_list`, within `LIST_TIMEOUT`, `send_text`, `status`), `pane_prompt` (a send's text, a prompt never a command), `HerdrPanes` (resolver), `PaneService`, `pane_segment`/`pane_from_segment`, `pane_receptacle`/`parse_pane_receptacle`. **The Herdr door's commands (#74):** `HerdrAdapter::{call, list, read, open_pane, worktree, pane_run, agent_start, wait_ready, focus, rename, stop}`, `HerdrList`, `Place`, `TargetKind`/`target_kind`, `plain_id`, `READY_TIMEOUT` (the door's tool lives in marv-session's `herdr_door`). `stop` types `/exit` past `pane_prompt`: only the user's `/peers stop` calls it. |
| new | `radio.rs` | **Phases 1 and 2 done, with tests (#124; `docs/specs/2026-10-14-mesh-radio.md`).** Phase 2 adds `msg` frames: `parse_frame` returns a `Frame` (presence or `RadioMessage`; a message is exactly its fields, its id `<link>:<seq>`, `by` user or model, text at most `max_text` bytes, refused not cut), `Radio::send_msg` (`To::All`/`To::Names`, the short name via `resolve`; refused whole when off, empty, over 2 KB or naming a Marv not on the radio; per peer when over the mirrored rate or the link's queue is full; one `RadioFrameRecord` per peer, never the text), the receiver's `rate_in` and the switchboard's radio queue (`deliver_radio`, `unread_radio`, `drain_radio`), `REFUSED` with a `RefusedCode` told back, `RadioLimits` (`with_limits` for tests), each link's writer and send queue (`Link::push`, `SEND_QUEUE`), `radio_note`/`RADIO_NOTE_HEADER`/`RADIO_FENCE_TAG`, `traffic_list` for the TUI, `MAX_WAKE_HOP`. Phase 1: presence only. `Radio` (one per Marv with an open door, off until `turn_on`; `turn_off` sends `left` and closes the links; `start` runs the liveness ticks and the 30 s re-list), `Presence` (exactly name, state, title, project, pane, since_ms, wake, version; `deny_unknown_fields`, `check`, `parse`; `clean_title` 56 chars, `clean_project` the last path component), `parse_frame` (a `from` is refused, the presence must name the link's peer, presence kind only), `accept_join` (on, a presence that keeps the rules, one link per pair: the name that sorts first keeps its link on a simultaneous join), the links (a writer per link: presence coalesced, at most one per `presence_gap`, a heartbeat every 15 s, `left` at once on turning off; a reader per link: `silent` at 45 s, dropped at 120 s, `lost` on an end without `left`, which sweeps the folder), `roster` (the TUI's), `listed` (names, state words, pane segments for `mesh_list`), `RadioRecord` (`joined`, `left`, `lost`, `silent`, `dropped`, `refused`; never a heartbeat or a title), `RadioTimings`. Phases 2 and 3 (messages, requests) aren't built. |
| new | `agentmesh_bridge.rs` | `endpoint_ok` (loopback only), `remote_receptacle` and `BridgeConfig` are done and tested. Still todo: `connect`, `call` and `publish`, for v3. |

Run `grep -rn 'todo!("port' crates/marv-mesh` to see what's left. Each mesh todo names its milestone (`port: mesh v1: …`).

## How the rules are enforced

| Rule (MESH.md) | Where | Guard test (in-crate) |
|---|---|---|
| 1. Nothing is wired by default. Only the user wires, and yolo never does. | `WireTable::allows` returns false on an empty table. `Approver` (`Tui`, `Cli` or `Headless`) has no variant for an agent or for yolo. The one prompt for a call and its new wire (#77) is asked of the person, `once` and `must_ask`, in yolo too. | `wire::default_deny`, `switchboard::unwired_messages_are_refused_and_logged`, guards' `mesh::the_one_prompt_for_a_call_and_its_new_wire_is_a_persons_yes_or_no` |
| A grant goes by name only. | Exact match on `PlugId`, `ReceptacleName` and `WireMode`, with no wildcards. Names are validated (lowercase, dot-separated). | `wire::a_wire_allows_exactly_its_plug_receptacle_and_mode`, `approval::approve_flags` |
| 2. Mesh text is data. | `Envelope.provenance` is always `Provenance::Mesh`, which taints. `fenced()` defuses a `</mesh-message` inside the body. | `message::*` |
| 3. Every message is logged. | `Switchboard::admit` writes a `MeshRecord` (sender, receptacle, mode, size, outcome; never the body) for each message, delivered or refused. | `switchboard::unwired_messages_are_refused_and_logged` |
| A message that must be text is text (#123). | `Board::check` refuses a `send` (an inbox, another Marv's inbox) or a `herdr.pane.<pane>.send` whose body isn't a JSON string or `{"text": "..."}` alone, before it's queued, forwarded or typed; `PaneService` and `PeerLink` take `Envelope::send_text`. `tools::check` (`MeshTool::check`) refuses a pane send's bad arguments before any wire prompt. | `message::send_text_takes_a_string_or_a_lone_text_field_and_refuses_the_rest`, `switchboard::an_inbox_send_that_isnt_text_is_refused_not_queued`, `herdr::a_send_that_isnt_text_is_refused_and_nothing_is_typed`, `tools::a_pane_send_must_be_text_before_anything_is_asked`, `session::mesh::tests::a_pane_send_that_isnt_text_is_invalid_input_and_asks_nothing` |
| 4. Size and rate limits per wire. | `WireLimits::check_size` refuses oversized messages rather than cutting them. `RateLimiter` is keyed by (plug, receptacle, mode). | `limits::*`, `switchboard::per_wire_limits_apply` |
| 5. No ambient authority and no self-wiring. | `WireTable::add` refuses a wire to a receptacle that its own plug owns. The sender is the socket the message came in on, never a field in the message. | `wire::no_self_wiring`, `switchboard::agents_cant_wire_their_own_inbox` |
| Cross-process wires are approved on both ends. | A wire with `cross_process` counts only when `Grant.remote` is set. | `wire::cross_process_wires_need_both_ends` |
| Mesh calls are Egress. | `tools::kind_of`: `mesh_call` and `mesh_send` are `Egress`, `mesh_list` is `Read`. | `tools::specs_are_constant_and_egress_where_data_leaves` |
| Mesh text taints; Marv's own doesn't. | `MeshTool::run` marks a call's answer and a list of wired receptacles `Provenance::Mesh`; an empty list, a send's acknowledgement, the other Marvs' names (valid mesh names only, no folders) and the Herdr panes' receptacles, agent kinds and states (plain identifiers only, no given names or folders) are Marv's. | `session::mesh::tests::mesh_list_names_the_other_marvs_and_taints_only_with_wired_descriptions`, `session::mesh::tests::mesh_list_shows_the_wireable_marvs_and_herdr_panes_and_a_send_there_asks_to_wire_it`, `tools::peers_are_listed_by_name_without_this_marv_or_its_links`, `tools::herdr_panes_are_listed_with_their_receptacles_kind_and_state_but_not_their_words` |
| v2: a wire to another Marv needs its user too. | `Switchboard::grant` asks the far door (`PeerLink::approve_remote`), which asks its person; `Switchboard::wire` makes every `peer.*` wire cross-process, so a one-ended grant doesn't count. | `discovery::a_cross_process_wire_needs_both_users_then_carries_messages`, `discovery::the_far_user_saying_no_leaves_it_unwired` (proven by reverting the cross-process forcing) |
| v2: a peer is known by its connection. | `marv/wire` checks the caller's pid (`SO_PEERCRED`) against the process listening on `<from>.sock`; its messages then arrive on a plug of its own (`peer-<from>`). | `discovery::the_far_end_checks_the_caller_by_pid` (proven by reverting the check) |
| v2: the door can't hand out another agent's plug. | `marv/plug` takes new names only, never `parent`, `worker-*` or `peer-*`. | `discovery::the_door_plugs_new_names_only_and_unplugs_them_on_leaving` |
| v2: discovery is private. | The folder is 0700 and this user's (else `list` refuses it); sockets must be 0600 and this user's to be connected to; no `/tmp` fallback; sandboxes see an empty `/run` and `/tmp`. | `discovery::ls_lists_live_private_doors_only`, guards' `sandbox.rs::system_is_read_only_and_run_is_empty_but_for_the_mesh_plug` |
| Radio (#124): off unless the user's own config or `/radio on`; both ends opt in. | `Door::radio_join` refuses without a radio (`marv -p`); `Radio::accept_join` refuses while off. No env variable, flag or project config reaches `mesh.radio` (marv-session). | guards' `radio.rs::a_marv_with_the_radio_off_refuses_joins_and_sends_no_presence`, `radio.rs::the_radio_is_off_unless_the_users_config_turns_it_on`, `radio.rs::a_projects_config_cant_turn_the_radio_on` |
| Radio: a join comes from the Marv it names; a frame's sender is its link. | `radio_join` checks the caller's pid against `<from>.sock` (`confirm_caller`, shared with `marv/wire`); `parse_frame` refuses a `from` and a presence naming another Marv. | guards' `radio.rs::a_join_from_a_process_that_isnt_the_named_marv_is_refused`, `radio.rs::a_frame_that_names_its_own_sender_is_refused` |
| Radio: presence is exactly its fields, and never reaches a model. | `Presence` (`deny_unknown_fields`, `check`); the TUI draws it through `printable()`; `mesh_list` gets `Radio::listed` (names and state words) and stays untainted. | guards' `radio.rs::presence_has_exactly_the_listed_fields`, `radio.rs::presence_never_reaches_a_model_and_doesnt_taint`, `tools::the_radio_section_names_who_is_on_it_and_nothing_more` |
| Radio: liveness. | Heartbeats; `silent` after 45 s, dropped after 120 s; `lost` sweeps the folder under its lock (`discovery::sweep_locked`). | guards' `radio.rs::a_silent_peer_greys_then_drops`, `radio.rs::a_killed_peer_shows_lost_and_its_socket_is_swept`, `radio::tests::*` (coalescing, one link per pair, the re-list) |
| Radio (phase 2): a message is exactly its fields, from its link, at most 2 KB (refused, never cut). | `parse_frame` (`MsgFields`, `deny_unknown_fields`; id `<link>:<seq>`; `by` user or model); `Radio::send_msg` refuses over `max_text_bytes` and over the frame line. | guards' `radio.rs::oversize_messages_are_refused_not_cut`, `radio::tests::a_message_is_exactly_its_fields_from_its_link_and_never_cut` |
| Radio: per-sender rates at the receiver; a full queue refuses, never stalls. | `Peer::rate_in` (20 a minute, bursts of 5) and the radio queue's depth in `Radio::receive`; `rate_out` mirrors the rate at the sender; `Link::push` refuses past `send_queue`, and each link's `write_loop` writes on its own. | guards' `radio.rs::per_sender_rates_apply_at_the_receiver`, `radio.rs::a_slow_peer_never_stalls_the_sender`, `radio::tests::a_full_radio_queue_refuses_and_tells_the_sender` |
| Radio: received text is fenced data; nothing of a frame reaches an approver; every frame is logged without its text. | `RadioMessage::fenced` (defused closing tag) and `radio_note`, read by marv-session's `inbox_note` (tainting); the frame path only queues; `RadioFrameRecord` has no text field. | guards' `radio.rs::a_body_cant_close_the_radio_fence`, `radio.rs::nothing_on_the_radio_can_answer_an_approval`, `radio.rs::every_frame_is_logged_without_its_text` |
| v2: Herdr sends go to one approved pane, to an agent, never into an approval. | One receptacle per pane and action; agent panes only, never Marv's own; sends only to an `idle` or `done` agent; control characters dropped, newlines as spaces. | `herdr::a_wired_send_types_clean_text_into_an_idle_agent_only` (proven by reverting the state check), `herdr::agent_panes_are_offered_but_never_marvs_own_or_a_shell` |
| A pane send is a prompt, not a command. | `herdr::pane_prompt` (the one path that cleans the text `send_text` types) refuses text whose first visible character is `/`, `!` or `#`, whole, before `pane send-text`; `tools::check` runs it too, so no wire is asked for. Every way into a pane (`mesh_call`, a `marv mesh wire`d plug, the `marv mesh plug` shim's `tools/call`) reaches `PaneService` and so `send_text`. | `herdr::a_send_that_starts_like_a_command_is_refused_and_nothing_is_typed`, `herdr::a_pane_prompt_is_cleaned_and_a_command_or_nothing_is_refused`, `tools::a_pane_send_of_a_command_is_refused_before_anything_is_asked`, `session::mesh::tests::a_pane_send_of_a_command_is_invalid_input_and_asks_nothing` (all proven by reverting the check) |
| A pane send lands only on a free prompt. | A Marv's TUI tells its port when anything but an empty prompt has the keyboard (`SessionPort::keyboard_held`: the splash, setup, the session picker, the palette, the radio's roster, a half-typed slash command); `Activity::herdr_state` reports working rather than idle then (the radio's presence is unchanged). `HerdrAdapter::send_text` sends one at a time and refuses a pane typed into within `SEND_GAP`. | `app::tests::the_port_hears_when_something_but_an_empty_prompt_has_the_keyboard`, `tui::tests::a_held_keyboard_is_working_to_herdr_and_free_again_is_idle`, `herdr::a_pane_just_sent_a_prompt_takes_no_other_for_a_while` |

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

- `discovery`: each switchboard also listens on `<runtime>/mesh/<name>.sock`, its door (`marv_sandbox::runtime_dir`: `$XDG_RUNTIME_DIR/marv` on Linux, `/tmp/marv-<uid>` on macOS, or `MARV_RUNTIME_DIR`). `marv mesh ls` lists the live ones, checking the owner uid and mode 0600. There's no `/tmp` fallback.
- `ReceptacleKind::Peer { session }`: `peer.<session>.agent.<agent>.inbox`, found on demand, routes to the other switchboard through a plug that switchboard gave this one. A cross-process wire needs `Grant.remote`, so it is approved on both ends.
- `HerdrAdapter`:
  - maps pane ids to Marvs (`marv mesh ls` shows each Marv's pane, and which agent panes are Marvs);
  - offers `herdr.pane.<pane>.send` (through `pane send-text` then `pane send-keys enter`, since Herdr 0.8.2 refuses `agent prompt` for Marv) and `herdr.pane.<pane>.status` (call; watch later; with `lines: N` it returns the pane's last N lines, so it is how a pane is read: kept one receptacle rather than a separate `.read`, so one wire reads both, #123). A `.send` takes `{"text": "..."}` alone, and a prompt only: text starting with `/`, `!` or `#` is refused (`pane_prompt`).
  - Calls go from the switchboard on the host, never from a sandbox.
- A stdio-to-socket shim, `marv mesh plug <marv> [--as <name>]`, so Claude Code, opencode or jcode in a pane can plug in as MCP clients.

#### Try it: two Marvs in two Herdr panes

1. In two Herdr panes, start two Marvs (TUI), each in its own project: `MARV_MESH_NAME=api marv` and `MARV_MESH_NAME=web marv`. Each shows "On the mesh as …".
2. In a third pane: `marv mesh ls`. Both are listed, with their panes, and the Herdr agent panes with their `herdr.pane.*` names.
3. Ask for a wire: `marv mesh wire 'api -> web:inbox'`. `api` asks you to approve `Wire parent -> peer.web.agent.parent.inbox (send)`; then `web` asks about `Wire peer-api -> agent.parent.inbox (send)`. Say yes in both.
   (Or let the agent ask: tell `api` "use mesh_send to tell peer.web.agent.parent.inbox that the API is ready". `api` asks once, for the message and the wire together ("Yes (wire it)"); then `web` asks about its side, as above.)
4. In `api`: "tell peer.web.agent.parent.inbox that the API is ready". `web` reads it fenced, from `peer-api`, at its next turn (send `web` any prompt, e.g. "any messages?"), and its session is tainted.
5. Herdr: in `api`, "use mesh_call on herdr.pane.<web's pane>.status" (the name from `marv mesh ls`). Approve the wire (`api -> herdr.pane.….status:call`); the answer is the pane's agent and state. `herdr.pane.….send` types a prompt into an idle agent's pane, as if you typed it (a prompt only: text starting with `/`, `!` or `#` is refused).
6. Claude Code in another pane: add an MCP server with command `marv` and args `["mesh", "plug", "web", "--as", "claude"]`, then `marv mesh wire 'web/claude -> web:inbox'` and approve it in `web`. Restart the MCP server so Claude Code lists the new tool (`agent_parent_inbox`; list-changed notifications are still to come).

### v3: across machines (AgentMesh)
- `AgentMeshBridge::connect` connects to a local `agentmesh-node` (MCP on `127.0.0.1:8080`, loopback only). It republishes the services listed in `config.import` as `mesh.remote.<service>`, and `publish` exports the receptacles listed in `config.export`.
- AgentMesh adds identity (OIDC), relays and mesh-wide policy. Marv's wire approvals still apply on top: a remote receptacle is `cross_process`, so it needs both ends.
- Remote descriptions and results are stranger-written. They are `Provenance::Mesh`, fenced like any other mesh message.

## Port notes

- **Plug sockets** live in `<runtime>/p/<pid>-<hex>/<plug>.sock` (`marv_session::mesh::start`, a folder per switchboard made exclusively: Marv desktop runs one per chat in one process, and a dead process's folder is never reused; #162, was `plugs/<session>/`, which a long session name pushed past `sun_path`). Without a runtime folder there's no mesh: no `/tmp` fallback, as for discovery. Both bind sites refuse a path over `sun_path` with a message (`marv_sandbox::runtime::check_socket_path`).
- **The mode rides in `_meta`.** `tools/call` carries `"_meta": {"marv/mode": "send"}` (default `call`), and `tools/list` gives each receptacle's wired modes as `"_meta": {"marv/modes": [...]}`. A refusal is a tool result with `isError`, so the agent reads it.
- **The model's tools use the socket too.** `MeshTool` runs in Marv, but it connects to its agent's own plug socket like anything in the sandbox would, so every message from a model goes through the same checks, identity and log.
- **Asking for a wire.** A `mesh_send`/`mesh_call` on a receptacle that exists, takes the mode and isn't the agent's own asks the session's `WireApprover` first. A wire to an agent's own inbox is never asked for. Yolo may *use* a granted wire (`auto_safe` when wired; tainted sessions still ask), never grant one.
  - **One prompt per new wire (#77).** Through `run_tool`, such a call is approved by `MeshTool::approve_itself` (`Tool::approve_itself`, which stands in for the registry's prompt) asking `WireApprover::approve_call`: for a person (`PersonWires`, the client's own approver, never the session's "always" layer or yolo) one prompt, `mesh::call_wire_approval` (tool `mesh_wire_call`, `once`, `must_ask`), with the payload, the wire, its limits and the taint warning. The TUI offers "Yes (wire it)" and "No" only. Yes grants the wire (`Switchboard::grant`; the far end asks its own user) and runs the call over it; no is a declined call, and nothing is wired or sent. A round trip between two Marvs' users is four prompts (it was six).
  - Where the approver can't show both (`HeadlessWires` for `--approve mesh:<receptacle>`, an SDK client's own `wire_approver`), the call is approved as any other (in `marv -p`, by the `mesh:<receptacle>:` scope `--approve mesh:<receptacle>` adds) and the wire asked for apart (`approve_wire`: `once`, `must_ask`, tool `mesh_wire`); on a no, the message still goes to the switchboard, which refuses and logs it. The far end's prompt and `marv mesh wire` are this `mesh_wire` prompt too.
- **Inboxes are read** before the turn's request (ahead of the user's message) and after each step (`with_inbox`, after the checks' note). Messages still unread when a subagent ends go with its plug. A message that arrives after a final answer waits for the next turn.
- **Wake (#78, opt-in `mesh.wake`).** A delivery into an inbox rings `Switchboard::mail`; `Switchboard::unread` lists the unread messages' wires without taking them. The TUI's bridge (`watch_mesh`) waits on the bell and, when wake is on, nothing runs and no approval waits, asks `marv_session::mesh::wake_check` (the main agent's oldest unread message on a wire still in the table, with a wake-up left: `WAKES_PER_MINUTE` per wire, told once when over) and starts `MarvSession::wake`: a turn with no user message whose inbox is drained as above (fenced, tainting before the first request), then `WAKE_NOTE`; the transcript gets "Woken by a message from <sender> on <receptacle>" as Marv's line. A message that arrives during a turn or an approval isn't retried: it waits for the next turn. `marv -p` and SDK sessions never wake, nor does the TUI once its App has left (`Bridge::close_mesh`). Guard: `tests/guards/tests/mesh.rs::a_woken_turn_reads_its_message_fenced_and_tainted_and_egress_asks`.

- **Mesh names (v2).** A process is on the mesh as `MARV_MESH_NAME`, else its project folder's name and four hex digits (`api-3f9a`). The name belongs to the process, not the conversation (session ids change with `/clear` and resume, and have uppercase letters), and is at most 43 characters so `peer-<name>` stays one segment.
- **`marv mesh wire` asks, it doesn't approve (differs from MESH.md rule 1).** Any process of the user's can run it, including an unsandboxed agent in another pane, so each end's Marv asks its person (the TUI's queue) or its `--approve mesh:<receptacle>` list. The CLI form is `<marv>[/<agent>] -> <marv>[/<agent>]:inbox`, since a bare `worker-1` can't say which Marv it's in.
- **A peer is the whole other Marv.** The far end sees every message from Marv `a` as from its plug `peer-a`; which of `a`'s agents may use the wire is `a`'s wire table's business. Only inboxes take cross-process wires.
- **Herdr receptacles are per pane** (`herdr.pane.<pane>.send`), not MESH.md's single `herdr.pane.send` with the pane in the message: a grant is decided on names only, so approving it approves typing into that one pane. Pane ids become one segment: `w2T:p1` is `w2_t-p1`.
- **The shim translates names.** MCP clients take tool names of `[A-Za-z0-9_-]` only, so `marv mesh plug` shows `agent.parent.inbox` as `agent_parent_inbox`, and a call to a send-only receptacle goes as a send.
- **`marv -p` is on the mesh only when asked** (`--approve mesh:<r>` or `MARV_MESH_NAME`); the TUI always is, with a runtime folder.
- `herdr.rs` replaces Marv's single-flight promise with a `watch::Sender<bool>` for "sending" plus one drain task. The effect is the same: only the newest queued state is sent, and `settled()` waits for the drain. `state()` needs a Tokio runtime.
- `HerdrReporter::release` drops any queued state, waits for the drain, then sends `pane.release_agent`. The CLI bounds it to 1 s, as in Marv.
- The adapter tests run a fake `herdr` script written into a temp folder. `FakeHerdr::new` runs it once (`--ready`) before handing it out, retrying while Linux says "Text file busy" (#92). That happens when another test's thread forks while the script is still being written: the child holds the write handle until it execs, and the adapter's first command used to fail under load. No production timeout was involved, and the real `herdr` is never a file that was just written.
- `marv-mesh` depends on `marv-mcp`, but only for `jsonrpc` and `Env`.
- **The radio (#124, phase 2).** A refusal at the receiver is told back as a fixed code only (`marv/radio.refused {id, reason}`, `RefusedCode`), shown in the sender's traffic list, never the frame's words and never to a model. The sender keeps the receiver's rate per peer (`rate_out`), so its model reads a refusal as `radio_tx`'s error. A message's `Delivered` outcome at the sender means queued on that link. Spec deviations, the safer reading each time: the radio queue is its own, beside the inbox, with the inbox's depth (50) rather than sharing one count; a gap in a peer's sequence numbers isn't shown yet (presence frames take numbers too); a link whose queue stays full without emptying for `write_timeout` (30 s) is dropped (`Link::full_since`, checked with liveness), as is one whose single write stalls that long. Rates are kept per Marv name across links (`Radio::rates`), so a rejoin doesn't refill them. Refusal reasons are Marv's fixed words, never serde's (which quote the frame). Receiver refusals reach the sender's TUI, not its model (spec §8.6, 5; noted in MESH.md).
- **The radio (#124, phase 1).** Links ride the doors (the spec's option A): `marv/radio.join` is a door request whose connection, once accepted, becomes the link (`Radio::adopt`); frames are `marv/radio.frame` notifications, at most 8 KB a line. `marv/info` has `radio: bool` (`#[serde(default)]`, so older Marvs read as off) and `marv mesh ls` a `radio` column. Only the TUI makes a radio (`mesh_cmd::Mesh::start_radio`); a door without one refuses joins. The header of `mesh_list`'s section doesn't name `radio_tx` (phase 2's tool doesn't exist yet). Every radio frame and join is logged to the trajectory only through `TrajectorySink::radio` (a default no-op, so sinks that don't care are unchanged).

## Interfaces other crates use

```rust
// switchboard (marv-session owns one per session, its own plug folder: marv_session::mesh::start(session, &env, cancel) -> Option<MeshHandle>)
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
start, log_to, PARENT, parent_id, WireApprover { approve_wire, approve_call }, CallWire, PersonWires { inner, by },
HeadlessWires(Vec<ReceptacleName>), wire_approval, call_wire_approval, MESH_WIRE_CALL,
wire_approver, inbox_note, with_inbox, MeshAccess::plug_subagent -> SubagentPlug { tools, sandbox, after_step }  // headless_grants is approvals::parse_approval

// Herdr (marv-cli makes one, the App reports)
herdr::{herdr_target(&env), HerdrReporter::new(target, None), .state(s, msg), async .session(id, path), async .release()}

// v2: services, discovery and the Herdr adapter (marv-cli's mesh_cmd::start / Mesh::open_door)
switchboard::{Service { deliver, approve_remote }, Resolve { resolve }}; sb.offer_service(r, svc); sb.add_resolver(res)
  async sb.lookup(&name) -> Option<Receptacle>; async sb.grant(WireRequest, Approver) -> Result; sb.agents(); sb.has_plug(&id)
discovery::{mesh_dir, mesh_name(&env, root), session_socket, list(&dir), open_door(sb, &dir, DoorInfo, Arc<dyn PeerApprover>) -> DoorGuard,
  DoorClient::{connect, request, info}, peer_receptacle, parse_peer, peer_plug, check_socket}
herdr::{HerdrAdapter::new(program, own_pane), HerdrPanes(Arc<HerdrAdapter>), pane_receptacle, PaneAction}
```
