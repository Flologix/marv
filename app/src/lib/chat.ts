import { useCallback, useEffect, useRef, useState } from "react";
import { type ChatSettings, type Connection, type ServerEvent, connect } from "./backend";

export interface DiffLine {
  kind: "add" | "del" | "ctx" | "gap";
  text: string;
}

export interface ApprovalRequest {
  id: number;
  tool: string;
  label: string;
  title: string;
  command?: string | null;
  diff?: DiffLine[] | null;
  text?: string | null;
  note?: string | null;
  warning?: string | null;
  scope: string;
  agent?: string | null;
  once: boolean;
}

export interface Usage {
  promptTokens: number;
  cachedTokens: number;
  completionTokens: number;
  cost: number;
  context?: number | null;
  contextLength?: number | null;
}

export type Item =
  | { kind: "user"; text: string }
  | { kind: "text"; text: string }
  | { kind: "thinking"; text: string }
  | {
      kind: "tool";
      id: string;
      tool: string;
      label: string;
      agent?: string;
      done: boolean;
      result?: string;
      summary?: string | null;
      error?: boolean;
      diff?: { lines: DiffLine[]; more: number };
    }
  | { kind: "approval"; request: ApprovalRequest; decision?: string }
  | { kind: "notice"; text: string; tone: "info" | "warn" | "error" }
  | { kind: "turn"; reason: string; usage?: Usage }
  /** What a /command printed. */
  | { kind: "output"; text: string };

/** An entry of the / menu: a command, or one of the user's skills. */
export interface Command {
  name: string;
  description: string;
  skill?: boolean;
}

const STOPS: Record<string, string> = {
  step_limit: "Stopped at the step limit.",
  cut_off: "The reply hit the output limit.",
  text_loop: "The reply was repeating itself.",
  tool_loop: "The same tool calls kept repeating.",
};

/** Folds one event into the transcript. */
function apply(items: Item[], e: ServerEvent): Item[] {
  const last = items[items.length - 1];
  const next = items.slice();
  const tool = (id: unknown) => next.findIndex((it) => it.kind === "tool" && it.id === id);
  switch (e.type) {
    case "text_delta":
    case "thinking_delta": {
      const kind = e.type === "text_delta" ? "text" : "thinking";
      if (last?.kind === kind) next[next.length - 1] = { ...last, text: last.text + e.text };
      else next.push({ kind, text: String(e.text) });
      return next;
    }
    case "assistant": {
      // The reply's whole text: already shown if it streamed in, even with tool calls after it in this step.
      const text = String(e.text ?? "").trim();
      for (let i = next.length - 1; i >= 0; i--) {
        const it = next[i];
        if (it.kind === "user" || it.kind === "turn") break;
        if (it.kind === "text" && it.text.trim() === text) return items;
      }
      if (text) next.push({ kind: "text", text });
      return next;
    }
    case "tool_start":
      if (tool(e.id) < 0) next.push({ kind: "tool", id: String(e.id), tool: String(e.tool), label: String(e.label ?? ""), agent: e.agent as string | undefined, done: false });
      return next;
    case "tool_end":
    case "tool_skipped": {
      const i = tool(e.id);
      const before = i >= 0 ? (next[i] as Extract<Item, { kind: "tool" }>) : undefined;
      const done: Item = {
        kind: "tool",
        id: String(e.id),
        tool: String(e.tool),
        label: String(e.label ?? before?.label ?? ""),
        agent: (e.agent as string | undefined) ?? before?.agent,
        done: true,
        result: String(e.result ?? e.reason ?? ""),
        summary: (e.summary as string | null) ?? (e.type === "tool_skipped" ? "skipped" : null),
        error: e.type === "tool_skipped" || e.error === true,
        diff: e.diff as { lines: DiffLine[]; more: number } | undefined,
      };
      if (i >= 0) next[i] = done;
      else next.push(done);
      return next;
    }
    case "approval":
      next.push({ kind: "approval", request: e as unknown as ApprovalRequest });
      return next;
    case "approval_done": {
      const i = next.findIndex((it) => it.kind === "approval" && it.request.id === e.id);
      if (i >= 0) next[i] = { ...(next[i] as Extract<Item, { kind: "approval" }>), decision: String(e.decision) };
      return next;
    }
    case "compaction":
      next.push({ kind: "notice", text: e.compacted ? "Conversation compacted to save context." : `Compaction failed (${e.reason}).`, tone: e.compacted ? "info" : "warn" });
      return next;
    case "tainted":
      next.push({ kind: "notice", text: `Read untrusted content (${e.source}): anything that sends data out now asks first.`, tone: "info" });
      return next;
    case "step_limit":
    case "cut_off":
    case "text_loop":
    case "tool_loop":
      if (e.continued) return items;
      next.push({ kind: "notice", text: STOPS[e.type], tone: "warn" });
      return next;
    case "notice":
      next.push({ kind: "notice", text: String(e.text), tone: "info" });
      return next;
    case "error":
      next.push({ kind: "notice", text: String(e.message), tone: "error" });
      return next;
    case "command_output":
      next.push({ kind: "output", text: String(e.text) });
      return next;
    case "turn_end":
      // A tool that never reported back (an interrupt, a dropped call) stops spinning with its turn.
      for (let i = 0; i < next.length; i++) {
        const it = next[i];
        if (it.kind === "tool" && !it.done) next[i] = { ...it, done: true };
      }
      next.push({ kind: "turn", reason: String(e.reason) });
      return next;
    case "usage":
      for (let i = next.length - 1; i >= 0; i--) {
        const it = next[i];
        if (it.kind === "turn") {
          next[i] = { ...it, usage: e as unknown as Usage };
          break;
        }
      }
      return next;
    default:
      return items;
  }
}

