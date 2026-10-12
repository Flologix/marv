// The official MCP Registry (registry.modelcontextprotocol.io, no key): /mcp search lists it, /mcp add <name>
// installs an entry. The registry isn't curated: a publisher proves only its namespace (io.github.<user> by
// signing in to GitHub as that user, a domain by DNS), so that and the repository are shown for the user to judge.
// A package then goes through the npm/PyPI resolver at the version the entry names, so npm's facts are shown too
// and the registry can't point at a version the package registry doesn't have.
import { z } from "zod";
import { printable } from "../../printable.ts";
import { resolveNpm } from "./npm.ts";
import { fact, httpProposal, repoOf, type Proposal, type ResolveDeps, type ResolveOptions } from "./proposal.ts";
import { resolvePypi } from "./pypi.ts";
import { parseSource, serverName, type Source } from "./source.ts";

const REGISTRY = "https://registry.modelcontextprotocol.io";
const OFFICIAL = "io.modelcontextprotocol.registry/official";
const MAX_RESULTS = 10;
const VAR = /^[A-Za-z_][A-Za-z0-9_]*$/;
const KNOWN_BASES: Record<string, string> = { npm: "https://registry.npmjs.org", pypi: "https://pypi.org" };

const Input = z
  .object({
    name: z.string().optional(),
    value: z.string().optional(),
    default: z.string().optional(),
    isRequired: z.boolean().optional(),
    isSecret: z.boolean().optional(),
    description: z.string().optional(),
    valueHint: z.string().optional(),
    type: z.string().optional(),
  })
  .loose();
const Package = z
  .object({
    registryType: z.string(),
    registryBaseUrl: z.string().optional(),
    identifier: z.string(),
    version: z.string().optional(),
    transport: z.object({ type: z.string() }).loose().optional(),
    packageArguments: z.array(Input).optional(),
    environmentVariables: z.array(Input).optional(),
  })
  .loose();
const Remote = z.object({ type: z.string(), url: z.string(), headers: z.array(Input).optional() }).loose();
const Server = z
  .object({
    name: z.string(),
    description: z.string().optional(),
    version: z.string().optional(),
    repository: z.object({ url: z.string().optional() }).loose().optional(),
    packages: z.array(Package).optional(),
    remotes: z.array(Remote).optional(),
  })
  .loose();
const Meta = z.record(z.string(), z.unknown()).optional();
const Record_ = z.object({ server: Server, _meta: Meta }).loose();
const List = z.object({ servers: z.array(Record_) }).loose();

type Entry = z.infer<typeof Record_>;
type Input = z.infer<typeof Input>;

/** active, deprecated or deleted, as the registry marks it (undefined: not said). */
function statusOf(entry: Entry): string | undefined {
  const official = entry._meta?.[OFFICIAL] as { status?: unknown } | undefined;
  return typeof official?.status === "string" ? official.status : undefined;
}

function parse<T>(schema: z.ZodType<T>, body: string): T {
  try {
    return schema.parse(JSON.parse(body));
  } catch {
    throw new Error("The MCP Registry answered with something Marv doesn't understand (a proxy, or a new format): nothing was added.");
  }
}

export interface RegistryHit {
  name: string;
  description?: string;
  version?: string;
  repository?: string;
}

/** At most 10 latest, active entries matching the words. */
export async function searchRegistry(words: string, deps: ResolveDeps): Promise<RegistryHit[]> {
  const url = `${deps.registry ?? REGISTRY}/v0/servers?search=${encodeURIComponent(words)}&version=latest&limit=30`;
  const got = await deps.get(url, { signal: deps.signal, headers: { accept: "application/json" } });
  if (got.status < 200 || got.status >= 300) throw new Error(`The MCP Registry answered ${got.status}.`);
  return parse(List, got.body)
    .servers.filter((e) => (statusOf(e) ?? "active") === "active")
    .slice(0, MAX_RESULTS)
    .map(({ server }) => ({
      name: server.name,
      ...(server.description ? { description: server.description } : {}),
      ...(server.version ? { version: server.version } : {}),
      ...(repoOf(server.repository?.url) ? { repository: repoOf(server.repository?.url) } : {}),
    }));
}

/** What a namespace proves: io.github.<user> a GitHub sign-in, anything else a domain's DNS. */
function namespaceProof(name: string): string {
  const namespace = name.split("/")[0]!;
  const github = /^io\.github\.(.+)$/.exec(namespace);
  if (github) return `the publisher signed in to GitHub as ${github[1]}`;
  return `the publisher proved it controls ${namespace.split(".").reverse().join(".")}`;
}

