"use client";

import { useCallback, useEffect, useRef, useState } from "react";

/** marv-web's WebSocket (crates/marv-web). */
const SERVER = process.env.NEXT_PUBLIC_MARV_WS;

export interface DiffLine {
  kind: "add" | "del" | "ctx" | "gap";
  text: string;
  oldLine?: number;
  newLine?: number;
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
  mustAsk: boolean;
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
      approval?: string;
      diff?: { lines: DiffLine[]; more: number };
    }
  | { kind: "approval"; request: ApprovalRequest; decision?: string }
  | { kind: "notice"; text: string; tone: "info" | "warn" | "error" }
  | { kind: "turn"; reason: string; usage?: Usage };

export interface Settings {
  model: string;
  maxSteps: number;
  /** Sandboxed commands and edits in the workspace run without asking. */
  yolo: boolean;
}

export type Status = "connecting" | "ready" | "closed" | "fatal";

const NOTICES: Record<string, string> = {
  step_limit: "Stopped at the step limit.",
  cut_off: "The reply hit the output limit.",
  text_loop: "The reply was repeating itself.",
  tool_loop: "The same tool calls kept repeating.",
};

/** Folds one server event into the transcript. */
function apply(items: Item[], e: Record<string, unknown>): Item[] {
  const last = items[items.length - 1];
  const next = items.slice();
  const find = (id: unknown) => next.findIndex((it) => it.kind === "tool" && it.id === id);
  switch (e.type) {
    case "text_delta":
    case "thinking_delta": {
      const kind = e.type === "text_delta" ? "text" : "thinking";
      if (last?.kind === kind) next[next.length - 1] = { ...last, text: last.text + e.text };
      else next.push({ kind, text: String(e.text) });
      return next;
    }
    case "assistant":
      if (last?.kind !== "text" && e.text) next.push({ kind: "text", text: String(e.text) });
      return next;
    case "tool_start":
      if (find(e.id) < 0) {
        next.push({ kind: "tool", id: String(e.id), tool: String(e.tool), label: String(e.label ?? ""), agent: e.agent as string | undefined, done: false });
      }
      return next;
    case "tool_end":
    case "tool_skipped": {
      const i = find(e.id);
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
        approval: e.approval as string | undefined,
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
      next.push({ kind: "notice", text: `Read untrusted content (${e.source}): anything that sends data out will ask first.`, tone: "info" });
      return next;
    case "step_limit":
    case "cut_off":
    case "text_loop":
    case "tool_loop":
      if (e.continued) return items;
      next.push({ kind: "notice", text: NOTICES[String(e.type)], tone: "warn" });
      return next;
    case "notice":
      next.push({ kind: "notice", text: String(e.text), tone: "info" });
      return next;
    case "error":
      next.push({ kind: "notice", text: String(e.message), tone: "error" });
      return next;
    case "turn_end":
      next.push({ kind: "turn", reason: String(e.reason) });
      return next;
    case "usage": {
      for (let i = next.length - 1; i >= 0; i--) {
        const it = next[i];
        if (it.kind === "turn") {
          next[i] = { ...it, usage: e as unknown as Usage };
          break;
        }
      }
      return next;
    }
    default:
      return items;
  }
}

const storageKey = (chatId: string) => `marv-chat-${chatId}`;

function load(chatId: string): Item[] {
  try {
    return JSON.parse(localStorage.getItem(storageKey(chatId)) ?? "[]");
  } catch {
    return [];
  }
}

/** A chat with marv-web: the transcript, the connection, and what the page can do. */
export function useMarv(chatId: string, settings: Settings | null) {
  const [items, setItems] = useState<Item[]>([]);
  const [status, setStatus] = useState<Status>("connecting");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const socket = useRef<WebSocket | null>(null);
  const settingsRef = useRef(settings);
  const sent = useRef<Settings | null>(null);

  useEffect(() => {
    settingsRef.current = settings;
  }, [settings]);

  // The chat's saved transcript, then the connection (opened a beat late, so a dev double-mount doesn't open two).
  useEffect(() => {
    if (!chatId || !settingsRef.current) return;
    setItems(load(chatId));
    let closed = false;
    let retries = 0;
    let timer = setTimeout(connect, 30);

    function connect() {
      const url = SERVER ?? `ws://${location.hostname}:8787/ws`;
      const ws = new WebSocket(url);
      socket.current = ws;
      setStatus("connecting");
      ws.onopen = () => {
        const s = settingsRef.current!;
        sent.current = s;
        ws.send(JSON.stringify({ type: "hello", chatId, ...s }));
      };
      ws.onmessage = (message) => {
        const e = JSON.parse(message.data);
        if (e.type === "ready") {
          retries = 0;
          setStatus("ready");
          setError(null);
        } else if (e.type === "fatal") {
          setError(e.message);
          // "Open in another tab" can be this tab's last connection, still closing: try again.
          if (!e.retry) {
            setStatus("fatal");
            closed = true;
          }
        } else {
          if (e.type === "turn_start") setBusy(true);
          if (e.type === "usage") setBusy(false);
          setItems((items) => apply(items, e));
        }
      };
      ws.onclose = () => {
        setBusy(false);
        if (closed) return;
        setStatus("closed");
        if (retries++ < 8) timer = setTimeout(connect, Math.min(500 * 2 ** retries, 8000));
      };
    }

    return () => {
      closed = true;
      clearTimeout(timer);
      socket.current?.close();
      socket.current = null;
    };
  }, [chatId, settings === null]); // eslint-disable-line react-hooks/exhaustive-deps

  // Settings changed mid-chat: from the next turn.
  useEffect(() => {
    const ws = socket.current;
    if (!settings || status !== "ready" || !ws || ws.readyState !== WebSocket.OPEN) return;
    const before = sent.current;
    if (before && before.model === settings.model && before.maxSteps === settings.maxSteps && before.yolo === settings.yolo) return;
    sent.current = settings;
    ws.send(JSON.stringify({ type: "configure", ...settings }));
  }, [settings, status]);

  useEffect(() => {
    if (!chatId) return;
    try {
      localStorage.setItem(storageKey(chatId), JSON.stringify(items.slice(-400)));
    } catch {}
  }, [chatId, items]);

  const send = useCallback((text: string) => {
    const ws = socket.current;
    if (!ws || ws.readyState !== WebSocket.OPEN) return false;
    setItems((items) => [...items, { kind: "user", text }]);
    setBusy(true);
    ws.send(JSON.stringify({ type: "send", text }));
    return true;
  }, []);

  const interrupt = useCallback(() => socket.current?.send(JSON.stringify({ type: "interrupt" })), []);

  const answer = useCallback(
    (id: number, decision: "yes" | "always" | "no") => socket.current?.send(JSON.stringify({ type: "approval", id, decision })),
    [],
  );

  return { items, status, error, busy, send, interrupt, answer };
}