const storageKey = (id: string) => `marv-transcript-${id}`;
/** The number of the last event the saved transcript has: a chat reopened gets what came after (#155). */
const seqKey = (id: string) => `marv-seq-${id}`;

function loadSeq(id: string): number {
  try {
    return Number(localStorage.getItem(seqKey(id)) ?? 0) || 0;
  } catch {
    return 0;
  }
}

/** What the page sends for a line: a /command runs in Marv, anything else is a message to the model. */
const message = (text: string) => (text.startsWith("/") ? { type: "command", line: text } : { type: "send", text });

function load(id: string): Item[] {
  try {
    return JSON.parse(localStorage.getItem(storageKey(id)) ?? "[]");
  } catch {
    return [];
  }
}

const COMMANDS_KEY = "marv-commands";

/** The / menu as the last chat's session gave it: for a new chat, before its session starts. */
export function knownCommands(): Command[] {
  try {
    return JSON.parse(localStorage.getItem(COMMANDS_KEY) ?? "[]");
  } catch {
    return [];
  }
}

export function forget(id: string) {
  try {
    localStorage.removeItem(storageKey(id));
    localStorage.removeItem(seqKey(id));
  } catch {}
}

export type Status = "idle" | "connecting" | "ready" | "failed";

/** A server as the open chat runs it. */
export interface ServerStatus {
  name: string;
  scope: "global" | "folder";
  state: "untrusted" | "connecting" | "connected" | "failed" | "removed" | "needs_login";
  tools: number;
  error?: string | null;
  canLogin: boolean;
  loggedIn: boolean;
}

/**
 * One open chat: its transcript (kept in this window's storage), its connection, and what the page can do.
 * `send` before the session is ready waits for it.
 */
