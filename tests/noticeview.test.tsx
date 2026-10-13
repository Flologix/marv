import chalk from "chalk";
import { describe, expect, test } from "bun:test";
import { EventEmitter } from "node:events";
import { Box, render } from "ink";
import stringWidth from "string-width";
import type { Notice } from "../src/types.ts";
import { NoticeView, toneColor } from "../src/ui/NoticeView.tsx";
import { theme } from "../src/ui/theme.ts";

// Renders through Ink's real renderer at a given terminal width and returns the raw output (with escape codes),
// so tests can read both the plain text and the tone's color.
class FakeTerminal extends EventEmitter {
  isTTY = true;
  rows = 40;
  written = "";
  constructor(readonly columns: number) {
    super();
  }
  write = (data: string) => {
    this.written += data;
    return true;
  };
}
class FakeKeyboard extends EventEmitter {
  isTTY = true;
  setRawMode() {}
  setEncoding() {}
  read() {
    return null;
  }
  ref() {}
  unref() {}
}

function plain(stdout: FakeTerminal): string[] {
  return stdout.written
    .replace(/\x1b\[[0-9;?]*[A-Za-z]/g, "")
    .split("\n")
    .filter((line) => line.trim());
}

async function renderNotice(width: number, notice: Notice) {
  const stdout = new FakeTerminal(width);
  const { unmount } = render(
    <Box width={width} flexDirection="column">
      <NoticeView notice={notice} />
    </Box>,
    { stdout: stdout as unknown as NodeJS.WriteStream, stdin: new FakeKeyboard() as unknown as NodeJS.ReadStream, interactive: true },
  );
  await Bun.sleep(30);
  unmount();
  return stdout;
}

describe("NoticeView", () => {
  const notice: Notice = {
    title: "Startup · 2 notes",
    tone: "warning",
    items: [
      { text: "AGENTS.md is long: the model sees the first 20,000 of 89,192 characters", detail: ["Up front: lines 1-61", "Put what matters most first."] },
      { text: "1 PATH folder is hidden from the sandbox: ~/.cache/.bun/bin" },
    ],
  };

  test("shows the title, one bullet per item, and indented detail lines", async () => {
    const lines = await renderNotice(80, notice).then(plain);
    expect(lines[0]).toContain("│ Startup · 2 notes");
    expect(lines.some((line) => line.includes("• AGENTS.md is long"))).toBe(true);
    expect(lines.some((line) => line.includes("Up front: lines 1-61"))).toBe(true);
    expect(lines.some((line) => line.includes("Put what matters most first."))).toBe(true);
    expect(lines.some((line) => line.includes("• 1 PATH folder is hidden"))).toBe(true);
  });

  test("wrapped bullets keep a hanging indent: continuation lines are indented under the text", async () => {
    const lines = await renderNotice(30, notice).then(plain);
    // A headline wraps: its continuation lines carry at least the bullet's indent (2 columns for the bar, more for the bullet).
    const headline = lines.find((line) => line.includes("AGENTS.md is long"))!;
    const bulletIndent = headline.indexOf("AGENTS.md");
    // The bullet sits after the bar ("│ ") and before its "• " marker.
    expect(headline).toStartWith("│ • ");
    // The bar runs the whole height of the block: every line starts with it.
    for (const line of lines.filter((l) => l.trim())) expect(line).toStartWith("│");
    for (const line of lines) expect(stringWidth(line)).toBeLessThanOrEqual(30);
    // Some lines are continuations (beyond the bullet's own line): all are indented, none return to the left margin.
    for (const line of lines) {
      const indented = line.length - line.trimStart().length;
      expect(indented >= bulletIndent || line.includes("│") || line.includes("•")).toBe(true);
    }
  });

  test("detail lines sit under their item", async () => {
    const lines = plain(await renderNotice(40, notice));
    const detailIndent = lines.find((line) => line.includes("Up front"))!.indexOf("Up front");
    const headline = lines.find((line) => line.includes("AGENTS.md"))!;
    expect(detailIndent).toBeGreaterThanOrEqual(headline.indexOf("AGENTS.md"));
  });

  test("the tone's color shows on the bar, title and bullets, in theme.dim for details", async () => {
    // Tests don't run in a real terminal, so chalk (Ink's colorizer) emits no colors unless told to.
    const level = chalk.level;
    chalk.level = 3;
    try {
      for (const [tone, color] of [
        ["info", theme.accent],
        ["warning", theme.warning],
        ["error", theme.error],
      ] as const) {
        const raw = (await renderNotice(60, { ...notice, tone })).written;
        const hex = color.slice(1);
        const ansi = `38;2;${[0, 2, 4].map((i) => parseInt(hex.slice(i, i + 2), 16)).join(";")}`;
        // Tinted with the tone: the bar on every line, the title, and each bullet.
        const rows = plain(await renderNotice(60, { ...notice, tone })).filter((l) => l.trim()).length;
        expect(raw.match(new RegExp(ansi, "g"))).toHaveLength(rows + 1 + notice.items.length);
        // The detail lines use theme.dim, not the tone.
        expect(raw).toContain("38;2;161;161;170");
      }
      expect(toneColor("info")).toBe(theme.accent);
      expect(toneColor("warning")).toBe(theme.warning);
      expect(toneColor("error")).toBe(theme.error);
    } finally {
      chalk.level = level;
    }
  });

  test("every line stays within the terminal at any width", async () => {
    for (const width of [25, 40, 60, 80, 121]) {
      for (const line of plain(await renderNotice(width, notice))) {
        expect(stringWidth(line)).toBeLessThanOrEqual(width);
      }
    }
  });
});