// What the github tool lets gh do (#47): pure, so every rule is a table test. gh runs outside the sandbox with the
// user's login, so its argv is read the way gh reads it (the command path first, then flags) and sorted into what
// runs freely (reads), what asks (writes, and reads of sensitive API paths), and what never runs (anything that
// would show the token, run a program, open a browser, run git, write files, or reach a host the project doesn't
// name). Anything not listed is refused, with the list of what runs.

export type Classified =
  | { kind: "read"; command: string }
  /** Changes GitHub (other people see it): asks, "don't ask again" per command. */
  | { kind: "write"; command: string; scope: string }
  /** A read of what's more than "check the project" needs (secret names, deploy keys, webhooks): asks. */
  | { kind: "sensitive"; command: string; scope: string }
  | { kind: "refused"; reason: string };

/** The project's GitHub host (its `origin`'s, or github.com outside a repository); null: unknown, so no host may be named. */
export interface ClassifyContext {
  host: string | null;
  /** Every host passes (for needsApproval, which can't see the project; run() checks the host again). */
  anyHost?: boolean;
}

const READS = [
  "pr list",
  "pr view",
  "pr checks",
  "pr diff",
  "pr status",
  "issue list",
  "issue view",
  "issue status",
  "run list",
  "run view",
  "workflow list",
  "workflow view",
  "repo view",
  "release list",
  "release view",
  "label list",
  "search issues",
  "search prs",
  "search repos",
  "search code",
  "search commits",
  "status",
  "api",
];
const WRITES = [
  "pr comment",
  "pr create",
  "pr edit",
  "pr close",
  "pr reopen",
  "pr merge",
  "pr review",
  "pr ready",
  "issue comment",
  "issue create",
  "issue edit",
  "issue close",
  "issue reopen",
  "release create",
  "release edit",
  "run rerun",
  "run cancel",
  "workflow run",
  "label create",
];
const LISTED = new Set([...READS, ...WRITES]);
/** The first words of every listed command: anything else as the first word could be an alias or an extension. */
const GROUPS = new Set([...LISTED].map((path) => path.split(" ")[0]!));

const RUNS = `It runs: ${READS.join(", ")} (reads), and asks for ${WRITES.join(", ")}.`;
const notRun = (command: string) => `gh ${command} isn't run by the github tool. ${RUNS}`;
const KEEP_TOKEN = "Marv keeps the token away from the conversation.";
const GIT_OUTSIDE =
  "it runs git outside the sandbox, and git runs the project's hooks and config, which commands may have written. To check out a pull request, use bash with git_write: true and network: true: git fetch origin pull/<N>/head:pr-<N>.";

/** Commands that are never run, and why, said so the model learns the rule. */
const NEVER: Record<string, string> = {
  auth: `gh auth … is never run: ${KEEP_TOKEN}`,
  config: `gh config … is never run: gh's settings (and the token it can print) are the user's. ${KEEP_TOKEN}`,
  extension: "gh extension … is never run: an extension is a program, and it would run outside the sandbox.",
  alias: "gh alias … is never run: an alias can run any command, outside the sandbox.",
  secret: "gh secret … is never run: secrets, variables and keys stay out of the conversation.",
  variable: "gh variable … is never run: secrets, variables and keys stay out of the conversation.",
  "ssh-key": "gh ssh-key … is never run: secrets, variables and keys stay out of the conversation.",
  "gpg-key": "gh gpg-key … is never run: secrets, variables and keys stay out of the conversation.",
  "run download": "gh run download is never run: it writes files outside the sandbox. Read a run with gh run view (--log, --log-failed).",
  "release download": "gh release download is never run: it writes files outside the sandbox.",
  "repo clone": `gh repo clone is never run: ${GIT_OUTSIDE}`,
  "pr checkout": `gh pr checkout is never run: ${GIT_OUTSIDE}`,
  // Whole commands gh has that are out of the tool's scope (not secrets): "isn't run", with the list.
  codespace: notRun("codespace"),
  gist: notRun("gist"),
  org: notRun("org"),
  browse: notRun("browse"),
};
const unknown = (command: string) =>
  `gh ${command} isn't something the github tool runs (an unknown word could be an alias or an extension, which run programs). ${RUNS}`;
