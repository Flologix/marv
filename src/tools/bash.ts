import { lstatSync, readFileSync, readlinkSync, realpathSync } from "node:fs";
import { rmdir } from "node:fs/promises";
import { homedir } from "node:os";
import { dirname, isAbsolute, join, relative, resolve } from "node:path";
import { z } from "zod";
import { sandboxArgs, sandboxAvailable, TOOLCHAINS } from "../sandbox.ts";
import { repairTimeoutSeconds } from "./repair.ts";
import { ToolError, type Tool } from "./types.ts";

const DEFAULT_TIMEOUT_S = 120;
const isInside = (outer: string, inner: string) => {
  const rel = relative(outer, inner);
  return rel === "" || (!rel.startsWith("..") && !isAbsolute(rel));
};

/**
 * How a command that runs without asking is kept away from the project's .git: read-only if it's there, an
 * empty read-only placeholder if it isn't (so it can't create one). bwrap can't mount on a symlink, so for a .git
 * that is one, the same goes for what it points to, when that's inside the project; outside it, the sandbox
 * already shows it read-only (or hides it, in the home folder).
 *
 * In a git worktree, .git is a file (`gitdir: <repo>/.git/worktrees/<name>`) and the repository's data lives
 * outside the project, usually in the hidden home folder, so git found nothing. Both the worktree's own folder
 * and the repository's common .git are shown read-only, and the .git file too, so a command can't repoint it.
 * The project's files say where those are, so they're only trusted when they check out the way git checks
 * them: the worktree folder sits in `<common>/worktrees/`, and its `gitdir` file points back at this .git.
 * Otherwise a planted `gitdir: /home/me/.ssh` would show that folder to every command that runs without asking.
 */
function gitGuard(root: string): { readOnly?: string[]; placeholders?: string[] } {
  const gitDir = join(root, ".git");
  let stat: ReturnType<typeof lstatSync>;
  try {
    stat = lstatSync(gitDir);
  } catch {
    return { placeholders: [gitDir] };
  }
  if (stat.isSymbolicLink()) {
    const target = resolve(dirname(gitDir), readlinkSync(gitDir));
    const realRoot = realpathSync(root);
    let real: string;
    try {
      real = realpathSync(target);
    } catch {
      // It leads nowhere yet: inside the project, keep that spot empty too.
      return isInside(realRoot, target) || isInside(root, target) ? { placeholders: [target] } : {};
    }
    return isInside(realRoot, real) ? { readOnly: [real] } : {};
  }
  if (stat.isFile()) return { readOnly: [gitDir, ...worktreeGitDirs(gitDir)] };
  return { readOnly: [gitDir] };
}

/** The real path, or the path itself when it doesn't resolve. */
function realOrSelf(path: string): string {
  try {
    return realpathSync(path);
  } catch {
    return path;
  }
}

/** For a project whose .git is a worktree's file: its trusted git folders (see worktreeGitDirs); else none. */
export function worktreeGit(root: string): string[] {
  const gitFile = join(root, ".git");
  try {
    return lstatSync(gitFile).isFile() ? worktreeGitDirs(gitFile) : [];
  } catch {
    return [];
  }
}

/** A worktree's own git folder and the repository's common .git, when its .git file checks out; else none. */
function worktreeGitDirs(gitFile: string): string[] {
  try {
    const match = /^gitdir:\s*(.+?)\s*$/m.exec(readFileSync(gitFile, "utf8"));
    if (!match) return [];
    const adminDir = realpathSync(resolve(dirname(gitFile), match[1]!));
    // git's own check: the worktree's record points back at this .git file.
    const back = readFileSync(join(adminDir, "gitdir"), "utf8").trim();
    if (realpathSync(resolve(adminDir, back)) !== realpathSync(gitFile)) return [];
    const commonDir = realpathSync(resolve(adminDir, readFileSync(join(adminDir, "commondir"), "utf8").trim()));
    // And it's one of that repository's worktrees.
    if (dirname(adminDir) !== join(commonDir, "worktrees")) return [];
    return [adminDir, commonDir];
  } catch {
    return []; // Missing or unreadable records: show nothing more (git fails, as before).
  }
}

/** Appended when a confined command fails on the read-only .git, so the model learns how to do it. */
const GIT_HINT = "Marv: .git is read-only for commands that run without asking. To change the repository, run the command again with git_write: true (the user is asked).";
/**
 * Appended when a sandboxed command looks into the hidden home folder. The prompt said "hidden", but `ls ~/.marv`
 * showed skills/ (mounted back in), so a model decided home was readable, took the missing config.json and
 * trajectories for real, and searched for them for 21 steps (trajectory, 2026-10-05).
 */
