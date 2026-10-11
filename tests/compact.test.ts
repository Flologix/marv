import { describe, expect, test } from "bun:test";
import { compactedHistory, summarize } from "../src/compact.ts";
import type { AgentEvent, ChatTurn } from "../src/provider/types.ts";
import { ScriptedProvider } from "./fake-provider.ts";

const HISTORY: ChatTurn[] = [
  { role: "user", text: "make the scroll smoother" },
  { role: "assistant", text: "", toolCalls: [{ id: "c1", name: "read_file", arguments: '{"path":"src/ui/ScrollView.tsx"}' }] },
  { role: "tool", callId: "c1", name: "read_file", text: "…file contents…" },
  { role: "assistant", text: "Done: it glides now." },
];
const say = (text: string): AgentEvent[] => [{ type: "text_delta", text }, { type: "done" }];

describe("summarize", () => {
  test("asks for a summary on top of the same prefix (system, tools, history), so the cache still hits", async () => {
    const model = new ScriptedProvider([say("Summary: scroll glide added in src/ui/ScrollView.tsx.")]);
    const tools = [{ name: "read_file", description: "d", parameters: {} }];
    const result = await summarize({ provider: model, history: HISTORY, system: "SYSTEM", tools, signal: new AbortController().signal });

    expect(result).toEqual({ summary: "Summary: scroll glide added in src/ui/ScrollView.tsx." });
    const request = model.requests[0]!;
    expect(request.options.system).toBe("SYSTEM");
    expect(request.options.tools).toBe(tools);
    expect(request.history.slice(0, HISTORY.length)).toEqual(HISTORY);
    expect(request.history.at(-1)).toMatchObject({ role: "user", text: expect.stringContaining("Summarize") });
  });

  test("a dropped connection mid-summary: only the retried attempt's text is the summary (review, #36)", async () => {
    // The provider retries from the start after sending part of a summary: the parts must not be joined, since
    // the summary replaces the whole conversation.
    const model = new ScriptedProvider([
      [
        { type: "text_delta", text: "Summary: half of a fir" },
        { type: "retry", attempt: 1, max: 2, message: "connection lost, retrying (1/2)…" },
        { type: "text_delta", text: "Summary: scroll glide added." },
        { type: "done" },
      ],
    ]);
    const result = await summarize({ provider: model, history: HISTORY, system: "S", tools: [], signal: new AbortController().signal });
    expect(result).toEqual({ summary: "Summary: scroll glide added." });
  });

  test("passes along what to focus on", async () => {
    const model = new ScriptedProvider([say("ok")]);
    await summarize({ provider: model, history: HISTORY, system: "S", tools: [], signal: new AbortController().signal, focus: "the sandbox" });
    expect((model.requests[0]!.history.at(-1) as { text: string }).text).toContain("Focus especially on: the sandbox");
  });

  test("reports errors and empty summaries instead of compacting", async () => {
    const failing = new ScriptedProvider([[{ type: "error", message: "Rate limited" }]]);
    expect(await summarize({ provider: failing, history: HISTORY, system: "S", tools: [], signal: new AbortController().signal })).toEqual({ error: "Rate limited" });
    const empty = new ScriptedProvider([say("   ")]);
    expect(await summarize({ provider: empty, history: HISTORY, system: "S", tools: [], signal: new AbortController().signal })).toMatchObject({ error: expect.any(String) });
  });
});

test("a reply that calls a tool, or is cut off, isn't a summary: the history is kept", async () => {
  const wantsTool = new ScriptedProvider([
    [{ type: "text_delta", text: "Let me re-read src/app.tsx first." }, { type: "tool_call", call: { id: "c1", name: "read_file", arguments: "{}" } }, { type: "done", reason: "tool_calls" }],
  ]);
  const cutOff = new ScriptedProvider([[{ type: "text_delta", text: "## Goal\nThe user wants to refac" }, { type: "done", reason: "length" }]]);
  for (const provider of [wantsTool, cutOff]) {
    const result = await summarize({ provider, history: [{ role: "user", text: "hi" }], system: "S", tools: [], signal: new AbortController().signal });
    expect(result).toEqual({ error: expect.stringMatching(/tool|cut off/) });
  }
});

test("compactedHistory starts the model's history over from the summary", () => {
  expect(compactedHistory("the summary")).toEqual([
    { role: "user", text: expect.stringContaining("the summary") },
    { role: "assistant", text: expect.any(String) },
  ]);
});
