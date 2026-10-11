// A change, drawn: numbered lines, green additions, red removals. Used by the approval prompt (before a change)
// and the transcript (after one), so both look the same.
import { Box, Text } from "ink";
import { printable } from "../printable.ts";
import { fileLines, previewLines } from "../tools/diff.ts";
import type { DiffLine } from "../tools/types.ts";
import { theme } from "./theme.ts";

const MARK: Record<DiffLine["kind"], string> = { add: "+ ", del: "- ", ctx: "  ", gap: "  " };
const COLOR: Record<DiffLine["kind"], string> = { add: theme.diffAdd, del: theme.diffDel, ctx: theme.dim, gap: theme.dim };

export function DiffView({
  lines,
  max,
  more = 0,
  hint,
  indent = 0,
}: {
  lines: DiffLine[];
  /** Show at most this many lines (the rest are counted). */
  max?: number;
  /** Lines left out before they got here (a result keeps at most MAX_SHOWN_DIFF). */
  more?: number;
  /** How to see the rest, e.g. "ctrl+o". Without one, `more` lines are said to be "not kept": nothing can show them. */
  hint?: string;
  indent?: number;
}) {
  const shown = max === undefined ? lines : previewLines(lines, max);
  const hiddenHere = fileLines(lines) - fileLines(shown);
  const hidden = hiddenHere + more;
  // Removed lines show their old number, the others their new one; one column wide enough for all of them, shown
  // or not, so the text doesn't shift when ctrl+o shows the rest.
  const number = (l: DiffLine) => (l.kind === "del" ? l.oldLine : l.newLine);
  // A loop, not Math.max(...lines): a new file's preview holds all its lines, too many to spread as arguments.
  let width = 1;
  for (const l of lines) width = Math.max(width, String(number(l) ?? "").length);
  const pad = " ".repeat(width);
  return (
    <Box flexDirection="column" paddingLeft={indent}>
      {shown.map((line, i) => (
        <Text key={i} color={COLOR[line.kind]} wrap="truncate-end">
          {line.kind === "gap" ? `${pad} ${MARK.gap}…` : `${String(number(line) ?? "").padStart(width)} ${MARK[line.kind]}${printable(String(line.text ?? ""))}`}
        </Text>
      ))}
      {hidden > 0 && (
        <Text color={theme.dim}>
          {`${pad} ${MARK.gap}… ${hidden} more line${hidden === 1 ? "" : "s"}${hint ? ` (${hint})` : hiddenHere === 0 ? " not kept" : ""}`}
        </Text>
      )}
    </Box>
  );
}