const WEB = "--web is never run: it opens the user's browser. Read it here instead (view, list, diff, checks).";
const CLUSTER = (token: string) => `Marv can't tell how gh reads ${token}: write its short flags separately (-w is never run, and -R must name the repository on its own).`;

const refused = (reason: string): Classified => ({ kind: "refused", reason });

/** The host an `origin` URL names, as gh reads it (https, ssh://, scp-like git@host:path); null for a local path. */
export function originHost(url: string): string | null {
  const trimmed = url.trim();
  if (!trimmed) return null;
  if (/^[a-z][a-z0-9+.-]*:\/\//i.test(trimmed)) {
    try {
      return new URL(trimmed).hostname.toLowerCase() || null;
    } catch {
      return null;
    }
  }
  const scp = /^(?:[^@/\s]+@)?([^:/\s]+):(?!\/\/)/.exec(trimmed);
  return scp ? scp[1]!.toLowerCase() : null;
}

/** Whether `name` is the project's host (gh compares hosts without case). */
const sameHost = (name: string, ctx: ClassifyContext) => Boolean(ctx.anyHost) || (ctx.host !== null && name.toLowerCase() === ctx.host.toLowerCase());
const otherHost = (name: string, ctx: ClassifyContext) =>
  `It names ${name}, and the github tool only reaches the project's own GitHub host${ctx.host ? ` (${ctx.host})` : " (none is known here)"}.`;

/** A -R/--repo value (or repo view's argument): OWNER/REPO anywhere on the project's host, HOST/OWNER/REPO or a URL only for it. */
function checkRepo(value: string, ctx: ClassifyContext): string | null {
  if (/^[a-z][a-z0-9+.-]*:\/\/|^[^@/\s]+@[^:]+:/i.test(value)) {
    const host = originHost(value);
    return host && sameHost(host, ctx) ? null : otherHost(host ?? value, ctx);
  }
  const parts = value.split("/");
  if (parts.length === 2 && parts.every(Boolean)) return null;
  if (parts.length === 3 && parts.every(Boolean)) return sameHost(parts[0]!, ctx) ? null : otherHost(parts[0]!, ctx);
  return `${value} isn't a repository gh can read (OWNER/REPO, or HOST/OWNER/REPO on the project's host).`;
}

/** A value that is a URL (or scp-like address) names a host: only the project's. */
function checkUrlValue(value: string, ctx: ClassifyContext): string | null {
  if (!/^(?:https?|ssh|git):\/\/|^git@/i.test(value)) return null;
  const host = originHost(value);
  return host && sameHost(host, ctx) ? null : otherHost(host ?? value, ctx);
}

/**
 * The flags after the command path, for every command but api: --web never, -R/--repo and --hostname name the
 * project's host only, a URL anywhere names it too. gh's other flags differ per command and only shape a read or
 * fill in a write the user is asked about, so they pass.
 */
function checkFlags(rest: string[], ctx: ClassifyContext, repoArg: boolean): string | null {
  let positional = false;
  let firstPositional = true;
  for (let i = 0; i < rest.length; i++) {
    const token = rest[i]!;
    if (positional || !token.startsWith("-") || token === "-") {
      const problem = repoArg && firstPositional ? checkRepo(token, ctx) : checkUrlValue(token, ctx);
      firstPositional = false;
      if (problem) return problem;
      continue;
    }
    if (token === "--") {
      positional = true;
      continue;
    }
    if (token.startsWith("--")) {
      const [name, inline] = splitLong(token);
      if (name === "web") return WEB;
      if (name === "repo" || name === "hostname") {
        const value = inline ?? rest[++i];
        if (value === undefined) return `--${name} needs a value.`;
        const problem = name === "repo" ? checkRepo(value, ctx) : sameHost(value, ctx) ? null : otherHost(value, ctx);
        if (problem) return problem;
        continue;
      }
      if (inline !== undefined) {
        const problem = checkUrlValue(inline, ctx);
        if (problem) return problem;
      }
      continue;
    }
    // A short flag: -w, -R value, -Rvalue; a cluster (-fw) could hide either, so it has to be written out.
    const letter = token[1]!;
    if (token.length === 2) {
      if (letter === "w") return WEB;
      if (letter === "R") {
        const value = rest[++i];
        if (value === undefined) return "-R needs a value.";
        const problem = checkRepo(value, ctx);
        if (problem) return problem;
      }
      continue;
    }
    if (letter === "R") {
      const problem = checkRepo(token.slice(2).replace(/^=/, ""), ctx);
      if (problem) return problem;
      continue;
    }
    if (/[wR]/.test(token.slice(1))) return CLUSTER(token);
    const problem = checkUrlValue(token.slice(2), ctx);
    if (problem) return problem;
  }
  return null;
}

