import type { ChatTurn } from "../src/provider/types.ts";

/**
 * What's wrong with a history's tool results, if anything: every assistant tool call must be followed by
 * exactly one result each, right after it and in call order, and no result may answer a call that isn't there.
 * The API rejects a request that breaks this, so every way a run ends must leave it holding. Empty: all good.
 */
export function answerProblems(history: ChatTurn[]): string[] {
  const problems: string[] = [];
  for (let i = 0; i < history.length; i++) {
    const turn = history[i]!;
    if (turn.role === "tool") {
      problems.push(`result ${turn.callId} at ${i} answers no call just before it`);
      continue;
    }
    if (turn.role !== "assistant" || !turn.toolCalls?.length) continue;
    const ids = turn.toolCalls.map((c) => c.id);
    const results: string[] = [];
    while (history[i + 1]?.role === "tool") results.push((history[++i] as Extract<ChatTurn, { role: "tool" }>).callId);
    if (results.join("\n") !== ids.join("\n")) problems.push(`calls [${ids}] got results [${results}]`);
  }
  return problems;
}
