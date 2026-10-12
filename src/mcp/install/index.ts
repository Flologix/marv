// /mcp add's lookup: from what the user typed to a Proposal (or, for a README naming several servers, the commands
// to choose from). Every step is deterministic code: no model is involved, and nothing here writes or starts anything.
import type { Env } from "../../config/config.ts";
import { get } from "../../tools/web/fetch.ts";
import { fetchReadme } from "../../tools/web/github.ts";
import { resolveGithub } from "./github.ts";
import { resolveNpm } from "./npm.ts";
import { httpProposal, type Choices, type Proposal, type ResolveDeps } from "./proposal.ts";
import { resolvePypi } from "./pypi.ts";
import { resolveRegistry } from "./registry.ts";
import { parseSource, serverName, type AddOptions } from "./source.ts";

export type Resolved = Proposal | Choices;

export async function resolveAdd(options: AddOptions, deps: ResolveDeps): Promise<Resolved> {
  const source = parseSource(options.source);
  const opts = { name: options.name, env: options.env, headers: options.headers, args: options.args };
  if ((source.kind === "npm" || source.kind === "pypi") && Object.keys(options.headers).length) {
    throw new Error("--header is for remote (URL) servers; a local server reads its secrets from --env.");
  }
  switch (source.kind) {
    case "npm":
      return resolveNpm(source, opts, deps).then(({ repository: _, ...p }) => p);
    case "pypi":
      return resolvePypi(source, opts, deps).then(({ repository: _, ...p }) => p);
    case "http":
      if (options.env.length || options.args.length) throw new Error("--env and server arguments are for local servers; a remote one takes --header.");
      return httpProposal(source, { name: options.name ?? serverName(source), headers: options.headers });
    case "registry":
      return resolveRegistry(source, opts, deps);
    case "github":
      return resolveGithub(source, opts, deps);
  }
}

/** The real network: Marv's own `get`, and GitHub's API with the user's GITHUB_TOKEN (sent to the API host only). */
export function liveDeps(env: Env, signal?: AbortSignal): ResolveDeps {
  const token = env.GITHUB_TOKEN ?? env.GH_TOKEN;
  return {
    get,
    signal,
    readme: (owner, repo, s) => fetchReadme(owner, repo, { signal: s, ...(token ? { token } : {}) }),
  };
}
