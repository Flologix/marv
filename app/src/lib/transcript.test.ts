// `npm test`: plain Node (node:test), the TypeScript stripped.
import assert from "node:assert/strict";
import { test } from "node:test";
import { type Item, apply, restore, settle } from "./transcript.ts";

const ask = (id: number, command = "git commit") => ({ type: "approval", id, tool: "bash", label: command, title: "Run a command", command, scope: "this exact command", once: false });
const done = (id: number, decision: string) => ({ type: "approval_done", id, decision });
const fold = (items: Item[], ...events: Record<string, unknown>[]) => events.reduce<Item[]>((acc, e) => apply(acc, e as { type: string }), items);
const cards = (items: Item[]) => items.flatMap((it) => (it.kind === "approval" ? [{ id: it.request.id, decision: it.decision, restored: it.restored }] : []));

test("approval_done marks the card still waiting, not one decided already with the same id", () => {
  const items = fold([], ask(1, "ls"), done(1, "yes"), ask(1, "rm -r build"), done(1, "no"));
  assert.deepEqual(cards(items), [
    { id: 1, decision: "yes", restored: undefined },
    { id: 1, decision: "no", restored: undefined },
  ]);
});

test("an approval sent twice is one card", () => {
  const items = fold([], ask(7), { type: "text_delta", text: "hi" }, ask(7));
  assert.equal(cards(items).length, 1);
  assert.equal(items.length, 2);
});

test("an approval_done with no waiting card changes nothing", () => {
  const items = fold([], ask(3), done(3, "yes"));
  assert.equal(apply(items, { type: "approval_done", id: 3, decision: "no" }), items);
  assert.equal(apply(items, { type: "approval_done", id: 4, decision: "no" }), items);
  // An answer for a request that's still live never lands on an expired card with another id.
  const both = fold(settle(restore(fold([], ask(8))), []), ask(9), done(9, "yes"));
  assert.deepEqual(cards(both).map((c) => c.decision), ["expired", "yes"]);
});

test("a restored transcript's undecided cards wait for the session, then expire after a restart", () => {
  const saved = JSON.parse(JSON.stringify(fold([], ask(10), done(10, "always"), ask(11)))) as Item[];
  const loaded = restore(saved);
  assert.deepEqual(cards(loaded), [
    { id: 10, decision: "always", restored: undefined },
    { id: 11, decision: undefined, restored: true },
  ]);
  // Another session: the request is gone.
  const restarted = settle(loaded, []);
  assert.deepEqual(cards(restarted), [
    { id: 10, decision: "always", restored: undefined },
    { id: 11, decision: "expired", restored: undefined },
  ]);
  // The new run's own request is a new, live card.
  assert.deepEqual(cards(fold(restarted, ask(900)))[2], { id: 900, decision: undefined, restored: undefined });
});

test("at ready, a restored card is live only if its id is among the approvals still waiting", () => {
  const loaded = restore(fold([], ask(4), ask(5)));
  assert.deepEqual(cards(settle(loaded, [5, 9])), [
    { id: 4, decision: "expired", restored: undefined },
    { id: 5, decision: undefined, restored: undefined },
  ]);
});

test("a restored card answered while the page was away: its replayed answer marks it", () => {
  const settled = settle(restore(fold([], ask(6))), []);
  assert.deepEqual(cards(fold(settled, done(6, "always"))), [{ id: 6, decision: "always", restored: undefined }]);
});

test("in the same session (a chat still running in the background) a restored card is live again", () => {
  const loaded = restore(fold([], ask(5)));
  const same = settle(loaded, [5]);
  assert.deepEqual(cards(same), [{ id: 5, decision: undefined, restored: undefined }]);
  // Replayed on reattach: still one card, and its answer lands on it.
  assert.deepEqual(cards(fold(same, ask(5), done(5, "yes"))), [{ id: 5, decision: "yes", restored: undefined }]);
});

test("settle leaves a transcript with nothing restored as it is", () => {
  const items = fold([], ask(1), done(1, "yes"));
  assert.equal(settle(items, []), items);
});
