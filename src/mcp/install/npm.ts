// An npm package as an MCP server: `npx -y <pkg>@<exact version>`. The version is resolved here, before the
// approval, and written into args: npm never lets a published version's files change, so it fixes the server's
// own code (its dependencies still follow their ranges: npx has no lockfile).
import { z } from "zod";
import { envEntries, fact, repoOf, type Proposal, type ResolveDeps, type ResolveOptions } from "./proposal.ts";
import { serverName, type Source } from "./source.ts";

const NPM = "https://registry.npmjs.org";
/** A packument has every version's metadata: popular packages run to megabytes. */
const MAX_BYTES = 32 * 1024 * 1024;
const YOUNG_DAYS = 7;
const DAY_MS = 24 * 60 * 60 * 1000;

const Person = z.object({ name: z.string() }).loose();
const Repository = z.union([z.string(), z.object({ url: z.string().optional() }).loose()]);
const Version = z
  .object({
    deprecated: z.string().optional(),
    repository: Repository.optional(),
    bin: z.union([z.string(), z.record(z.string(), z.string())]).optional(),
    _npmUser: Person.optional(),
  })
  .loose();
const Packument = z
  .object({
    name: z.string(),
    "dist-tags": z.record(z.string(), z.string()).optional(),
    versions: z.record(z.string(), Version),
    time: z.record(z.string(), z.string()).optional(),
    maintainers: z.array(Person).optional(),
    repository: Repository.optional(),
  })
  .loose();

const day = (iso: string | undefined) => (iso && /^\d{4}-\d{2}-\d{2}/.test(iso) ? iso.slice(0, 10) : undefined);
const urlOf = (repo: z.infer<typeof Repository> | undefined) => (typeof repo === "string" ? repo : repo?.url);

/** The registry's URL for a name: a scope's slash is encoded (`@scope%2fname`), as npm itself does. */
export const packumentUrl = (base: string, name: string) => `${base}/${name.startsWith("@") ? `@${encodeURIComponent(name.slice(1))}` : encodeURIComponent(name)}`;

export async function resolveNpm(source: Extract<Source, { kind: "npm" }>, opts: ResolveOptions, deps: ResolveDeps): Promise<Proposal & { repository?: string }> {
  const url = packumentUrl(deps.npmRegistry ?? NPM, source.name);
  const got = await deps.get(url, { signal: deps.signal, maxBytes: MAX_BYTES, headers: { accept: "application/json" } });
  if (got.status === 404) throw new Error(`There's no npm package named ${source.name}.`);
  if (got.status < 200 || got.status >= 300) throw new Error(`npm answered ${got.status} for ${source.name}.`);
  let data: z.infer<typeof Packument>;
  try {
    data = Packument.parse(JSON.parse(got.body));
  } catch {
    throw new Error(`npm's answer for ${source.name} isn't what Marv expected (a proxy?): nothing was added.`);
  }
  const version = source.version ?? data["dist-tags"]?.latest;
  if (!version) throw new Error(`npm lists no latest version of ${source.name}: give one, like ${source.name}@1.2.3.`);
  // Exact, never a fallback: a version that doesn't exist is the user's typo or a range, and npx would resolve it later.
  const meta = data.versions[version];
  if (!meta) throw new Error(`${source.name} has no version ${version} on npm (give an exact version; the latest is ${data["dist-tags"]?.latest ?? "unknown"}).`);

  const published = day(data.time?.[version]);
  const by = meta._npmUser?.name;
  const maintainers = (data.maintainers ?? []).map((m) => m.name);
  const repository = repoOf(urlOf(meta.repository) ?? urlOf(data.repository));
  const facts = [
    fact("npm", `${source.name} ${version}${published ? `, published ${published}` : ""}${by ? ` by ${by}` : ""}`),
    ...(maintainers.length ? [fact("owners", `${maintainers.slice(0, 5).join(", ")}${maintainers.length > 5 ? ` and ${maintainers.length - 5} more` : ""}`)] : []),
    fact("repo", repository ? `${repository} (as the package states it: npm doesn't check it)` : "none stated"),
  ];
  const warnings: string[] = [];
  if (meta.deprecated) warnings.push(`npm says ${version} is deprecated: ${meta.deprecated.slice(0, 300)}`);
  const created = data.time?.created ? Date.parse(data.time.created) : NaN;
  const age = ((deps.now?.() ?? new Date()).getTime() - created) / DAY_MS;
  if (age < YOUNG_DAYS) warnings.push(`${source.name} was first published ${age < 1 ? "less than a day" : `${Math.floor(age)} day${Math.floor(age) === 1 ? "" : "s"}`} ago: check that it's the package you meant (lookalike names are a common trick).`);
  if (meta.bin === undefined) warnings.push(`${source.name} ${version} declares no command (bin): npx may not be able to run it.`);

  const env = envEntries(opts.env);
  return {
    name: opts.name ?? serverName(source),
    entry: { command: "npx", args: ["-y", `${source.name}@${version}`, ...opts.args], ...(opts.env.length ? { env } : {}) },
    facts,
    warnings,
    ...(repository ? { repository } : {}),
  };
}
