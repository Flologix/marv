import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { checkCommand, detectChecker, diagnosticsNote, formatError, MAX_REPORTED, missingInterpreter, newErrors, parseTsc, type TsError } from "../src/diagnostics/tsc.ts";

describe("parseTsc", () => {
  test("located errors, errors about the configuration, and continuation lines", () => {
    const output = [
      "src/a.ts(12,5): error TS2345: Argument of type 'string' is not assignable to parameter of type 'number'.",
      "src/b.ts(3,1): error TS2322: Type '{ a: string; }' is not assignable to type 'T'.",
      "  Property 'b' is missing in type '{ a: string; }' but required in type 'T'.",
      "error TS5023: Unknown compiler option 'foo'.",
      "",
    ].join("\n");
    expect(parseTsc(output, 2)).toEqual([
      { file: "src/a.ts", line: 12, column: 5, code: "TS2345", message: "Argument of type 'string' is not assignable to parameter of type 'number'." },
      { file: "src/b.ts", line: 3, column: 1, code: "TS2322", message: "Type '{ a: string; }' is not assignable to type 'T'.\nProperty 'b' is missing in type '{ a: string; }' but required in type 'T'." },
      { code: "TS5023", message: "Unknown compiler option 'foo'." },
    ]);
  });

  test("a crash after an error can't be trusted: null", () => {
    expect(parseTsc("a.ts(1,1): error TS2304: Cannot find name 'x'.\npanic: boom\n    at foo (bar.js:1:1)\n", 2)).toBeNull();
    expect(parseTsc("Error: Debug Failure.\n    at foo (bar.js:1:1)\n", 1)).toBeNull();
  });

  test("CRLF output, a (group) path, and a located tsconfig error", () => {
    const output = "src/(group)/a.ts(2,7): error TS2322: Type 'A' is not assignable to type 'B'.\r\ntsconfig.json(1,21): error TS5023: Unknown compiler option 'foo'.\r\n";
    expect(parseTsc(output, 2)).toEqual([
      { file: "src/(group)/a.ts", line: 2, column: 7, code: "TS2322", message: "Type 'A' is not assignable to type 'B'." },
      { file: "tsconfig.json", line: 1, column: 21, code: "TS5023", message: "Unknown compiler option 'foo'." },
    ]);
  });

  test("elaboration keeps its nesting, tabs don't continue", () => {
    const output = "a.ts(1,1): error TS2322: top\n  level one\n    level two\n";
    const [e] = parseTsc(output, 2)!;
    expect(e!.message).toBe("top\nlevel one\n  level two");
    expect(formatError(e!)).toBe("a.ts:1:1 TS2322 top\n  level one\n    level two");
    expect(parseTsc("a.ts(1,1): error TS2322: top\n\tnot a continuation\n", 2)).toBeNull();
  });

  test("exit code 0 is no errors, whatever was printed", () => {
    expect(parseTsc("", 0)).toEqual([]);
  });

  test("a failure that printed no errors can't be read (a crash, or not tsc at all): null", () => {
    expect(parseTsc("Segmentation fault\n", 139)).toBeNull();
    expect(parseTsc("", 1)).toBeNull();
    expect(parseTsc("", null)).toBeNull();
  });

  test("Node's runtime warnings (stderr is merged into the output) are skipped, not a failed run", () => {
    const out = "src/a.ts(1,1): error TS2304: Cannot find name 'x'.\n(node:123) ExperimentalWarning: something\n(Use `node --trace-warnings ...` to show where the warning was created)\n";
    expect(parseTsc(out, 2)).toEqual([{ file: "src/a.ts", line: 1, column: 1, code: "TS2304", message: "Cannot find name 'x'." }]);
  });
});

describe("newErrors", () => {
  const e = (file: string, line: number, code: string, message: string): TsError => ({ file, line, column: 1, code, message });

  test("matched by file, code and message: an old error that moved isn't new", () => {
    const before = [e("a.ts", 3, "TS2304", "Cannot find name 'x'.")];
    const now = [e("a.ts", 9, "TS2304", "Cannot find name 'x'."), e("b.ts", 1, "TS2304", "Cannot find name 'y'.")];
    expect(newErrors(before, now)).toEqual([now[1]!]);
  });

  test("counts, not a set: a second copy of an old error is new", () => {
    const before = [e("a.ts", 3, "TS2304", "Cannot find name 'x'.")];
    const now = [e("a.ts", 3, "TS2304", "Cannot find name 'x'."), e("a.ts", 7, "TS2304", "Cannot find name 'x'.")];
    expect(newErrors(before, now)).toEqual([now[1]!]);
  });

  test("a fixed error isn't reported", () => {
    expect(newErrors([e("a.ts", 1, "TS2304", "x")], [])).toEqual([]);
  });
});

