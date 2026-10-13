import { dirname, join } from "node:path";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Box, useApp, useInput, useWindowSize, type DOMElement } from "ink";
import { agentArgs } from "./agent.ts";
import { applyEvent, createAgentLog, type AgentLog } from "./agent-log.ts";
import { GENERAL_PURPOSE, type AgentType } from "./agents.ts";
import { copyToClipboard } from "./clipboard.ts";
import type { HerdrReporter } from "./herdr.ts";
import { checksStatus, commands, diagnosticsStatus, isCommand, runCommand, stepsStatus, trajectoriesStatus, yoloStatus } from "./commands/index.ts";
import {
  needsSetup,
  PRESETS,
  resolveConfig,
  type Config,
  type ConfigStore,
  type Env,
  type FileConfig,
  type ProviderId,
} from "./config/config.ts";
import { projectKey, shortenHome } from "./paths.ts";
import { mouse, type MouseEvent } from "./mouse.ts";
import { configFactory } from "./provider/factory.ts";
import { createProvider } from "./provider/index.ts";
import { listModels, type ModelInfo } from "./provider/models.ts";
import type { Provider, Usage } from "./provider/types.ts";
import { selection } from "./selection.ts";
import { costText, emptyTotals, tokens, type Totals } from "./usage.ts";
import { addMemory, findMemory, loadMemory, removeMemory, type Memories, type MemoryPaths } from "./memory.ts";
import { MarvSession, type CompactResult, type Resumed } from "./session.ts";
import { timeAgo, type SessionStore, type SessionSummary } from "./sessions.ts";
import { skillMessage, type Skill } from "./skills.ts";
import type { TrajectoryStore } from "./trajectory.ts";
import type { McpLogin, McpManager, McpServerStatus } from "./mcp/manager.ts";
import { canOpenBrowser, isCallback, openBrowser } from "./mcp/oauth.ts";
import { parseMcpServers } from "./mcp/config.ts";
import { liveDeps, resolveAdd, type Resolved } from "./mcp/install/index.ts";
import { addedNotice, approvalFor } from "./mcp/install/proposal.ts";
import { searchRegistry, searchText, type RegistryHit } from "./mcp/install/registry.ts";
import { parseAddArgs, type AddOptions } from "./mcp/install/source.ts";
import { addPersonalServer, personalServerNames, removePersonalServer } from "./mcp/personal-file.ts";
import { printable } from "./printable.ts";
import type { AgentProgress, ApprovalRequest, Decision, PlanItem } from "./tools/types.ts";
import { checkNotice } from "./diagnostics/turn.ts";
import { projectCheckNotice, type ProjectCheckResult } from "./checks/kind.ts";
import { loadChecks, type ProjectCheck } from "./checks/load.ts";
import { hiddenPathFoldersItem } from "./commands/sandbox-notice.ts";
import { instructionsCutItem, type InstructionsCut } from "./prompt.ts";
import type { CommandAction } from "./commands/index.ts";
import type { Message, Notice, NoticeItem, NoticeTone } from "./types.ts";
import { AgentView, AgentViewHeader } from "./ui/AgentView.tsx";
import { PLAN_MIN_WINDOW, PlanView } from "./ui/PlanView.tsx";
import { Approval } from "./ui/Approval.tsx";
import { SessionPicker } from "./ui/SessionPicker.tsx";
import { MessageView } from "./ui/MessageView.tsx";
import { PromptInput } from "./ui/PromptInput.tsx";
import { ScrollView } from "./ui/ScrollView.tsx";
import { Setup } from "./ui/Setup.tsx";
import { Splash } from "./ui/Splash.tsx";
import { formatUsage, StatusBar } from "./ui/StatusBar.tsx";
import { DraftView, ThinkingView } from "./ui/ThinkingView.tsx";
import { agentEntryAt, Transcript, type TranscriptItem } from "./ui/Transcript.tsx";

const EXIT_CONFIRM_MS = 1500;
const NOTICE_MS = 2000;
/**
 * Streamed text is shown at most this often (Ink draws at most 30 frames/s
 * anyway). Updating React on every token re-parsed the whole Markdown reply
 * for frames nobody would see.
 */
/** Events after which a streaming tool call is done streaming: complete, cut off, or abandoned. */
const STEP_ENDS = new Set(["assistant", "tool_start", "tool_skipped", "cut_off", "empty_reply", "error", "done"]);
const STREAM_FLUSH_MS = 33;

/** An untrusted project server, for the trust notice: what it runs, what it reads from your environment, and which project files. */
function describeUntrusted(s: McpServerStatus): string {
  const extras = [
    s.reads.length > 0 && `reads ${s.reads.map((v) => `$${v}`).join(", ")} from your environment`,
    s.runsProjectFiles.length > 0 && `runs this project's ${s.runsProjectFiles.join(", ")} (and whatever those load)`,
  ].filter(Boolean);
  return `${s.name} (${s.target})${extras.length ? `, which ${extras.join(" and ")}` : ""}`;
}

/** How severe a tone is, so the startup notice can use its worst. */
const TONE_RANK: Record<NoticeTone, number> = { info: 0, warning: 1, error: 2 };

/** The worst of a set of tones (error beats warning beats info). */
function worstTone(tones: NoticeTone[]): NoticeTone {
  return tones.reduce((worst, tone) => (TONE_RANK[tone] > TONE_RANK[worst] ? tone : worst), "info");
}

/** Collates the startup notices known at once into one block: a bullet per notice, titled with how many there are. */
function startupNotice(notes: { tone: NoticeTone; item: NoticeItem }[]): Notice | null {
  if (notes.length === 0) return null;
  return {
    title: `Startup · ${notes.length} note${notes.length === 1 ? "" : "s"}`,
    tone: worstTone(notes.map((n) => n.tone)),
    items: notes.map((n) => n.item),
  };
}

/** Opens a login's URL where a browser can open; elsewhere the transcript's link is all there is. */
function defaultOpenUrl(url: URL) {
  if (canOpenBrowser(process.env, process.platform)) openBrowser(url);
}

/** "github: logged in · connected · 41 tools", or why not: the end of /mcp login. */
function loginResult(s: McpServerStatus): { text: string; isError: boolean } {
  if (s.state === "connected") return { text: `${s.name}: logged in · connected · ${s.tools.length} tool${s.tools.length === 1 ? "" : "s"}. Marv can use them from your next message.`, isError: false };
  if (s.state === "needs-login") return { text: `${s.name}: logged in, but the server still refused the token (see /mcp).`, isError: true };
  return { text: `${s.name}: logged in, but it didn't connect: ${s.error ?? s.state} (see /mcp).`, isError: true };
}

// Defaults defined once, so they're the same objects on every render (the
// effects that report problems depend on them).
const DEFAULT_AGENTS: AgentType[] = [GENERAL_PURPOSE];
const NO_PROBLEMS: string[] = [];

/** The project's checks for /checks, read when it runs (a check can be added at any time); other commands skip the disk. */
function checksOnDisk(root: string, text: string): { list: ProjectCheck[]; problems: string[] } {
  if (!/^\/checks\b/.test(text.trim())) return { list: [], problems: [] };
  const { checks, problems } = loadChecks(root);
  return { list: checks, problems };
}

