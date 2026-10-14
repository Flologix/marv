import { Box, Text, useInput, useWindowSize } from "ink";
import type { ApprovalRequest, Decision, Door } from "../tools/types.ts";
import { printable } from "../printable.ts";
import { DiffView } from "./DiffView.tsx";
import { ScrollView } from "./ScrollView.tsx";
import { Select } from "./Select.tsx";
import { theme } from "./theme.ts";

const MAX_DIFF_LINES = 24;
/** Each of several files' diffs (/undo), so a few files fit on the screen at once. */
const MAX_FILE_DIFF_LINES = 8;
/**
 * The rows the rest of the screen keeps when several files' diffs are shown: the panel's border, title, notes and
 * choices, the status bar, a little transcript. The file list gets what's left (at least MIN_FILES_ROWS) and scrolls,
 * so a big /undo never pushes Yes/No off the screen.
 */
const RESERVED_ROWS = 16;
const MIN_FILES_ROWS = 4;

/** The rows a file of /undo's preview takes: the gap above, its title, its diff (capped) and the "more" line. */
function fileRows(file: NonNullable<ApprovalRequest["preview"]["files"]>[number]): number {
  const lines = file.diff?.lines.length ?? 0;
  return 2 + Math.min(lines, MAX_FILE_DIFF_LINES) + (lines > MAX_FILE_DIFF_LINES || (file.diff?.more ?? 0) > 0 ? 1 : 0);
}

/**
 * One warning line per door out of the sandbox, in the warning color: what saying yes lets out. A door's own
 * detail (the repository a github write is visible on) comes in its preview's warning, below this line.
 */
export const DOOR_WARNINGS: Record<Door, string> = {
  host: "Runs OUTSIDE the sandbox, as you, with your files, network and credentials.",
  github: "Visible to other people on GitHub.",
  process: "Keeps running after this turn, until stopped or Marv exits.",
};

/**
 * Asks before a tool changes something: what it will do (a diff, or the
 * command), then yes / yes for the rest of the session / no. Shown in place
 * of the prompt while the agent waits for the answer.
 */
export function Approval({
  request,
  onDecide,
  onCancel,
  waiting = 0,
  escapeCancels = true,
  windowRows,
}: {
  request: ApprovalRequest;
  onDecide: (decision: Decision) => void;
  /** Esc: no to this and to everything waiting behind it. */
  onCancel?: () => void;
  /** Other requests queued behind this one (parallel subagents). */
  waiting?: number;
  /** Off while a subagent's view is open: Esc then only closes the view, it doesn't stop the run. */
  escapeCancels?: boolean;
  /** The window's height (default: the terminal's), which limits a file list; tests pass one. */
  windowRows?: number;
}) {
  const { preview, scope } = request;
  const { rows } = useWindowSize();
  const filesRows = Math.max(MIN_FILES_ROWS, (windowRows ?? rows) - RESERVED_ROWS);
  const files = preview.files ?? [];
  const scrolls = files.reduce((sum, file) => sum + fileRows(file), 0) > filesRows;

  useInput((_input, key) => {
    if (key.escape && escapeCancels) (onCancel ?? (() => onDecide("no")))();
  });

  const diff = preview.diff ?? [];
  const warned = Boolean(preview.warning || request.door);

  return (
    <Box flexDirection="column" borderStyle="round" borderColor={warned ? theme.warning : theme.accent} paddingX={1}>
      {request.agent && <Text color={theme.dim}>[{printable(request.agent)}]</Text>}
      <Text bold>{printable(preview.title)}</Text>

      {(diff.length > 0 || preview.command || preview.text) && (
        <Box flexDirection="column" marginTop={1}>
          {preview.command && (
            <Text color={theme.code}>
              <Text color={theme.dim}>$ </Text>
              {printable(preview.command)}
            </Text>
          )}
          {preview.text && <Text>{printable(preview.text)}</Text>}
          {diff.length > 0 && <DiffView lines={diff} max={MAX_DIFF_LINES} />}
        </Box>
      )}
      {files.length > 0 && (
        // At most filesRows tall, scrolled with PgUp/PgDn or the wheel (the App's transcript doesn't scroll meanwhile).
        <Box flexDirection="column" maxHeight={filesRows} flexShrink={0}>
          <ScrollView fromTop>
            {files.map((file, i) => (
              <Box key={i} flexDirection="column" marginTop={1}>
                <Text>
                  {printable(file.title)}
                  {file.note && <Text color={theme.dim}> · {printable(file.note)}</Text>}
                </Text>
                {file.diff && file.diff.lines.length > 0 && <DiffView lines={file.diff.lines} more={file.diff.more} max={MAX_FILE_DIFF_LINES} indent={2} />}
              </Box>
            ))}
          </ScrollView>
        </Box>
      )}
      {scrolls && <Text color={theme.dim}>PgUp/PgDn or the wheel scroll the files</Text>}

      {preview.note && <Text color={theme.dim}>{printable(preview.note)}</Text>}
      {request.door && <Text color={theme.warning}>⚠ {DOOR_WARNINGS[request.door]}</Text>}
      {preview.warning && <Text color={theme.warning}>⚠ {printable(preview.warning)}</Text>}
      {waiting > 0 && <Text color={theme.dim}>{waiting} more waiting</Text>}

      <Box marginTop={1} flexDirection="column">
        <Text>Do you want to proceed?</Text>
        <Select
          items={[
            { value: "yes" as const, label: "Yes" },
            ...(request.once ? [] : [{ value: "always" as const, label: `Yes, and don't ask again for ${scope.description} this session` }]),
            { value: "no" as const, label: request.once ? "No" : "No, and tell Marv what to do instead", hint: "esc" },
          ]}
          onSelect={onDecide}
        />
      </Box>
    </Box>
  );
}
