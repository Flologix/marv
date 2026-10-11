// The agent loop: the heart of a coding agent.
//
//   send the conversation (+ tool definitions) → the model replies
//     ├─ it asked for tools?  run them, append the results, go again
//     └─ plain answer?        done
//
// The model decides which tools to use; this loop only runs what it asks for
// and reports back. It knows nothing about the UI or the vendor: it yields
// events, and `history` (the conversation the model sees) is appended to in
// place, never rewritten, so every request starts with the previous one and
// the provider's prompt cache keeps hitting.
//
// Tool calls run one at a time, except subagents: several `agent` calls in
// one reply run at the same time (at most MAX_PARALLEL), and their results are
// still appended in call order, so the history doesn't depend on which
// finished first.
import type { ChatTurn, Provider, ToolCall, ToolSpec, Usage } from "./provider/types.ts";
import type { ToolResult } from "./tools/types.ts";

export const DEFAULT_MAX_STEPS = 25;
export const MAX_PARALLEL = 4;

/** What the model is told about a call that never ran. */
export const NOT_RUN = {
  aborted: "Interrupted by the user before this tool ran.",
  declined: "Not run: the user declined an earlier action.",
  cutOff:
    "Not run: your reply hit the output limit before it finished, so this call may be incomplete. Redo it in smaller steps (for a big file: write_file the first part, then add the rest with append: true).",
};

/**
 * Told to the model (as a user message from Marv) when a reply with nothing to run was cut off at the output
 * limit: without it, the model would only see its own unfinished reply, or nothing (Ollama drops a tool call it
 * was still writing), and would likely try the same thing again.
 */
export const CUT_OFF_NOTE =
  "(Automatic note from Marv, the program running you) Your last reply hit the output limit and was cut off before it finished; nothing in it was run. Continue in smaller steps (for a big file: write_file the first part, then add the rest with append: true).";

/**
 * The same for a reply that reasoned and left nothing (thinking models). It either reasoned until the limit or
 * started a tool call that was cut off and dropped (Ollama drops an unfinished one): Marv can't tell which.
 */
export const CUT_OFF_REASONING_NOTE =
  "(Automatic note from Marv, the program running you) Your last reply used up the output limit, while reasoning or while writing a tool call (an unfinished call is dropped), so it did nothing. Reason briefly, and write a big file in parts (write_file the first part, then add the rest with append: true).";

/**
 * Told to the model after a second empty reply in a row (no text, no tool calls). The first is asked for again
 * unchanged: upstream hosts sometimes bill for a reply and send nothing (a dropped tool call), and a resample
 * usually comes back whole. A second suggests the model itself stopped, so it's told what it did.
 */
export const EMPTY_REPLY_NOTE =
  "(Automatic note from Marv, the program running you) Your last reply was empty: no text and no tool calls. If the task isn't finished, continue it with your tools; if it is, say what you did.";

/**
 * A copy of the history in which every tool call has a result: calls still waiting at the end (a run that was
 * cut short, e.g. by quitting while it wound down) get "not run". Without one, the API rejects every request
 * made from this history, so a session saved this way couldn't be continued.
 */
export function answerAllCalls(history: ChatTurn[]): ChatTurn[] {
  const answered = new Set(history.flatMap((t) => (t.role === "tool" ? [t.callId] : [])));
  const last = history.findLast((t) => t.role === "assistant" && t.toolCalls?.length);
  const waiting = last?.role === "assistant" ? (last.toolCalls ?? []).filter((c) => !answered.has(c.id)) : [];
  return [...history, ...waiting.map((c) => ({ role: "tool" as const, callId: c.id, name: c.name, text: NOT_RUN.aborted }))];
}

type Result = ToolResult & { label: string };
type GroupEvent =
  | { type: "start"; index: number; call: ToolCall }
  | { type: "end"; index: number; call: ToolCall; result: Result }
  | { type: "skip"; index: number; result: Result };

export type LoopEvent =
  | { type: "text_delta"; text: string }
  | { type: "thinking_delta"; text: string }
  | { type: "usage"; usage: Usage }
  /** One step's reply text is complete. */
  | { type: "assistant"; text: string }
  /** A piece of a tool call still streaming (see AgentEvent): for showing it being written, nothing runs yet. */
  | { type: "tool_call_delta"; index: number; name: string; text: string }
  | { type: "tool_start"; call: ToolCall; label: string }
  | { type: "tool_end"; call: ToolCall; result: ToolResult & { label: string } }
  /** A call answered without running (after a "no" or an interrupt): no tool_start came first. */
  | { type: "tool_skipped"; call: ToolCall; output: string }
  | { type: "error"; message: string }
  /** It reached a multiple of maxSteps and asked (onLimit) whether to keep going. */
  | { type: "step_limit"; steps: number; continued: boolean }
  /**
   * A reply had no text and no tool calls. `next`: asked for again unchanged ("retry", the first time), with a note
   * to the model ("nudge", the second), or the run ends as if it had answered ("stop", the third).
   */
  | { type: "empty_reply"; next: "retry" | "nudge" | "stop" }
  /** A reply hit the output limit: nothing in it ran. `continued`: the model was told and goes on (once per run). */
  | { type: "cut_off"; continued: boolean }
  | { type: "done"; reason: "end" | "length" | "aborted" | "declined" | "max_steps" | "error" };

