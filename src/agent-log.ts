// A subagent's own transcript, for the view a click on its entry opens. Built
// from the same loop events the main transcript is built from (App.send()),
// but kept apart: only the subagent's final report reaches the parent's
// conversation, and none of this reaches any model.
import type { LoopEvent } from "./agent.ts";
import type { Message } from "./types.ts";

export interface AgentLog {
  /** "general-purpose · Read notes" */
  title: string;
  /** Finished entries; an updated entry is replaced by a new object (MessageView is memoized). */
  messages: Message[];
  /** The reply streaming in, and any reasoning before it. */
  streaming: string;
  thinking: string;
  /** A tool call still streaming in (the latest, when a reply has several), shown as it's written. */
  draft?: { index: number; name: string; args: string };
  running: boolean;
}

/** Events after which a streaming tool call is done streaming (as in the App). */
const STEP_ENDS = new Set<LoopEvent["type"]>(["assistant", "tool_start", "tool_skipped", "cut_off", "empty_reply", "error", "done"]);

/** Mutable bookkeeping kept off the log's public shape. */
const state = new WeakMap<AgentLog, { nextId: number; tools: Map<string, number>; thinkingSince: number | null }>();

export function createAgentLog({ title, prompt }: { title: string; prompt: string }): AgentLog {
  const log: AgentLog = { title, messages: [{ id: 0, role: "user", text: prompt }], streaming: "", thinking: "", running: true };
  state.set(log, { nextId: 1, tools: new Map(), thinkingSince: null });
  return log;
}

const STOPPED: Partial<Record<Extract<LoopEvent, { type: "done" }>["reason"], string>> = {
  aborted: "Interrupted.",
  declined: "Stopped: the user declined.",
  length: "The reply was cut off at the model's output limit, so nothing in it ran.",
  max_steps: "It hit its step limit.",
};

/** Folds one event into the log. */
export function applyEvent(log: AgentLog, event: LoopEvent, now = Date.now()): void {
  const s = state.get(log)!;
  const add = (message: Omit<Message, "id">) => log.messages.push({ id: s.nextId++, ...message });
  // Like the main transcript: reasoning collapses to one line once the step moves on.
  const noteThought = () => {
    if (log.thinking) add({ role: "system", text: `✻ Thought for ${Math.max(1, Math.round((now - (s.thinkingSince ?? now)) / 1000))}s` });
    log.thinking = "";
    s.thinkingSince = null;
  };
  // What streamed before an error or a stop isn't lost.
  const keepPartial = () => {
    if (log.streaming) add({ role: "assistant", text: log.streaming });
    log.streaming = "";
  };

  if (STEP_ENDS.has(event.type)) delete log.draft;
  switch (event.type) {
    case "tool_call_delta":
      if (log.draft?.index !== event.index) log.draft = { index: event.index, name: event.name, args: "" };
      log.draft.name = event.name || log.draft.name;
      log.draft.args += event.text;
      break;
    case "thinking_delta":
      s.thinkingSince ??= now;
      log.thinking += event.text;
      break;
    case "text_delta":
      log.streaming += event.text;
      break;
    case "retry":
      // The provider asks again from the start: drop what the failed attempt drew, and say why.
      log.streaming = "";
      log.thinking = "";
      s.thinkingSince = null;
      delete log.draft;
      add({ role: "system", text: event.message });
      break;
    case "assistant":
      noteThought();
      add({ role: "assistant", text: event.text });
      log.streaming = "";
      break;
    case "tool_start":
      noteThought();
      keepPartial();
      s.tools.set(event.call.id, log.messages.length);
      add({ role: "tool", text: event.call.name, tool: { label: event.label, status: "running" } });
      break;
    case "tool_end": {
      const index = s.tools.get(event.call.id);
      if (index === undefined) break;
      const { result } = event;
      // Same rule as the main transcript: a plain failure shows its message.
      const summary = result.isError && result.summary === "error" ? result.output.split("\n")[0] : result.summary;
      const status = result.declined ? "declined" : result.isError ? "error" : "done";
      log.messages[index] = { ...log.messages[index]!, tool: { label: result.label, status, summary, diff: result.diff } };
      s.tools.delete(event.call.id);
      break;
    }
    case "error":
      noteThought();
      keepPartial();
      add({ role: "system", text: event.message, isError: true });
      break;
    case "done": {
      noteThought();
      keepPartial();
      const stopped = STOPPED[event.reason];
      if (stopped) add({ role: "system", text: stopped });
      log.running = false;
      break;
    }
    case "usage":
      break;
  }
}
