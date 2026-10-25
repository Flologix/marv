// `npm test`: plain Node (node:test), the TypeScript stripped.
import assert from "node:assert/strict";
import { test } from "node:test";
import { EMPTY, afterTurn, push, release, take, unread } from "./queue.ts";

test("a finished turn sends the oldest queued message next, one per turn", () => {
  const q = push(push(EMPTY, "run the tests"), "then lint");
  const first = afterTurn(q, "end");
  assert.equal(first.next, "run the tests");
  assert.deepEqual(first.queue, { texts: ["then lint"], held: false });
  const second = afterTurn(first.queue, "end");
  assert.equal(second.next, "then lint");
  assert.deepEqual(afterTurn(second.queue, "end"), { queue: EMPTY });
});

test("a stopped or failed turn holds the queue until the person sends", () => {
  for (const reason of ["aborted", "interrupted", "error", "max_steps"]) {
    const held = afterTurn(push(EMPTY, "a"), reason);
    assert.equal(held.next, undefined, reason);
    assert.deepEqual(held.queue, { texts: ["a"], held: true });
    // Held: even a finished turn (a woken one) doesn't send it on its own.
    assert.equal(afterTurn(held.queue, "end").next, undefined);
    // The person sent something: the queue follows that turn.
    assert.equal(afterTurn(release(held.queue), "end").next, "a");
  }
});

test("send-now text the turn never read goes first; take edits, drops or sends one", () => {
  const q = unread(push(EMPTY, "later"), ["wait, use pnpm"]);
  assert.deepEqual(q.texts, ["wait, use pnpm", "later"]);
  const last = take(q, q.texts.length - 1);
  assert.equal(last.text, "later");
  assert.deepEqual(last.queue.texts, ["wait, use pnpm"]);
  assert.deepEqual(take(last.queue, 5), { queue: last.queue });
  // Emptied, a held queue holds nothing any more.
  const emptied = take({ texts: ["x"], held: true }, 0);
  assert.deepEqual(emptied, { text: "x", queue: EMPTY });
});