interface Options {
  provider: Provider;
  /** The conversation; appended to as the run goes. */
  history: ChatTurn[];
  system: string;
  tools: ToolSpec[];
  runTool: (call: ToolCall) => Promise<ToolResult & { label: string }>;
  signal: AbortSignal;
  maxSteps?: number;
  /**
   * Asked each time the run reaches another multiple of maxSteps, after that
   * step's tool results are in (so the history is valid whatever the answer):
   * true keeps going for another maxSteps. Without it, maxSteps is a hard stop.
   */
  onLimit?: (steps: number) => Promise<boolean>;
  /** Calls that may run at the same time as their neighbours (subagents). Default: none. */
  isParallel?: (call: ToolCall) => boolean;
  maxParallel?: number;
  /**
   * After a step's tool calls are all answered, when the run goes on: a note for the model, appended as a message
   * from Marv before the next request (like CUT_OFF_NOTE), or null. Never rejects. The session's typecheck uses it (src/diagnostics).
   */
  afterStep?: (results: ToolResult[]) => Promise<string | null>;
}

/**
 * A subagent's label ("explore · Find the config"): used here before the call
 * runs and by the agent tool's own `label` (src/tools/agent.ts) after, so the
 * transcript entry doesn't change when the call starts.
 */
export const agentLabel = (type: unknown, description: string) => `${typeof type === "string" ? type : "general-purpose"} · ${description}`;

/** What an agent call asked for: its task (the first entry in its view) and the rest, for its trajectory. */
export function agentArgs(args: string): { prompt: string; type: string; description: string; isolation?: string } {
  let parsed: Record<string, unknown> = {};
  try {
    const value: unknown = JSON.parse(args);
    // The model can send any JSON ("null", "[]", "3"): only an object has fields. runTool reports the bad input.
    if (value && typeof value === "object" && !Array.isArray(value)) parsed = value as Record<string, unknown>;
  } catch {}
  const text = (key: string) => (typeof parsed[key] === "string" ? (parsed[key] as string) : undefined);
  return { prompt: text("prompt") ?? "", type: text("type") ?? "general-purpose", description: text("description") ?? "", isolation: text("isolation") };
}

/** The transcript label for a call before it runs ("src/app.ts"), falling back to the raw arguments. */
function labelOf(call: ToolCall): string {
  try {
    const args = JSON.parse(call.arguments) as Record<string, unknown>;
    const agent = typeof args.description === "string" ? agentLabel(args.type, args.description) : undefined;
    return String(args.path ?? args.pattern ?? args.command ?? agent ?? call.arguments);
  } catch {
    return call.arguments;
  }
}

/**
 * Runs a group of calls at the same time (at most `limit` at once) and reports
 * each start and end as it happens. Calls still queued after an interrupt or a
 * "no" are answered without running.
 */