const HOME_NOTE =
  "Marv: in the sandbox the home folder is an empty stand-in: only toolchain folders (like ~/.bun, ~/.local/share/mise, ~/.asdf), git config and ~/.marv/skills show through, read-only. " +
  "Anything else there (Marv's own config, sessions and trajectories in ~/.marv included) is invisible, so a missing file or an empty folder under ~ says nothing about the real home folder.";
/**
 * Appended when a sandboxed command leaves processes running in the background: each command runs
 * in its own PID namespace, so everything it started is killed when it ends, and (without network)
 * its localhost isn't the user's anyway. A model started a dev server detached, curled it
 * successfully inside the sandbox, and told the user "the server's up" with a dead link (issue #50).
 */
const BACKGROUND_NOTE =
  "Marv: anything this command started in the background was stopped when the command ended, and a server in the sandbox can't be reached from the user's browser (the sandbox has its own network namespace; with network: true its localhost is the host's). " +
  "Don't say a server is running for the user: to check one yourself, run it in the foreground within one command, query it there, and let the command end; to let the user try it, give them the command to run in their own terminal.";

const WORD_START = String.raw`(?:^|[\s=:'"(<>;|&])`;
/**
 * Whether a command leaves processes running in the background, as far as its text can say: a
 * background `&` (an unquoted one, outside `&&`), `nohup`/`setsid`/`disown`, or a command whose
 * very job is to keep serving (`dev`, `start`, `serve` — as the command or a `run`/package-runner
 * script, not as an argument like `echo start`). A guess, for the note below only.
 */