describe("the note", () => {
  test("one line per error, as file:line:column code message, continuation lines indented", () => {
    expect(formatError({ file: "src/a.ts", line: 4, column: 2, code: "TS2322", message: "Type 'A' is not assignable.\nProperty 'b' is missing." })).toBe(
      "src/a.ts:4:2 TS2322 Type 'A' is not assignable.\n  Property 'b' is missing.",
    );
    expect(formatError({ code: "TS5023", message: "Unknown compiler option 'foo'." })).toBe("TS5023 Unknown compiler option 'foo'.");
  });

  test("starts with Marv: and says how many; shows at most MAX_REPORTED and counts the rest", () => {
    const one = diagnosticsNote([{ file: "a.ts", line: 1, column: 1, code: "TS2304", message: "Cannot find name 'x'." }]);
    expect(one).toBe("Marv: the typecheck (tsc) after your changes found 1 new error:\na.ts:1:1 TS2304 Cannot find name 'x'.");
    const many = Array.from({ length: MAX_REPORTED + 3 }, (_, i): TsError => ({ file: "a.ts", line: i + 1, column: 1, code: "TS2304", message: `Cannot find name 'v${i}'.` }));
    const note = diagnosticsNote(many);
    expect(note.split("\n")[0]).toBe(`Marv: the typecheck (tsc) after your changes found ${MAX_REPORTED + 3} new errors:`);
    expect(note.split("\n")).toHaveLength(1 + MAX_REPORTED + 1);
    expect(note.split("\n").at(-1)).toBe("and 3 more.");
  });
});

describe("missingInterpreter", () => {
  test("env's 'No such file or directory' for the shebang interpreter", () => {
    expect(missingInterpreter("env: ‘node’: No such file or directory\n", 127)).toBe("node");
    expect(missingInterpreter("env: 'node': No such file or directory\n", 127)).toBe("node"); // straight quotes too
    expect(missingInterpreter("env: ‘python3’: No such file or directory", 127)).toBe("python3");
  });

  test("a direct exec failing with ENOENT", () => {
    expect(missingInterpreter("exec: No such file or directory", 127)).toBeUndefined();
  });

  test("not other failures, other exit codes, or success", () => {
    expect(missingInterpreter("error TS2304: Cannot find name 'x'.\n", 2)).toBeUndefined();
    expect(missingInterpreter("env: ‘node’: No such file or directory\n", 2)).toBeUndefined();
    expect(missingInterpreter("env: ‘node’: No such file or directory\n", 0)).toBeUndefined();
    expect(missingInterpreter("", 127)).toBeUndefined();
    expect(missingInterpreter("env: ‘node’: permission denied\n", 126)).toBeUndefined();
  });
});

describe("detectChecker", () => {
  let root: string;
  beforeEach(() => (root = mkdtempSync(join(tmpdir(), "marv-detect-"))));
  afterEach(() => rmSync(root, { recursive: true, force: true }));
  const bin = (name: string) => {
    mkdirSync(join(root, "node_modules", ".bin"), { recursive: true });
    writeFileSync(join(root, "node_modules", ".bin", name), "");
  };

  test("a tsconfig.json and the project's own tsc", () => {
    writeFileSync(join(root, "tsconfig.json"), "{}");
    bin("tsc");
    expect(detectChecker(root)).toBe("node_modules/.bin/tsc");
  });

  test("tsgo (TypeScript's native preview) first", () => {
    writeFileSync(join(root, "tsconfig.json"), "{}");
    bin("tsc");
    bin("tsgo");
    expect(detectChecker(root)).toBe("node_modules/.bin/tsgo");
  });

  test("nothing without a tsconfig.json, or without a compiler in the project", () => {
    bin("tsc");
    expect(detectChecker(root)).toBeNull();
    rmSync(join(root, "node_modules"), { recursive: true });
    writeFileSync(join(root, "tsconfig.json"), "{}");
    expect(detectChecker(root)).toBeNull();
  });

  test("the command: no output files, plain lines, build info into the sandbox's throwaway /tmp", () => {
    expect(checkCommand("node_modules/.bin/tsc")).toBe("node_modules/.bin/tsc --noEmit --pretty false --tsBuildInfoFile /tmp/marv.tsbuildinfo -p tsconfig.json");
  });
});
