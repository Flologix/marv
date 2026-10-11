import { describe, expect, test } from "bun:test";
import { applyEvent, createAgentLog } from "../src/agent-log.ts";

const call = { id: "r1", name: "read_file", arguments: '{"path":"a.ts"}' };

describe("a subagent's log", () => {
  test("starts with its task, as the user's message", () => {
    const log = createAgentLog({ title: "general-purpose · Read notes", prompt: "What's in notes.txt?" });
    expect(log.messages).toMatchObject([{ role: "user", text: "What's in notes.txt?" }]);
    expect(log.running).toBe(true);
  });

  test("streams text and thinking, then keeps each finished reply", () => {
    const log = createAgentLog({ title: "t", prompt: "p" });
    applyEvent(log, { type: "thinking_delta", text: "hmm" });
    expect(log.thinking).toBe("hmm");
    applyEvent(log, { type: "text_delta", text: "Look" });
    applyEvent(log, { type: "text_delta", text: "ing." });
    expect(log.streaming).toBe("Looking.");
    applyEvent(log, { type: "assistant", text: "Looking." });
    expect(log.streaming).toBe("");
    expect(log.thinking).toBe("");
    expect(log.messages.map((m) => [m.role, m.text])).toEqual([
      ["user", "p"],
      ["system", expect.stringMatching(/^✻ Thought for \d+s$/)],
      ["assistant", "Looking."],
    ]);
  });

  test("a tool call is one entry, updated when it ends", () => {
    const log = createAgentLog({ title: "t", prompt: "p" });
    applyEvent(log, { type: "tool_start", call, label: "a.ts" });
    expect(log.messages.at(-1)).toMatchObject({ role: "tool", text: "read_file", tool: { label: "a.ts", status: "running" } });
    const before = log.messages.at(-1);
    applyEvent(log, { type: "tool_end", call, result: { output: "x", summary: "12 lines", label: "a.ts" } });
    expect(log.messages).toHaveLength(2);
    expect(log.messages.at(-1)).toMatchObject({ tool: { status: "done", summary: "12 lines" } });
    expect(log.messages.at(-1)).not.toBe(before); // a new object, so the memoized view re-renders it
  });

  test("a failed call shows its message; a declined one says so", () => {
    const log = createAgentLog({ title: "t", prompt: "p" });
    applyEvent(log, { type: "tool_start", call, label: "a.ts" });
    applyEvent(log, { type: "tool_end", call, result: { output: "File not found: a.ts\nmore", summary: "error", isError: true, label: "a.ts" } });
    expect(log.messages.at(-1)).toMatchObject({ tool: { status: "error", summary: "File not found: a.ts" } });
    const w = { ...call, id: "w1", name: "write_file" };
    applyEvent(log, { type: "tool_start", call: w, label: "b.ts" });
    applyEvent(log, { type: "tool_end", call: w, result: { output: "no", summary: "declined", declined: true, label: "b.ts" } });
    expect(log.messages.at(-1)).toMatchObject({ tool: { status: "declined" } });
  });

  test("a subagent's edit keeps its diff for its view", () => {
    const log = createAgentLog({ title: "t", prompt: "p" });
    const call = { id: "c1", name: "edit_file", arguments: "{}" };
    const diff = { lines: [{ kind: "add" as const, text: "x", newLine: 1 }], more: 0 };
    applyEvent(log, { type: "tool_start", call, label: "a.txt" });
    applyEvent(log, { type: "tool_end", call, result: { output: "ok", summary: "+1 −0", label: "a.txt", diff } });
    expect(log.messages.at(-1)!.tool!.diff).toEqual(diff);
  });

  test("errors and how it stopped are shown", () => {
    const log = createAgentLog({ title: "t", prompt: "p" });
    applyEvent(log, { type: "text_delta", text: "half a rep" });
    applyEvent(log, { type: "error", message: "connection reset" });
    applyEvent(log, { type: "done", reason: "aborted" });
    expect(log.messages.slice(1).map((m) => [m.role, m.text, m.isError ?? false])).toEqual([
      ["assistant", "half a rep", false], // what had streamed isn't lost
      ["system", "connection reset", true],
      ["system", "Interrupted.", false],
    ]);
    expect(log.running).toBe(false);
  });
});

describe("a tool call still streaming", () => {
  test("is kept as a draft while it streams, and dropped when the call starts (or the step ends)", () => {
    const log = createAgentLog({ title: "t", prompt: "p" });
    applyEvent(log, { type: "tool_call_delta", index: 0, name: "write_file", text: '{"path":"a.js",' });
    applyEvent(log, { type: "tool_call_delta", index: 0, name: "write_file", text: '"content":"x' });
    expect(log.draft).toEqual({ index: 0, name: "write_file", args: '{"path":"a.js","content":"x' });
    applyEvent(log, { type: "tool_call_delta", index: 1, name: "bash", text: "{" });
    expect(log.draft).toEqual({ index: 1, name: "bash", args: "{" });
    applyEvent(log, { type: "tool_start", call: { id: "c1", name: "write_file", arguments: "{}" }, label: "a.js" });
    expect(log.draft).toBeUndefined();
  });
});

test("a retry drops what the failed attempt drew and says why (review, #36)", () => {
  const log = createAgentLog({ title: "t", prompt: "p" });
  applyEvent(log, { type: "thinking_delta", text: "hmm" });
  applyEvent(log, { type: "text_delta", text: "Half a rep" });
  applyEvent(log, { type: "tool_call_delta", index: 0, name: "read_file", text: '{"pa' });
  applyEvent(log, { type: "retry", attempt: 1, max: 2, message: "connection lost, retrying (1/2)…" });
  expect(log.streaming).toBe("");
  expect(log.thinking).toBe("");
  expect(log.draft).toBeUndefined();
  expect(log.messages.at(-1)).toMatchObject({ role: "system", text: "connection lost, retrying (1/2)…" });
});
