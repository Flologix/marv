// Numbers from trajectory logs (src/trajectory.ts), to see whether a change
// (a model, a prompt, a Marv version) made runs better: how turns ended, what
// they cost, and how the user rated them. Run it with `bun run stats`.
import { tokens } from "./usage.ts";

type Rec = Record<string, any>;

export interface GroupStats {
  turns: number;
  /** How the main agent's runs ended: end, declined, aborted, error, max_steps… */
  ended: Record<string, number>;
  /** All requests in the turn, subagents' included. */
  tokensPerTurn: number;
  /** The main agent's tool calls (a subagent counts as one). */
  toolsPerTurn: number;
  medianSeconds: number;
  /** Turns with a rating: the user's own (/good, /bad) if any, otherwise what their next message implied. */
  rated: number;
  good: number;
  bad: number;
  /** Tool calls through each door out of the sandbox (host, github, process), subagents' included. */
  doors: Record<string, number>;
}

export type GroupBy = "model" | "marv";

interface Turn {
  group: string;
  tokens: number;
  /** The main agent's agent_end record. */
  end?: Rec;
  explicit?: number;
  implicit?: number;
  doors: Record<string, number>;
}

/** Groups turns by model (as of the turn) or by Marv version (as of its session), and sums them up. */
export function summarize(records: Rec[], by: GroupBy = "model"): Map<string, GroupStats> {
  const marvOf = new Map<string, string>(); // session → its latest "session" record's version
  const turns = new Map<string, Turn>();
  for (const r of records) {
    if (r.type === "session") marvOf.set(r.session, r.marv);
    else if (r.type === "turn_start") turns.set(r.turn, { group: by === "model" ? r.model : (marvOf.get(r.session) ?? "unknown"), tokens: 0, doors: {} });
    const t = turns.get(r.turn);
    if (!t) continue;
    if (r.type === "request") t.tokens += r.usage.promptTokens + r.usage.completionTokens;
    else if (r.type === "agent_end" && r.agent === "main") t.end = r;
    else if (r.type === "tool" && r.door) t.doors[r.door] = (t.doors[r.door] ?? 0) + 1;
    else if (r.type === "feedback" && r.score !== 0) {
      if (r.source === "explicit") t.explicit = r.score; // the latest one counts
      else t.implicit = r.score;
    }
  }

  const groups = new Map<string, Turn[]>();
  for (const t of turns.values()) groups.set(t.group, [...(groups.get(t.group) ?? []), t]);
  const stats = new Map<string, GroupStats>();
  for (const [group, list] of groups) {
    const ended: Record<string, number> = {};
    for (const t of list) if (t.end) ended[t.end.reason] = (ended[t.end.reason] ?? 0) + 1;
    const scores = list.map((t) => t.explicit ?? t.implicit).filter((s) => s !== undefined);
    const finished = list.filter((t) => t.end);
    const doors: Record<string, number> = {};
    for (const t of list) for (const [door, n] of Object.entries(t.doors)) doors[door] = (doors[door] ?? 0) + n;
    stats.set(group, {
      turns: list.length,
      ended,
      tokensPerTurn: Math.round(list.reduce((sum, t) => sum + t.tokens, 0) / list.length),
      toolsPerTurn: finished.length ? finished.reduce((sum, t) => sum + t.end!.tools, 0) / finished.length : 0,
      medianSeconds: median(finished.map((t) => t.end!.ms / 1000)),
      rated: scores.length,
      good: scores.filter((s) => s > 0).length,
      bad: scores.filter((s) => s < 0).length,
      doors,
    });
  }
  return stats;
}

function median(values: number[]): number {
  if (values.length === 0) return 0;
  const sorted = [...values].sort((a, b) => a - b);
  const mid = Math.floor(sorted.length / 2);
  return sorted.length % 2 ? sorted[mid]! : (sorted[mid - 1]! + sorted[mid]!) / 2;
}

/** A plain-text table, one row per group. */
export function formatStats(stats: Map<string, GroupStats>): string {
  const header = ["", "turns", "good", "rated", "tokens/turn", "tools/turn", "median", "ended", "doors"];
  const rows = [...stats].map(([group, s]) => [
    group,
    String(s.turns),
    s.rated ? `${Math.round((100 * s.good) / s.rated)}%` : "-",
    `${s.rated} (${s.good}+ ${s.bad}-)`,
    tokens(s.tokensPerTurn),
    s.toolsPerTurn.toFixed(1),
    `${s.medianSeconds.toFixed(1)}s`,
    Object.entries(s.ended)
      .map(([reason, n]) => `${reason} ${n}`)
      .join(", "),
    Object.entries(s.doors)
      .map(([door, n]) => `${door} ${n}`)
      .join(", ") || "-",
  ]);
  const widths = header.map((_, i) => Math.max(...[header, ...rows].map((row) => row[i]!.length)));
  return [header, ...rows].map((row) => row.map((cell, i) => cell.padEnd(widths[i]!)).join("  ").trimEnd()).join("\n");
}
