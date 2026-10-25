import { useCallback, useEffect, useRef, useState } from "react";
import { type ChatSettings, type Connection, connect } from "./backend";
import * as Q from "./queue";
import { type Answer, type Item, apply, restore, settle } from "./transcript";

export type { Answer, ApprovalRequest, DiffLine, Item, Usage } from "./transcript";

/** An entry of the / menu: a command, or one of the user's skills. */
export interface Command {
  name: string;
  description: string;
  skill?: boolean;
}

const storageKey = (id: string) => `marv-transcript-${id}`;
/** The number of the last event the saved transcript has: a chat reopened gets what came after (#155). */
const seqKey = (id: string) => `marv-seq-${id}`;
/** Which numbering that number is in: the app numbers a chat's events afresh each time it starts. */
const epochKey = (id: string) => `marv-epoch-${id}`;

function loadEpoch(id: string): string | null {
  try {
    return localStorage.getItem(epochKey(id));
  } catch {
    return null;
  }
}

function loadSeq(id: string): number {
  try {
    return Number(localStorage.getItem(seqKey(id)) ?? 0) || 0;
  } catch {
    return 0;
  }
}

/** What the page sends for a line: a /command runs in Marv, anything else is a message to the model. */
const message = (text: string) => (text.startsWith("/") ? { type: "command", line: text } : { type: "send", text });

/** The saved transcript; its undecided approvals wait for the session to say whether they're still live. */
function load(id: string): Item[] {
  try {
    return restore(JSON.parse(localStorage.getItem(storageKey(id)) ?? "[]"));
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
    localStorage.removeItem(epochKey(id));
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
  /** "Send now" messages sent into the running turn and not in it yet. */
  const [sending, setSending] = useState(0);
  /** #184: messages queued for after the running turn (`queue.ts`); the ref is what the event handler reads. */
  /** Kept with the chat it belongs to: another chat starts with none. */
  const [owned, setOwned] = useState<{ id: string | null; queue: Q.Queue }>({ id, queue: Q.EMPTY });
  const queue = owned.id === id ? owned.queue : Q.EMPTY;
  const queueRef = useRef<{ id: string | null; queue: Q.Queue }>({ id, queue: Q.EMPTY });
  const idRef = useRef(id);
  /** The open chat's queue, as the handlers see it. */
  const currentQueue = useCallback(() => (queueRef.current.id === idRef.current ? queueRef.current.queue : Q.EMPTY), []);
  const setQueue = useCallback((q: Q.Queue) => {
    queueRef.current = { id: idRef.current, queue: q };
    setOwned(queueRef.current);
  }, []);
  /** How the last turn ended, until its usage closes it. */
  const lastEnd = useRef<string | null>(null);
  /** `send`, for the event handler (declared below it). */
  const sendRef = useRef<(text: string) => void>(() => {});
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
    idRef.current = id;
  }, [id]);

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
      connect(id, settingsRef.current, { generation: reload, after: received.current, epoch: loadEpoch(id) }, (e) => {
        if (!live) return;
        // Another numbering than the saved one (the app restarted): this page's numbers start over with it, and the
        // app sends every event it kept.
        if (e.type === "ready") {
          // Approvals restored undecided: live again only in the same run (the epoch the page saved) and only if the
          // session says their request still waits; after a restart none is, whatever its id.
          const sameRun = typeof e.epoch === "string" && e.epoch === loadEpoch(id);
          const waiting =
            sameRun && Array.isArray(e.waiting) ? e.waiting.filter((w): w is number => typeof w === "number") : [];
          setItems((items) => settle(items, waiting));
        }
        if (e.type === "ready" && typeof e.epoch === "string" && e.epoch !== loadEpoch(id)) {
          received.current = applied.current = 0;
          try {
            localStorage.setItem(epochKey(id), e.epoch);
          } catch {}
        }
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
          if (e.type === "sent_now") setSending((n) => Math.max(0, n - 1));
          if (e.type === "turn_end") setSending(0);
          // "Send now" text the turn never read: first in the queue, where the turn's end decides (#184).
          if (e.type === "not_sent") {
            setSending(0);
            const texts = Array.isArray(e.texts) ? (e.texts as unknown[]).map(String) : [];
            setQueue(Q.unread(currentQueue(), texts));
          }
          setItems((items) => {
            if (typeof e.seq === "number") applied.current = e.seq;
            return apply(items, e);
          });
          // The turn is over once its usage came (the host sends it after turn_end, and it ends `busy`): after a turn
          // that finished the oldest queued message goes next; after a stop they wait.
          if (e.type === "turn_end") lastEnd.current = String(e.reason);
          if (e.type === "usage" && lastEnd.current !== null) {
            const { next, queue } = Q.afterTurn(currentQueue(), lastEnd.current);
            lastEnd.current = null;
            setQueue(queue);
            if (next !== undefined) sendRef.current(next);
          }
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
    // The person sent: a queue a stop held follows this turn.
    setQueue(Q.release(currentQueue()));
    setItems((items) => [...items, { kind: "user", text }]);
    // A /command only turns busy if it starts a turn (a skill): turn_start says so.
    if (!text.startsWith("/")) setBusy(true);
    if (ready.current && connection.current) connection.current.send(message(text));
    else pending.current.push(message(text));
  }, [currentQueue, setQueue]);

  /** A new chat's first message: call just before switching to `chatId`; it's sent once the session is ready. */
  const startWith = useCallback((chatId: string, text: string) => {
    first.current = { id: chatId, text };
  }, []);

  const mcp = useCallback((action: string, name?: string) => connection.current?.send({ type: "mcp", action, name }), []);

  /** "Send now": into the running turn at its next step (after the current reply and tool calls), not after it. */
  /** False when nothing was sent (the chat isn't connected and ready): the composer keeps the text. */
  const sendNow = useCallback((text: string): boolean => {
    if (!ready.current || !connection.current) return false;
    connection.current.send({ type: "send_now", text });
    setSending((n) => n + 1);
    return true;
  }, []);
  useEffect(() => {
    sendRef.current = send;
  }, [send]);

  /** Enter while a turn runs (#184): queued, sent when the turn finishes. */
  const enqueue = useCallback((text: string) => setQueue(Q.push(currentQueue(), text)), [currentQueue, setQueue]);
  /** Takes queued message `i` out (to edit or drop it); the newest when `i` is left out. */
  const unqueue = useCallback(
    (i?: number): string | undefined => {
      const { text, queue } = Q.take(currentQueue(), i ?? currentQueue().texts.length - 1);
      setQueue(queue);
      return text;
    },
    [currentQueue, setQueue],
  );
  /** Idle, with a queue a stop held: sends the oldest now. */
  const sendQueued = useCallback(() => {
    const { text, queue } = Q.take(currentQueue(), 0);
    setQueue(queue);
    if (text !== undefined) send(text);
  }, [currentQueue, send, setQueue]);

  const interrupt = useCallback(() => connection.current?.send({ type: "interrupt" }), []);
  const answer = useCallback((rid: number, decision: Answer) => connection.current?.send({ type: "approval", id: rid, decision }), []);

  return {
    items,
    status,
    busy,
    error,
    commands,
    meshName,
    servers,
    mcp,
    send,
    sendNow,
    sending,
    queued: queue.texts,
    held: queue.held,
    enqueue,
    unqueue,
    sendQueued,
    startWith,
    interrupt,
    answer,
  };
}
