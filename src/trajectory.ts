// Trajectories: a record of what every agent run did, for measuring Marv over
// time (which model, prompt or tool change made runs better) and, later, for
// training data. One append-only JSONL file per session:
//
//   ~/.marv/trajectories/<project>/<session id>.jsonl
//
// Each line is one record (see TrajectoryRecord), stamped with the format
// version, the time and the session. A turn is one user message and
// everything the agents did until they stopped; subagents' records carry
// their own agent id and point to the turn they ran in. Feedback (explicit
// labels and implicit signals from the user's next message) is appended as
// it comes, pointing back to its turn.
//
// Separate from sessions (src/sessions.ts) on purpose: a session is the
// latest state, rewritten after every turn, for resuming; a trajectory is the
// history, only ever appended to, with timings, approvals and subagents'
// steps that a session never had. Private (0600): it holds code and prompts.
import { appendFile, chmod, mkdir } from "node:fs/promises";
import { dirname, join } from "node:path";
import type { LoopEvent } from "./agent.ts";
import { projectKey } from "./paths.ts";
import type { ToolCall, Usage } from "./provider/types.ts";
import type { Decision } from "./tools/types.ts";

export const FORMAT_VERSION = 1;

/** "main", or a subagent's id ("<turn>.<n>"). */
type Who = { turn: string; agent: string };

export type TrajectoryRecord =
  /** Once per session (again on resume): what the runs that follow ran with. */
  | { type: "session"; root: string; marv: string; provider: string; model: string; system: string; tools: string[]; git?: string }
  /** The user's message; `forModel` when the model got something else (a /skill's instructions). */
  | { type: "turn_start"; turn: string; text: string; forModel?: string; provider: string; model: string; yolo?: boolean; sandbox?: boolean; diagnostics?: boolean; checks?: boolean }
  /** One model request: its tokens (and cost) and how long since the previous step ended. */
  | (Who & { type: "request"; step: number; usage: Usage; ms: number })
  /** A finished reply, with the reasoning before it (thinking models). */
  | (Who & { type: "assistant"; text: string; thinking?: string })
  /** A tool call and exactly what the model got back. approval: "none" (read-only), "auto" (yolo), or the answer. */
  // `ms` of a write includes waiting for the turn's typecheck baseline (started by the preview, awaited before writing).
  | (Who & {
      type: "tool";
      call: ToolCall;
      output: string;
      summary: string;
      isError: boolean;
      declined: boolean;
      approval: "none" | "auto" | "interrupted" | Decision;
      ms: number;
    })
  | (Who & { type: "error"; message: string })
  /** It hit the step limit and the user was asked whether it should keep going. */
  | (Who & { type: "step_limit"; steps: number; continued: boolean })
  /** A reply hit the output limit, so nothing in it ran; `continued`: the model was told (CUT_OFF_NOTE) and went on. */
  | (Who & { type: "cut_off"; continued: boolean })
  /** A reply was stuck repeating and was stopped; `continued`: the model was told (TEXT_LOOP_NOTE) and went on (once per run). */
  | (Who & { type: "text_loop"; continued: boolean })
  /** Tool calls repeated without progress; `continued`: the model was warned (once), then the run was stopped. */
  | (Who & { type: "tool_loop"; continued: boolean })
  /** A reply had no text and no tool calls; `next`: asked again, nudged (EMPTY_REPLY_NOTE), or stopped there. */
  | (Who & { type: "empty_reply"; next: "retry" | "nudge" | "stop" })
  /**
   * A check after a step's file changes: the typecheck (src/diagnostics), or with `source: "checks"` the project's
   * own checks (src/checks). How it went, and how many problems were new.
   */
  | (Who & { type: "check"; source?: "checks"; status: "done" | "failed" | "off"; ms?: number; before?: number; errors?: number; added?: number; reason?: string; note?: string })
  /** `agent` started a subagent with the agent tool call `call`; its records follow with `agent: subagent`. */
  | (Who & { type: "subagent_start"; subagent: string; call: string; agentType: string; description: string; prompt: string; isolation?: string })
  /** How an agent's run ended (for "main", the turn). */
  | (Who & { type: "agent_end"; reason: string; steps: number; tools: number; ms: number })
  /** The conversation was replaced by a summary (what the model sees from here on). */
  | { type: "compact"; turn?: string; summary: string }
  /** About a turn: 1 good, -1 bad, 0 just labeled. */
  | { type: "feedback"; turn: string; score: 1 | -1 | 0; source: "explicit" | "implicit"; labels?: string[]; note?: string; phrase?: string };

export class TrajectoryStore {
  constructor(readonly dir: string) {}

  open(root: string, session: string): Trajectory {
    return new Trajectory(join(this.dir, projectKey(root), `${session}.jsonl`), session);
  }
}

export class Trajectory {
  /** Logging must never break a run: the first failure is reported here, the rest are dropped quietly. */
  onError: (message: string) => void = () => {};
  private queue: Promise<void> = Promise.resolve();
  private ready = false;
  private failed = false;

