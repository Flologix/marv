# Deferred MCP tools

Issue: #14 (spec first). Related: #6 (installing servers), #10 (tool-call repair).

## Problem

Every MCP tool's full spec (description up to 2000 chars plus the server's JSON schema) goes into every request: `MarvSession.offered()` returns `[...this.specs, ...mcp.specs]` and `runTurn` passes that list to `runAgent`. A server brings 5–30 tools, so three servers can add several thousand tokens to every request, before the user has said anything. That costs money on OpenRouter, eats the 32k window Marv asks Ollama for (`num_ctx`), and gives small models dozens of tools to choose from on every step, when most turns use none of them. It gets worse once installing a server is one command (#6).

The fix, as Claude Code does it: the model sees only the MCP tools' **names and one-line descriptions**, and loads a tool's full schema when it needs it.

## Decisions

- **Two fixed built-in tools, `tool_search` and `mcp_call`, replace the MCP specs** when deferral is on. A loaded schema reaches the model **in a tool result**, so neither the tool list nor the system prompt ever changes mid-conversation (design A below).
- **The list of names goes in an appended note from Marv**, at the start of the first turn's message once the servers have settled; later additions (`/mcp trust`, a resume) get a note of their own. The system prompt is untouched, and the history is only appended to.
- **Subagents: unchanged.** They get no MCP tools today and get none in v1.
- **Threshold:** defer only when the MCP specs would cost more than **min(4000 tokens, 10% of the context window)**, decided once per conversation. Below that, everything is sent as today.
- **Small models** get a repair for each of the two predictable mistakes (#10): calling `mcp__x__y` directly, and sending `arguments` as a JSON string.

## The crux: how a schema reaches the model (prompt cache)

Every provider caches a *prefix*. In what both adapters send, the tool list is at the front of the prefix: OpenRouter's upstream models render `tools` before (Anthropic) or alongside (OpenAI) the system prompt, and Ollama's chat templates put the tool block inside the system message (`src/provider/ollama.ts` sends `tools` with every `/api/chat`). `tests/agent.test.ts` checks that every request extends the previous one exactly, and CLAUDE.md's "Prompt cache" section explains why: one changed byte at the front and the whole prompt is recomputed (on Ollama, measured: 76 ms vs 1.2 s for a short conversation; tens of seconds for a long one on a local GPU).

### A. A generic dispatcher; the schema arrives in a tool result (chosen)

The offered list gains two fixed specs and loses the MCP ones:

- `tool_search({ query?: string, names?: string[] })`: returns the full spec (name, description, JSON schema, read-only or not) of up to 5 matching MCP tools. `names` loads exact names; `query` matches words in names and descriptions (case-insensitive word overlap, ties by list order; no embeddings).
- `mcp_call({ tool: string, arguments: object })`: runs `tool` (an `mcp__<server>__<tool>` name) with `arguments`.

Both specs are constants, so the tool list is byte-identical all session, whatever servers connect, fail, or are trusted later. The schema enters the conversation as an ordinary tool result: appended, never edited, so the cache holds on every request.

`runTool` resolves an `mcp_call` to the real MCP `Tool` before anything else and runs **that** tool's pipeline: its `preview`, approval (its `kind`, its scope `mcp:<server>:<tool>`, no `autoSafe`, so yolo still never skips it), label and result. Approval prompts, "don't ask again" scopes, the transcript and trajectories therefore look as they do today (the trajectory's `tool` record keeps the call as sent, plus the resolved name). An unknown `tool` is an error that lists the closest names. A call to a tool not yet loaded with `tool_search` is allowed (the server validates the arguments); if the server rejects it, the error result gets that tool's schema appended once, so a guess costs one step, not two.

Cost: the model writes the arguments one level down, inside `arguments`, and providers that constrain tool-call JSON to the schema (OpenAI's strict mode; Marv doesn't use it) can't do so for the inner object. The server validates, as it does today (`makeMcpTool`'s `input` is already a loose record).

### B. Add loaded tools' specs to the tool list (rejected)

`tool_search` would record the loaded names, and the next request (or the next turn, as a "cache boundary") would offer their real specs. It gives the model real, flat tool calls. But there is no cache boundary in front of the tool list: any change re-prefills the whole prompt, system prompt and history included. Every first use of a tool would cost a full re-prefill, which on Ollama is exactly the slowness deferral is meant to remove; waiting until the next turn doesn't make it cheaper, only later. It would also break the invariant `tests/agent.test.ts` guards, and compaction, resume and `offered()` would all need to know which specs a conversation has loaded.

### C. Provider-side deferral (not available)

Anthropic's API can defer tools server-side (Claude Code uses it): the full list is sent once, marked deferred, and loading one doesn't break the cache. Marv talks to OpenRouter's OpenAI-compatible API and to Ollama, neither of which has it. If a native Anthropic adapter is ever added, it can map A's `tool_search`/`mcp_call` onto that feature behind the provider seam; nothing above the provider needs to change.

## Where the list goes: an appended note, never the system prompt

The system prompt doesn't change: `buildSystem()` still runs in the constructor, before the servers have settled, and the fixed `MCP_TOOLS` paragraph (`src/prompt.ts`) stays as it is. The list of names goes into the **conversation**, as a note from Marv, so every byte the model has seen stays where it was and the cache holds.

```
Marv: MCP tools you can use (not loaded: call tool_search with their names to see their parameters, then run one with mcp_call):
- mcp__github__create_issue: Create a new issue in a GitHub repository.
- mcp__github__search_code: Search for code across GitHub repositories.
…
```

One line per tool: the name, then the description's first sentence, at most 100 characters (`printable`-clean, no newlines). Sorted by server in config order, then as the server listed them (the order `McpManager` already keeps), so the text is deterministic.

**One rule: announce what hasn't been announced.** The session keeps `announced`, the MCP tool names this conversation has been told about. When a turn starts, after it has waited for `mcp.ready` (the wait `runTurn` already does, "Waiting for MCP servers to start…"), the tools offered minus `announced` are written into a note, and the names added to `announced`. The note goes **at the start of the user message the model gets** (before the user's text, so their request stays last), not as a message of its own, so the history never has two user messages in a row. The transcript shows the user's text only; the trajectory's `turn_start` records the note as `mcpNote`, keeping `forModel` what the client sent. With nothing new, nothing is added: zero tokens. The same rule covers every case:

- **First turn:** everything that connected is announced. A server that failed is left out; one that dies later stays announced, and its calls fail clearly (as its specs do today).
- **`/mcp trust` mid-conversation:** the newly trusted tools are announced at the next turn. Today trusting changes the tool list and the cache starts over; with deferral it holds.
- **Compaction** clears `announced`: a summary may drop the list, and the cache starts over there anyway. Before a turn, the user message that follows announces everything again; mid-turn, the list is appended to `MID_TURN_NOTE`.
- **`clear()`** starts a new conversation with `announced` empty.
- **`resume()`** restores `announced` with the conversation (`SavedSession.mcp: { mode, announced }`, optional, schema-checked). If the servers differ from the saved session's, the next note lists the new tools and, in one line, the announced ones that are gone (`mcp_call` to one fails with "not connected" anyway). A save without the field is treated as nothing announced.

**Mode.** Whether the conversation defers (see Threshold) is decided at the same moment as the first note, after the servers have settled, and frozen with the conversation (saved with it, reset by `clear()`): a conversation never switches between deferred and eager, so the tool list `offered()` returns never changes either. In eager mode there's no note at all, as today.

## Subagents

No change in v1: `SUBAGENT_TOOLS` doesn't include MCP tools, and `runSubagent` offers only built-ins. Giving subagents MCP tools is its own decision (they'd run outside the sandbox, from parallel agents, under worktree auto-approval rules that assume sandboxed work), not this issue's. When it comes, the deferred pair is the way to do it: two constant specs added to the subagent's list, and the same note at the start of its prompt message, so a subagent pays nothing for servers it doesn't use. `tool_search` and `mcp_call` are therefore kept out of `SUBAGENT_TOOLS` for now, so an agent type that lists them gets the usual "not a subagent tool" problem.

## Threshold

Deferral costs one extra step per tool first used (a `tool_search` call) and makes calls nested, so for a handful of small tools sending everything is better.