interface Props {
  store: ConfigStore;
  /** The config file as loaded at startup; null on first run. */
  initialFile: FileConfig | null;
  env: Env;
  version: string;
  /** For display, e.g. "~/Projects/ekko-agent". */
  cwd: string;
  /** Absolute project root; the tools can't reach outside it. */
  root: string;
  /** The project's AGENTS.md, read at startup. */
  instructions?: string;
  /** Skills found at startup, and any that couldn't be loaded. */
  skills?: Skill[];
  skillProblems?: string[];
  /** 0 skips the splash entirely (used by tests). */
  splashMs?: number;
  /** Swappable so tests can inject an instant provider. */
  makeProvider?: (config: Config, model?: Pick<ModelInfo, "reasoning">) => Provider;
  /** Swappable so tests don't touch the real clipboard. Returns how it copied. */
  copy?: (text: string) => Promise<string>;
  /** Swappable so tests don't hit OpenRouter or Ollama for the model picker. */
  loadModels?: (config: Pick<Config, "provider" | "baseUrl">) => Promise<ModelInfo[]>;
  /** Where sessions are saved; without it, nothing is saved. */
  sessions?: SessionStore;
  /** Where every turn is logged (trajectories); without it, nothing is logged. */
  trajectories?: TrajectoryStore;
  /** The session's MCP servers (already starting); their tools join the built-in ones. */
  mcp?: McpManager;
  /** MCP config files that couldn't be read, and why. */
  mcpProblems?: string[];
  /** /mcp add's lookup and /mcp search: by default through Marv's own `get` (tests stub them, so nothing hits the network). */
  resolveMcp?: (options: AddOptions, signal: AbortSignal) => Promise<Resolved>;
  searchMcp?: (words: string, signal: AbortSignal) => Promise<RegistryHit[]>;
  /**
   * /mcp login: opens the authorization URL in the browser. By default xdg-open/open where a browser can open (not
   * over SSH, not without a display); tests inject their own, so no browser ever opens.
   */
  openUrl?: (url: URL) => void;
  /** Hands cli.tsx a function to run before the process exits: saves the session and flushes the trajectory. */
  onFlush?: (flush: () => Promise<void>) => void;
  /** Start by resuming: the latest session here (marv -c), or a picker (marv -r). */
  resume?: "latest" | "pick";
  /** Where memory lives, and what it held at startup. */
  memory?: { paths: MemoryPaths; initial: Memories };
  /** Agent types the agent tool can start, and files that couldn't be loaded. */
  agents?: AgentType[];
  agentProblems?: string[];
  /** PATH folders under the home folder the sandbox can't see, shortened (~) for display; said once at startup. */
  hiddenPathFolders?: string[];
  /** Where a too-long AGENTS.md was cut (issue #40); said once at startup. */
  instructionsCut?: InstructionsCut;
  /** Where subagents' worktrees go (~/.marv/worktrees/<project>); without it, no worktrees. */
  worktreesDir?: string;
  /** Running in a Herdr pane: Marv reports its state there (src/herdr.ts). */
  herdr?: HerdrReporter;
}

// "model" is the /model picker: the setup screen, starting at the model step.
type SetupMode = "first-run" | "reconfigure" | "model" | null;

