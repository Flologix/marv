import { expect, test } from "bun:test";
import { formatStats, summarize } from "../src/trajectory-stats.ts";

const base = { v: 1, t: "2026-10-05T00:00:00Z", session: "s1" };
const turn = (id: string, model: string) => ({ ...base, type: "turn_start", turn: id, text: "x", provider: "ollama", model });
const end = (id: string, reason: string, ms = 1000, tools = 1) => ({ ...base, type: "agent_end", turn: id, agent: "main", reason, steps: 2, tools, ms });
const request = (id: string, agent: string, tokens: number) => ({
  ...base,
  type: "request",
  turn: id,
  agent,
  step: 1,
  usage: { promptTokens: tokens, completionTokens: 0 },
  ms: 10,
});
const feedback = (id: string, score: number, source: string) => ({ ...base, type: "feedback", turn: id, score, source });

test("per model: turns, how they ended, tokens (subagents included), and feedback (explicit wins over implicit)", () => {
  const records = [
    { ...base, type: "session", marv: "0.1.0", model: "a" },
    turn("t1", "a"),
    request("t1", "main", 1000),
    request("t1", "t1.1", 500),
    end("t1", "end", 2000, 3),
    feedback("t1", -1, "implicit"),
    feedback("t1", 1, "explicit"), // the user's own rating decides
    turn("t2", "a"),
    request("t2", "main", 3000),
    end("t2", "declined"),
    feedback("t2", -1, "implicit"),
    turn("t3", "b"),
    end("t3", "end"),
    feedback("t3", 0, "explicit"), // a label alone isn't a rating
  ];
  const stats = summarize(records);
  expect(stats.get("a")).toEqual({
    turns: 2,
    ended: { end: 1, declined: 1 },
    tokensPerTurn: 2250,
    toolsPerTurn: 2,
    medianSeconds: 1.5,
    rated: 2,
    good: 1,
    bad: 1,
    doors: {},
  });
  expect(stats.get("b")).toMatchObject({ turns: 1, rated: 0, good: 0, bad: 0 });
  const table = formatStats(stats);
  expect(table).toContain("a");
  expect(table).toContain("50%"); // good of rated
});

test("can group by Marv version instead", () => {
  const records = [{ ...base, type: "session", marv: "0.2.0", model: "a" }, turn("t1", "a"), end("t1", "end")];
  expect([...summarize(records, "marv").keys()]).toEqual(["0.2.0"]);
});

test("counts calls through each door out of the sandbox, subagents' included (#52)", () => {
  const tool = (id: string, agent: string, door?: string) => ({ ...base, type: "tool", turn: id, agent, ...(door ? { door } : {}) });
  const records = [turn("t1", "a"), tool("t1", "main", "host"), tool("t1", "main"), tool("t1", "t1.1", "github"), end("t1", "end"), turn("t2", "a"), tool("t2", "main", "host"), end("t2", "end")];
  const stats = summarize(records);
  expect(stats.get("a")?.doors).toEqual({ host: 2, github: 1 });
  expect(formatStats(stats)).toContain("host 2, github 1");
});