/**
 * The registry's arguments: fixed values and defaults, then what the user gave after -- fills the required rest.
 * A registry value that starts with "-" is refused: anyone can publish an entry, and a value like --from or -- could
 * be read as an option to npx/uvx rather than the package. A named argument's own name (--mode) is the package's.
 */
function argsOf(inputs: Input[], given: string[], what: string): string[] {
  const args: string[] = [];
  const rest = [...given];
  const missing: string[] = [];
  for (const input of inputs) {
    const value = input.value ?? input.default;
    const named = input.type === "named" && input.name;
    if (value?.startsWith("-")) {
      throw new Error(`${what}'s entry gives ${named ? `the argument ${input.name} the value` : "the argument value"} ${value}, which starts with "-": Marv only adds values that can't be read as options. Nothing was added.`);
    }
    if (value !== undefined) {
      args.push(...(named ? [input.name!, value] : [value]));
    } else if (input.isRequired) {
      const next = rest.shift();
      if (next === undefined) missing.push(`${named ? `${input.name} ` : ""}<${input.valueHint ?? input.name ?? "value"}>${input.description ? ` (${input.description})` : ""}`);
      else args.push(...(named ? [input.name!, next] : [next]));
    }
    // An optional argument without a value is left out: the server's own default applies.
  }
  if (missing.length) {
    throw new Error(`${what} needs ${missing.length === 1 ? "an argument" : "arguments"} the registry gives no value for: ${missing.join(", ")}. Give ${missing.length === 1 ? "it" : "them"} after --, e.g. /mcp add ${what} -- <value>.`);
  }
  return [...args, ...rest];
}

/**
 * "Bearer {api_key}" → "Bearer ${API_KEY}"; a value-less header → ${HEADER_NAME}; a secret's fixed value too. Only
 * references reach the file, as with a typed --header: a fixed value is refused, so a registry entry can't plant a
 * static Authorization or Cookie header in your config.
 */
function headerValue(input: Input, what: string, url: string): string | undefined {
  const variable = (text: string) => text.toUpperCase().replace(/[^A-Z0-9_]/g, "_").replace(/^(\d)/, "_$1");
  if (input.value === undefined) return input.isRequired ? `\${${variable(input.name ?? "TOKEN")}}` : undefined;
  if (/\{[^}]+\}/.test(input.value)) return input.value.replace(/\{([^}]+)\}/g, (_, name: string) => `\${${variable(name)}}`);
  if (input.isSecret) return `\${${variable(input.name ?? "TOKEN")}}`;
  throw new Error(
    `${what}'s entry gives the header ${input.name} a fixed value: Marv writes only \${VAR} references into your config, so a registry entry can't plant a header. Nothing was added; to add it by its URL with your own: /mcp add ${url} --header "${input.name}: Bearer \${TOKEN}"`,
  );
}

/** The registry's environment variables: required ones as ${NAME} (a default as ${NAME:-default}); optional ones are only named. */
function envOf(inputs: Input[]): { env: Record<string, string>; optional: string[] } {
  const env: Record<string, string> = {};
  const optional: string[] = [];
  for (const input of inputs) {
    if (!input.name || !VAR.test(input.name)) continue;
    const fallback = input.default !== undefined && !/[}$]/.test(input.default) ? input.default : undefined;
    if (input.isRequired) env[input.name] = fallback !== undefined ? `\${${input.name}:-${fallback}}` : `\${${input.name}}`;
    else optional.push(input.name);
  }
  return { env, optional };
}