async function* runGroup(
  group: ToolCall[],
  runTool: Options["runTool"],
  limit: number,
  signal: AbortSignal,
): AsyncGenerator<GroupEvent> {
  // Calls finish whenever they like (in promise callbacks); their events wait
  // in `ready` until the generator's consumer gets to them, and `wake` resumes
  // the generator if it is waiting for one.
  const ready: GroupEvent[] = [];
  let wake: (() => void) | null = null;
  const emit = (event: GroupEvent) => {
    ready.push(event);
    wake?.();
    wake = null;
  };
  let next = 0;
  let active = 0;
  let settled = 0;
  let declined = false;
  // The abort first: Esc declines what's waiting *and* aborts, and that's an interrupt, not a "no".
  const notRun = (call: ToolCall): Result => ({ output: signal.aborted ? NOT_RUN.aborted : NOT_RUN.declined, summary: "not run", label: call.name });
  /** Reserves free slots for queued calls (or answers them, after an interrupt or a "no"). Runs nothing. */
  const startMore = () => {
    while (active < limit && next < group.length) {
      const index = next++;
      const call = group[index]!;
      if (signal.aborted || declined) {
        settled++;
        emit({ type: "skip", index, result: notRun(call) });
        continue;
      }
      active++;
      emit({ type: "start", index, call });
    }
  };
  const launch = (index: number, call: ToolCall) => {
    // Called right away, so whatever the tool does first (e.g. ask for
    // approval) happens before the next call is considered; a synchronous
    // throw becomes an error result instead of a call that never settles.
    let running: Promise<Result>;
    try {
      running = runTool(call);
    } catch (err) {
      running = Promise.reject(err);
    }
    void running
      .catch((err: unknown): Result => ({
        output: `${call.name} failed: ${err instanceof Error ? err.message : String(err)}`,
        summary: "error",
        isError: true,
        label: call.name,
      }))
      .then((result) => {
        active--;
        settled++;
        declined ||= Boolean(result.declined);
        emit({ type: "end", index, call, result });
        startMore();
      });
  };
  startMore();
  while (settled < group.length || ready.length > 0) {
    if (ready.length === 0) await new Promise<void>((resolve) => (wake = resolve));
    while (ready.length > 0) {
      const event = ready.shift()!;
      if (event.type === "start" && (signal.aborted || declined)) {
        // Reserved before the interrupt or the "no", but not shown yet: answer
        // it instead (no tool_start after either), and let the rest follow.
        active--;
        settled++;
        yield { type: "skip", index: event.index, result: notRun(event.call) };
        startMore();
        continue;
      }
      yield event;
      // A call runs only once its start has been delivered: a consumer that
      // stops at tool_start (it threw, or the user quit) never sees a tool
      // run that it can't show or stop.
      if (event.type === "start") launch(event.index, event.call);
    }
  }
}

