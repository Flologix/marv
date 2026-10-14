import { describe, expect, test } from "bun:test";
import { render } from "ink-testing-library";
import type { PlanItem } from "../src/tools/types.ts";
import { PLAN_ROWS, PlanView, planRows } from "../src/ui/PlanView.tsx";

const items = (statuses: PlanItem["status"][]): PlanItem[] => statuses.map((status, i) => ({ text: `step ${i + 1}`, status }));

describe("PlanView (#15)", () => {
  test("a short plan is shown whole, the current item marked", () => {
    const { lastFrame } = render(<PlanView items={items(["done", "done", "in_progress", "pending", "pending"])} width={60} />);
    const lines = lastFrame()!.split("\n");
    expect(lines[0]).toStartWith(" Plan 2/5 ─");
    expect(lines.slice(1)).toEqual(["  ✓ step 1", "  ✓ step 2", "  ▸ step 3", "  ○ step 4", "  ○ step 5"]);
  });

  test("12 items, 4 done: at most 6 rows, the current item, the next ones, then +N more", () => {
    const list = items(["done", "done", "done", "done", "in_progress", ...Array<PlanItem["status"]>(7).fill("pending")]);
    const { lastFrame } = render(<PlanView items={list} width={60} />);
    const lines = lastFrame()!.split("\n");
    expect(lines).toHaveLength(PLAN_ROWS);
    expect(lines[0]).toStartWith(" Plan 4/12 ");
    expect(lines.slice(1)).toEqual(["  ▸ step 5", "  ○ step 6", "  ○ step 7", "  ○ step 8", "  +4 more"]);
  });

  test("planRows: a long plan without a current item starts at the first one not done", () => {
    expect(planRows(items(["done", "done", "pending", "pending", "pending", "pending"]))).toEqual({ shown: items(["done", "done", "pending", "pending", "pending", "pending"]).slice(2), more: 0 });
  });

  test("nothing when every item is done, or there's no plan", () => {
    expect(render(<PlanView items={items(["done", "done"])} width={60} />).lastFrame()).toBe("");
    expect(render(<PlanView items={null} width={60} />).lastFrame()).toBe("");
  });

  test("item text is drawn through printable(): no escape sequences reach the terminal", () => {
    const { lastFrame } = render(<PlanView items={[{ text: "evil \x1b]52;c;aGk=\x07 text", status: "in_progress" }]} width={60} />);
    expect(lastFrame()).not.toContain("\x1b]52");
    expect(lastFrame()).toContain("▸ evil");
  });
});