- **Measure:** `estimateTokens(JSON.stringify(mcp.specs))` (`src/usage.ts`, 4 chars a token), i.e. the bytes the MCP specs add to each request. Compared with what the deferred version costs instead: the two fixed specs plus the note (about 200 tokens + 25 per tool, the note paid once per conversation rather than per request, though it stays in every request's history).
- **Defer when** (decided with the user: tokens, not a tool count, since schemas vary 20× in size and tokens are what cost money and context) the MCP specs exceed **min(4000, 10% of the provider's `contextLength`)** tokens, and deferring saves at least half of that. With Ollama's 32k default that's 3.2k tokens, roughly one mid-sized server (GitHub's official server is far over it; a 3-tool local server is far under). With an unknown window (OpenRouter before the model list arrives), 4000.
- **Override:** config `mcpTools: "auto" | "deferred" | "eager"` (default `auto`), SDK `mcpTools`, read when the conversation's mode is decided. Needed for the eval comparison, and for a user whose model handles nested calls badly.
- **Checking the number:** `bun run stats` already reports tokens per turn; the `turn_start` record of the turn that decides gains `mcpTools: { mode, specTokens, deferredTokens, count }`, and each note gets an `mcp_note` record, so real sessions show what each mode costs.

## Ollama, small models and repair (#10)

- **Why it matters most there.** Ollama recomputes the uncached prompt on the user's GPU, and small models pick tools worse as the list grows. Deferral shrinks the fixed prefix and the choice; A keeps Ollama's cache intact.
- **Nested arguments.** Two predictable mistakes get a certain repair (same rules as #10: only when the intent is unambiguous, with a `Marv:` note in the result saying what was fixed):
  1. **A direct call to `mcp__x__y`** (the model saw the name in the prompt, or in a `tool_search` result, and called it like a tool). If the name is a configured MCP tool, the call is rewritten to `mcp_call({ tool, arguments })` *before the reply enters the history* (where `runAgent` already makes ids unique), so the history only ever names offered tools: a provider may reject a conversation whose history calls a tool that isn't in `tools`, and every later request would fail.
  2. **`arguments` as a JSON string** (`"arguments": "{\"repo\": …}"`) that parses to an object: parsed (`mcp_call`'s `repair`).
- **`tool_search` output** is the spec as compact JSON (no indentation), so a loaded schema costs what it would have cost as a spec, once instead of every request.
- **Cut-off replies** need nothing new: an `mcp_call` cut off at the output limit gets `NOT_RUN.cutOff` like any call.

## Testing (failing first)

- `tests/mcp-deferred.test.ts` (new, red first): the threshold (`auto` with small and large spec sets, `contextLength` known and unknown, the overrides); the note (first sentence, 100-char cap, order, deterministic, only the names not yet announced, gone tools on one line); `tool_search` by `names` and by `query`, at most 5, unknown names reported; `mcp_call` resolving to the real tool, an unknown tool's closest names, the schema appended to a server's rejection once.
- `tests/mcp.test.ts`, against `tests/fixtures/mcp-server.ts` with enough tools to cross the threshold: a session offers `tool_search` and `mcp_call` and no `mcp__` specs; an `mcp_call` asks with the real tool's approval prompt and scope, and "always" covers later calls; yolo doesn't skip it; a read-only tool runs unasked.
- `tests/agent.test.ts`: the existing "every request extends the previous one" check, run through two turns with a scripted `tool_search` → `mcp_call` sequence and a `/mcp trust` between them (fails first if anything rewrites the tool list, the system prompt or an earlier message).
- `tests/session.test.ts`: the system prompt is the same before and after the servers settle; the first turn's model message starts with the note and a server that connects after a 50 ms delay is in it; the second turn adds nothing; `/mcp trust` announces only the new tools at the next turn; compaction re-announces (before a turn and mid-turn, in `MID_TURN_NOTE`); `clear()` re-announces; `resume()` restores `announced` and mode, and announces a changed server set's difference; the transcript never shows the note.
- `tests/tool-call-repair.test.ts`: both repairs, with their notes; a direct call to a name that isn't configured is left as an unknown-tool error.
- `tests/system-prompt.test.ts`: the system prompt is byte-identical in deferred and eager mode.

## Evals

The current 13 tasks use no MCP server, so they can only show that nothing regressed: run them with `--mcp-tools eager` and `deferred` and expect identical pass rates. Deferral's effect needs tasks that do:

- `evals/run.ts` gets `--mcp <config.json>`, passed to the session as `mcpServers`, and `--mcp-tools auto|deferred|eager`.
- A fixture server in `evals/mcp/` (local, deterministic, no network) with ~25 tools, of which the task needs one or two (an issue tracker: `get_issue`, `list_issues`, `add_comment`…, plus distractors with similar names).
- Three tasks: `mcp-fix-from-issue` (read an issue through the server, fix the bug it describes; checked by the repo's tests), `mcp-comment` (do a change, then post a comment; checked by the fixture's recorded calls), `mcp-unused` (a plain JS task with the server connected; it measures what the specs cost when no MCP tool is needed).
- Same day, same models (one OpenRouter model, one small Ollama-class model via OpenRouter, e.g. a 9B), eager vs deferred, 3 repetitions. Compare pass rate, steps, prompt tokens per request and cost. Ship `auto` as the default only if deferred is no worse on pass rate and clearly cheaper on `mcp-unused`.

## Decided with the user (2026-10-12)

1. The threshold is token-based: it saves money and resources, which a tool count doesn't measure.
2. The list is announced in appended notes rather than a lazily built system prompt, provided the cache is preserved: it is, since the system prompt and tool list never change and notes are only ever appended (`tests/agent.test.ts` guards it).
