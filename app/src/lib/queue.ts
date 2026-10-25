// #184: messages typed while a turn runs, queued for after it (the TUI's queue, kept by the page here). Pure, so
// `npm test` covers it; `useChat` holds the state.

/** The queue, oldest first, and whether a stop (or a failure) holds it for the person. */
export type Queue = { texts: string[]; held: boolean };

export const EMPTY: Queue = { texts: [], held: false };

/** Enter while a turn runs: last in line. */
export function push(q: Queue, text: string): Queue {
  return { ...q, texts: [...q.texts, text] };
}

/** "Send now" text the turn never read (it ended first): ahead of what was queued after it. */
export function unread(q: Queue, texts: string[]): Queue {
  return texts.length ? { ...q, texts: [...texts, ...q.texts] } : q;
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
