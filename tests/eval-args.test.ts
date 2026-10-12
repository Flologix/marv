import { describe, expect, test } from "bun:test";
import { parseArgs } from "../evals/args.ts";

describe("parseArgs", () => {
  test("--report takes the URLs after the flag and ignores later flags", () => {
    const args = parseArgs(["--report", "a.jsonl", "b.jsonl", "--label", "x"]);
    expect(args.report).toEqual(["a.jsonl", "b.jsonl"]);
  });

  test("--tasks and --models split on commas", () => {
    expect(parseArgs(["--tasks", "a, b,,c"]).tasks).toEqual(["a", "b", "c"]);
    expect(parseArgs(["--models", "m1,m2"]).models).toEqual(["m1", "m2"]);
  });

  test("the numeric options are numbers", () => {
    const args = parseArgs(["--models", "a", "--repeat", "2", "--budget", "1.5", "--concurrency", "3", "--timeout", "300", "--max-steps", "10"]);
    expect(args).toMatchObject({ repeat: 2, budget: 1.5, concurrency: 3, timeout: 300, maxSteps: 10 });
  });

  test("--mcp and --mcp-tools are passed through raw (#14): the runner checks them", () => {
    expect(parseArgs(["--models", "a", "--mcp", "servers.json", "--mcp-tools", "deferred"])).toMatchObject({ mcp: "servers.json", mcpTools: "deferred" });
    expect(parseArgs(["--models", "a"])).toMatchObject({ mcp: undefined, mcpTools: undefined });
  });
});

test("a numeric option that isn't a number is an error, not a NaN", () => {
  expect(() => parseArgs(["--timeout", "abc"])).toThrow('--timeout takes a number, not "abc"');
  expect(() => parseArgs(["--repeat", "-1"])).toThrow("--repeat");
});