export function useChat(id: string | null, settings: ChatSettings, reload = 0) {
  const [items, setItems] = useState<Item[]>([]);
  const [status, setStatus] = useState<Status>("idle");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [commands, setCommands] = useState<Command[]>([]);
  /** This chat's name on the machine's mesh, when it's on it. */
  const [meshName, setMeshName] = useState<string | null>(null);
  const [servers, setServers] = useState<ServerStatus[]>([]);
  // The session's tool list is fixed when it starts: switching a server on or off reopens the chat (it continues).
  const sessionKey = `${id}|${[...settings.mcpOff].sort().join(",")}|${reload}`;
  const connection = useRef<Connection | null>(null);
  const pending = useRef<object[]>([]);
  const first = useRef<{ id: string; text: string } | null>(null);
  /** The last event number this page got, and the last one in `items` (saved with them). */
  const received = useRef(0);
  const applied = useRef(0);
  const ready = useRef(false);
  const settingsRef = useRef(settings);
  const sent = useRef(settings);

  useEffect(() => {
    settingsRef.current = settings;
  }, [settings]);

  useEffect(() => {
    if (!id) {
      // A draft: nothing open yet.
      setItems([]);
      setStatus("idle");
      setBusy(false);
      setError(null);
      return;
    }
    let live = true;
    let conn: Connection | null = null;
    ready.current = false;
    pending.current = [];
    // Kept until the session is ready: React's dev double-mount runs this twice.
    const opening = first.current?.id === id ? first.current.text : null;
    if (opening) pending.current.push(message(opening));
    setItems(opening ? [...load(id), { kind: "user", text: opening }] : load(id));
    received.current = applied.current = loadSeq(id);
    setStatus("connecting");
    setBusy(opening !== null && !opening.startsWith("/"));
    setError(null);
    sent.current = settingsRef.current;
    // Opened a beat late, so React's dev double-mount doesn't open the chat twice.
    const timer = setTimeout(() => {
      connect(id, settingsRef.current, { generation: reload, after: received.current }, (e) => {
        if (!live) return;
        // Numbered events: one this page already has (a reattach racing the last one) is skipped.
        if (typeof e.seq === "number") {
          if (e.seq <= received.current) return;
          received.current = e.seq;
        }
        if (e.type === "ready") {
          // Reopened while a turn runs in the background.
          if (e.busy === true) setBusy(true);
          ready.current = true;
          first.current = null;
          setStatus("ready");
          setMeshName((e.mesh as string | null) ?? null);
          conn?.send({ type: "mcp", action: "status" });
          const list = (e.commands as Command[]) ?? [];
          setCommands(list);
          try {
            localStorage.setItem(COMMANDS_KEY, JSON.stringify(list));
          } catch {}
          for (const m of pending.current.splice(0)) conn?.send(m);
        } else if (e.type === "mcp") {
          setServers((e.servers as ServerStatus[]) ?? []);
        } else if (e.type === "fatal") {
          setStatus("failed");
          setError(String(e.message));
        } else if (e.type === "closed") {
          setStatus("failed");
          setBusy(false);
        } else {
          if (e.type === "turn_start") setBusy(true);
          if (e.type === "usage") setBusy(false);
          setItems((items) => {
            if (typeof e.seq === "number") applied.current = e.seq;
            return apply(items, e);
          });
        }
      })
        .then((c) => {
          conn = c;
          if (live) connection.current = c;
          else c.close();
        })
        .catch((e) => {
          if (!live) return;
          setStatus("failed");
          setError(String(e));
        });
    }, 30);
    return () => {
      live = false;
      clearTimeout(timer);
      conn?.close();
      connection.current = null;
    };
  }, [sessionKey]); // eslint-disable-line react-hooks/exhaustive-deps

  // Settings changed with the chat open: from its next turn.
  useEffect(() => {
    const before = sent.current;
    if (status !== "ready" || (before.model === settings.model && before.maxSteps === settings.maxSteps && before.yolo === settings.yolo)) return;
    sent.current = settings;
    connection.current?.send({ type: "configure", ...settings });
  }, [settings, status]);

  // The id the items belong to: on the render that switches chats they're still the last chat's.
  const owner = useRef<string | null>(null);
  useEffect(() => {
    if (owner.current !== id) {
      owner.current = id;
      return;
    }
    // Never an empty list: on open that's the state before the saved transcript loads (and would erase it).
    if (!id || items.length === 0) return;
    try {
      localStorage.setItem(storageKey(id), JSON.stringify(items.slice(-500)));
      localStorage.setItem(seqKey(id), String(applied.current));
    } catch {}
  }, [id, items]);

  const send = useCallback((text: string) => {
    setItems((items) => [...items, { kind: "user", text }]);
    // A /command only turns busy if it starts a turn (a skill): turn_start says so.
    if (!text.startsWith("/")) setBusy(true);
    if (ready.current && connection.current) connection.current.send(message(text));
    else pending.current.push(message(text));
  }, []);

  /** A new chat's first message: call just before switching to `chatId`; it's sent once the session is ready. */
  const startWith = useCallback((chatId: string, text: string) => {
    first.current = { id: chatId, text };
  }, []);

  const mcp = useCallback((action: string, name?: string) => connection.current?.send({ type: "mcp", action, name }), []);

  const interrupt = useCallback(() => connection.current?.send({ type: "interrupt" }), []);
  const answer = useCallback((rid: number, decision: "yes" | "always" | "no") => connection.current?.send({ type: "approval", id: rid, decision }), []);

  return { items, status, busy, error, commands, meshName, servers, mcp, send, startWith, interrupt, answer };
}
