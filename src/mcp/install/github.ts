// A GitHub repository as a source: its README *nominates* a published package or a URL, and the resolver decides.
// Only a package name, a URL and env variable *names* leave this module: the rest of a README's config block (extra
// args, env values) is dropped, never copied, since a README is stranger-written and its text never reaches the
// screen unfiltered or the model at all. Cloning and building a repository isn't supported (a build can do anything).
import { marked } from "marked";
import { resolveNpm } from "./npm.ts";
import { fact, httpProposal, type Choices, type Proposal, type ResolveDeps, type ResolveOptions } from "./proposal.ts";
import { resolvePypi } from "./pypi.ts";
import { parseSource, serverName, type Source } from "./source.ts";

export type Candidate = { source: Source; envHints: string[] } | { unsupported: string };

const VAR = /^[A-Za-z_][A-Za-z0-9_]*$/;
const MAX_SHOWN = 80;

const isObject = (v: unknown): v is Record<string, unknown> => typeof v === "object" && v !== null && !Array.isArray(v);
const strings = (v: unknown): string[] => (Array.isArray(v) ? v.filter((x): x is string => typeof x === "string") : []);
const short = (text: string) => (text.length > MAX_SHOWN ? `${text.slice(0, MAX_SHOWN - 1)}…` : text);

/** The server entries in one parsed block: Claude's {"mcpServers": …}, VS Code's {"servers": …} or {"mcp": {"servers": …}}, or one entry. */
function entriesIn(data: unknown): Record<string, unknown>[] {
  if (!isObject(data)) return [];
  for (const map of [data.mcpServers, data.servers, isObject(data.mcp) ? data.mcp.servers : undefined]) {
    if (isObject(map)) return Object.values(map).filter(isObject);
  }
  return "command" in data || "url" in data ? [data] : [];
}

/** The package an `npx`/`uvx` line runs: its first word that isn't a flag. `uvx --from x cmd` runs a command, not a package: unsupported. */
function packageArg(args: string[]): string | undefined {
  if (args.includes("--from")) return undefined;
  return args.find((a) => !a.startsWith("-"));
}

function candidateOf(entry: Record<string, unknown>): Candidate | null {
  const envHints = isObject(entry.env) ? Object.keys(entry.env).filter((k) => VAR.test(k)) : [];
  const command = typeof entry.command === "string" ? entry.command : undefined;
  const args = strings(entry.args);
  const asSource = (text: string): Source | null => {
    try {
      return parseSource(text);
    } catch {
      return null;
    }
  };
  if (command === "npx" || command === "uvx") {
    const pkg = packageArg(args);
    const source = pkg ? asSource(`${command === "npx" ? "npm" : "pypi"}:${pkg}`) : null;
    return source ? { source, envHints } : { unsupported: short([command, ...args].join(" ")) };
  }
  if (typeof entry.url === "string" && (entry.type === undefined || entry.type === "http" || entry.type === "streamable-http")) {
    const source = asSource(entry.url);
    return source?.kind === "http" && source.url.startsWith("https://") ? { source, envHints: [] } : { unsupported: short(entry.url) };
  }
  if (command) return { unsupported: short([command, ...args].join(" ")) };
  return null;
}

/** What a README's fenced JSON blocks nominate, deduplicated, in order. */
export function readmeCandidates(markdown: string): Candidate[] {
  const found: Candidate[] = [];
  const seen = new Set<string>();
  marked.walkTokens(marked.lexer(markdown), (token) => {
    if (token.type !== "code") return;
    let data: unknown;
    try {
      data = JSON.parse(token.text);
    } catch {
      return;
    }
    for (const entry of entriesIn(data)) {
      const candidate = candidateOf(entry);
      if (!candidate) continue;
      const key = JSON.stringify("source" in candidate ? candidate.source : candidate);
      if (seen.has(key)) continue;
      seen.add(key);
      found.push(candidate);
    }
  });
  return found;
}

/** The text that, typed after /mcp add, means this source. */
export function sourceText(source: Source): string {
  switch (source.kind) {
    case "npm":
      return `${source.name}${source.version ? `@${source.version}` : ""}`;
    case "pypi":
      return `pypi:${source.name}${source.version ? `@${source.version}` : ""}`;
    case "http":
      return source.url;
    case "github":
      return `https://github.com/${source.owner}/${source.repo}`;
    case "registry":
      return source.name;
  }
}

export async function resolveGithub(source: Extract<Source, { kind: "github" }>, opts: ResolveOptions, deps: ResolveDeps): Promise<Proposal | Choices> {
  const repo = `github.com/${source.owner}/${source.repo}`;
  if (!deps.readme) throw new Error("Can't read GitHub READMEs here.");
  const readme = await deps.readme(source.owner, source.repo, deps.signal);
  if (readme === null) throw new Error(`${repo} has no README (or doesn't exist): add the server by its npm or PyPI package, or its URL.`);
  const candidates = readmeCandidates(readme);
  const usable = candidates.filter((c): c is Extract<Candidate, { source: Source }> => "source" in c);
  if (usable.length === 0) {
    const others = candidates.flatMap((c) => ("unsupported" in c ? [c.unsupported] : []));
    const named = others.length ? ` It shows ${others.map((o) => `\`${o}\``).join(", ")}: that needs building or Docker, which /mcp add doesn't do yet.` : "";
    throw new Error(`${repo}'s README names no npm or PyPI package or https URL to add.${named}`);
  }
  if (usable.length > 1) return { choices: usable.map((c) => `/mcp add ${sourceText(c.source)}`) };

  const [{ source: nominated, envHints }] = usable as [Extract<Candidate, { source: Source }>];
  const name = opts.name ?? serverName(nominated);
  let proposal: Proposal & { repository?: string };
  if (nominated.kind === "npm") proposal = await resolveNpm(nominated, { ...opts, name }, deps);
  else if (nominated.kind === "pypi") proposal = await resolvePypi(nominated, { ...opts, name }, deps);
  else if (nominated.kind === "http") proposal = httpProposal(nominated, { name, headers: opts.headers });
  else throw new Error(`${repo}'s README names something /mcp add can't follow.`);

  const facts = [fact("github", `the README of ${repo} names ${sourceText(nominated)}`), ...proposal.facts];
  const hints = envHints.filter((h) => !opts.env.includes(h));
  if (hints.length) facts.push(fact("readme", `its example sets ${hints.join(", ")}: not written unless you pass --env ${hints[0]}`));
  const warnings = [...proposal.warnings];
  const own = proposal.repository?.toLowerCase();
  const expected = repo.toLowerCase();
  if (nominated.kind !== "http" && !(own === expected || own?.startsWith(`${expected}/`))) {
    const says = proposal.repository ? `says its repository is ${proposal.repository}` : "names no repository";
    // First, where it's seen first: the README may nominate a package someone else publishes.
    facts.unshift(fact("mismatch", `the package ${says}, not ${repo}`));
    warnings.unshift(`The package ${says}, not ${repo}: make sure it's the one this README means.`);
  }
  const { repository: _, ...rest } = proposal;
  return { ...rest, facts, warnings };
}
