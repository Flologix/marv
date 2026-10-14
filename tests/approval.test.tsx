import { afterEach, describe, expect, mock, test } from "bun:test";
import { cleanup, render } from "ink-testing-library";
import type { ApprovalRequest } from "../src/tools/types.ts";
import { Approval } from "../src/ui/Approval.tsx";

const ENTER = "\r";
const DOWN = "\x1b[B";
const ESC = "\x1b";
const tick = (ms = 30) => Bun.sleep(ms);

afterEach(cleanup);

const EDIT: ApprovalRequest = {
  tool: "edit_file",
  label: "src/greet.ts",
  scope: { key: "files", description: "file changes" },
  preview: {
    title: "Edit src/greet.ts",
    diff: [
      { kind: "ctx", text: "export function greet(name: string) {" },
      { kind: "del", text: "  return `Hello, ${name}!`;" },
      { kind: "add", text: "  return `Hi, ${name}!`;" },
      { kind: "ctx", text: "}" },
    ],
  },
};

const COMMAND: ApprovalRequest = {
  tool: "bash",
  label: "bun test",
  scope: { key: "bash:bun test", description: "this exact command" },
  preview: { title: "Run a command", command: "bun test", note: "sandboxed · no network" },
};

describe("Approval", () => {
  test("a new key gives the next request a fresh prompt (cursor back on Yes)", async () => {
    // The App renders each queued request with key={id}; without it React keeps Select's cursor.
    const { stdin, lastFrame, rerender } = render(<Approval key={1} request={COMMAND} onDecide={() => {}} />);
    stdin.write(DOWN);
    await tick();
    expect(lastFrame()).toMatch(/❯ Yes, and don't ask again/);
    rerender(<Approval key={2} request={EDIT} onDecide={() => {}} />);
    await tick();
    expect(lastFrame()).toMatch(/❯ Yes\s+│/);
  });

  test("once: Yes and No only, no 'don't ask again' (/mcp add: each install is its own decision)", async () => {
    const onDecide = mock((_d: string) => {});
    const { stdin, lastFrame } = render(<Approval request={{ ...COMMAND, once: true }} onDecide={onDecide} />);
    expect(lastFrame()).not.toContain("don't ask again");
    stdin.write(DOWN);
    await tick();
    expect(lastFrame()).toMatch(/❯ No/);
    stdin.write(ENTER);
    await tick();
    expect(onDecide).toHaveBeenCalledWith("no");
  });

  test("a door shows its warning line: one per door (#52)", () => {
    const host = render(<Approval request={{ ...COMMAND, door: "host", once: true }} onDecide={() => {}} />).lastFrame()!;
    expect(host).toContain("Runs OUTSIDE the sandbox, as you, with your files, network and credentials.");
    expect(host).not.toContain("don't ask again");
    expect(render(<Approval request={{ ...COMMAND, door: "github" }} onDecide={() => {}} />).lastFrame()).toContain("Visible to other people on GitHub");
    expect(render(<Approval request={{ ...COMMAND, door: "process" }} onDecide={() => {}} />).lastFrame()).toContain("Keeps running after this turn, until stopped or Marv exits");
    // An ordinary call has no door line.
    expect(render(<Approval request={COMMAND} onDecide={() => {}} />).lastFrame()).not.toContain("OUTSIDE");
  });

  test("says which subagent is asking, and how many more are waiting", () => {
    const frame = render(<Approval request={{ ...EDIT, agent: "implementer · Task 2" }} waiting={2} onDecide={() => {}} />).lastFrame()!;
    expect(frame).toContain("[implementer · Task 2]");
    expect(frame).toContain("2 more waiting");
  });

  test("Esc cancels everything when the App passes onCancel", async () => {
    const onDecide = mock();
    const onCancel = mock();
    const { stdin } = render(<Approval request={EDIT} onDecide={onDecide} onCancel={onCancel} />);
    stdin.write(ESC);
    await tick();
    expect(onCancel).toHaveBeenCalled();
    expect(onDecide).not.toHaveBeenCalled();
  });

  test("shows the change as a diff and the three choices", () => {
    const { lastFrame } = render(<Approval request={EDIT} onDecide={() => {}} />);
    const frame = lastFrame()!;
    expect(frame).toContain("Edit src/greet.ts");
    expect(frame).toContain("-   return `Hello, ${name}!`;");
    expect(frame).toContain("+   return `Hi, ${name}!`;");
    expect(frame).toContain("Yes");
    expect(frame).toContain("don't ask again for file changes this session");
    expect(frame).toContain("No");
  });

  test("shows a command with its sandbox note", () => {
    const frame = render(<Approval request={COMMAND} onDecide={() => {}} />).lastFrame()!;
    expect(frame).toContain("$ bun test");
    expect(frame).toContain("sandboxed · no network");
  });

  test("shows plain text (like a memory) without the $ prompt", () => {
    const request = { ...COMMAND, preview: { title: "Remember (personal, all projects)", text: "Prefers short answers." } };
    const frame = render(<Approval request={request} onDecide={() => {}} />).lastFrame()!;
    expect(frame).toContain("Prefers short answers.");
    expect(frame).not.toContain("$ Prefers");
  });

  test("shows a warning when a command isn't sandboxed", () => {
    const request = { ...COMMAND, preview: { ...COMMAND.preview, note: undefined, warning: "runs WITHOUT a sandbox" } };
    expect(render(<Approval request={request} onDecide={() => {}} />).lastFrame()).toContain("runs WITHOUT a sandbox");
  });

  test("a long diff is cut, saying how much more there is", () => {
    const diff = Array.from({ length: 60 }, (_, i) => ({ kind: "add" as const, text: `line ${i}` }));
    const frame = render(<Approval request={{ ...EDIT, preview: { title: "Create big.ts", diff } }} onDecide={() => {}} />).lastFrame()!;
    expect(frame).toContain("line 0");
    expect(frame).not.toContain("line 59");
    expect(frame).toMatch(/… \d+ more lines/);
  });

  test("a huge new file's preview draws its first lines without a stack overflow", () => {
    const diff = Array.from({ length: 1_000_000 }, (_, i) => ({ kind: "add" as const, text: `l${i + 1}`, newLine: i + 1 }));
    const frame = render(<Approval request={{ ...EDIT, preview: { title: "Create huge.ts", diff } }} onDecide={() => {}} />).lastFrame()!;
    expect(frame).toContain("│       1 + l1");
    expect(frame).toContain("… 999976 more lines");
  });

  test("a numbered diff counts exactly what it leaves out", () => {
    const diff = Array.from({ length: 100 }, (_, i) => ({ kind: "add" as const, text: `line ${i + 1}`, newLine: i + 1 }));
    const frame = render(<Approval request={{ ...EDIT, preview: { title: "Create big.ts", diff } }} onDecide={() => {}} />).lastFrame()!;
    expect(frame).toContain("│   1 + line 1"); // a column wide enough for line 100, though it isn't shown
    expect(frame).toContain("│  24 + line 24");
    expect(frame).toContain("… 76 more lines");
  });

  test.each([
    [[ENTER], "yes"],
    [[DOWN, ENTER], "always"],
    [[DOWN, DOWN, ENTER], "no"],
  ])("keys %j decide %s", async (keys, decision) => {
    const onDecide = mock();
    const { stdin } = render(<Approval request={EDIT} onDecide={onDecide} />);
    for (const key of keys) {
      stdin.write(key);
      await tick();
    }
    expect(onDecide).toHaveBeenCalledWith(decision);
  });

  test("Esc means no", async () => {
    const onDecide = mock();
    const { stdin } = render(<Approval request={EDIT} onDecide={onDecide} />);
    stdin.write(ESC);
    await tick(100); // a lone ESC is held briefly to tell it apart from escape sequences
    expect(onDecide).toHaveBeenCalledWith("no");
  });
});

describe("Approval: an undo of many files (Greptile on #71)", () => {
  const PAGE_DOWN = "\x1b[6~";
  const diff = (n: number) => ({
    lines: Array.from({ length: 8 }, (_, i) => ({ kind: "add" as const, text: `file ${n} line ${i + 1}`, newLine: i + 1 })),
    more: 0,
  });
  const UNDO: ApprovalRequest = {
    tool: "undo",
    label: "turn 3",
    once: true,
    scope: { key: "undo", description: "undo" },
    preview: {
      title: "Undo turn 3",
      files: Array.from({ length: 12 }, (_, i) => ({ title: `restore src/file${i + 1}.ts`, diff: diff(i + 1) })),
      note: "12 files",
    },
  };

  test("the file list fits the window and scrolls; Yes and No stay visible", async () => {
    const { stdin, lastFrame } = render(<Approval request={UNDO} onDecide={() => {}} windowRows={30} />);
    await tick();
    const first = lastFrame()!;
    expect(first.split("\n").length).toBeLessThanOrEqual(30);
    expect(first).toContain("restore src/file1.ts");
    expect(first).not.toContain("restore src/file12.ts");
    expect(first).toMatch(/❯ Yes/);
    expect(first).toContain("No");
    for (let i = 0; i < 20; i++) {
      stdin.write(PAGE_DOWN);
      await tick(5);
    }
    await tick();
    const last = lastFrame()!;
    expect(last.split("\n").length).toBeLessThanOrEqual(30);
    expect(last).toContain("restore src/file12.ts");
    expect(last).toContain("file 12 line 8");
    expect(last).toMatch(/❯ Yes/);
    expect(last).toContain("No");
  });
});