  constructor(
    readonly path: string,
    readonly session: string,
  ) {}

  /** Appends a record. Writes are queued, so records land in the order they were written. */
  write(record: TrajectoryRecord): void {
    const line = `${JSON.stringify({ v: FORMAT_VERSION, t: new Date().toISOString(), session: this.session, ...record })}\n`;
    this.queue = this.queue.then(async () => {
      if (this.failed) return;
      try {
        if (!this.ready) {
          await mkdir(dirname(this.path), { recursive: true, mode: 0o700 });
          await appendFile(this.path, "", { mode: 0o600 });
          await chmod(this.path, 0o600); // `mode` only applies when the file is created
          // A crash (disk full, power loss) can leave a half-written last line: start on a fresh line, or the next
          // record would be glued onto it and both would be unreadable.
          const file = Bun.file(this.path);
          if (file.size > 0 && (await file.slice(file.size - 1).text()) !== "\n") await appendFile(this.path, "\n");
          this.ready = true;
        }
        await appendFile(this.path, line);
      } catch (err) {
        this.failed = true;
        this.onError(`Couldn't write the trajectory log (${(err as Error).message}); this session won't be logged.`);
      }
    });
  }

  /** Resolves once everything written so far is on disk. */
  flush(): Promise<void> {
    return this.queue;
  }
}

/**
 * Turns one agent's loop events into records: the same events the transcript
 * is built from, plus timings. One per agent per turn (the main agent, and
 * each subagent).
 */
export class AgentRecorder {
  private readonly started: number;
  private stepStart: number;
  private thinking = "";
  private steps = 0;
  private tools = 0;
  private toolStarts = new Map<string, number>();
  private ended = false;

  constructor(
    private readonly sink: (record: TrajectoryRecord) => void,
    private readonly who: Who,
    private readonly now: () => number = Date.now,
  ) {
    this.started = this.stepStart = now();
  }

  /** The step-end typecheck isn't the model's time: the next request's clock starts after it. */
  restartClock(): void {
    this.stepStart = this.now();
  }

  event(event: LoopEvent): void {
    if (this.ended) return;
    const { turn, agent } = this.who;
    switch (event.type) {
      case "thinking_delta":
        this.thinking += event.text;
        break;
      case "usage":
        this.steps++;
        this.sink({ type: "request", turn, agent, step: this.steps, usage: event.usage, ms: this.now() - this.stepStart });
        break;
      case "assistant":
        this.sink({ type: "assistant", turn, agent, text: event.text, ...this.takeThinking() });
        break;
      case "tool_start":
        // Reasoning that led straight to a tool call, with no text.
        if (this.thinking) this.sink({ type: "assistant", turn, agent, text: "", ...this.takeThinking() });
        this.tools++;
        this.toolStarts.set(event.call.id, this.now());
        break;
      case "tool_end": {
        const { result } = event;
        const started = this.toolStarts.get(event.call.id) ?? this.now();
        this.toolStarts.delete(event.call.id);
        this.sink({
          type: "tool",
          turn,
          agent,
          call: event.call,
          output: result.output,
          summary: result.summary,
          isError: Boolean(result.isError),
          declined: Boolean(result.declined),
          approval: result.approval ?? "none",
          ms: this.now() - started,
        });
        // The next request goes out once the last tool is done.
        this.stepStart = this.now();
        break;
      }
      case "tool_skipped":
        // Never ran, but the model got this as its result: a trajectory must hold every result it saw.
        this.sink({ type: "tool", turn, agent, call: event.call, output: event.output, summary: "not run", isError: false, declined: true, approval: "none", ms: 0 });
        break;
      case "error":
        this.sink({ type: "error", turn, agent, message: event.message });
        break;
      case "step_limit":
        this.sink({ type: "step_limit", turn, agent, steps: event.steps, continued: event.continued });
        break;
      case "cut_off":
        this.sink({ type: "cut_off", turn, agent, continued: event.continued });
        break;
      case "text_loop":
        this.sink({ type: "text_loop", turn, agent, continued: event.continued });
        break;
      case "tool_loop":
        this.sink({ type: "tool_loop", turn, agent, continued: event.continued });
        break;
      case "empty_reply":
        this.sink({ type: "empty_reply", turn, agent, next: event.next });
        break;
      case "done":
        this.finish(event.reason);
        break;
      case "text_delta":
        break; // the whole reply comes with "assistant"
    }
  }

  /** Records how the run ended (only the first call counts: a run that threw never sends done). */
  finish(reason: string): void {
    if (this.ended) return;
    this.ended = true;
    if (this.thinking) this.sink({ type: "assistant", ...this.who, text: "", ...this.takeThinking() });
    this.sink({ type: "agent_end", ...this.who, reason, steps: this.steps, tools: this.tools, ms: this.now() - this.started });
  }

  private takeThinking(): { thinking?: string } {
    const thinking = this.thinking;
    this.thinking = "";
    return thinking ? { thinking } : {};
  }
}
