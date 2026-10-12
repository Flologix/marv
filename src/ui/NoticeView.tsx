import { memo, type ReactNode } from "react";
import { Box, Text } from "ink";
import type { Notice } from "../types.ts";
import { printable } from "../printable.ts";
import { theme } from "./theme.ts";

/** A notice's tone maps to a color: info uses the accent, warning and error their own. */
export function toneColor(tone: Notice["tone"]): string {
  return tone === "info" ? theme.accent : tone === "warning" ? theme.warning : theme.error;
}

/** A fixed-width column so bullets and their wrapped text keep a hanging indent (the Gutter note in CLAUDE.md). */
function Marker({ width, color, children }: { width: number; color?: string; children?: ReactNode }) {
  return (
    <Box width={width} flexShrink={0}>
      <Text color={color}>{children}</Text>
    </Box>
  );
}

/**
 * A distinct, colored block for Marv's own notices (startup problems, MCP
 * servers that failed): a left bar, a bold title, and one bullet per item
 * with detail lines indented under it. Transcript only, never sent to the model.
 */
export const NoticeView = memo(function NoticeView({ notice }: { notice: Notice }) {
  const color = toneColor(notice.tone);
  return (
    // A left border, not a one-line marker: the bar runs the whole height of the block, however long it wraps.
    <Box marginBottom={1} borderStyle="single" borderTop={false} borderRight={false} borderBottom={false} borderColor={color} paddingLeft={1}>
      <Box flexDirection="column" flexShrink={1}>
        <Text bold color={color}>
          {printable(notice.title)}
        </Text>
        {notice.items.map((item, i) => (
          <Box key={i} flexDirection="column">
            <Box>
              <Marker width={2} color={color}>
                •
              </Marker>
              <Box flexShrink={1}>
                <Text>{printable(item.text)}</Text>
              </Box>
            </Box>
            {item.detail?.map((line, j) => (
              <Box key={j}>
                <Marker width={2} />
                <Box flexShrink={1}>
                  <Text color={theme.dim}>{printable(line)}</Text>
                </Box>
              </Box>
            ))}
          </Box>
        ))}
      </Box>
    </Box>
  );
});