export function backgroundsProcess(command: string): boolean {
  const clean = command
    .replace(/"(?:[^"\\]|\\.)*"|'(?:[^'\\]|\\.)*'/g, " ") // quoted text, including any & in it
    .replace(/(^|\s)#[^\n]*/g, "$1") // comments
    .replace(/&&/g, " ")
    .replace(/(\d)>&(\d)/g, "$1=$2"); // 2>&1 isn't backgrounding
  if (/(?:^|[\s;|])&(?=$|[\s#])/.test(clean)) return true;
  return clean
    .split(/[;|\n&]/)
    .some((segment) => {
      const words = segment.trim().split(/\s+/).filter(Boolean);
      if (!words.length) return false;
      const [head, second, third] = words;
      if (head === undefined) return false;
      if (/^(?:nohup|setsid|disown)$/.test(head)) return true;
      if (/^(?:dev|start|serve)$/.test(head)) return true;
      if (/^(?:npm|pnpm|yarn|bun|npx|bunx)$/.test(head)) {
        if (second === "run") return /^(?:dev|start|serve)$/.test(third ?? "");
        return /^(?:dev|start|serve)$/.test(second ?? "");
      }
      return false;
    });
}
const WORD_END = String.raw`(?=$|[/\s'");|&<>])`;
const PATH_REST = String.raw`([^\s'";|&<>()]*)`;
const escapeRegex = (text: string) => text.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
/** `~`, `$HOME` or the home folder's path, at the start of a word, and the path after it. */
const homeRefs = (home: string) =>
  new RegExp(`${WORD_START}(?:~|\\$HOME|\\$\\{HOME\\}|${escapeRegex(home)})${WORD_END}${PATH_REST}`, "g");

/**
 * Whether a command refers to a part of the home folder the sandbox hides: not the project (often inside the home
 * folder) and not the toolchain folders it mounts. A guess from the command's text, for the note above only.
 */
export function touchesHiddenHome(command: string, root: string, home: string): boolean {
  for (const [, rest = ""] of command.matchAll(homeRefs(home))) {
    const path = join(home, rest);
    if (!isInside(root, path) && !TOOLCHAINS.some((dir) => isInside(join(home, dir), path))) return true;
  }
  return false;
}
/**
 * Appended when a sandboxed command without network fails in a way that looks like the network's missing. After a
 * fresh `gh auth login`, `gh auth status` said the token was invalid, and the model walked the user through logging
 * out and in again; with network on it worked straight away (issue #23).
 */
const NETWORK_NOTE =
  "Marv: this command ran in the sandbox without network, and its failure looks like it may come from that: there, every connection and DNS lookup fails, so logins look invalid and servers look down. " +
  "Before drawing conclusions, run it again with network: true.";
/**
 * Appended to the note above when the failed command is `gh` (issue #27): the GitHub MCP server is the way to use
 * GitHub, and the failure still isn't proof the login is stale, so "gh auth login" stays the wrong advice. Where the
 * session has the github tool (#47), that's the way instead (GH_TOOL_NOTE).
 */
export const GH_NOTE =
  "For GitHub, don't suggest gh auth login, and never mount gh's config into the sandbox (a token there must not reach command output): suggest the GitHub MCP server instead, which the user connects in ~/.marv/mcp.json — see Marv's README, \"Connect GitHub\".";
/** GH_NOTE where the session has the github tool: gh with the user's login is one call away. */
export const GH_TOOL_NOTE =
  "For GitHub, use the github tool instead: it runs gh with the user's own login, outside the sandbox (in bash gh has no login, and that's on purpose). Don't suggest gh auth login, and never mount gh's config into the sandbox.";
/** Errors a blocked network produces: DNS, refused or unreachable connections, timeouts. */
const NETWORK_ERRORS =
  /could not resolve (?:host|hostname|proxy)|temporary failure in name resolution|name or service not known|getaddrinfo|\b(?:ENOTFOUND|EAI_AGAIN|ECONNREFUSED|ECONNRESET|ENETUNREACH|EHOSTUNREACH|ETIMEDOUT|ETIMEOUT)\b|ConnectionRefused|network is unreachable|connection refused|connection timed out|failed to connect|unable to connect|dns error|no such host|error connecting to/i;
/** Commands that need the network to do their job. */
const NETWORK_COMMANDS =
  /(?:^|[\s;&|(`])(?:gh|curl|wget|ssh|scp|rsync|ping|git\s+(?:fetch|pull|push|clone|ls-remote|submodule\s+update)|(?:npm|pnpm|yarn|bun)\s+(?:install|i|add|ci|update|upgrade|publish|view|info|outdated|create)|pip3?\s+install|uv\s+(?:sync|add|pip\s+install)|cargo\s+(?:install|fetch|update|add)|go\s+(?:get|mod\s+download)|docker\s+(?:pull|push|login))(?=$|[\s;&|)`])/;

/**
 * Whether a failed command's failure may come from the sandbox's missing network: its output has a network error,
 * or the command is one that needs the network. A guess, for the note above only.
 */
export function looksLikeNetworkFailure(command: string, output: string): boolean {
  return NETWORK_ERRORS.test(output) || NETWORK_COMMANDS.test(command);
}
const MAX_TIMEOUT_S = 600;
/** Kept in memory per command; anything beyond is dropped (the middle is cut for the model anyway). */
export const MAX_CAPTURE = 2_000_000;
/** What the model sees: the start and the end of long output. */
const MAX_MODEL_CHARS = 30_000;
const HEAD_CHARS = 10_000;

export interface CommandResult {
  output: string;
  /** null when the command was killed. */
  exitCode: number | null;
  timedOut: boolean;
  aborted: boolean;
  timeoutMs?: number;
}

interface RunOptions {
  command: string;
  root: string;
  sandbox: boolean;
  network: boolean;
  timeoutMs: number;
  signal?: AbortSignal;
  readOnly?: string[];
  writable?: string[];
  placeholders?: string[];
  /** Mount the project read-only (see SandboxOptions.projectReadOnly). */
  projectReadOnly?: boolean;
  /** The home folder the sandbox hides (default: the user's); tests pass a fake one. */
  home?: string;
  /** Not sandboxed: this environment instead of the minimal one (the host door's, hostEnv). */
  env?: Record<string, string>;
}

const hasSetsid = Bun.which("setsid") !== null;

/**
 * Runs a shell command in the project folder with stdout and stderr combined.
 * Sandboxed: inside bwrap (see src/sandbox.ts). Not sandboxed: still with a
 * minimal environment, so API keys don't leak into commands.
 */
export async function runCommand({ command, root, sandbox, network, timeoutMs, signal, readOnly, writable, placeholders, projectReadOnly, home = homedir(), env }: RunOptions): Promise<CommandResult> {
  const path = process.env.PATH ?? "/usr/bin:/bin";
  const script = `exec 2>&1\n${command}`; // stderr into stdout, so the output stays in order
  const argv = sandbox
    ? ["bwrap", ...sandboxArgs({ root, home, network, path, readOnly, writable, placeholders, projectReadOnly }), "--", "bash", "-c", script]
    : // setsid: its own process group, so killing it also kills everything it started.
      [...(hasSetsid ? ["setsid"] : []), "bash", "-c", script];

  const proc = Bun.spawn(argv, {
    cwd: root,
    stdin: "ignore",
    stdout: "pipe",
    // The command's own stderr goes to stdout (`exec 2>&1`), so only what comes before it lands here:
    // bwrap's own errors, e.g. when it can't set the sandbox up.
    stderr: "pipe",
    env: sandbox ? { PATH: path } : (env ?? { PATH: path, HOME: home, LANG: process.env.LANG ?? "C.UTF-8", TERM: "dumb" }),
  });

  let timedOut = false;
  let aborted = false;
  const kill = () => {
    try {
      if (!sandbox && hasSetsid) process.kill(-proc.pid, "SIGKILL");
      else proc.kill("SIGKILL"); // bwrap takes its whole sandbox down with it
    } catch {
      // already gone
    }
  };
  const timer = setTimeout(() => {
    timedOut = true;
    kill();
  }, timeoutMs);
  const onAbort = () => {
    aborted = true;
    kill();
  };
  signal?.addEventListener("abort", onAbort, { once: true });
  if (signal?.aborted) onAbort();

  // Both at once, so neither pipe fills up and blocks the other.
  let [output, errors] = await Promise.all([capture(proc.stdout), capture(proc.stderr)]);
  await proc.exited;
  clearTimeout(timer);
  signal?.removeEventListener("abort", onAbort);

  const exitCode = timedOut || aborted ? null : proc.exitCode;
  // Without this, a sandbox that never started looks like a command that silently failed.
  if (sandbox && exitCode !== 0 && exitCode !== null && output.trim() === "" && errors.trim() !== "") {
    output = `[sandbox failed to start: ${errors.trim()}]\n`;
  }
  return { output, exitCode, timedOut, aborted, timeoutMs };
}

/** A stream's text, up to MAX_CAPTURE characters (the rest is read and dropped). */
export async function capture(stream: ReadableStream<Uint8Array>): Promise<string> {
  let text = "";
  const decoder = new TextDecoder();
  for await (const chunk of stream) {
    if (text.length < MAX_CAPTURE) text += decoder.decode(chunk, { stream: true });
  }
  return text;
}

/** The command's result as the model sees it: output (start and end if long), then how it ended. */
export function formatOutput({ output, exitCode, timedOut, aborted, timeoutMs }: CommandResult): string {
  let text = output.trim() === "" ? "(no output)" : output.trimEnd();
  if (text.length > MAX_MODEL_CHARS) {
    const tail = MAX_MODEL_CHARS - HEAD_CHARS;
    text = `${text.slice(0, HEAD_CHARS)}\n\n… (${text.length - MAX_MODEL_CHARS} characters omitted) …\n\n${text.slice(-tail)}`;
  }
  const ending = timedOut
    ? `[timed out after ${Math.round((timeoutMs ?? 0) / 1000)}s]`
    : aborted
      ? "[stopped by the user]"
      : `[exit code ${exitCode}]`;
  return `${text}\n\n${ending}`;
}

const input = z.object({
  command: z.string().min(1).describe("The shell command (bash). Runs in the project folder."),
  timeout: z.number().int().min(1).max(MAX_TIMEOUT_S).optional().describe(`Seconds before it's stopped. Default ${DEFAULT_TIMEOUT_S}.`),
  network: z.boolean().optional().describe("Allow network access (e.g. to install packages). Off by default."),
  git_write: z
    .boolean()
    .optional()
    .describe("Set when the command changes the git repository (commit, checkout, merge, rebase, stash, branch, tag, init…). Otherwise .git may be read-only."),
  // In the spec for every session (one bash spec: the prompt cache); refused in preview where the door is closed.
  sandbox: z
    .boolean()
    .optional()
    .describe(
      "false runs the command on the user's machine, outside the sandbox, with their permissions; always asks. Only for what can't work in the sandbox (a system service, a tool under the home folder, hardware), after a sandboxed attempt failed.",
    ),
});

/** Said when a call asks for the host where no one approves each command (marv -p, the SDK without hostCommands, subagents). */
export const NO_HOST_DOOR = "Commands outside the sandbox can only run in Marv's terminal, with the user approving each one.";
/** Added to the network and hidden-home notes where the host door is open: the last resort, which asks. */
const HOST_LAST_RESORT = "If this can't work in the sandbox at all, bash with sandbox: false asks the user to run it on their machine.";

/**
 * Whether a call goes through the host door: it says sandbox: false where the sandbox is on and works. With the
 * sandbox already off (/sandbox off, or no bubblewrap) the flag changes nothing: the call is an ordinary one.
 */
const throughHost = (sandboxFlag: boolean | undefined, sandbox = true) => sandboxFlag === false && sandbox && sandboxAvailable();

/**
 * The environment of a command on the host: the user's own (systemctl --user needs XDG_RUNTIME_DIR and D-Bus, a
 * tool under the home folder its config), minus Marv's provider keys.
 */
export function hostEnv(env: NodeJS.ProcessEnv = process.env): Record<string, string> {
  const kept: Record<string, string> = {};
  for (const [name, value] of Object.entries(env)) {
    if (value === undefined || name === "OPENROUTER_API_KEY" || name.startsWith("MARV_")) continue;
    kept[name] = value;
  }
  return kept;
}

export const bash: Tool<typeof input> = {
  name: "bash",
  description:
    "Run a shell command in the project folder, e.g. to run tests, a build, git status, or wc -l. " +
    "It runs in a sandbox: only the project folder is writable, the home folder is an empty stand-in (toolchains like ~/.bun, ~/.local/share/mise, ~/.asdf, git config and ~/.marv/skills show through, read-only; nothing else in it is visible), " +
    "and there's no network unless you set network: true. The user may be asked to approve a command; they always are for network: true or git_write: true. " +
    "Not interactive: commands can't prompt for input. " +
    // A model that wrote files with heredocs put the terminator on the same line as `&& ...`, read bash's warning
    // as "truncation", and retried for 10 steps (trajectory, 2026-10-05). The file tools have no such pitfall.
    "Don't write files with it (no heredocs or echo >): use write_file or edit_file. " +
    "Output (stdout and stderr together) is cut in the middle if very long.",
  input,
  kind: "execute",
  label: ({ command }) => command,
  // On the host, network and git_write mean nothing (it has both): one key, never remembered (`once`).
  scope: ({ command, network, git_write, sandbox: flag }, { sandbox }) =>
    throughHost(flag, sandbox)
      ? { key: `bash:host:${command}`, description: "this exact command on your machine" }
      : { key: `bash:${network ? "net:" : ""}${git_write ? "git:" : ""}${command}`, description: "this exact command" },
  usesNetwork: ({ network, sandbox: flag }, { sandbox }) => Boolean(network) || throughHost(flag, sandbox),
  door: ({ sandbox: flag }, { sandbox }) => (throughHost(flag, sandbox) ? "host" : undefined),
  once: ({ sandbox: flag }, { sandbox }) => throughHost(flag, sandbox),
  // In the sandbox, without network, and (as runTool confines it) with .git read-only, a command can only change the
  // project's own files. Without the sandbox, nothing limits it; on the host, everything is in reach.
  autoSafe: ({ network, git_write, sandbox: flag }, { sandbox = true }) => sandbox && sandboxAvailable() && !network && !git_write && flag !== false,
  // A timeout given in milliseconds (a whole multiple of 1000, above the
  // maximum) was meant in seconds: divided and clamped (issue #10). Anything
  // else could still be seconds, and fails with the ordinary error.
  repair: (raw) => {
    if (typeof raw !== "object" || raw === null) return null;
    const { timeout } = raw as { timeout?: unknown };
    if (typeof timeout !== "number") return null;
    const fixed = repairTimeoutSeconds(timeout, MAX_TIMEOUT_S);
    if (!fixed.note) return null;
    return { input: { ...raw, timeout: fixed.seconds }, note: fixed.note };
  },

  async preview({ command, network, timeout, sandbox: flag }, { sandbox = true, doors }) {
    if (throughHost(flag, sandbox)) {
      // Refused before any ask where no one approves each command: an approver can be told to say yes to an exact
      // scope key (marv -p --approve), so the door is the context's to open, not the approver's.
      if (!doors?.host) throw new ToolError(NO_HOST_DOOR);
      // The warning line is the door's (ApprovalRequest.door), so it's the same wherever the request is shown.
      return { title: "Run a command on your machine", command, note: `asked every time${timeout ? ` · timeout ${timeout}s` : ""}` };
    }
    const sandboxed = sandbox && sandboxAvailable();
    const notes = [sandboxed && "sandboxed", sandboxed && (network ? "network allowed" : "no network"), timeout && `timeout ${timeout}s`];
    return {
      title: "Run a command",
      command,
      note: notes.filter(Boolean).join(" · ") || undefined,
      warning: sandboxed
        ? undefined
        : sandbox
          ? "bubblewrap isn't available here, so this runs WITHOUT a sandbox"
          : "the sandbox is off (/sandbox on to turn it back on), so this runs WITHOUT a sandbox",
    };
  },

  async run({ command, network = false, timeout = DEFAULT_TIMEOUT_S, sandbox: flag }, { root, signal, sandbox = true, readOnly = [], confined, doors }) {
    if (throughHost(flag, sandbox)) {
      // Checked again here, so no path to run() skips the door (preview refuses first).
      if (!doors?.host) throw new ToolError(NO_HOST_DOOR);
      const result = await runCommand({ command, root, sandbox: false, network: true, timeoutMs: timeout * 1000, signal, env: hostEnv() });
      return { output: formatOutput(result), summary: `${commandSummary(result, timeout)} · on your machine` };
    }
    // A command that runs without asking sees .git read-only: git runs hooks and config from it outside any sandbox
    // (the user's next commit, their editor), so a planted hook would escape. Without a .git, it can't create one
    // either (an empty read-only placeholder sits there), or `git status` in that folder would run its config.
    const guard = confined ? gitGuard(root) : {};
    // An approved command (yolo off, git_write, network) in a git worktree: it may change the repository like any
    // approved command may change a .git inside the project, but it has to see it first. The same check as above
    // decides whether the worktree's records are trusted.
    // A folder the caller made read-only stays that way: a worktree subagent's shared .git is read-only even for
    // its approved commands (no commits, hooks or config from a subagent; src/worktree.ts).
    const kept = readOnly.map((dir) => realOrSelf(dir));
    const approvedGit = confined ? [] : worktreeGit(root).filter((dir) => !kept.some((ro) => isInside(ro, realOrSelf(dir))));
    const sandboxed = sandbox && sandboxAvailable();
    let result: CommandResult;
    try {
      result = await runCommand({
        command,
        root,
        sandbox: sandboxed,
        network,
        timeoutMs: timeout * 1000,
        signal,
        readOnly: [...readOnly, ...(guard.readOnly ?? [])],
        writable: approvedGit,
        placeholders: guard.placeholders,
      });
    } finally {
      // bwrap leaves a placeholder's empty mount point; rmdir only removes it while it's empty.
      for (const path of guard.placeholders ?? []) await rmdir(path).catch(() => {});
    }
    if (confined && /\.git\b.*Read-only file system/.test(result.output)) result.output += `\n${GIT_HINT}\n`;
    const summary = commandSummary(result, timeout);
    const lastResort = doors?.host ? ` ${HOST_LAST_RESORT}` : "";
    if (sandboxed && touchesHiddenHome(command, root, homedir())) result.output += `\n${HOME_NOTE}${lastResort}\n`;
    if (sandboxed && backgroundsProcess(command)) result.output += `\n${BACKGROUND_NOTE}\n`;
    // Not for a command that isn't there (127) or can't run (126): the network has nothing to do with those.
    const failed = result.timedOut || (!result.aborted && result.exitCode !== 0 && result.exitCode !== 126 && result.exitCode !== 127);
    if (sandboxed && !network && failed && looksLikeNetworkFailure(command, result.output)) {
      result.output += `\n${NETWORK_NOTE}${lastResort}\n`;
      // A gh failure usually isn't the network, and the missing network isn't the fix: the MCP server is (#27).
      // gh is a NETWORK_COMMAND, so the note above is already there for it; this adds what to do instead.
      if (/(?:^|[\s;&|(`])gh(?=$|[\s;&|)`])/.test(command)) result.output += `\n${doors?.github ? GH_TOOL_NOTE : GH_NOTE}\n`;
    }
    return { output: formatOutput(result), summary };
  },
};

/** A few words on how a command ended, for the transcript. */
function commandSummary(result: CommandResult, timeout: number): string {
  const lines = result.output.trimEnd() === "" ? 0 : result.output.trimEnd().split("\n").length;
  return result.timedOut
    ? `timed out after ${timeout}s`
    : result.aborted
      ? "stopped"
      : `exit ${result.exitCode}${lines ? ` · ${lines} line${lines === 1 ? "" : "s"} of output` : ""}`;
}