export async function resolveRegistry(source: Extract<Source, { kind: "registry" }>, opts: ResolveOptions, deps: ResolveDeps): Promise<Proposal> {
  const url = `${deps.registry ?? REGISTRY}/v0/servers/${encodeURIComponent(source.name)}/versions/latest`;
  const got = await deps.get(url, { signal: deps.signal, headers: { accept: "application/json" } });
  if (got.status === 404) throw new Error(`There's no server named ${source.name} in the MCP Registry (/mcp search <words> finds one).`);
  if (got.status < 200 || got.status >= 300) throw new Error(`The MCP Registry answered ${got.status} for ${source.name}.`);
  const entry = parse(Record_, got.body);
  const { server } = entry;
  const status = statusOf(entry);
  if (status && status !== "active") throw new Error(`The MCP Registry marks ${server.name} as ${status}: not added.`);

  const name = opts.name ?? serverName(source);
  const stdio = (p: z.infer<typeof Package>) => (p.transport?.type ?? "stdio") === "stdio";
  const packages = server.packages ?? [];
  const pkg = packages.find((p) => p.registryType === "npm" && stdio(p)) ?? packages.find((p) => p.registryType === "pypi" && stdio(p));
  const remote = (server.remotes ?? []).find((r) => r.type === "streamable-http");
  const registryFacts = [
    fact("registry", `${server.name}${server.version ? ` ${server.version}` : ""} (namespace verified: ${namespaceProof(server.name)})`),
    ...(server.description ? [fact("about", server.description.slice(0, 200))] : []),
  ];

  if (pkg) {
    const base = pkg.registryBaseUrl?.replace(/\/+$/, "");
    if (base && base !== KNOWN_BASES[pkg.registryType]) throw new Error(`${server.name}'s package is on ${base}, not the public ${pkg.registryType} registry: not supported.`);
    if (!pkg.version) throw new Error(`${server.name}'s entry names no version of ${pkg.identifier}: add it yourself with an exact version.`);
    const args = argsOf(pkg.packageArguments ?? [], opts.args, source.name);
    const { env, optional } = envOf(pkg.environmentVariables ?? []);
    for (const n of opts.env) env[n] = `\${${n}}`;
    const kind = pkg.registryType as "npm" | "pypi";
    // The identifier goes through the same parser as typed input: a registry entry can't smuggle a URL or flags in.
    const parsed = parseSource(`${kind}:${pkg.identifier}@${pkg.version}`);
    const resolved =
      parsed.kind === "npm"
        ? await resolveNpm(parsed, { name, env: [], args }, deps)
        : await resolvePypi(parsed as Extract<Source, { kind: "pypi" }>, { name, env: [], args }, deps);
    const hasEnv = Object.keys(env).length > 0;
    return {
      ...resolved,
      entry: { ...(resolved.entry as { command: string; args: string[] }), ...(hasEnv ? { env } : {}) },
      facts: [...resolved.facts, ...registryFacts, ...(optional.length ? [fact("optional", `${optional.join(", ")} (the server can read these too: add with --env NAME)`)] : [])],
    };
  }

  if (remote) {
    const parsed = parseSource(remote.url);
    if (parsed.kind !== "http") throw new Error(`${server.name}'s remote URL ${remote.url} isn't one Marv can add.`);
    const headers: Record<string, string> = {};
    for (const h of remote.headers ?? []) {
      const value = h.name && /^[A-Za-z0-9-]+$/.test(h.name) ? headerValue(h, server.name, remote.url) : undefined;
      if (value !== undefined) headers[h.name!] = value;
    }
    Object.assign(headers, opts.headers);
    const proposal = httpProposal(parsed, { name, headers });
    return { ...proposal, facts: [...proposal.facts, ...registryFacts] };
  }

  const oci = packages.find((p) => p.registryType === "oci");
  if (oci) throw new Error(`Docker images aren't supported yet; the entry says: docker run ${oci.identifier}`);
  const other = packages[0] ?? server.remotes?.[0];
  const what = other ? ("registryType" in other ? `${other.registryType} ${other.identifier}` : `a ${other.type} remote`) : "nothing";
  throw new Error(`${server.name} offers ${what}, which Marv can't add yet (it adds npm and PyPI packages and https remotes).`);
}

/** /mcp search's answer, as Markdown. Descriptions are the publishers' own words, made printable and kept short. */
export function searchText(words: string, hits: RegistryHit[]): string {
  if (!hits.length) return `Nothing in the MCP Registry matches "${printable(words)}".`;
  const clean = (text: string, max: number) => {
    const one = printable(text).replace(/\s+/g, " ").replace(/`/g, "'").trim();
    return one.length > max ? `${one.slice(0, max - 1)}…` : one;
  };
  const lines = hits.map((h) => {
    const about = [h.version, h.description && clean(h.description, 160), h.repository && clean(h.repository, 100)].filter(Boolean).join(" · ");
    return `- \`${h.name}\`${about ? `: ${about}` : ""}\n  \`/mcp add ${h.name}\``;
  });
  return [`**MCP Registry** matches for "${printable(words)}". The registry isn't curated: check the publisher and the repository before adding one.`, ...lines].join("\n\n");
}
