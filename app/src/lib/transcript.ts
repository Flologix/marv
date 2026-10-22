// The transcript a chat shows, and how each event folds into it. No imports, so its tests run under plain Node
// (`npm test`).

/** An event from the session (`backend.ts`'s `ServerEvent`). */
type ServerEvent = Record<string, unknown> & { type: string };

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
  /** #185: a model's call, which may be declined with a reason (not the step-limit question, a wire, /undo, /mcp add). */
  reason?: boolean;
}

/** The person's answer to an approval. `declined` (#185): not run, the reason goes to the model and the turn goes on. */
export type Answer = "yes" | "always" | "no" | { declined: string };

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
  /**
   * `restored`: undecided in a transcript loaded from storage, and not yet known to still wait (its chat may have
   * restarted since): shown without buttons until the session says (`settle`). `decision` "expired": its request no
   * longer waits (the app quit or the chat restarted), so there's nothing left to answer.
   */
  | { kind: "approval"; request: ApprovalRequest; decision?: string; reason?: string; restored?: boolean }
  | { kind: "notice"; text: string; tone: "info" | "warn" | "error" }
  | { kind: "turn"; reason: string; usage?: Usage }
  /** What a /command printed. */
  | { kind: "output"; text: string };

const STOPS: Record<string, string> = {
  step_limit: "Stopped at the step limit.",
  cut_off: "The reply hit the output limit.",
  text_loop: "The reply was repeating itself.",
  tool_loop: "The same tool calls kept repeating.",
};

/** Folds one event into the transcript. */
export function apply(items: Item[], e: ServerEvent): Item[] {
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
    case "tool_progress": {
      // A running call's status line (#201: "waiting for beta to accept the wire"); tool_end replaces it.
      const i = tool(e.id);
      if (i < 0 || (next[i] as Extract<Item, { kind: "tool" }>).done) return items;
      next[i] = { ...(next[i] as Extract<Item, { kind: "tool" }>), summary: String(e.line ?? "") };
      return next;
    }
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
      // Sent again (a reattach replaying what it kept): one card per request, as with `tool_start`. Only a card still
      // waiting counts, so a decided card with the same id (a transcript from before ids were unique) never hides a
      // new request.
      if (next.some((it) => it.kind === "approval" && it.request.id === e.id && !it.decision)) return items;
      next.push({ kind: "approval", request: e as unknown as ApprovalRequest });
      return next;
    case "approval_done": {
      // The card still waiting for this answer, never one decided already; else one `settle` gave up on (answered
      // while the page was away, the answer replayed after `ready`).
      const card = (expired: boolean) => next.findIndex((it) => it.kind === "approval" && it.request.id === e.id && (expired ? it.decision === "expired" : !it.decision));
      const waiting = card(false);
      const i = waiting >= 0 ? waiting : card(true);
      if (i < 0) return items;
      next[i] = { ...(next[i] as Extract<Item, { kind: "approval" }>), decision: String(e.decision), reason: typeof e.reason === "string" ? e.reason : undefined };
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

/** A transcript loaded from storage: its undecided approvals may be from a session that's gone, so none is live yet. */
export function restore(items: Item[]): Item[] {
  return items.map((it) => (it.kind === "approval" && !it.decision ? { ...it, restored: true } : it));
}

/**
 * The session is ready, and says which approvals still wait (`waiting`: their ids). A restored card is live again
 * only if its request is among them (a chat still running in the background). Any other's request is gone (the app
 * or the chat restarted, or it was answered while the page was away): its card says it wasn't answered here, and an
 * `approval_done` replayed after this still marks it.
 */
export function settle(items: Item[], waiting: readonly number[]): Item[] {
  if (!items.some((it) => it.kind === "approval" && it.restored)) return items;
  return items.map((it) => {
    if (it.kind !== "approval" || !it.restored) return it;
    const { restored: _, ...card } = it;
    return card.decision || waiting.includes(card.request.id) ? card : { ...card, decision: "expired" };
  });
}
