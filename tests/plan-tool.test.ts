import { describe, expect, test } from "bun:test";
import { runTool } from "../src/tools/index.ts";
import type { PlanItem, ToolContext } from "../src/tools/types.ts";

const call = (items: unknown) => ({ id: "c1", name: "plan", arguments: JSON.stringify({ items }) });
const host = () => {
  const seen: PlanItem[][] = [];
  return { seen, plan: { set: (items: PlanItem[]) => void seen.push(items) } };
};
const ctx = (extra: Partial<ToolContext> = {}): ToolContext => ({ root: "/tmp", ...extra });

describe("plan tool", () => {
  test("saves the list and says where it stands", async () => {
    const h = host();
    const items = [
      { text: "Read", status: "done" },
      { text: "Write tests", status: "in_progress" },
      { text: "Docs", status: "pending" },
    ];
    const r = await runTool(call(items), ctx({ plan: h.plan }));
    expect(r.isError).toBeFalsy();
    expect(r.output).toBe("Plan saved: 1 of 3 done; now: Write tests.");
    expect(r.summary).toBe("1/3 done");
    expect(r.label).toBe("Plan");
    expect(r.plan).toEqual(items as PlanItem[]);
    expect(h.seen).toEqual([items as PlanItem[]]);
  });

  test("without an item in progress it says so", async () => {
    const h = host();
    const r = await runTool(call([{ text: "A", status: "done" }, { text: "B", status: "pending" }]), ctx({ plan: h.plan }));
    expect(r.output).toBe("Plan saved: 1 of 2 done.");
  });

  test("an empty list clears it", async () => {
    const h = host();
    const r = await runTool(call([]), ctx({ plan: h.plan }));
    expect(r.output).toBe("Plan cleared.");
    expect(r.summary).toBe("cleared");
    expect(h.seen).toEqual([[]]);
  });

  test("refuses two in_progress, 21 items, empty or 201-char text, as errors the model reads", async () => {
    const h = host();
    const bad = [
      [{ text: "A", status: "in_progress" }, { text: "B", status: "in_progress" }],
      Array.from({ length: 21 }, (_, i) => ({ text: `step ${i}`, status: "pending" })),
      [{ text: "", status: "pending" }],
      [{ text: "x".repeat(201), status: "pending" }],
      [{ text: "A", status: "started" }],
    ];
    for (const items of bad) {
      const r = await runTool(call(items), ctx({ plan: h.plan }));
      expect(r.isError).toBe(true);
      expect(r.output).toStartWith("Invalid input for plan:");
    }
    const two = await runTool(call(bad[0]), ctx({ plan: h.plan }));
    expect(two.output).toContain("At most one item can be in_progress");
    expect(h.seen).toEqual([]);
  });

  test("without a host it's a ToolError", async () => {
    const r = await runTool(call([{ text: "A", status: "pending" }]), ctx());
    expect(r.isError).toBe(true);
    expect(r.output).toBe("The plan tool isn't available here.");
  });

  test("never asks, even with an approver that says no and yolo off", async () => {
    const h = host();
    let asked = 0;
    const r = await runTool(
      call([{ text: "A", status: "in_progress" }]),
      ctx({ plan: h.plan, yolo: false, approve: async () => (asked++, "no") }),
    );
    expect(asked).toBe(0);
    expect(r.isError).toBeFalsy();
    expect(r.approval).toBeUndefined();
    expect(h.seen).toHaveLength(1);
  });
});
