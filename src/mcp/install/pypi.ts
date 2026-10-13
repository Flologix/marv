// A PyPI package as an MCP server: `uvx <pkg>@<exact version>`. PyPI refuses to re-upload a file name, so a
// pinned version's files can't change. Known limit: a package whose command has another name than the package
// needs the registry's entry or a hand edit.
import { z } from "zod";
import { envEntries, fact, repoOf, type Proposal, type ResolveDeps, type ResolveOptions } from "./proposal.ts";
import { serverName, type Source } from "./source.ts";

const PYPI = "https://pypi.org";
const MAX_BYTES = 32 * 1024 * 1024;
const YOUNG_DAYS = 7;
const DAY_MS = 24 * 60 * 60 * 1000;

const File = z.object({ upload_time_iso_8601: z.string().optional(), yanked: z.boolean().optional(), yanked_reason: z.string().nullable().optional() }).loose();
const Project = z
  .object({
    info: z
      .object({
        name: z.string(),
        version: z.string(),
        author: z.string().nullable().optional(),
        project_urls: z.record(z.string(), z.string()).nullable().optional(),
        home_page: z.string().nullable().optional(),
      })
      .loose(),
    releases: z.record(z.string(), z.array(File)).optional(),
    ownership: z.object({ organization: z.string().nullable().optional() }).loose().optional(),
  })
  .loose();

/** The repository among project_urls, if one looks like it. */
function repositoryOf(urls: Record<string, string> | null | undefined, home: string | null | undefined): string | undefined {
  const entries = Object.entries(urls ?? {});
  const named = entries.find(([k]) => /^(repository|source|source code|code|github)$/i.test(k.trim()));
  const hosted = entries.find(([, v]) => /github\.com|gitlab\.com|codeberg\.org/.test(v));
  return repoOf(named?.[1] ?? hosted?.[1] ?? home ?? undefined);
}

export async function resolvePypi(source: Extract<Source, { kind: "pypi" }>, opts: ResolveOptions, deps: ResolveDeps): Promise<Proposal & { repository?: string }> {
  const got = await deps.get(`${deps.pypi ?? PYPI}/pypi/${encodeURIComponent(source.name)}/json`, { signal: deps.signal, maxBytes: MAX_BYTES, headers: { accept: "application/json" } });
  if (got.status === 404) throw new Error(`There's no PyPI package named ${source.name}.`);
  if (got.status < 200 || got.status >= 300) throw new Error(`PyPI answered ${got.status} for ${source.name}.`);
  let data: z.infer<typeof Project>;
  try {
    data = Project.parse(JSON.parse(got.body));
  } catch {
    throw new Error(`PyPI's answer for ${source.name} isn't what Marv expected (a proxy?): nothing was added.`);
  }
  const version = source.version ?? data.info.version;
  const files = data.releases?.[version];
  if (source.version !== undefined && !files?.length) {
    throw new Error(`${source.name} has no version ${version} on PyPI (give an exact version; the latest is ${data.info.version}).`);
  }
  const name = data.info.name;
  const uploaded = files?.map((f) => f.upload_time_iso_8601).filter(Boolean).sort()[0]?.slice(0, 10);
  const repository = repositoryOf(data.info.project_urls, data.info.home_page);
  const by = data.ownership?.organization ?? data.info.author ?? undefined;
  const facts = [
    fact("pypi", `${name} ${version}${uploaded ? `, uploaded ${uploaded}` : ""}${by ? ` by ${by}` : ""}`),
    fact("repo", repository ? `${repository} (as the package states it: PyPI doesn't check it)` : "none stated"),
  ];
  const warnings: string[] = [];
  const yanked = files?.find((f) => f.yanked);
  if (yanked) warnings.push(`PyPI says ${version} was yanked${yanked.yanked_reason ? `: ${yanked.yanked_reason.slice(0, 300)}` : ""}.`);
  const first = Object.values(data.releases ?? {})
    .flat()
    .map((f) => f.upload_time_iso_8601)
    .filter((t): t is string => Boolean(t))
    .sort()[0];
  const age = first ? ((deps.now?.() ?? new Date()).getTime() - Date.parse(first)) / DAY_MS : Infinity;
  if (age < YOUNG_DAYS) warnings.push(`${name} was first uploaded ${age < 1 ? "less than a day" : `${Math.floor(age)} day${Math.floor(age) === 1 ? "" : "s"}`} ago: check that it's the package you meant (lookalike names are a common trick).`);

  return {
    name: opts.name ?? serverName(source),
    entry: { command: "uvx", args: [`${source.name}@${version}`, ...opts.args], ...(opts.env.length ? { env: envEntries(opts.env) } : {}) },
    facts,
    warnings,
    ...(repository ? { repository } : {}),
  };
}