export function App({
  store,
  initialFile,
  env,
  version,
  cwd,
  root,
  instructions,
  skills = [],
  skillProblems = [],
  splashMs = 1200,
  makeProvider = createProvider,
  copy = copyToClipboard,
  loadModels = listModels,
  sessions,
  trajectories,
  mcp,
  mcpProblems = NO_PROBLEMS,
  resolveMcp,
  searchMcp,
  openUrl = defaultOpenUrl,
  onFlush,
  resume,
  memory,
  agents = DEFAULT_AGENTS,
  agentProblems = NO_PROBLEMS,
  hiddenPathFolders = NO_PROBLEMS,
  instructionsCut,
  worktreesDir,
  herdr,
}: Props) {
  const { exit } = useApp();
  const { columns, rows } = useWindowSize();
  const [phase, setPhase] = useState<"splash" | "main">(splashMs > 0 ? "splash" : "main");

  // Config: the saved file + env overrides → the Config we run with (the session makes the Provider from it).
  const [file, setFile] = useState(initialFile);
  const config = useMemo(() => resolveConfig(file, env), [file, env]);
  const [setupMode, setSetupMode] = useState<SetupMode>(() => (needsSetup(initialFile, config) ? "first-run" : null));

  // What the user sees (includes help text, errors, the welcome banner). What the model sees is the session's
  // conversation (src/session.ts): Marv's own notices never reach it.
  const [items, setItems] = useState<TranscriptItem[]>([{ kind: "welcome", id: "welcome-0" }]);
  // Read when the session saves, which can be after unmounting (on the way out).
  const itemsRef = useRef(items);
  itemsRef.current = items;
  const configRef = useRef(config);
  configRef.current = config;
  const [picker, setPicker] = useState<SessionSummary[] | null>(null);
  // marv -c: the last session is still loading.
  const [loadingSession, setLoadingSession] = useState(false);

  // Memory as of the start of this conversation, for the welcome banner (the session reloads it on /clear).
  const [memories, setMemories] = useState<Memories | undefined>(memory?.initial);
  // Skills show up in the / menu next to the built-in commands.
  const baseMenu = useMemo(() => [...commands, ...skills.map(({ name, description }) => ({ name, description }))], [skills]);
  // Server names after "/mcp login " and "/mcp logout " (the servers that can log in; read per render, they change).
  const loginable = mcp?.loginable().join("\n") ?? "";
  const menu = useMemo(
    () => [
      ...baseMenu,
      ...loginable
        .split("\n")
        .filter(Boolean)
        .flatMap((name) => [
          { name: `mcp login ${name}`, description: `Log in to ${name} (OAuth, in your browser)` },
          { name: `mcp logout ${name}`, description: `Forget ${name}'s login` },
        ]),
    ],
    [baseMenu, loginable],
  );
  // The session's numbers, for the status bar: the latest request's tokens (how full the context is), and the
  // whole session's (they survive /clear: that's money spent).
  const [usage, setUsage] = useState<Usage | null>(null);
  const [totals, setTotals] = useState<Totals>(emptyTotals);
  const [compacting, setCompacting] = useState(false);
  // What a turn is waiting for before its first request (MCP servers starting), shown in place of "Thinking…".
  const [waitingFor, setWaitingFor] = useState<string | null>(null);

  const [input, setInput] = useState("");
  const [history, setHistory] = useState<string[]>([]);
  const [streaming, setStreaming] = useState<string | null>(null);
  // Tools running right now (subagents run several at once), and how many of
  // them are subagents. While any run, their transcript lines show the
  // progress, so no "Thinking…".
  const [toolsRunning, setToolsRunning] = useState(0);
  const [agentsRunning, setAgentsRunning] = useState(0);
  // ctrl+o: show subagents' steps and whole diffs under their entries.
  const [showSteps, setShowSteps] = useState(false);
  // Each subagent's own transcript, by its entry's id (`msg-12`), for the view
  // a click on the entry opens. Only this session's runs: sessions don't save them.
  const agentLogs = useRef(new Map<string, AgentLog>());
  // The subagent whose view is open, if any; `logVersion` moves when its log changed.
  const [viewing, setViewing] = useState<string | null>(null);
  /** The plan tool's list (#15), pinned above the prompt while it has something left to do. */
  const [plan, setPlan] = useState<readonly PlanItem[] | null>(null);
  const viewingRef = useRef(viewing);
  viewingRef.current = viewing;
  const [logVersion, setLogVersion] = useState(0);
  // Where each subagent entry is drawn, for hit-testing clicks (see Transcript).
  const agentEntries = useRef(new Map<string, DOMElement>());
  const onAgentRef = useCallback((id: string, element: DOMElement | null) => {
    if (element) agentEntries.current.set(id, element);
    else agentEntries.current.delete(id);
  }, []);
  const openView = useCallback((id: string | null) => {
    // The two transcripts have different rows: a selection or remembered row of one means nothing in the other.
    selection.reset();
    setViewing(id);
  }, []);
  // Tools waiting for the user's yes/no, oldest first (parallel subagents can
  // ask at the same time). The first one is shown in place of the prompt.
  type Pending = { id: number; request: ApprovalRequest; resolve: (d: Decision) => void };
  const nextApprovalId = useRef(1);
  const approvals = useRef<Pending[]>([]);
  const [approval, setApproval] = useState<{ head: Pending; waiting: number } | null>(null);
  const showApprovals = useCallback(() => {
    const [head] = approvals.current;
    setApproval(head ? { head, waiting: approvals.current.length - 1 } : null);
  }, []);

  // The session asks here for each call that needs a yes. "Don't ask again" scopes are the session's: it answers
  // those itself, so they never reach this queue.
  const approve = useCallback(
    (request: ApprovalRequest): Promise<Decision> =>
      new Promise((resolve) => {
        approvals.current.push({ id: nextApprovalId.current++, request, resolve });
        showApprovals();
      }),
    [showApprovals],
  );
  const decide = useCallback(
    (decision: Decision) => {
      const [head, ...rest] = approvals.current;
      if (!head) return;
      let remaining = rest;
      if (decision === "always" && !head.request.once) {
        // The session remembers the scope from now on; requests already waiting in it are covered too, except the
        // ones asked every time (`once`: a door to the host).
        const key = head.request.scope.key;
        const covered = (pending: (typeof rest)[number]) => pending.request.scope.key === key && !pending.request.once;
        for (const pending of rest) if (covered(pending)) pending.resolve("yes");
        remaining = rest.filter((pending) => !covered(pending));
      }
      approvals.current = remaining;
      showApprovals();
      head.resolve(decision);
    },
    [showApprovals],
  );
  /** No to everything waiting (ctrl+c at an approval, and part of Esc). */
  const declineAll = useCallback(() => {
    const pending = approvals.current;
    approvals.current = [];
    showApprovals();
    for (const p of pending) p.resolve("no");
  }, [showApprovals]);
  // Esc at an approval: decline everything and stop the run, like ctrl+c, so
  // parallel subagents that are still running don't raise new prompts.
  const cancelAll = useCallback(() => {
    declineAll();
    abortRef.current?.abort();
  }, [declineAll]);
  // The model's reasoning while it thinks. Shown live, never sent back to the model.
  const [thinking, setThinking] = useState("");
  // A tool call the model is still writing (its JSON so far), shown as it streams.
  const [draft, setDraft] = useState<{ name: string; args: string } | null>(null);
  const [confirmExit, setConfirmExit] = useState(false);
  const [notice, setNotice] = useState<string | null>(null);
  // Set while the session works (a turn, or /compact): Esc and ctrl+c stop it through this.
  const abortRef = useRef<{ abort(): void } | null>(null);
  const nextId = useRef(1);
  // Bumped on every submit so the transcript jumps back to the newest message.
  const [followKey, setFollowKey] = useState(0);

  const busy = streaming !== null;

  const addMessage = useCallback((message: Omit<Message, "id">) => {
    const id = nextId.current++;
    setItems((prev) => [...prev, { kind: "message", id: `msg-${id}`, message: { id, ...message } }]);
    return id;
  }, []);

  const updateMessage = useCallback((id: number, patch: Partial<Message>) => {
    setItems((prev) =>
      prev.map((item) => (item.kind === "message" && item.message.id === id ? { ...item, message: { ...item.message, ...patch } } : item)),
    );
  }, []);

  // The session: the conversation, the agent loop, tools, MCP, compaction, trajectories and saving
  // (src/session.ts). Made once from the props; changed settings reach it through configure() below.
  const [, setSessionVersion] = useState(0);
  const [session] = useState(
    () =>
      new MarvSession({
        root,
        cwd,
        provider: configFactory(config, makeProvider, loadModels),
        version,
        instructions,
        skills,
        agents,
        memory,
        mcp,
        approve,
        sandbox: config.sandbox,
        // The user approves each command at this terminal: bash may ask to run one on the host (#52).
        hostCommands: true,
        yolo: config.yolo,
        diagnostics: config.diagnostics,
        checks: config.checks,
        mcpTools: config.mcpTools,
        maxSteps: config.maxSteps,
        sessions,
        trajectories,
        logTrajectories: config.trajectories,
        worktreesDir,
        transcript: () => itemsRef.current.flatMap((item) => (item.kind === "message" ? [item.message] : [])),
        onChange: () => setSessionVersion((v) => v + 1),
        onWarning: (text) => addMessage({ role: "system", text, isError: true }),
      }),
  );
  // The project checks' last pass, for /checks.
  const lastChecks = useRef<ProjectCheckResult | undefined>(undefined);
  // Settings changed (setup, /model, /think, /sandbox, /yolo, /trajectories, /diagnostics, /checks): the session uses them from its next turn.
  const configured = useRef(config);
  useEffect(() => {
    if (configured.current === config) return;
    configured.current = config;
    session.configure({
      provider: configFactory(config, makeProvider, loadModels),
      sandbox: config.sandbox,
      yolo: config.yolo,
      trajectories: config.trajectories,
      diagnostics: config.diagnostics,
      checks: config.checks,
      mcpTools: config.mcpTools,
      maxSteps: config.maxSteps,
    });
    setSessionVersion((v) => v + 1);
  }, [config, session, makeProvider, loadModels]);
  // Herdr: blocked while an approval waits for you (the main agent's, a subagent's, or "Keep going?"), working
  // while a turn or /compact runs, idle otherwise. Herdr's sidebar and `herdr agent wait` follow this.
  const herdrState = approval ? "blocked" : busy ? "working" : "idle";
  const herdrMessage = approval ? [approval.head.request.agent, approval.head.request.label].filter(Boolean).join(": ") : undefined;
  useEffect(() => herdr?.state(herdrState, herdrMessage), [herdr, herdrState, herdrMessage]);
  // The session id changes on /clear and on a resume.
  const sessionId = session.id;
  useEffect(() => void herdr?.session(sessionId, sessions?.file(root, sessionId)), [herdr, sessionId, sessions, root]);
  // What the model list said about the model (OpenRouter), and its context window.
  const info = session.modelInfo;
  const contextLength = session.contextLength;

  /** The status bar's numbers, from the session. */
  const syncUsage = useCallback(() => {
    const { last, totals } = session.usage();
    setUsage(last ?? null);
    setTotals(totals);
  }, [session]);

  const clearTranscript = useCallback(() => {
    const failed = (err: unknown) =>
      addMessage({ role: "system", isError: true, text: `Couldn't start a new conversation: ${(err as Error).message}` });
    // A new conversation (and session file), with the memories saved during the last one. It throws during a turn
    // (handleSubmit doesn't get here while busy), and then nothing is cleared.
    let cleared: Promise<void>;
    try {
      cleared = session.clear();
    } catch (err) {
      failed(err);
      return;
    }
    void cleared.then(() => setMemories(session.memory), failed);
    selection.reset();
    agentLogs.current.clear();
    setViewing(null);
    setUsage(null);
    setPlan(null);
    setItems([{ kind: "welcome", id: "welcome-0" }]);
  }, [session, addMessage]);

  /** Says how a compaction went (automatic: before a message, because the context was nearly full). */
  const reportCompaction = useCallback(
    (result: CompactResult, automatic: boolean) => {
      if (!result.compacted) {
        if (result.reason === "empty") addMessage({ role: "system", text: "Nothing to compact yet." });
        // Stopped before a message: that's the message too (it would run on the nearly full context).
        else if (result.reason === "stopped")
          addMessage({ role: "system", text: automatic ? "Stopped: nothing was compacted, and your message wasn't sent." : "Compaction stopped; nothing changed." });
        else addMessage({ role: "system", isError: true, text: `Couldn't compact the conversation: ${result.error}` });
        return;
      }
      const usedBefore = result.tokensBefore;
      const context = session.contextLength;
      const why = automatic && usedBefore && context ? ` (the context was ${Math.round((100 * usedBefore) / context)}% full)` : "";
      const size = usedBefore ? `: ${tokens(usedBefore)} → about ${tokens(Math.round(result.summary.length / 4))} tokens` : "";
      addMessage({
        role: "system",
        text: `✻ Compacted the conversation${why}${size}. Marv continues from a summary; your transcript is unchanged.`,
      });
    },
    [session, addMessage],
  );

  /** /compact [focus]: summarizes the conversation now (src/compact.ts). The transcript is untouched; Esc cancels it. */
  const compact = useCallback(
    async (focus: string | undefined) => {
      abortRef.current = { abort: () => session.interrupt() };
      setStreaming("");
      setCompacting(true);
      let result: CompactResult;
      try {
        result = await session.compact(focus);
      } catch (err) {
        result = { compacted: false, reason: "failed", error: (err as Error).message };
      }
      abortRef.current = null;
      setStreaming(null);
      setCompacting(false);
      syncUsage();
      reportCompaction(result, false);
    },
    [session, syncUsage, reportCompaction],
  );

  const send = useCallback(
    // `forModel`: what the model gets, when it differs from what the user typed (a /skill).
    async (text: string, forModel = text) => {
      addMessage({ role: "user", text });
      // Busy from here, so a second message can't start a second turn; Esc or ctrl+c (they abort whatever
      // abortRef holds) stop this one, also while it waits for MCP servers or compacts.
      abortRef.current = { abort: () => session.interrupt() };
      setStreaming("");

      // Per step: the reply streaming in, and any reasoning before it.
      let reply = "";
      let thought = "";
      let stepStarted = Date.now();
      let thoughtMs = 0;
      // The tool call streaming in, if any: the latest one, when a reply has several.
      let draft: { index: number; name: string; args: string } | null = null;
      // After a retry the waiting line shows its message; the new attempt's first output clears it.
      let retrying = false;
      const retried = () => {
        if (!retrying) return;
        retrying = false;
        setWaitingFor(null);
      };
      const noteThought = () => {
        if (thought) {
          const seconds = Math.max(1, Math.round((thoughtMs || Date.now() - stepStarted) / 1000));
          addMessage({ role: "system", text: `✻ Thought for ${seconds}s` });
        }
        thought = "";
        thoughtMs = 0;
        setThinking("");
      };
      const toolLines = new Map<string, { line: number; label: string }>();
      // Calls that got their tool_end; any other entry is closed in `finally`.
      const ended = new Set<string>();
      // Subagents' progress, batched into the same flush as streamed text: with
      // several running, updating the transcript on every step would re-render
      // it far more often than Ink can draw.
      const progress = new Map<string, AgentProgress>();
      const steps = new Map<string, string[]>();
      // Subagents' logs, by call id (the view finds them by entry id); the open one re-renders in the flush.
      const logs = new Map<string, AgentLog>();
      let logChanged = false;
      const isViewed = (log: AgentLog) => viewingRef.current !== null && agentLogs.current.get(viewingRef.current) === log;
      // A compaction the user stopped has already said the message wasn't sent.
      let compactionStopped = false;
      // Tokens accumulate in `reply`/`thought` and reach React in batches.
      let flushTimer: ReturnType<typeof setTimeout> | null = null;
      const flush = () => {
        if (flushTimer) clearTimeout(flushTimer);
        flushTimer = null;
        setStreaming(reply);
        setThinking(thought);
        setDraft(draft && { name: draft.name, args: draft.args });
        if (logChanged) setLogVersion((v) => v + 1);
        logChanged = false;
        for (const [callId, p] of progress) {
          const entry = toolLines.get(callId);
          if (entry) updateMessage(entry.line, { tool: { label: entry.label, status: "running", summary: p.line, steps: p.steps } });
        }
        progress.clear();
      };
      // The session drops events that come after turn_end, so nothing arrives here once the turn is over.
      const scheduleFlush = () => {
        flushTimer ??= setTimeout(flush, STREAM_FLUSH_MS);
      };

      try {
        for await (const event of session.send(text, { forModel })) {
          // The step moved on: the call is complete (or never will be), so its draft goes.
          if (draft && STEP_ENDS.has(event.type)) {
            draft = null;
            setDraft(null);
          }
          switch (event.type) {
            case "status":
              setWaitingFor(event.status === "waiting_for_mcp" ? "Waiting for MCP servers to start…" : event.status === "checking" ? "Checking…" : null);
              setCompacting(event.status === "compacting");
              if (event.status === "running") stepStarted = Date.now(); // the wait for the next reply starts after a check
              break;
            case "check": {
              const notice = checkNotice(event.result);
              if (notice) addMessage({ role: "system", text: notice });
              break;
            }
            case "checks": {
              lastChecks.current = event.result;
              const notice = projectCheckNotice(event.result);
              if (notice) addMessage({ role: "system", text: notice });
              break;
            }
            case "compaction":
              setCompacting(false);
              syncUsage();
              reportCompaction(event.result, true);
              compactionStopped = !event.result.compacted && event.result.reason === "stopped";
              break;
            case "subagent_progress":
              progress.set(event.callId, event.progress);
              steps.set(event.callId, event.progress.steps);
              scheduleFlush();
              break;
            case "subagent": {
              // Subagents' requests count toward the session's tokens and cost.
              if (event.event.type === "usage") setTotals(session.usage().totals);
              const log = logs.get(event.callId);
              if (!log) break;
              applyEvent(log, event.event);
              // Only the open view costs a render; the others just keep their log.
              if (isViewed(log)) {
                logChanged = true;
                scheduleFlush();
              }
              break;
            }
            case "thinking_delta":
              retried();
              thought += event.text;
              scheduleFlush();
              break;
            case "text_delta":
              retried();
              if (thought && !thoughtMs) thoughtMs = Date.now() - stepStarted;
              reply += event.text;
              scheduleFlush();
              break;
            case "tool_call_delta":
              retried();
              if (thought && !thoughtMs) thoughtMs = Date.now() - stepStarted;
              if (!draft || draft.index !== event.index) draft = { index: event.index, name: event.name, args: "" };
              draft.name = event.name || draft.name;
              draft.args += event.text;
              scheduleFlush();
              break;
            case "retry":
              // The connection was lost and the provider is asking again from the start: what the failed attempt
              // drew (reply, reasoning, a tool call being written) goes, and the waiting line says what happened
              // until the new attempt streams.
              reply = "";
              thought = "";
              thoughtMs = 0;
              draft = null;
              flush();
              setWaitingFor(event.message);
              retrying = true;
              break;
            case "assistant":
              flush(); // a step ended: show everything before moving on
              noteThought();
              addMessage({ role: "assistant", text: event.text });
              reply = "";
              setStreaming("");
              break;
            case "tool_start": {
              flush();
              noteThought();
              setToolsRunning((n) => n + 1);
              if (event.call.name === "agent") setAgentsRunning((n) => n + 1);
              // Providers reuse call ids from step to step (Ollama's call_0…):
              // nothing from an earlier call with this id carries over.
              steps.delete(event.call.id);
              progress.delete(event.call.id);
              ended.delete(event.call.id);
              const line = addMessage({ role: "tool", text: event.call.name, tool: { label: event.label, status: "running" } });
              toolLines.set(event.call.id, { label: event.label, line });
              logs.delete(event.call.id);
              if (event.call.name === "agent") {
                const log = createAgentLog({ title: event.label, prompt: agentArgs(event.call.arguments).prompt });
                logs.set(event.call.id, log);
                agentLogs.current.set(`msg-${line}`, log);
              }
              break;
            }
            case "tool_end": {
              const { result } = event;
              const entry = toolLines.get(event.call.id);
              progress.delete(event.call.id); // a late progress flush mustn't turn it back to "running"
              const callSteps = steps.get(event.call.id);
              steps.delete(event.call.id);
              ended.add(event.call.id);
              // A plain failure shows its message; a subagent that stopped shows its own summary.
              const summary = result.isError && result.summary === "error" ? result.output.split("\n")[0] : result.summary;
              const status = result.declined ? "declined" : result.isError ? "error" : "done";
              if (entry) updateMessage(entry.line, { tool: { label: result.label, status, summary, steps: callSteps, diff: result.diff } });
              // The main agent's plan call: a new list (an empty one clears it). Subagents have no plan tool.
              if (event.call.name === "plan" && result.plan) setPlan(result.plan.length ? result.plan : null);
              const log = logs.get(event.call.id);
              if (log) {
                // It may have ended before its loop started (a failed check): either way it's over now.
                log.running = false;
                logs.delete(event.call.id);
                if (isViewed(log)) setLogVersion((v) => v + 1);
              }
              setToolsRunning((n) => Math.max(0, n - 1));
              if (event.call.name === "agent") setAgentsRunning((n) => Math.max(0, n - 1));
              stepStarted = Date.now();
              break;
            }
            case "usage":
              syncUsage();
              break;
            case "error":
              addMessage({ role: "system", text: event.message, isError: true });
              break;
            case "cut_off":
              // The run ending here is said by done ("length"); going on, the user should know why the model changes tack.
              if (event.continued) addMessage({ role: "system", text: "✻ The reply hit the model's output limit, so nothing in it ran. Marv told the model to continue in smaller steps." });
              break;
            case "text_loop":
              // The reply was stuck repeating itself: it was stopped and dropped. The partial reply on screen goes.
              reply = "";
              flush();
              if (event.continued) addMessage({ role: "system", text: "✻ The reply got stuck repeating itself, so Marv stopped it, dropped it, and told the model to take another approach." });
              break;
            case "tool_loop":
              if (event.continued) addMessage({ role: "system", text: "✻ The model kept making the same tool call and getting the same result, so Marv answered with a note instead of running it again." });
              break;
            case "empty_reply":
              // A first empty reply is asked for again without a word: usually the provider dropped it.
              if (event.next === "nudge") addMessage({ role: "system", text: "✻ The model sent two empty replies in a row, so Marv asked it to continue." });
              if (event.next === "stop") addMessage({ role: "system", text: "The model's replies stayed empty, so the run ended. Say \"continue\" to try again." });
              break;
            case "done":
              if (event.reason === "aborted") addMessage({ role: "system", text: "Interrupted." });
              if (event.reason === "declined") addMessage({ role: "system", text: "Stopped. Tell Marv what to do instead." });
              if (event.reason === "length") addMessage({ role: "system", text: "The reply was cut off at the model's output limit, so nothing in it ran." });
              if (event.reason === "loop") addMessage({ role: "system", text: "Stopped: the model stayed stuck in a loop after Marv's note, so the task isn't finished. Say what to try instead, or \"continue\"." });
              break;
            case "turn_end":
              // Stopped before the loop began: while waiting for MCP servers (or compacting, which said so itself).
              if (event.reason === "interrupted" && !compactionStopped) addMessage({ role: "system", text: "Interrupted." });
              break;
          }
        }
      } catch (err) {
        // Drawing an event failed; leaving the loop has already stopped the turn.
        addMessage({ role: "system", text: `Error: ${(err as Error).message}`, isError: true });
      } finally {
        if (flushTimer) clearTimeout(flushTimer);
        // The turn ended early: close the entries it never ended.
        for (const [callId, entry] of toolLines) {
          if (!ended.has(callId)) updateMessage(entry.line, { tool: { label: entry.label, status: "error", summary: "interrupted", steps: steps.get(callId) } });
        }
        for (const log of logs.values()) log.running = false;
        if ([...logs.values()].some(isViewed)) setLogVersion((v) => v + 1);
        noteThought();
        setDraft(null);
        // Anything still queued belongs to this turn, which is over: declined.
        declineAll();
        abortRef.current = null;
        setStreaming(null);
        setWaitingFor(null);
        setCompacting(false);
        setToolsRunning(0);
        setAgentsRunning(0);
        syncUsage();
      }
    },
    [session, addMessage, updateMessage, declineAll, syncUsage, reportCompaction],
  );

  // /memory, /remember, /forget: you editing Marv's memory directly (no approval needed).
  const nextTime = "Marv will use it from the next conversation (/clear starts one).";
  const showMemory = async () => {
    if (!memory) return addMessage({ role: "system", text: "Memory isn't available." });
    const { personal, project } = await loadMemory(memory.paths);
    const list = (entries: string[]) => (entries.length ? entries.map((e) => `- ${e}`).join("\n") : "*(nothing yet)*");
    addMessage({
      role: "system",
      markdown: true,
      text: [
        `**Personal** (all projects): \`${shortenHome(memory.paths.personal)}\``,
        list(personal),
        `**This project**: \`${shortenHome(memory.paths.project)}\``,
        list(project),
        "Edit these files directly, use `/remember` and `/forget`, or ask Marv to remember or forget something.",
      ].join("\n\n"),
    });
  };
  const rememberNote = async (scope: "personal" | "project", text: string) => {
    if (!memory) return;
    const { added, error } = await addMemory(memory.paths[scope], text);
    addMessage(
      error
        ? { role: "system", isError: true, text: error }
        : { role: "system", text: added ? `Saved to ${scope} memory. ${nextTime}` : `That's already in ${scope} memory.` },
    );
  };
  const forgetNote = async (text: string) => {
    if (!memory) return;
    // Look in both scopes; remove only when exactly one memory matches.
    const matches = [
      ...(await findMemory(memory.paths.personal, text)).map((m) => ({ scope: "personal" as const, m })),
      ...(await findMemory(memory.paths.project, text)).map((m) => ({ scope: "project" as const, m })),
    ];
    if (matches.length !== 1) {
      return addMessage({
        role: "system",
        isError: true,
        text: matches.length ? `${matches.length} memories match; use more of the text: ${matches.map((x) => `"${x.m}"`).join(", ")}` : `No memory matches "${text}".`,
      });
    }
    const result = await removeMemory(memory.paths[matches[0]!.scope], matches[0]!.m);
    addMessage("error" in result ? { role: "system", isError: true, text: result.error } : { role: "system", text: `Forgot: ${result.removed}` });
  };

  const handleSubmit = (raw: string) => {
    const text = raw.trim();
    if (!text) return;
    // A login's callback address (pasted from a browser that couldn't reach Marv) holds the authorization code: it
    // goes to the login, never to the model, the prompt history or the saved transcript. Even during a turn.
    if (isCallback(text)) {
      setInput("");
      pasteCallback(text);
      return;
    }
    if (busy) return;
    if (loadingSession) {
      setNotice("Loading your last session…");
      return;
    }
    setInput("");
    setFollowKey((n) => n + 1);
    if (viewing) openView(null);
    setHistory((prev) => (prev.at(-1) === text ? prev : [...prev, text]));

    if (!isCommand(text)) {
      void send(text);
      return;
    }
    const action = runCommand(text, {
      config,
      configPath: shortenHome(store.path),
      skills,
      skillProblems,
      agents,
      agentProblems,
      hiddenPathFolders,
      usage: { totals, last: usage, contextLength },
      trajectoriesPath: trajectories && shortenHome(join(trajectories.dir, projectKey(root))),
      mcp: { servers: mcp?.status() ?? [], problems: mcpProblems },
      checks: { ...checksOnDisk(root, text), last: lastChecks.current },
    });
    switch (action.type) {
      case "print":
        addMessage({ role: "system", text: action.text, isError: action.isError, markdown: action.markdown });
        break;
      case "clear":
        clearTranscript();
        break;
      case "setup":
        setSetupMode("reconfigure");
        break;
      case "thinking":
        void saveConfig(
          { ...(file ?? { provider: config.provider }), thinking: action.on },
          action.on
            ? "Thinking on: models may reason before answering (slower, often better)."
            : `Thinking off: models answer directly${info?.reasoning === "mandatory" ? ` (except ${config.model}, which always reasons)` : ""}.`,
        );
        break;
      case "memory":
        void showMemory();
        break;
      case "remember":
        void rememberNote(action.scope, action.text);
        break;
      case "forget":
        void forgetNote(action.text);
        break;
      case "compact":
        void compact(action.focus);
        break;
      case "resume":
        void openPicker();
        break;
      case "skill":
        void send(text, skillMessage(action.skill, action.args));
        break;
      case "sandbox":
        void saveConfig(
          { ...(file ?? { provider: config.provider }), sandbox: action.on },
          action.on
            ? "Sandbox on: bash commands run in bubblewrap."
            : "Sandbox off: bash commands run directly on your system (each still needs your approval).",
        );
        break;
      case "trajectories":
        void saveConfig({ ...(file ?? { provider: config.provider }), trajectories: action.on }, `Trajectories ${trajectoriesStatus({ ...config, trajectories: action.on })}`);
        break;
      case "checks":
        void saveConfig({ ...(file ?? { provider: config.provider }), checks: action.on }, `Checks ${checksStatus({ ...config, checks: action.on })}`);
        break;
      case "diagnostics":
        void saveConfig({ ...(file ?? { provider: config.provider }), diagnostics: action.on }, `Diagnostics ${diagnosticsStatus({ ...config, diagnostics: action.on })}`);
        break;
      case "steps": {
        // `/steps` with no argument only showed the setting; a number (0 included) and "off" save it. The env var
        // keeps beating the file until the process ends, as with MARV_MODEL.
        const every = action.every;
        if (every === undefined) break;
        const saved = { ...(file ?? { provider: config.provider }), maxSteps: every };
        // MARV_MAX_STEPS still wins over the file: say what's saved, and what this session actually uses.
        const effective = resolveConfig(saved, env).maxSteps;
        const notice =
          effective === every
            ? stepsStatus({ ...config, maxSteps: every })
            : `Saved, but MARV_MAX_STEPS=${env.MARV_MAX_STEPS} overrides it while it's set. Now: ${stepsStatus({ ...config, maxSteps: effective })}`;
        void saveConfig(saved, notice);
        break;
      }
      case "feedback":
        rateLastTurn(action);
        break;
      case "mcp-trust":
        void trustMcp();
        break;
      case "mcp-add":
        void addMcp(action.args);
        break;
      case "mcp-search":
        void findMcp(action.words);
        break;
      case "mcp-remove":
        void removeMcp(action.name);
        break;
      case "mcp-login":
        void loginMcp(action.server);
        break;
      case "mcp-logout":
        void logoutMcp(action.server);
        break;
      case "yolo":
        void saveConfig({ ...(file ?? { provider: config.provider }), yolo: action.on }, yoloStatus({ ...config, yolo: action.on }));
        break;
      case "model":
        if (action.id) void completeSetup({ ...(file ?? { provider: config.provider }), model: action.id });
        else setSetupMode("model");
        break;
      case "exit":
        exit();
        break;
    }
  };

  /** /mcp trust: the project's servers the user hasn't trusted yet, trusted (for this exact config) and started. */
  const trustMcp = async () => {
    const waiting = mcp?.untrusted() ?? [];
    if (!mcp || waiting.length === 0) {
      addMessage({ role: "system", text: "No MCP servers are waiting to be trusted (/mcp lists them)." });
      return;
    }
    addMessage({ role: "system", text: `Starting ${waiting.map((s) => s.name).join(", ")}…` });
    try {
      const started = await mcp.trustAll();
      const lines = started.map((s) => (s.state === "connected" ? `${s.name}: connected · ${s.tools.length} tools` : `${s.name}: failed: ${s.error}`));
      addMessage({ role: "system", text: lines.join("\n"), isError: started.some((s) => s.state !== "connected") });
    } catch (err) {
      addMessage({ role: "system", text: `Couldn't trust the servers: ${(err as Error).message}`, isError: true });
    }
  };

  // The /mcp login waiting for its callback: a pasted callback address goes to it.
  const pendingLogin = useRef<{ server: string; login: McpLogin } | null>(null);
  // Servers already said to need a login (at startup or since), so a mid-session expiry is told once.
  const toldNeedsLogin = useRef(new Set<string>());

  /**
   * /mcp login: not a turn (the prompt stays free, and a running turn keeps its tools). The browser opens where it
   * can; the link is shown either way, with the paste fallback for a browser on another machine.
   */
  const loginMcp = async (name: string) => {
    if (!mcp) return;
    let login: McpLogin;
    try {
      login = mcp.login(name, {
        authorize: (url) => {
          addMessage({
            role: "system",
            text: `Log in to ${name} at ${url.host}: opening your browser… If it doesn't open, open this link and, if your browser then shows an error page, paste its address here:\n${url.href}`,
          });
          openUrl(url);
        },
      });
    } catch (err) {
      addMessage({ role: "system", isError: true, text: (err as Error).message });
      return;
    }
    const mine = { server: name, login };
    pendingLogin.current = mine;
    toldNeedsLogin.current.add(name);
    try {
      const status = await login.done;
      if (status.state === "connected") toldNeedsLogin.current.delete(name);
      addMessage({ role: "system", ...loginResult(status) });
    } catch (err) {
      addMessage({ role: "system", isError: true, text: `${name}: not logged in: ${(err as Error).message}` });
    } finally {
      if (pendingLogin.current === mine) pendingLogin.current = null;
    }
  };

  /** A pasted callback address: shown as "(login callback)", handed to the waiting login. */
  const pasteCallback = (text: string) => {
    addMessage({ role: "user", text: "(login callback)" });
    const pending = pendingLogin.current;
    if (!pending) {
      addMessage({ role: "system", isError: true, text: "No login is waiting for that address (/mcp login <server> starts one). It wasn't sent to the model." });
      return;
    }
    const check = pending.login.paste(text);
    if (check.ok) addMessage({ role: "system", text: `Got it: finishing the login to ${pending.server}…` });
    else if (check.why === "state" || check.why === "path") addMessage({ role: "system", isError: true, text: "That address isn't from this login: paste the address the browser ended up on after the latest /mcp login." });
    else if (check.why === "used") addMessage({ role: "system", isError: true, text: "This login already has its callback." });
    // An error= callback: login.done rejects with the reason, and loginMcp says so.
  };

  /** /mcp logout: revoke where the server can, forget the login. */
  const logoutMcp = async (name: string) => {
    if (!mcp) return;
    toldNeedsLogin.current.add(name);
    try {
      const { revoked } = await mcp.logout(name);
      const grant =
        revoked === "revoked"
          ? "The authorization server revoked the grant."
          : revoked === "unsupported"
            ? "The authorization server can't revoke it from here, so the grant still exists on its side: remove it in your account's settings there (usually under applications or integrations)."
            : "Revoking the grant failed, so it may still exist on the authorization server's side: remove it in your account's settings there.";
      addMessage({ role: "system", text: `Logged out of ${name}: its tokens are forgotten. ${grant}` });
    } catch (err) {
      addMessage({ role: "system", isError: true, text: (err as Error).message });
    }
  };

  // ~/.marv (or MARV_CONFIG_DIR): where config.json is, mcp.json is too.
  const configDir = dirname(store.path);

  /** Busy work outside a turn (/mcp add's lookup, /mcp search): Esc stops it, `label` shows where "Thinking…" would. */
  const busyWith = async <T,>(label: string, work: (signal: AbortSignal) => Promise<T>): Promise<T> => {
    const controller = new AbortController();
    abortRef.current = controller;
    setStreaming("");
    setWaitingFor(label);
    try {
      return await work(controller.signal);
    } finally {
      abortRef.current = null;
      setStreaming(null);
      setWaitingFor(null);
    }
  };

  /**
   * /mcp add: look the source up (all fetching happens here, before the approval), ask once with the exact entry,
   * write that entry to ~/.marv/mcp.json, and start it. Nothing fetched after the yes changes what runs: the file
   * gets the JSON the approval showed, parsed back.
   */
  const addMcp = async (args: string) => {
    let options: AddOptions;
    try {
      options = parseAddArgs(args);
    } catch (err) {
      addMessage({ role: "system", isError: true, text: (err as Error).message });
      return;
    }
    type Outcome = { choices: string[] } | { declined: string } | { added: string; entry: unknown } | { failed: string };
    let outcome: Outcome;
    try {
      outcome = await busyWith(`Looking up ${printable(options.source)}…`, async (signal): Promise<Outcome> => {
        const resolved = await (resolveMcp ?? ((o, s) => resolveAdd(o, liveDeps(env, s))))(options, signal);
        if ("choices" in resolved) return resolved;
        // Removed in this conversation: its tools stay listed until /clear, so it can't come back yet (McpManager.add).
        if (mcp?.status().some((s) => s.name === resolved.name && s.state === "removed")) {
          throw new Error(`"${resolved.name}" was removed in this conversation and its tools stay listed until the next one: /clear first, or pick another --name.`);
        }
        if ((await personalServerNames(configDir)).includes(resolved.name)) {
          throw new Error(`You already have a server named "${resolved.name}": pick another with --name, or /mcp remove ${resolved.name} first.`);
        }
        // A project server with that name is running (or waiting for trust) under it: McpManager.add can't start a
        // second one, and replacing it mid-conversation would swap the tools under the model. Another name is simpler.
        if (mcp?.status().some((s) => s.name === resolved.name && s.source === "project")) {
          throw new Error(`This project has a server named "${resolved.name}": pick another with --name.`);
        }
        if (signal.aborted) return { failed: "Stopped: nothing was added." };
        const { request, json } = approvalFor(resolved, { env });
        if ((await approve(request)) !== "yes") return { declined: resolved.name };
        const entry: unknown = JSON.parse(json);
        await addPersonalServer(configDir, resolved.name, entry);
        return { added: resolved.name, entry };
      });
    } catch (err) {
      const stopped = (err as Error).name === "AbortError";
      outcome = { failed: stopped ? "Stopped: nothing was added." : `Couldn't add the server: ${(err as Error).message}` };
    }
    if ("failed" in outcome) {
      addMessage({ role: "system", isError: true, text: outcome.failed });
      return;
    }
    if ("choices" in outcome) {
      addMessage({ role: "system", markdown: true, text: `That README names several servers. Add one with:\n\n${outcome.choices.map((c) => `- \`${printable(c)}\``).join("\n")}` });
      return;
    }
    if ("declined" in outcome) {
      addMessage({ role: "system", text: `Not added: ${outcome.declined}.` });
      return;
    }
    // Loaded exactly as at startup, so ${VAR}s are filled in the same way.
    const { added: name, entry } = outcome;
    const { servers, problems } = parseMcpServers({ [name]: entry }, { ...env, MARV_PROJECT_DIR: root });
    const server = servers[0];
    if (!server) {
      addMessage({ role: "system", isError: true, text: `Added ${name} to ~/.marv/mcp.json, but didn't start it: ${problems[0]?.replace(/^"[^"]*" in mcpServers: /, "") ?? "it couldn't be loaded"}. Set it in your shell and restart Marv.` });
      return;
    }
    if (!mcp) {
      addMessage({ role: "system", text: `Added ${name} to ~/.marv/mcp.json. Restart Marv to start it.` });
      return;
    }
    addMessage({ role: "system", text: `Starting ${name}…` });
    try {
      addMessage({ role: "system", ...addedNotice(await mcp.add(server)) });
    } catch (err) {
      addMessage({ role: "system", isError: true, text: `Added ${name} to ~/.marv/mcp.json, but didn't start it now: ${(err as Error).message} It starts when you restart Marv.` });
    }
  };

  /** /mcp search: the MCP Registry's latest, active entries. */
  const findMcp = async (words: string) => {
    try {
      const hits = await busyWith("Searching the MCP Registry…", (signal) => (searchMcp ?? ((w, s) => searchRegistry(w, liveDeps(env, s))))(words, signal));
      addMessage({ role: "system", markdown: true, text: searchText(words, hits) });
    } catch (err) {
      addMessage({ role: "system", isError: true, text: `Couldn't search the MCP Registry: ${(err as Error).message}` });
    }
  };

  /** /mcp remove: one of your servers out of ~/.marv/mcp.json, and stopped. No approval: the user typed it, and it takes capability away. */
  const removeMcp = async (name: string) => {
    if (!(await personalServerNames(configDir)).includes(name)) {
      const project = mcp?.status().some((s) => s.name === name && s.source === "project");
      addMessage({
        role: "system",
        isError: true,
        text: project ? `"${name}" is this project's server: remove it from .mcp.json.` : `You have no server named "${name}" in ~/.marv/mcp.json (/mcp lists them).`,
      });
      return;
    }
    try {
      await removePersonalServer(configDir, name);
    } catch (err) {
      addMessage({ role: "system", isError: true, text: (err as Error).message });
      return;
    }
    const running = mcp?.status().some((s) => s.name === name && s.source === "personal" && s.state !== "removed");
    if (running) await mcp!.remove(name).catch(() => {});
    addMessage({
      role: "system",
      text: `Removed ${name} from ~/.marv/mcp.json${running ? " and stopped it. Its tools stay listed (failing) until /clear or a restart." : "."}`,
    });
  };

  /** /good, /bad, /label: feedback on the latest turn of this session, in its trajectory. */
  const rateLastTurn = ({ score, note, labels }: Extract<CommandAction, { type: "feedback" }>) => {
    const outcome = session.rate({ score, note, labels });
    if (outcome === "off") {
      addMessage({ role: "system", text: "Trajectory logging is off, so there's nothing to rate (/trajectories on).", isError: true });
    } else if (outcome === "nothing") {
      addMessage({ role: "system", text: "Nothing to rate yet: ratings apply to the last turn of this conversation.", isError: true });
    } else {
      const what = labels ? `Labeled the last turn: ${labels.join(", ")}` : `Rated the last turn: ${score > 0 ? "good" : "bad"}`;
      addMessage({ role: "system", text: `${what}${note ? ` (${note})` : ""}.` });
    }
  };

  const saveConfig = async (next: FileConfig, notice: string) => {
    try {
      await store.save(next);
      setFile(next);
      setSetupMode(null);
      addMessage({ role: "system", text: notice });
    } catch (err) {
      addMessage({ role: "system", text: `Could not save config: ${(err as Error).message}`, isError: true });
    }
  };

  const completeSetup = (next: FileConfig) => {
    const label = PRESETS[next.provider ?? "openrouter"].label;
    return saveConfig(next, `Saved to ${shortenHome(store.path)} · ${label} · ${next.model}`);
  };

  // The picker lists models for whichever provider is being chosen. A custom
  // endpoint (baseUrl) only applies to the provider it was saved with.
  const loadModelsFor = useCallback(
    (p: ProviderId) => loadModels(resolveConfig({ provider: p, baseUrl: p === file?.provider ? file.baseUrl : undefined }, env)),
    [loadModels, file, env],
  );

  // On first run there is nothing to go back to, so cancelling setup quits.
  const cancelSetup = () => (setupMode === "first-run" ? exit() : setSetupMode(null));

  // Esc stops a running reply or tool, and declines any approval already
  // queued: one can join the queue just before its prompt is drawn, and must
  // not be left waiting on a stopped run. (At an approval prompt the Approval's
  // own Esc handler runs cancelAll too; doing it twice is harmless.)
  // With a subagent's view open, Esc only closes it (the run carries on).
  useInput(
    (_char, key) => {
      if (!key.escape) return;
      if (viewing) openView(null);
      else if (abortRef.current) cancelAll();
    },
    { isActive: phase === "main" && setupMode === null },
  );

  // ctrl+o: show or hide details under their entries: subagents' steps and whole diffs.
  useInput(
    (char, key) => {
      if (key.ctrl && char === "o") setShowSteps((s) => !s);
    },
    { isActive: phase === "main" && setupMode === null },
  );

  // ctrl+c: interrupt a reply → clear the prompt → ask to confirm → exit.
  // (While setup is open, Setup handles ctrl+c itself.)
  useInput(
    (char, key) => {
      if (!(key.ctrl && char === "c")) return;
      if (abortRef.current) {
        declineAll(); // pending approvals count as declined
        abortRef.current.abort();
      } else if (input) {
        setInput("");
      } else if (confirmExit) {
        exit();
      } else {
        setConfirmExit(true);
      }
    },
    { isActive: phase === "main" && setupMode === null },
  );

  useEffect(() => {
    if (!confirmExit) return;
    const timer = setTimeout(() => setConfirmExit(false), EXIT_CONFIRM_MS);
    return () => clearTimeout(timer);
  }, [confirmExit]);

  useEffect(() => {
    if (!notice) return;
    const timer = setTimeout(() => setNotice(null), NOTICE_MS);
    return () => clearTimeout(timer);
  }, [notice]);

  // Mouse selection: drag to highlight (past the top or bottom to auto-scroll),
  // release to copy. The selection store draws the highlight itself through
  // Ink's patched transformOutput/repaint, so a drag doesn't re-render React.
  useEffect(() => {
    const onMouse = (event: MouseEvent) => {
      switch (event.type) {
        case "press":
          selection.press({ x: event.x, y: event.y });
          break;
        case "drag":
          selection.drag({ x: event.x, y: event.y });
          break;
        case "release": {
          const text = selection.release();
          if (!text) {
            // A click: on a subagent's entry, open its view.
            const row = viewingRef.current ? null : selection.rowAt({ x: event.x, y: event.y });
            const id = row === null ? null : agentEntryAt(row, agentEntries.current);
            if (!id) break;
            if (agentLogs.current.has(id)) openView(id);
            else setNotice("Only this session's subagents can be opened (ctrl+o shows the steps)");
            break;
          }
          void copy(text).then((how) =>
            setNotice(how === "osc52" ? `Sent ${text.length} chars to the terminal clipboard` : `Copied ${text.length} chars`),
          );
          break;
        }
        case "scroll":
          break; // the highlight is attached to the text, so it scrolls along
      }
    };
    mouse.on("event", onMouse);
    return () => {
      mouse.off("event", onMouse);
    };
  }, [copy, openView]);

  // Typing anything clears the highlight, like in a terminal.
  useInput(() => selection.clear(), { isActive: phase === "main" });

  // Save once a turn is over (not mid-run), shortly after things settle. The session saves after each turn too;
  // this also catches what Marv prints between turns (/cost, /memory…), which is part of the transcript.
  useEffect(() => {
    if (!sessions || busy) return;
    const timer = setTimeout(() => void session.save().catch(() => {}), 200);
    return () => clearTimeout(timer);
  }, [sessions, busy, items, totals, session]);

  // On the way out (cli.tsx awaits this before exiting): quitting cancels the timers above, which could lose the
  // last turn, so save now, and let pending trajectory records reach the disk.
  useEffect(() => {
    onFlush?.(() => session.flush());
  }, [onFlush, session]);

  const WORKING = "Marv is working: stop the current turn (Esc) before resuming another session.";

  /** Shows a session the session object has just resumed: its transcript, its cost, and where it left off. */
  const showResumed = useCallback(
    (resumed: Resumed) => {
      selection.reset();
      agentLogs.current.clear();
      setViewing(null);
      setPlan(resumed.plan);
      // Never lower it: an update still on its way for a message of this process must not hit a restored one.
      nextId.current = Math.max(nextId.current, Math.max(0, ...resumed.transcript.map((m) => m.id)) + 1);
      syncUsage();
      const switched = resumed.model !== configRef.current.model ? ` (it used ${resumed.model}; continuing with ${configRef.current.model})` : "";
      setItems([
        { kind: "welcome", id: "welcome-0" },
        ...resumed.transcript.map((message) => ({ kind: "message" as const, id: `msg-${message.id}`, message })),
      ]);
      addMessage({ role: "system", text: `Resumed a session from ${timeAgo(resumed.updatedAt)}${switched}.` });
      setFollowKey((n) => n + 1);
    },
    [addMessage, syncUsage],
  );

  /** Swaps in a saved session. Never into a running turn: its reply would land in the other conversation, and be saved there. */
  const resumeSession = useCallback(
    async (id: string | "latest"): Promise<Resumed | null | "busy"> => {
      if (abortRef.current || session.busy) return "busy";
      try {
        return await session.resume(id);
      } catch (err) {
        if (session.busy) return "busy"; // a turn started while it loaded
        throw err;
      }
    },
    [session],
  );

  const openPicker = useCallback(async () => {
    if (!sessions) return;
    let list: SessionSummary[];
    try {
      list = (await sessions.list(root)).filter((s) => s.id !== session.id);
    } catch (err) {
      addMessage({ role: "system", isError: true, text: `Couldn't list the saved sessions: ${(err as Error).message}` });
      return;
    }
    // A turn started while the list loaded: don't put the picker over it.
    if (abortRef.current) addMessage({ role: "system", isError: true, text: WORKING });
    else if (list.length === 0) addMessage({ role: "system", text: "No saved sessions for this project yet." });
    else setPicker(list);
  }, [sessions, root, session, addMessage]);

  const pickSession = useCallback(
    async (id: string) => {
      setPicker(null);
      let resumed: Resumed | null | "busy";
      try {
        resumed = await resumeSession(id);
      } catch (err) {
        addMessage({ role: "system", isError: true, text: `That session couldn't be loaded: ${(err as Error).message}.` });
        return;
      }
      if (resumed === "busy") addMessage({ role: "system", isError: true, text: WORKING });
      else if (resumed) showResumed(resumed);
      else addMessage({ role: "system", isError: true, text: "That session couldn't be loaded." });
    },
    [resumeSession, showResumed, addMessage],
  );

  // marv -c / marv -r
  const resumed = useRef(false);
  useEffect(() => {
    if (resumed.current || !resume || !sessions || phase !== "main") return;
    resumed.current = true;
    if (resume === "pick") void openPicker();
    else {
      // Until it's loaded, the prompt keeps what's typed but doesn't send it (handleSubmit): a message sent now would
      // be swapped out by the restore.
      setLoadingSession(true);
      void resumeSession("latest")
        .then((latest) =>
          latest === "busy"
            ? addMessage({ role: "system", isError: true, text: WORKING })
            : latest
              ? showResumed(latest)
              : addMessage({ role: "system", text: "No saved session to continue in this project." }),
        )
        .catch((err: Error) => addMessage({ role: "system", isError: true, text: `Couldn't load the last session: ${err.message}` }))
        .finally(() => setLoadingSession(false));
    }
  }, [resume, sessions, phase, openPicker, resumeSession, showResumed, addMessage]);

  // Startup problems known at once (skills, PATH, AGENTS.md cut, MCP config, agents) become ONE notice: a short
  // headline per problem, one bullet each, the title saying how many. They never reach the model's conversation.
  useEffect(() => {
    const notes: { tone: NoticeTone; item: NoticeItem }[] = [];
    if (skillProblems.length) {
      const count = skillProblems.length;
      notes.push({ tone: "error", item: { text: `${count} skill${count === 1 ? "" : "s"} couldn't be loaded (see /skills)`, detail: skillProblems } });
    }
    if (hiddenPathFolders.length) notes.push({ tone: "warning", item: hiddenPathFoldersItem(hiddenPathFolders) });
    if (instructionsCut) notes.push({ tone: "info", item: instructionsCutItem(instructionsCut) });
    if (mcpProblems.length) {
      const count = mcpProblems.length;
      notes.push({ tone: "error", item: { text: `${count} MCP config problem${count === 1 ? "" : "s"} (see /mcp)`, detail: mcpProblems } });
    }
    if (agentProblems.length) {
      const count = agentProblems.length;
      notes.push({ tone: "error", item: { text: `${count} agent file${count === 1 ? "" : "s"} couldn't be loaded (see /agents)`, detail: agentProblems } });
    }
    const notice = startupNotice(notes);
    if (notice) addMessage({ role: "system", text: "", notice });
  }, [skillProblems, hiddenPathFolders, instructionsCut, mcpProblems, agentProblems, addMessage]);

  // MCP: once the servers have started, a block for any that failed, and a project's servers waiting to be
  // trusted (they don't start until then). A separate notice, since it isn't known at startup.
  useEffect(() => {
    if (!mcp) return;
    let cancelled = false;
    void mcp.ready.then(() => {
      if (cancelled) return;
      const status = mcp.status();
      const failed = status.filter((s) => s.state === "failed");
      const waiting = status.filter((s) => s.state === "untrusted");
      const login = status.filter((s) => s.state === "needs-login");
      for (const s of login) toldNeedsLogin.current.add(s.name);
      // From now on, a server whose login runs out mid-session (a refresh refused) is told once.
      mcp.onChange = () => {
        for (const s of mcp.status()) {
          if (s.state === "needs-login" && !toldNeedsLogin.current.has(s.name)) {
            toldNeedsLogin.current.add(s.name);
            addMessage({ role: "system", text: "", notice: { title: "MCP", tone: "warning", items: [{ text: `${s.name} needs a new login: /mcp login ${s.name}`, detail: ["Its tools fail until then."] }] } });
          } else if (s.state === "connected") {
            toldNeedsLogin.current.delete(s.name);
          }
        }
      };
      const count = failed.length + waiting.length + login.length;
      if (count === 0) return;
      const notes: NoticeItem[] = [];
      if (failed.length) notes.push({ text: `${failed.length} MCP server${failed.length === 1 ? "" : "s"} failed to start (see /mcp)`, detail: failed.map((s) => `${s.name}: ${s.error}`) });
      if (waiting.length)
        notes.push({
          text: `This project's .mcp.json lists ${waiting.length} MCP server${waiting.length === 1 ? "" : "s"} you haven't trusted yet`,
          // The trust decision is the security gate (a cloned repository's .mcp.json could run anything): say what's at stake.
          detail: [...waiting.map((s) => describeUntrusted(s)), "They'd run with your permissions, outside the sandbox. /mcp trust starts them."],
        });
      if (login.length)
        notes.push({
          text: `${login.length} MCP server${login.length === 1 ? " needs" : "s need"} a login`,
          detail: login.map((s) => `${s.name}: /mcp login ${s.name}`),
        });
      addMessage({ role: "system", text: "", notice: { title: `MCP · ${count} note${count === 1 ? "" : "s"}`, tone: failed.length ? "error" : "warning", items: notes } });
    });
    return () => {
      cancelled = true;
      mcp.onChange = () => {};
    };
  }, [mcp, addMessage]);

  const finishSplash = useCallback(() => setPhase("main"), []);

  // Marv runs in the alternate screen (see cli.tsx), so the root fills the
  // terminal: the transcript takes the leftover height and scrolls, and the
  // prompt and status bar stay pinned to the bottom.
  if (phase === "splash") {
    return (
      <Box height={rows} width={columns} justifyContent="center" alignItems="center">
        <Splash version={version} cwd={cwd} durationMs={splashMs} onDone={finishSplash} />
      </Box>
    );
  }

  const view = viewing ? agentLogs.current.get(viewing) : undefined;

  return (
    <Box flexDirection="column" height={rows} width={columns}>
      <ScrollView followKey={followKey} isActive={setupMode === null} onViewport={selection.setViewport} hidden={view !== undefined}>
        <Transcript
          items={items}
          version={version}
          cwd={cwd}
          instructions={Boolean(instructions)}
          skills={skills.length}
          memories={memories ? memories.personal.length + memories.project.length : 0}
          mcpServers={mcp?.status().length ?? 0}
          showSteps={showSteps}
          onAgentRef={onAgentRef}
        />

        {streaming !== null &&
          (streaming === "" ? (
            toolsRunning === 0 && !draft && <ThinkingView key={compacting ? "compact" : (waitingFor ?? "thinking")} thought={thinking} label={compacting ? "Compacting the conversation…" : (waitingFor ?? undefined)} />
          ) : (
            <MessageView message={{ role: "assistant", text: streaming }} streaming />
          ))}
        {streaming !== null && draft && toolsRunning === 0 && <DraftView name={draft.name} args={draft.args} />}
      </ScrollView>

      {view && <AgentViewHeader log={view} />}
      {view && (
        <ScrollView key={viewing} isActive={setupMode === null} onViewport={selection.setViewport}>
          <AgentView log={view} version={logVersion} showSteps={showSteps} />
        </ScrollView>
      )}

      {rows >= PLAN_MIN_WINDOW && <PlanView items={plan} width={columns} />}

      <Box flexDirection="column" flexShrink={0}>
        {picker && !setupMode && !approval ? (
          <SessionPicker sessions={picker} onPick={(id) => void pickSession(id)} onCancel={() => setPicker(null)} />
        ) : approval && !setupMode ? (
          <>
            <Approval
              key={approval.head.id}
              request={approval.head.request}
              waiting={approval.waiting}
              onDecide={decide}
              onCancel={cancelAll}
              escapeCancels={!view}
            />
            <StatusBar model={session.providerName} cwd={cwd} confirmExit={false} notice={notice} busy agents={agentsRunning} yolo={config.yolo} viewing={Boolean(view)} />
          </>
        ) : setupMode ? (
          <Setup
            initial={file}
            env={env}
            loadModels={loadModelsFor}
            startStep={setupMode === "model" ? "model" : "provider"}
            onComplete={completeSetup}
            onCancel={cancelSetup}
          />
        ) : (
          <>
            <PromptInput
              value={input}
              onChange={setInput}
              onSubmit={handleSubmit}
              history={history}
              busy={busy}
              commands={menu}
            />
            <StatusBar
              model={session.providerName}
              cwd={cwd}
              usage={[usage && formatUsage(usage, contextLength), costText(totals)].filter(Boolean).join(" · ") || undefined}
              confirmExit={confirmExit}
              notice={notice}
              busy={busy}
              agents={agentsRunning}
              yolo={config.yolo}
              viewing={Boolean(view)}
            />
          </>
        )}
      </Box>
    </Box>
  );
}