/** `--name=value` → [name, value]; `--name` → [name, undefined]. */
function splitLong(token: string): [string, string | undefined] {
  const eq = token.indexOf("=");
  return eq === -1 ? [token.slice(2), undefined] : [token.slice(2, eq), token.slice(eq + 1)];
}

const API_VALUE_FLAGS: Record<string, string> = {
  X: "method",
  f: "raw-field",
  F: "field",
  H: "header",
  q: "jq",
  t: "template",
  p: "preview",
};
const API_LONG_VALUE = new Set(["method", "raw-field", "field", "header", "input", "jq", "template", "hostname", "cache", "preview"]);
const API_BOOL = new Set(["paginate", "slurp", "include", "silent", "verbose"]);
const API_SHORT_BOOL: Record<string, string> = { i: "include" };
/** Headers a read may set: neither changes what the request does (no method overrides, no other credentials). */
const API_HEADERS = new Set(["accept", "x-github-api-version"]);
/** Path segments of API reads that ask: names of secrets and variables, deploy keys, webhooks (their URLs), installations, invitations. */
const SENSITIVE_SEGMENTS = new Set(["secrets", "variables", "keys", "hooks", "installations", "invitations", "authorizations"]);
const API_FLAGS_LISTED = "-X/--method, -f/--raw-field, -F/--field, -H/--header (Accept, X-GitHub-Api-Version), --jq/-q, --template/-t, --paginate, --slurp, -i/--include, --silent, --cache, --hostname";

