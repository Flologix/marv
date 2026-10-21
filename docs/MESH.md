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
  | `herdr.pane.<pane>.send` | types a prompt (`{"text": "..."}`, nothing else) into one Herdr agent pane (only when that agent is idle or done, so it can't answer an approval). A prompt, never a command: text starting with `/`, `!` or `#` is refused, and nothing is typed. For a pane running a Marv that's on the mesh, `mesh_list` points to that Marv's inbox instead: it can answer. |
  | `herdr.pane.<pane>.status` | reads one Herdr pane: its agent's state and, with `lines: N`, the pane's last N lines |
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
   - A new wire needs the user's approval in the TUI, or an explicit `marv mesh wire a -> b:inbox`. When an agent's `mesh_send` or `mesh_call` needs the wire, that's one prompt for the message and the wire together ("Yes (wire it)" or "No").
   - Headless runs need `--approve mesh:<receptacle>`. Once a message has arrived (the session is tainted), sending also needs `--approve-tainted`.
   - Yolo never wires anything by itself.
   - **The radio (#124) is one decision, not a wire:** being on it is one opt-in in the user's own config (`"mesh": {"radio": true}`, or `/radio on`). After that, traffic between the user's Marvs on the radio asks nothing per peer: presence never reaches a model, and a message (`/tx`, `radio_tx`) only reaches the user's own Marvs on this machine, each of which reads it as tainting data and asks before anything leaves it (see Topology 2).
2. **Mesh text is data.** Every message that arrives through the mesh is `Provenance::Mesh`. It taints the session (`docs/SECURITY.md`), and the model sees it fenced as a message from another agent, not as instructions.
   - An agent reads its inbox when a turn starts and between steps. By default a message that arrives while it's idle waits for the next turn.
   - **Wake (opt-in, `/mesh wake on` or `"mesh": {"wake": true}`):** in the TUI, a message on a wired inbox starts a turn while the session is idle. That turn has no user message: it reads the inbox exactly as above (fenced, tainting), then a note that the user didn't write it; the transcript says who woke it. Never during a turn or while an approval waits (the running turn reads the message between steps, and one that lands after its last read wakes a turn once that turn ends; under an approval with no turn running, it waits for the next message or turn), at most `WAKES_PER_MINUTE` (3) wake-ups a minute per wire (over that, the user is told once and the messages wait), and never in `marv -p`, which runs one turn.
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
   - Each switchboard also listens on `<runtime>/mesh/<session-id>.sock` (mode 0600, the user only), in Marv's runtime folder (`marv_sandbox::runtime_dir`): `$XDG_RUNTIME_DIR/marv` on Linux (when every sandbox hides it; made 0700 in an existing `XDG_RUNTIME_DIR`, with the same checks as the override below), `/tmp/marv-<uid>` on macOS (made 0700, then it must be a folder, not a symlink, of the user's), or `MARV_RUNTIME_DIR` on either, checked the same way, short enough for the longest socket, and in an existing parent judged by its real path, with no symlink on the way (but the root's `/tmp`, and a root-owned link in a folder only root may change, such as Silverblue's `/home -> var/home`, whose target is checked the same way) and every folder on the way the user's or root's and not writable by others unless sticky; for a session, not inside its project nor holding it (#168). No runtime folder, no mesh. The plugs' sockets are `<runtime>/p/<pid>-<hex>/<plug>.sock`, a folder per switchboard made exclusively (Marv desktop runs several in one process); a socket path longer than `sun_path` allows (103 bytes on macOS, 107 on Linux) is refused with a message.
   - `marv mesh ls` discovers them.
   - Cross-process wires are approved on **both** ends.
   - The Herdr adapter maps pane ids to sessions and offers `herdr.pane.*` receptacles. This replaces hand-typed orchestration with wired inboxes, while each worker stays sandboxed.
   - The Herdr door (#74) is the main agent's own way into Herdr, outside the mesh: the `herdr` tool reads panes (fenced, tainting), focuses, renames and prompts (asked), and runs commands or spawns persistent workers (asked every time, TUI only). It uses the same adapter and keeps the pane send's prompts-only rule; a spawned worker's pane is reachable on the mesh as `herdr.pane.<pane>.send|status` once the user wires it. See `docs/SECURITY.md`, "The doors out of the sandbox".
   - **The radio (#124; `docs/specs/2026-10-14-mesh-radio.md`).** A live channel every Marv of the user's with the radio on is part of. **Phase 1 (done) is presence only:**
     - **Opt-in, off by default:** only the user's own config (`"mesh": {"radio": true}`, or `{"radio": {"titles": false}}`) or the person's `/radio on|off`. A project's `.marv/config.json` setting `mesh` (radio or wake) is ignored with a startup note. No environment variable, no flag, no model path; `marv -p` and subagents are never on it.
     - **Links over the doors:** a Marv joining calls `marv/radio.join {from, presence}` on every door whose `marv/info` says its radio is on. The far door refuses unless its own radio is on, checks the caller by pid (`SO_PEERCRED`) against `<from>.sock` exactly as `marv/wire` does, and keeps one link per pair (on a simultaneous join, the one opened by the name that sorts first). The connection then carries `marv/radio.frame` notifications both ways. A frame never names its sender: one with a `from` is refused, and so is a presence naming another Marv than the link's.
     - **Presence is exactly:** `name`, `state` (`idle`, `working`, `waiting`, `left`), `title` (the conversation's first user message, one printable line, at most 56 characters, empty with titles off), `project` (the folder's last path component), `pane`, `since_ms`, `wake`, `version`. Any other field is refused. It's drawn in the TUI only (the strip above the prompt, and `/radio`); `mesh_list` gives the main agent an "On the radio" section with names and states only, which doesn't taint. Nothing a peer says reaches a model or the history.
     - **Liveness:** a heartbeat every 15 s, changes coalesced (the newest, at most one per 2 s). No frame for 45 s: `silent`; for 120 s: the link is closed and the name drops. A link that ends without a `left` is `lost`, and the folder is swept at once (`sweep_dead` under the folder's lock), as it is on the 30 s re-list that also joins doors a Marv missed.
     - **Logged** to the trajectory (`radio` records): joins, leaves, `lost`, `silent`, drops, refused frames and refusals a peer told back; never a heartbeat or a title.
     - **Phase 2 (done): short messages.** A `msg` frame (`v, id, seq, kind, to, by, reply_to, hop, text, at_ms`, nothing else; its id `<the link's Marv>:<seq>`) to named Marvs or to all (`"*"`), sent by the person (`/tx`, `by: user`) or the model (`radio_tx`, `by: model`). Text at most 2 KB, refused (never cut) at both ends; per sender at each receiver 20 a minute (bursts of 5), at most 50 unread; a refusal goes back as a fixed code (`marv/radio.refused {id, reason}`). Each link writes from its own queue of 64 frames, so a peer that stops reading is refused ("isn't keeping up"), never waited on. Received messages wait in the switchboard's radio queue and the main agent reads them where it reads its inbox (a turn's start, between steps), fenced as `<radio-message>` and tainting (`Provenance::Mesh`). With wake on, an addressed message (never a broadcast, never at hop 2 or more) may start a turn, within 3 a minute per sender (1 every 10 s) and 10 in 10 minutes overall; that turn runs with automatic approval off. `radio_tx` is the main agent's alone, in a conversation that started with the radio on, and doesn't ask per message. Every message is logged (`mesh` records with `radio: true`) without its text. The TUI flashes a received message on the strip for 10 s and lists the last 100 in `/radio`.
       - Deviation (spec §8.6, 5): a refusal made at the *receiver* (its rate, its full queue) reaches the sender's TUI and trajectory as a fixed code, not its model; the sender's own mirror of the rate is what its model reads as `radio_tx`'s error. Phase 3 carries a receiver's refusal of a *request* back to the model, as a `failed` in Marv's words; a refused `msg` still reaches the sender's TUI and trajectory only.
       - Turns: a turn a wired inbox woke (#78) doesn't read the radio's queue; only the person's own turn and a turn the radio started do. A radio-started turn's hop is one past the deepest message it read.
     - **Phase 3 (done): requests that answer themselves.** A `request` (`/ask @name text`, `by: user`; `radio_tx` with `kind: "request"`, `by: model`) goes to exactly one Marv. The receiver acknowledges at once without a model call (`ack`, `by: marv`, a fixed `state`: `on_it`, `queued_busy`, `queued_waiting_user`), reads it only at a turn's start (never between steps, so "the turn that handles it" is defined), fenced as `<radio-message kind="request" id=…>` with Marv's constant request note, and when that turn ends sends `done` (the final reply's first paragraph, code dropped, at most 1 KB) or `failed` (stopped, declined, the turn ended early, or it left the radio), again with no model call. The model may answer itself first (`kind: "reply"`, `reply_to`, `status`): then no automatic report follows; one request gets one ack and one report. A reply is taken only for an open request the receiver itself sent to that peer, and never gets a reply, a report or a refusal back. **A turn that handles a request runs with automatic approval off, the person's own turn included** (marv-rs goes past spec §8.4 here). Hops: a request from a turn at hop 2 or more is refused at the sender ("the radio stops chains of more than 2: ask your user") and at the receiver; a report wakes the requester only for a request its person's own turn sent; an ack never wakes. At most 2 open requests per sender at each receiver (mirrored at the sender), 10 per receiver; replies go ahead of chatter in a full link queue. A request refused or lost (the peer refused it, left or crashed) is closed at the requester with a `failed` in Marv's words (bead `marv-2uc`, for requests). Open requests live in the session's state and its saved file, not the history: compaction and a resume keep them. `radio_tx`'s `wait` (0-300 s) returns the ack and the report fenced, tainting first.
     - **Pane sends (#123, §9.3):** `herdr.pane.<pane>.send` (and the Herdr door's `prompt`) to a pane whose Marv's door says its radio is on is refused, nothing typed: "<marv> is on the radio: use radio_tx (kind: request) to get a reply"; `mesh_list` offers such a pane for `.status` only and steers to `radio_tx`. The Herdr door's spawn still types its approved task into the new pane.
3. **Across machines: an AgentMesh bridge (v3).**
   - The switchboard connects to a local `agentmesh-node` (which exposes the mesh as an MCP server on `127.0.0.1:8080`).
   - It republishes chosen remote services as `mesh.remote.*` receptacles.
   - It can publish chosen local receptacles to the mesh.
   - AgentMesh adds identity (OIDC), relays and mesh-wide policy. Marv's wire approvals still apply on top.

## Not in scope yet
- jcode-style automatic swarms: agents spawning teams and file-change broadcast. Mesh `watch` makes this possible later.
- A shared daemon for all sessions (see `ARCHITECTURE.md`, Runtime shape).
