// #184: messages typed while a turn runs, queued for after it (the TUI's queue, kept by the page here). Pure, so
// `npm test` covers it; `useChat` holds the state.

/** The queue, oldest first, and whether a stop (or a failure) holds it for the person. */
import type { Item } from "./transcript.ts";

export type Queue = { texts: string[]; held: boolean };

export const EMPTY: Queue = { texts: [], held: false };

/** Enter while a turn runs: last in line. The person sent another message, so a held queue goes on again. */
export function push(q: Queue, text: string): Queue {
  return { texts: [...q.texts, text], held: false };
}

/** "Send now" text the turn never read (it ended first): ahead of what was queued after it. */
export function unread(q: Queue, texts: string[]): Queue {
  return texts.length ? { ...q, texts: [...texts, ...q.texts] } : q;
}

/** A queued message the host refused (a mesh wake took the session first): first in line again, to go when idle. */
export function refused(q: Queue, text: string): Queue {
  return { ...q, texts: [text, ...q.texts] };
}

/**
 * The host's `send_refused` for `text`: first in the queue again, and its user item (the newest with that text) out of
 * the transcript, since it was never sent. Returns the queue and a function of the items, for `setItems`.
 */
export function refusedSend(q: Queue, text: string): { queue: Queue; items: (items: Item[]) => Item[] } {
  return {
    queue: refused(q, text),
    items: (items) => {
      const at = items.findLastIndex((it) => it.kind === "user" && it.text === text);
      return at < 0 ? items : [...items.slice(0, at), ...items.slice(at + 1)];
    },
  };
}

/** Takes entry `i` out (to edit it, to drop it, or the oldest to send now). An empty queue holds nothing. */
export function take(q: Queue, i: number): { text?: string; queue: Queue } {
  if (i < 0 || i >= q.texts.length) return { queue: q };
  const texts = q.texts.filter((_, j) => j !== i);
  return { text: q.texts[i], queue: { texts, held: q.held && texts.length > 0 } };
}

/**
 * A turn ended (`reason`, the session's name for it): after one that finished ("end") the oldest message goes next,
 * one per turn; after a stop or a failure the queue is held, and nothing starts on its own.
 */
export function afterTurn(q: Queue, reason: string): { next?: string; queue: Queue } {
  if (q.texts.length === 0) return { queue: EMPTY };
  if (reason !== "end" || q.held) return { queue: { ...q, held: true } };
  const [next, ...texts] = q.texts;
  return { next, queue: { texts, held: false } };
}

/** The person sent a message: a held queue follows that turn. */
export function release(q: Queue): Queue {
  return q.held ? { ...q, held: false } : q;
}