function classifyApi(rest: string[], ctx: ClassifyContext): Classified {
  let method: string | undefined;
  let fields = false;
  const endpoints: string[] = [];
  let positional = false;
  for (let i = 0; i < rest.length; i++) {
    const token = rest[i]!;
    if (positional || !token.startsWith("-") || token === "-") {
      endpoints.push(token);
      continue;
    }
    if (token === "--") {
      positional = true;
      continue;
    }
    let name: string;
    let value: string | undefined;
    if (token.startsWith("--")) {
      [name, value] = splitLong(token);
      if (API_BOOL.has(name)) {
        if (value !== undefined) return refused(`--${name} takes no value here: write it on its own.`);
      } else if (API_LONG_VALUE.has(name)) {
        value ??= rest[++i];
        if (value === undefined) return refused(`--${name} needs a value.`);
      } else {
        return refused(`--${name} isn't one the github tool passes to gh api. It takes ${API_FLAGS_LISTED}.`);
      }
    } else {
      const letter = token[1]!;
      if (API_SHORT_BOOL[letter] && token.length === 2) {
        name = API_SHORT_BOOL[letter];
      } else if (API_VALUE_FLAGS[letter]) {
        name = API_VALUE_FLAGS[letter];
        value = token.length > 2 ? token.slice(2) : rest[++i];
        if (value === undefined) return refused(`-${letter} needs a value.`);
      } else if (token.length > 2) {
        return refused(CLUSTER(token));
      } else {
        return refused(`${token} isn't one the github tool passes to gh api. It takes ${API_FLAGS_LISTED}.`);
      }
    }
    switch (name) {
      case "input":
        return refused("gh api --input is never run: it reads a file from the user's machine and sends it. Pass the body as -f/-F fields.");
      case "verbose":
        return refused("gh api --verbose is never run: it prints the request, token included.");
      case "preview":
        return refused(`--preview isn't one the github tool passes to gh api. It takes ${API_FLAGS_LISTED}.`);
      case "method":
        method = value!.toUpperCase();
        break;
      case "raw-field":
      case "field":
        fields = true;
        break;
      case "header": {
        const header = value!.split(":")[0]!.trim().toLowerCase();
        if (!API_HEADERS.has(header)) return refused(`gh api's header ${value!.split(":")[0]!.trim()} isn't passed: only Accept and X-GitHub-Api-Version, which don't change what the request does.`);
        break;
      }
      case "hostname":
        if (!sameHost(value!, ctx)) return refused(otherHost(value!, ctx));
        break;
    }
  }
  if (endpoints.length === 0) return refused("gh api needs an endpoint, like repos/{owner}/{repo}/pulls.");
  if (endpoints.length > 1) return refused(`gh api takes one endpoint; got ${endpoints.join(", ")}.`);
  const endpoint = endpoints[0]!;
  const bad = badEndpoint(endpoint);
  if (bad) return refused(bad);
  const path = endpoint.replace(/^\//, "").split("?")[0]!;
  if (path === "graphql") return { kind: "write", command: "api", scope: "github:api graphql" };
  // Any field makes gh send a body (POST, unless -X says otherwise); a GET with fields still sends them somewhere.
  const verb = method ?? (fields ? "POST" : "GET");
  if (verb !== "GET" || fields) return { kind: "write", command: "api", scope: `github:api ${verb}` };
  if (sensitivePath(path)) return { kind: "sensitive", command: "api", scope: "github:api sensitive" };
  return { kind: "read", command: "api" };
}

/** An endpoint must be a plain API path: no other host, no encoding or dot segments that could hide what it names. */
function badEndpoint(endpoint: string): string | null {
  const plain = "gh api's endpoint must be a plain path, like repos/{owner}/{repo}/pulls";
  if (/^[a-z][a-z0-9+.-]*:/i.test(endpoint)) return `${plain}: a URL could name another host.`;
  if (endpoint.startsWith("//")) return `${plain}.`;
  const path = endpoint.split("?")[0]!;
  if (/[%\\\s]/.test(path) || path.split("/").some((segment) => segment === "." || segment === "..")) return `${plain}.`;
  return null;
}

function sensitivePath(path: string): boolean {
  const lower = path.toLowerCase();
  const segments = lower.split("/");
  return segments.some((s) => SENSITIVE_SEGMENTS.has(s)) || lower === "user/emails" || lower.includes("actions/runners");
}

/** Sorts a gh argv (without "gh") into read, write, sensitive or refused. */
export function classify(args: readonly string[], ctx: ClassifyContext): Classified {
  if (args.length === 0) return refused(`The github tool needs at least one argument. ${RUNS}`);
  const [first, second] = args;
  // gh finds its command among the arguments after stripping flags, and whether a flag eats the next word depends
  // on the flag: the command path first is the only order Marv reads exactly as gh does.
  if (first!.startsWith("-")) return refused(`Put the command first, then its flags: ["pr", "view", "12", "--comments"]. ${RUNS}`);
  if (NEVER[first!]) return refused(NEVER[first!]!);
  if (!GROUPS.has(first!)) return refused(unknown(first!));
  if (first === "api") return classifyApi(args.slice(1), ctx);
  if (first === "status") {
    const problem = checkFlags(args.slice(1), ctx, false);
    return problem ? refused(problem) : { kind: "read", command: "status" };
  }
  if (second === undefined || second.startsWith("-")) return refused(unknown(first!));
  const command = `${first} ${second}`;
  if (NEVER[command]) return refused(NEVER[command]!);
  if (!LISTED.has(command)) return refused(EXISTING.has(command) ? notRun(command) : unknown(command));
  const problem = checkFlags(args.slice(2), ctx, command === "repo view");
  if (problem) return refused(problem);
  if (WRITES.includes(command)) return { kind: "write", command, scope: `github:${command}` };
  return { kind: "read", command };
}

/** gh commands that exist but aren't the github tool's (so the refusal says "isn't run", not "unknown"). */
const EXISTING = new Set(["repo create", "repo delete", "repo fork", "repo edit", "repo sync", "repo list", "repo archive", "repo rename"]);
