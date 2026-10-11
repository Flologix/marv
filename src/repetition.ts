// Repetition detection: catch a model stuck in a loop repeating text or tool calls.
//
// Text: the same sentence (normalized) repeated 8+ times consecutively, across
// the last ~4 KB of the reply. Sentences are split on . ! ? and newlines.
//
// Tool calls: a period-1 (A,A,A,A…) or period-2 (A,B,A,B…) cycle across steps,
// detected from a short history. A write tool (edit_file, write_file, bash)
// resets, since a re-read after a change is legitimate.

/** Split text into sentence-like units. Splits on . ! ? followed by whitespace or end, and on newlines. */
export function splitSentences(text: string): string[] {
  const parts: string[] = [];
  let start = 0;
  for (let i = 0; i < text.length; i++) {
    const ch = text[i]!;
    if (ch === "\n" || (ch === "." || ch === "!" || ch === "?") && (i === text.length - 1 || /\s/.test(text[i + 1]!))) {
      const unit = text.slice(start, i + 1).trim();
      if (unit) parts.push(unit);
      start = i + 1;
    }
  }
  const rest = text.slice(start).trim();
  if (rest) parts.push(rest);
  return parts.length ? parts : text.split("\n").filter(Boolean);
}

/**
 * Check whether the accumulated reply text shows degenerate repetition:
 * the same sentence-like unit (after trimming and lowercasing) appears 8+
 * times in a row. Only catches units that are "sentence-like" (>= 20 chars,
 * >= 2 words) to avoid catching code fragments, list markers, etc.
 *
 * Only the last ~4 KB of text is checked, so a burst near the end is caught
 * early and earlier text doesn't keep a busy reply from ever triggering.
 */
export function detectTextRepetition(text: string): boolean {
  if (text.length < 20) return false;

  // Take the last ~4 KB.
  const tail = text.length > 4096 ? text.slice(-4096) : text;

  const units = splitSentences(tail);
  if (units.length < 8) return false;

  let consecutive = 1;
  for (let i = units.length - 2; i >= 0; i--) {
    const current = units[i]!.trim().toLowerCase();
    const prev = units[i + 1]!.trim().toLowerCase();
    if (current === prev && isSentenceLike(current)) {
      consecutive++;
      if (consecutive >= 8) return true;
    } else {
      consecutive = 1;
    }
  }
  return false;
}

/** A "sentence-like" unit: at least 20 characters and at least 2 space-separated words. */
function isSentenceLike(unit: string): boolean {
  return unit.length >= 20 && unit.split(/\s+/).filter(Boolean).length >= 2;
}

/** A normalized key for a tool call: compared across steps. */
export interface ToolCallKey {
  name: string;
  /** Arguments, normalized by parsing and re-stringifying with sorted keys. */
  args: string;
}

/** State kept across steps for tool-call loop detection. */
export interface ToolLoopState {
  /** Recent steps' calls (at most 8). */
  history: ToolCallKey[][];
  /** Whether we already warned about the loop. */
  warned: boolean;
}

/** The value with every object's keys sorted, at every depth (nothing dropped), so key order doesn't matter. */
function sortKeys(value: unknown): unknown {
  if (Array.isArray(value)) return value.map(sortKeys);
  if (value && typeof value === "object") {
    return Object.fromEntries(Object.keys(value).sort().map((key) => [key, sortKeys((value as Record<string, unknown>)[key])]));
  }
  return value;
}

export function makeToolKey(call: { name: string; arguments: string; id?: string }): ToolCallKey {
  let normalized: string;
  try {
    // Not JSON.stringify(parsed, sortedTopLevelKeys): a key list there filters nested objects too, so
    // {"filter":{"id":1}} and {"filter":{"id":2}} would compare equal.
    normalized = JSON.stringify(sortKeys(JSON.parse(call.arguments)));
  } catch {
    normalized = call.arguments;
  }
  return { name: call.name, args: normalized };
}

/**
 * Detect a period-1 or period-2 cycle across recent steps from `history`.
 * Example: A,A,A,A,A (period 1) or A,B,A,B,A,B (period 2).
 * The pattern must appear at least 2 full periods in the window (i.e., for
 * period 1: at least 3 entries; for period 2: at least 5 entries), and the
 * oldest entries are dropped as new ones arrive.
 */
function cycleDetected(history: ToolCallKey[][]): boolean {
  if (history.length < 3) return false;
  const keys = history.map((calls) => JSON.stringify(calls));

  // Period 1: all same.
  if (keys.length >= 3 && keys.every((k) => k === keys[0]!)) return true;

  // Period 2: A,B,A,B,A,B…  Must have at least 5 entries.
  if (keys.length >= 5 && keys[0] === keys[2] && keys[1] === keys[3]) {
    // Check the rest follow the pattern.
    for (let i = 4; i < keys.length; i++) {
      if (keys[i] !== keys[i % 2]!) return false;
    }
    return true;
  }

  return false;
}

/**
 * Update the state with a new step's calls. Returns true when a loop is
 * detected (a period-1 or period-2 cycle across recent steps).
 *
 * A step that did something write-like (edit_file, write_file, bash) resets
 * the history: re-reading a file after editing it is legitimate.
 */
export function toolLoopDetected(state: ToolLoopState, calls: ToolCallKey[]): boolean {
  // A write tool resets the entire history.
  if (calls.some((c) => c.name === "edit_file" || c.name === "write_file" || c.name === "bash")) {
    state.history = [calls];
    return false;
  }

  // Only steps with the same number of calls are comparable.
  const prev = state.history.at(-1);
  if (prev && prev.length !== calls.length) {
    state.history = [calls];
    return false;
  }

  state.history.push(calls);
  // Keep at most 8 entries: enough to spot periods 1 and 2, cheap.
  if (state.history.length > 8) state.history.shift();

  return cycleDetected(state.history);
}