// Compaction: when the conversation gets close to the model's context
// window, ask the model to summarize it, then continue from the summary.
//
// The summary request reuses the exact prefix of a normal request (system
// prompt, tools, history) and only appends one message, so it hits the
// prompt cache and is cheap. Afterwards the model's history is just the
// summary: the cache starts over once, deliberately. The transcript the user
// sees is not touched.
import type { ChatTurn, Provider, ToolSpec, Usage } from "./provider/types.ts";

/** Compact before the next message once this much of the context window is used. */
export const COMPACT_AT = 0.85;

const INSTRUCTIONS = `Summarize our conversation so far, so we can continue it from your summary alone (the earlier messages will be removed).

Include, specifically:
- What the user wants overall, and what they asked for most recently.
- Decisions made, and why.
- Files read or changed (paths), and what changed in them.
- Commands run and their important results (test results, errors).
- What's done, what's left, and the next step.
- Anything the user said to do or avoid.

Keep names, paths, numbers and error messages exact. Write it as notes for yourself. Don't call any tools, and don't do anything else.`;

interface Options {
  provider: Provider;
  history: ChatTurn[];
  system: string;
  tools: ToolSpec[];
  signal: AbortSignal;
  /** What the user wants the summary to keep, from /compact <focus>. */
  focus?: string;
  /** The summary request is a real request: count its tokens and cost too. */
  onUsage?: (usage: Usage) => void;
}

export async function summarize({ provider, history, system, tools, signal, focus, onUsage }: Options): Promise<{ summary: string } | { error: string }> {
  const request: ChatTurn[] = [...history, { role: "user", text: focus ? `${INSTRUCTIONS}\n\nFocus especially on: ${focus}` : INSTRUCTIONS }];
  let text = "";
  let toolCall = false;
  let reason: string | undefined;
  for await (const event of provider.stream(request, { system, tools, signal })) {
    if (event.type === "text_delta") text += event.text;
    else if (event.type === "retry") {
      // The connection dropped and the provider is asking again from the start: what the failed attempt sent
      // isn't part of the summary that will replace the conversation.
      text = "";
      toolCall = false;
      reason = undefined;
    } else if (event.type === "tool_call") toolCall = true;
    else if (event.type === "done") reason = event.reason;
    else if (event.type === "usage") onUsage?.(event.usage);
    else if (event.type === "error") return { error: event.message };
  }
  if (signal.aborted) return { error: "Stopped." };
  // The tools are offered (so the cache prefix matches), and a model may reach for one ("Let me re-read
  // src/app.tsx first.") instead of summarizing; or the summary hits the output limit. Either way the text isn't
  // a summary, and replacing the history with it would lose the conversation for good.
  if (toolCall) return { error: "The model tried to use a tool instead of summarizing." };
  if (reason === "length") return { error: "The summary was cut off by the model's output limit." };
  return text.trim() ? { summary: text.trim() } : { error: "The model returned an empty summary." };
}

/** The model's history after compacting: the summary, acknowledged. */
export function compactedHistory(summary: string): ChatTurn[] {
  return [
    { role: "user", text: `This conversation was compacted to save space. Here is a summary of everything before this point:\n\n${summary}` },
    { role: "assistant", text: "Got it. I have the context from the summary and will continue from here." },
  ];
}

/**
 * Appended after the compacted history when the compaction happened mid-turn (between two steps), where the next
 * request would otherwise end with the compacted history's own assistant acknowledgement and the model would have
 * no instruction to go on: this user turn tells it to. Not used when compacting before a user message — there the
 * user's message itself follows the history.
 */
export const MID_TURN_NOTE =
  "(Automatic note from Marv, the program running you) Your conversation was just compacted to save space in the middle of this task (everything before this point is summarized above). Continue where you left off.";
