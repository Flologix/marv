import type { DiffShown } from "./tools/types.ts";

// A single entry in the transcript the user sees.
// "system" is for Marv's own notices (help text, errors) — not the LLM system prompt.
// "tool" is a tool call the agent made, shown with its outcome.
export type Role = "user" | "assistant" | "system" | "tool";

export interface ToolStatus {
  /** What it was called on, e.g. the file path. */
  label: string;
  status: "running" | "done" | "error" | "declined";
  /** e.g. "42 lines", or the error. */
  summary?: string;
  /** A subagent's finished tool calls, shown with ctrl+o. */
  steps?: string[];
  /** What a file change did (edit_file, write_file), drawn under the entry: collapsed, all of it with ctrl+o. */
  diff?: DiffShown;
}

/** How important a notice is; its left bar, title and worst-of tone use its color. */
export type NoticeTone = "info" | "warning" | "error";

/** One bullet of a notice: a short headline, then (optionally) indented detail lines. */
export interface NoticeItem {
  text: string;
  detail?: string[];
}

/** A distinct, colored block of startup and other issues (see NoticeView), transcript only, never sent to the model. */
export interface Notice {
  title: string;
  tone: NoticeTone;
  items: NoticeItem[];
}

export interface Message {
  id: number;
  role: Role;
  /** For "tool": the tool name. */
  text: string;
  /** Renders a system message in the error color. */
  isError?: boolean;
  /** Render a system message as Markdown (lists get hanging indents). */
  markdown?: boolean;
  /** Renders a system message as a distinct notice block (a left bar, a colored title, bullets). */
  notice?: Notice;
  tool?: ToolStatus;
}
