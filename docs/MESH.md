# The mesh: plugs and receptacles for sandboxed agents

Marv's agents run sandboxed: no network, `.git` read-only, every process killed when its command ends. That's the safety story, but it also means two agents can't talk to each other or to the user's other tools. This includes Marv workers in Herdr panes or tmux.

The mesh gives sandboxed agents **deliberate, wired, auditable lines out**, without opening the sandbox. It works like a wall socket:
- a **receptacle** is a named service someone offers;
- a **plug** is a grant to use it;
- nothing is live until it's wired.

## Words
- **Switchboard.** One per Marv process, on the host, outside every sandbox.
  - It owns the wiring table and routes every message.
  - It is the only thing that touches the real network or other processes.
- **Receptacle.** A named service, for example:

  | Receptacle | What it is |
  |---|---|
  | `agent.<name>.inbox` | an agent's message inbox |
  | `herdr.pane.<pane>.send` | sends a prompt to one Herdr agent pane (only when that agent is idle or done, so it can't answer an approval) |
  | `herdr.pane.<pane>.status` | reads one Herdr pane's status |
  | `github.read` | the Marv #47 door |
  | `process.<name>` | a background dev server (Marv #51) |
  | `mesh.remote.<service>` | anything published on an AgentMesh network |

  Names are the only addresses: grants are evaluated on names, never on hosts or ports. This is borrowed from Google's AgentMesh.
- **Plug.** An agent's handle onto the switchboard. Inside a sandbox it is a **Unix socket bind-mounted at `/run/marv/mesh.sock`**.
  - This works with `--unshare-net`, because a bind-mounted socket is a file, not a network.
  - The agent reaches the mesh through the model-facing tools `mesh_list`, `mesh_call` and `mesh_send`, not through raw sockets.
- **Wire.** A grant from one plug to one receptacle, with a direction and a mode:
  - `call`: request and response;
  - `send`: fire-and-forget into an inbox;
  - `watch`: subscribe to events, such as "agent B changed a file you read".

## Rules
1. **Nothing is wired by default.**
   - A new wire needs the user's approval in the TUI, or an explicit `marv mesh wire a -> b:inbox`.
   - Headless runs need `--approve mesh:<receptacle>`.
   - Yolo never wires anything by itself.
2. **Mesh text is data.** Every message that arrives through the mesh is `Provenance::Mesh`. It taints the session (`docs/SECURITY.md`), and the model sees it fenced as a message from another agent, not as instructions.
3. **The switchboard logs every message** to the trajectory, with sender, receptacle and size.
4. **Size and rate limits per wire**, so a looping agent can't flood another.
5. **No ambient authority.** A plug can do nothing beyond its wires. In particular, an agent can't wire itself.

## Wire format
**MCP (JSON-RPC 2.0) over the Unix socket:**
- each receptacle is an MCP tool on the switchboard;
- `mesh_call` is `tools/call`;
- `watch` uses notifications.

Using MCP means:
- Marv's own MCP client code works as the plug;
- any MCP-speaking agent (Claude Code, opencode, jcode) can plug in from a Herdr or tmux pane through a small stdio-to-socket shim;
- AgentMesh slots in naturally (see below).

## Topologies (simple first)
1. **One Marv, its subagents (v1).** The parent's switchboard. A subagent's sandbox gets a plug. Typical wires:
   - `worker-N → agent.parent.inbox` (send);
   - `parent → agent.worker-N.inbox` (send);
   - optionally `worker-N → worker-M` for swarms, but only when wired.
2. **Several Marvs on one machine: Herdr or tmux (v2, done in #16; see `docs/subsystems/mesh.md` for where it refines this design).**
   - Each switchboard also listens on `$XDG_RUNTIME_DIR/marv/mesh/<session-id>.sock` (mode 0600, the user only).
   - `marv mesh ls` discovers them.
   - Cross-process wires are approved on **both** ends.
   - The Herdr adapter maps pane ids to sessions and offers `herdr.pane.*` receptacles. This replaces hand-typed orchestration with wired inboxes, while each worker stays sandboxed.
3. **Across machines: an AgentMesh bridge (v3).**
   - The switchboard connects to a local `agentmesh-node` (which exposes the mesh as an MCP server on `127.0.0.1:8080`).
   - It republishes chosen remote services as `mesh.remote.*` receptacles.
   - It can publish chosen local receptacles to the mesh.
   - AgentMesh adds identity (OIDC), relays and mesh-wide policy. Marv's wire approvals still apply on top.

## Not in scope yet
- jcode-style automatic swarms: agents spawning teams and file-change broadcast. Mesh `watch` makes this possible later.
- A shared daemon for all sessions (see `ARCHITECTURE.md`, Runtime shape).