export async function* runAgent({
  provider,
  history,
  system,
  tools,
  runTool,
  signal,
  maxSteps = DEFAULT_MAX_STEPS,
  onLimit,
  isParallel = () => false,
  maxParallel = MAX_PARALLEL,
  afterStep,
}: Options): AsyncGenerator<LoopEvent> {
  // Ids already in the conversation (a resumed session's too): a new call may not reuse one.
  const usedIds = new Set(history.flatMap((turn) => (turn.role === "assistant" ? (turn.toolCalls ?? []).map((c) => c.id) : [])));
  // Replies cut off at the output limit in a row.
  let cutOffs = 0;
  // Empty replies in a row (see EMPTY_REPLY_NOTE).
  let empties = 0;
  for (let step = 0; ; step++) {
    // The step limit guards against a model that never finishes. With
    // onLimit, the user decides at each multiple whether it goes on.
    if (step > 0 && step % maxSteps === 0) {
      const continued = onLimit ? await onLimit(step) : false;
      if (onLimit) yield { type: "step_limit", steps: step, continued };
      if (signal.aborted) {
        yield { type: "done", reason: "aborted" };
        return;
      }
      if (!continued) {
        yield {
          type: "error",
          message: onLimit
            ? `Stopped at ${step} steps, as you asked. Say "continue" to pick up where it left off.`
            : `Stopped after ${step} steps without a final answer. Say "continue" to pick up where it left off.`,
        };
        yield { type: "done", reason: "max_steps" };
        return;
      }
    }
    let text = "";
    // Whether this step's reply reasoned (thinking models): the reasoning isn't kept in the history.
    let reasoned = false;
    const calls: ToolCall[] = [];
    let error: string | null = null;
    let stopReason: string | undefined;

    for await (const event of provider.stream(history, { system, tools, signal })) {
      switch (event.type) {
        case "text_delta":
          text += event.text;
          yield event;
          break;
        case "thinking_delta":
          reasoned = true;
          yield event;
          break;
        case "usage":
          yield event;
          break;
        case "tool_call_delta":
          yield event;
          break;
        case "tool_call":
          calls.push(event.call);
          break;
        case "done":
          stopReason = event.reason;
          break;
        case "error":
          error = event.message;
          break;
      }
    }

    // Every call needs an id of its own: results are matched to calls by id, by the API and by the UI. Providers
    // can repeat them (Ollama numbers each step's calls from call_0) or leave them out, so fix them here, before
    // the history (and anyone else) sees them.
    for (const call of calls) {
      if (call.id && !usedIds.has(call.id)) {
        usedIds.add(call.id);
        continue;
      }
      const base = call.id || "call";
      let n = 1;
      while (usedIds.has(`${base}_${n}`)) n++;
      call.id = `${base}_${n}`;
      usedIds.add(call.id);
    }
    // Nothing came back: no text (whitespace doesn't count) and nothing to run. Not after an error, a cut-off (handled
    // below) or an interrupt. Nothing goes into the history for it, so a retry is the same request (cached).
    if (!text.trim() && calls.length === 0 && !error && stopReason !== "length" && !signal.aborted) {
      const next = (["retry", "nudge", "stop"] as const)[Math.min(empties++, 2)]!;
      yield { type: "empty_reply", next };
      if (next === "nudge") history.push({ role: "user", text: EMPTY_REPLY_NOTE });
      if (next !== "stop") continue;
      yield { type: "done", reason: "end" };
      return;
    }
    empties = 0;
    if (text || calls.length > 0) {
      history.push(calls.length > 0 ? { role: "assistant", text, toolCalls: calls } : { role: "assistant", text });
    }
    if (text) yield { type: "assistant", text };

    if (error) {
      yield { type: "error", message: error };
      yield { type: "done", reason: "error" };
      return;
    }
    // Cut off at the output limit: whatever the reply was doing is unfinished, so its calls aren't run (a
    // write_file whose content stops mid-line would break the file), and the model is told, so it can redo the
    // work in smaller pieces. Two in a row end the run (a model stuck on a reply too big for the limit); after a
    // step that finished, it may happen again (writing several big files in parts).
    if (stopReason === "length" && !signal.aborted) {
      const continued = ++cutOffs === 1;
      // The history first, so it's valid even if the consumer stops at one of the events below.
      for (const call of calls) history.push({ role: "tool", callId: call.id, name: call.name, text: NOT_RUN.cutOff });
      // With calls, their results say it; with none, the model is told directly. Its reasoning isn't in the
      // history, so a reply that was all reasoning gets told that, not to split a file it never started.
      if (continued && calls.length === 0) history.push({ role: "user", text: reasoned && !text ? CUT_OFF_REASONING_NOTE : CUT_OFF_NOTE });
      yield { type: "cut_off", continued };
      for (const call of calls) yield { type: "tool_skipped", call, output: NOT_RUN.cutOff };
      if (!continued) {
        yield { type: "done", reason: "length" };
        return;
      }
      continue;
    }
    cutOffs = 0;
    if (calls.length === 0) {
      yield { type: "done", reason: signal.aborted ? "aborted" : "end" };
      return;
    }

    // Every result of this step, in call order, for afterStep.
    const stepResults: ToolResult[] = [];
    // Every call must get a result, even after an interrupt or a "no": a
    // request with an unanswered tool call is rejected by the API.
    let declined = false;
    // How many calls have their result in the history: they're appended in call order, so the rest are a suffix.
    let answered = 0;
    const answer = (call: ToolCall, text: string) => {
      history.push({ role: "tool", callId: call.id, name: call.name, text });
      answered++;
    };
    try {
      for (let i = 0; i < calls.length; ) {
        // A run of consecutive parallel calls goes together; anything else, one at a time.
        let end = i + 1;
        if (isParallel(calls[i]!)) while (end < calls.length && isParallel(calls[end]!)) end++;
        const group = calls.slice(i, end);
        i = end;
        if (signal.aborted || declined) {
          // The abort first, as in runGroup: after Esc the model reads "Interrupted", not "the user declined".
          for (const call of group) {
            const output = signal.aborted ? NOT_RUN.aborted : NOT_RUN.declined;
            answer(call, output);
            yield { type: "tool_skipped", call, output };
          }
          continue;
        }
        const results: Result[] = [];
        for await (const event of runGroup(group, runTool, maxParallel, signal)) {
          if (event.type === "start") {
            yield { type: "tool_start", call: event.call, label: labelOf(event.call) };
          } else {
            results[event.index] = event.result;
            if (event.type === "end") yield { type: "tool_end", call: event.call, result: event.result };
            else yield { type: "tool_skipped", call: group[event.index]!, output: event.result.output };
          }
        }
        // In call order, whatever order they finished in (prompt cache).
        group.forEach((call, k) => answer(call, results[k]!.output));
        stepResults.push(...results);
        declined = results.some((r) => r.declined);
      }
    } finally {
      // The consumer stopped mid-tools (it threw while handling an event, or broke out of its loop): the calls
      // without a result would leave the history invalid, and the API would reject every request from now on.
      for (const call of calls.slice(answered)) history.push({ role: "tool", callId: call.id, name: call.name, text: NOT_RUN.aborted });
    }
    if (signal.aborted) {
      yield { type: "done", reason: "aborted" };
      return;
    }
    // The user said no: stop here and let them say what to do instead.
    if (declined) {
      yield { type: "done", reason: "declined" };
      return;
    }
    if (afterStep) {
      const note = await afterStep(stepResults);
      if (signal.aborted) {
        yield { type: "done", reason: "aborted" };
        return;
      }
      if (note) history.push({ role: "user", text: note });
    }
  }
}
