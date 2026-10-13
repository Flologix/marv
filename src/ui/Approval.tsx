import { Box, Text, useInput } from "ink";
import type { ApprovalRequest, Decision, Door } from "../tools/types.ts";
import { printable } from "../printable.ts";
import { DiffView } from "./DiffView.tsx";
import { Select } from "./Select.tsx";
import { theme } from "./theme.ts";

const MAX_DIFF_LINES = 24;

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
}: {
  request: ApprovalRequest;
  onDecide: (decision: Decision) => void;
  /** Esc: no to this and to everything waiting behind it. */
  onCancel?: () => void;
  /** Other requests queued behind this one (parallel subagents). */
  waiting?: number;
  /** Off while a subagent's view is open: Esc then only closes the view, it doesn't stop the run. */
  escapeCancels?: boolean;
}) {
  const { preview, scope } = request;

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
