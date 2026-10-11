import { describe, expect, test } from "bun:test";
import { detectTextRepetition, makeToolKey, splitSentences, toolLoopDetected, type ToolCallKey, type ToolLoopState } from "../src/repetition.ts";

describe("splitSentences", () => {
  test("splits on . followed by space", () => {
    expect(splitSentences("Hello world. How are you? I'm fine.")).toEqual(["Hello world.", "How are you?", "I'm fine."]);
  });

  test("splits on newlines too", () => {
    expect(splitSentences("Line one.\nLine two.\nLine three.")).toEqual(["Line one.", "Line two.", "Line three."]);
  });

  test("a long run-on with no punctuation is one unit", () => {
    expect(splitSentences("Fix the test scripts now Fix the test scripts now")).toEqual([
      "Fix the test scripts now Fix the test scripts now",
    ]);
  });

  test("empty input returns empty", () => {
    expect(splitSentences("")).toEqual([]);
  });
});

describe("detectTextRepetition", () => {
  test("'Fix the test scripts now. ' repeated 50 times with no newline is caught", () => {
    const text = "Fix the test scripts now. ".repeat(50);
    expect(detectTextRepetition(text)).toBe(true);
  });

  test("a normal paragraph with two repeated sentences isn't caught", () => {
    // Two identical sentences in a normal paragraph — that's fine.
    const text = "I looked at the file and it needs work. Fix the test scripts now. Fix the test scripts now. The rest of the function looks okay.";
    expect(detectTextRepetition(text)).toBe(false);
  });

  test("a sentence repeated 7 times is not caught", () => {
    const lines = Array.from({ length: 7 }, () => "Fix the test scripts now.");
    expect(detectTextRepetition(lines.join(". "))).toBe(false);
  });

  test("different sentences are not caught", () => {
    const text = "First step: install dependencies. Second step: configure the module. Third step: run the tests. Fourth step: verify the output.";
    expect(detectTextRepetition(text)).toBe(false);
  });

  test("short units (≤ 15 chars) are ignored (code fragments, list markers)", () => {
    const text = Array.from({ length: 10 }, () => "- milk").join("\n");
    expect(detectTextRepetition(text)).toBe(false);
  });

  test("blank lines don't trigger", () => {
    const text = ["Some text", ...Array.from({ length: 10 }, () => "")].join("\n");
    expect(detectTextRepetition(text)).toBe(false);
  });

  test("code with repeated lines but different content isn't caught", () => {
    const text = [
      "const x = 1;",
      "const y = 2;",
      "const z = 3;",
      "const w = 4;",
      "const v = 5;",
      "const u = 6;",
      "const t = 7;",
      "const s = 8;",
      "const r = 9;",
    ].join("\n");
    expect(detectTextRepetition(text)).toBe(false);
  });

  test("fewer than 8 units doesn't trigger", () => {
    expect(detectTextRepetition("One line.")).toBe(false);
    expect(detectTextRepetition("")).toBe(false);
  });

  test("a sentence repeated 8+ times with newlines is caught", () => {
    const text = Array.from({ length: 8 }, () => "Fix the test scripts now.").join("\n");
    expect(detectTextRepetition(text)).toBe(true);
  });
});

describe("makeToolKey", () => {
  test("parsed JSON arguments are normalized", () => {
    const key = makeToolKey({ id: "c1", name: "read_file", arguments: '{"path": "a.ts"}' });
    expect(key).toEqual({ name: "read_file", args: '{"path":"a.ts"}' });
  });

  test("unparseable JSON is kept as-is", () => {
    const key = makeToolKey({ id: "c1", name: "bash", arguments: "not json" });
    expect(key).toEqual({ name: "bash", args: "not json" });
  });

  test("differently ordered keys are normalized", () => {
    const a = makeToolKey({ id: "c1", name: "read_file", arguments: '{"path": "a.ts", "offset": 1}' });
    const b = makeToolKey({ id: "c2", name: "read_file", arguments: '{"offset": 1, "path": "a.ts"}' });
    expect(a).toEqual(b);
  });
});

describe("toolLoopDetected", () => {
  const readA = ({ name: "read_file", args: '{"path":"a.ts"}' } satisfies ToolCallKey);
  const readB = ({ name: "read_file", args: '{"path":"a.ts","offset":95,"limit":40}' } satisfies ToolCallKey);
  const readC = ({ name: "read_file", args: '{"path":"a.ts","offset":98,"limit":40}' } satisfies ToolCallKey);

  function fresh(): ToolLoopState {
    return { history: [], warned: false };
  }

  test("single step: nothing", () => {
    const state = fresh();
    expect(toolLoopDetected(state, [readA])).toBe(false);
    expect(state.history).toEqual([[readA]]);
  });

  test("A,A,A,A: period-1 detected", () => {
    const state = fresh();
    toolLoopDetected(state, [readA]);
    toolLoopDetected(state, [readA]);
    expect(toolLoopDetected(state, [readA])).toBe(true); // 3 entries: cycleDetected picks it up
  });

  test("A,B,A,B,A: period-2 detected", () => {
    const state = fresh();
    toolLoopDetected(state, [readB]); // offset 95
    toolLoopDetected(state, [readC]); // offset 98
    toolLoopDetected(state, [readB]); // offset 95 again
    toolLoopDetected(state, [readC]); // offset 98 again
    expect(toolLoopDetected(state, [readB])).toBe(true); // 5 entries: A,B,A,B,A → period-2
  });

  test("A,B,C,A,B,C doesn't trigger (not period 1 or 2)", () => {
    const state = fresh();
    for (let i = 0; i < 6; i++) {
      toolLoopDetected(state, [[readA, readB, readC][i % 3]!]);
    }
    // The last step: A vs B in the pattern, which doesn't match the 3-cycle pattern. It returns false.
    expect(state.history.length).toBeLessThanOrEqual(8);
    // No detection.
    expect(toolLoopDetected(state, [[readA, readB, readC][0]!])).toBe(false);
  });

  test("a write tool resets the history", () => {
    const state = fresh();
    toolLoopDetected(state, [readA]);
    toolLoopDetected(state, [readA]);
    toolLoopDetected(state, [{ name: "write_file", args: '{"path":"a.ts","content":"hi"}' }]);
    expect(state.history).toHaveLength(1); // only the write step
    toolLoopDetected(state, [readA]);
    expect(state.history).toHaveLength(2);
    expect(state.history[0]![0]!.name).toBe("write_file");
  });

  test("a step with different-length calls resets", () => {
    const state = fresh();
    toolLoopDetected(state, [readA]);
    toolLoopDetected(state, [readA]);
    // Now a step with 2 calls
    toolLoopDetected(state, [readA, readB]);
    expect(state.history).toHaveLength(1);
  });
});