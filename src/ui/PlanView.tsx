// The plan tool's checklist (#15), pinned between the transcript and the prompt while it has something left to do:
// the transcript scrolls away within a few tool calls, and "where is Marv in this task" is asked while it works.
import { Box, Text } from "ink";
import { memo } from "react";
import { printable } from "../printable.ts";
import { planOpen, planProgress } from "../tools/plan.ts";
import type { PlanItem } from "../tools/types.ts";
import { theme } from "./theme.ts";

/** At most this many rows, the header included: the transcript needs the rest. */
export const PLAN_ROWS = 6;
/** Below this many rows the window has no room for it. */
export const PLAN_MIN_WINDOW = 20;

const MARK: Record<PlanItem["status"], string> = { done: "✓", in_progress: "▸", pending: "○" };

/**
 * The items to draw under the header, and how many more there are. A plan that fits is shown whole; a longer one
 * drops its done items (the header counts them) and shows the current item and the ones after it, then `+N more`.
 */
export function planRows(items: readonly PlanItem[], max = PLAN_ROWS - 1): { shown: PlanItem[]; more: number } {
  if (items.length <= max) return { shown: [...items], more: 0 };
  const current = items.findIndex((i) => i.status === "in_progress");
  const start = current >= 0 ? current : Math.max(0, items.findIndex((i) => i.status !== "done"));
  const left = items.slice(start).filter((i, n) => n === 0 || i.status !== "done");
  if (left.length <= max) return { shown: left, more: 0 };
  return { shown: left.slice(0, max - 1), more: left.length - (max - 1) };
}

interface Props {
  items: readonly PlanItem[] | null;
  width: number;
}

function PlanViewImpl({ items, width }: Props) {
  if (!planOpen(items as PlanItem[] | null)) return null;
  const list = items as PlanItem[];
  const { shown, more } = planRows(list);
  const title = ` Plan ${planProgress(list).replace(" done", "")} `;
  return (
    <Box flexDirection="column" flexShrink={0} width={width}>
      <Text color={theme.dim} wrap="truncate">
        <Text color={theme.accent} bold>
          {title}
        </Text>
        {"─".repeat(Math.max(0, width - title.length - 1))}
      </Text>
      {shown.map((item, n) => (
        <Text key={n} wrap="truncate" color={item.status === "in_progress" ? theme.accent : theme.dim} bold={item.status === "in_progress"}>
          {`  ${MARK[item.status]} ${printable(item.text)}`}
        </Text>
      ))}
      {more > 0 && <Text color={theme.dim}>{`  +${more} more`}</Text>}
    </Box>
  );
}

/** Re-renders only when the plan (a new array from each call) or the width changes. */
export const PlanView = memo(PlanViewImpl);
