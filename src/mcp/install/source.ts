// What the user typed after /mcp add: which kind of source it is (npm, PyPI, a URL, a GitHub repository, a
// name in the MCP Registry), the server's name, and the options. Pure: nothing here fetches anything.
import { parseGithub } from "../../tools/web/github.ts";
import { validServerName } from "../config.ts";

export type Source =
  | { kind: "npm"; name: string; version?: string }
  | { kind: "pypi"; name: string; version?: string }
  | { kind: "http"; url: string }
  | { kind: "github"; owner: string; repo: string }
  | { kind: "registry"; name: string };

/** The options of /mcp add, checked: secrets only as ${VAR} references. */
export interface AddOptions {
  source: string;
  name?: string;
  /** Variable names: each is written as "NAME": "${NAME}". */
  env: string[];
  /** Header values, each a ${VAR} reference (with an optional scheme like "Bearer"). */
  headers: Record<string, string>;
  /** Passed to the server, after the package (what came after --). */
  args: string[];
}

export const ADD_USAGE = '/mcp add <npm package | pypi:<package> | https URL | GitHub repo | registry name> [--name <name>] [--env VAR]... [--header "Name: Bearer ${VAR}"]... [-- <server args>...]';

// npm's rules for a package name (lowercase; a scope is "@scope/"), and what a version can look like.
const NPM_NAME = /^(@[a-z0-9-~][a-z0-9-._~]*\/)?[a-z0-9-~][a-z0-9-._~]*$/;
const PYPI_NAME = /^[A-Za-z0-9]([A-Za-z0-9._-]*[A-Za-z0-9])?$/;
const VERSION = /^[0-9A-Za-z][0-9A-Za-z.+!_-]*$/;
// The registry's names are a reverse-DNS namespace, a slash, and a name: io.github.acme/weather.
const REGISTRY_NAME = /^[A-Za-z0-9][A-Za-z0-9.-]*\/[A-Za-z0-9][A-Za-z0-9._-]*$/;
const VAR = /^[A-Za-z_][A-Za-z0-9_]*$/;
// A header value Marv will write: an optional scheme word, then one ${VAR}. Anything else could be a secret typed in.
const HEADER_VALUE = /^([A-Za-z][A-Za-z0-9-]* )?\$\{[A-Za-z_][A-Za-z0-9_]*\}$/;

const notASource = (input: string) =>
  new Error(`"${input}" isn't an npm package, a URL, a GitHub repository or an MCP Registry name. Usage: ${ADD_USAGE}`);

/** "name@1.2.3" → name and version; the "@" of a scope doesn't count. */
function splitVersion(text: string): { name: string; version?: string } {
  const at = text.lastIndexOf("@");
  if (at <= 0) return { name: text };
  return { name: text.slice(0, at), version: text.slice(at + 1) };
}

function npm(text: string, input: string): Source {
  const { name, version } = splitVersion(text);
  if (!NPM_NAME.test(name) || name.length > 214 || (version !== undefined && !VERSION.test(version))) throw notASource(input);
  return { kind: "npm", name, ...(version !== undefined ? { version } : {}) };
}

function pypi(text: string, input: string): Source {
  const { name, version } = splitVersion(text);
  if (!PYPI_NAME.test(name) || (version !== undefined && !VERSION.test(version))) throw notASource(input);
  return { kind: "pypi", name, ...(version !== undefined ? { version } : {}) };
}

/** localhost, 127.0.0.0/8 and [::1]: the only hosts plain http is allowed to (nothing on the way to read a header). */
function loopback(host: string): boolean {
  return host === "localhost" || host === "[::1]" || /^127\.\d{1,3}\.\d{1,3}\.\d{1,3}$/.test(host);
}

/** Which source the text names, in the spec's order: GitHub, other URLs, npm:/pypi:, a scoped or bare npm name, a registry name. */
export function parseSource(input: string): Source {
  const text = input.trim();
  if (/^[a-z][a-z0-9+.-]*:\/\//i.test(text)) {
    let url: URL;
    try {
      url = new URL(text);
    } catch {
      throw notASource(input);
    }
    if (url.hostname === "github.com" || url.hostname === "www.github.com") {
      // Any link into a repository names the repository: its README is what's read.
      const [owner, repo] = url.pathname.split("/").filter(Boolean);
      const link = owner && repo ? parseGithub(new URL(`https://github.com/${owner}/${repo}`)) : null;
      if (!link || link.kind !== "repo" || url.protocol !== "https:") throw new Error(`${text} isn't a GitHub repository (https://github.com/<owner>/<repo>).`);
      return { kind: "github", owner: link.owner, repo: link.repo };
    }
    if (url.protocol === "https:") return { kind: "http", url: url.href };
    if (url.protocol === "http:") {
      if (loopback(url.hostname)) return { kind: "http", url: url.href };
      throw new Error(`Use https for ${url.host}: over plain http, a token in a header would travel in clear (plain http is for localhost only).`);
    }
    throw notASource(input);
  }
  if (text.startsWith("npm:")) return npm(text.slice(4), input);
  if (text.startsWith("pypi:")) return pypi(text.slice(5), input);
  if (text.startsWith("@")) return npm(text, input);
  if (text.includes("/")) {
    if (!REGISTRY_NAME.test(text)) throw notASource(input);
    return { kind: "registry", name: text };
  }
  return npm(text, input);
}

const PREFIXES = ["mcp-server-", "server-", "mcp-"];
const SUFFIXES = ["-mcp-server", "-mcp"];
const GENERIC_LABELS = new Set(["mcp", "www", "api"]);

function fromPackage(name: string): string {
  let base = name.replace(/^@[^/]+\//, "");
  const prefix = PREFIXES.find((p) => base.startsWith(p) && base.length > p.length);
  if (prefix) base = base.slice(prefix.length);
  const suffix = SUFFIXES.find((s) => base.endsWith(s) && base.length > s.length);
  if (suffix) base = base.slice(0, -suffix.length);
  return base;
}

/** What a source would be called in mcp.json (`--name` overrides it). Throws when nothing usable comes out. */
export function serverName(from: Source | string): string {
  const source = typeof from === "string" ? parseSource(from) : from;
  let raw: string;
  switch (source.kind) {
    case "npm":
    case "pypi":
      raw = fromPackage(source.name);
      break;
    case "github":
      raw = fromPackage(source.repo);
      break;
    case "registry":
      raw = fromPackage(source.name.split("/").at(-1)!);
      break;
    case "http": {
      const labels = new URL(source.url).hostname.split(".");
      raw = labels.find((l) => !GENERIC_LABELS.has(l)) ?? labels[0]!;
      break;
    }
  }
  const name = raw
    .replace(/[^A-Za-z0-9_-]/g, "_")
    .replace(/_{2,}/g, "_")
    .replace(/^_+|_+$/g, "")
    .slice(0, 32)
    .replace(/_+$/, "");
  if (!validServerName(name)) throw new Error(`Couldn't make a server name from it: pass --name <name> (letters, digits, - and _).`);
  return name;
}

/** Splits like a shell, with quotes but no expansion: `${VAR}` stays as typed. */
export function splitWords(text: string): string[] {
  const words: string[] = [];
  let word = "";
  let inWord = false;
  let quote: string | null = null;
  for (const ch of text) {
    if (quote) {
      if (ch === quote) quote = null;
      else word += ch;
    } else if (ch === '"' || ch === "'") {
      quote = ch;
      inWord = true;
    } else if (/\s/.test(ch)) {
      if (inWord) words.push(word);
      word = "";
      inWord = false;
    } else {
      word += ch;
      inWord = true;
    }
  }
  if (quote) throw new Error(`A ${quote} isn't closed.`);
  if (inWord) words.push(word);
  return words;
}

/** /mcp add's arguments. Secret values are refused: only ${VAR} references are ever written to the file. */
export function parseAddArgs(text: string): AddOptions {
  const words = splitWords(text);
  const options: AddOptions = { source: "", env: [], headers: {}, args: [] };
  let source: string | undefined;
  for (let i = 0; i < words.length; i++) {
    const word = words[i]!;
    const value = () => {
      const next = words[++i];
      if (next === undefined) throw new Error(`${word} needs a value. Usage: ${ADD_USAGE}`);
      return next;
    };
    if (word === "--") {
      options.args = words.slice(i + 1);
      break;
    } else if (word === "--name") {
      const name = value();
      if (!validServerName(name)) throw new Error(`"${name}" can't be a server name: use letters, digits, - and _ (at most 32), without "__" or a trailing "_".`);
      options.name = name;
    } else if (word === "--env") {
      const name = value();
      if (name.includes("=")) {
        const variable = name.slice(0, name.indexOf("="));
        throw new Error(
          `--env takes a variable's name, not its value: Marv writes "${variable}": "\${${variable}}" and the server gets the value from your environment, so it never sits in the file. Set ${variable} in your shell and pass --env ${variable}.`,
        );
      }
      if (!VAR.test(name)) throw new Error(`--env ${name}: not a variable name.`);
      if (!options.env.includes(name)) options.env.push(name);
    } else if (word === "--header") {
      const header = value();
      const colon = header.indexOf(":");
      const key = colon > 0 ? header.slice(0, colon).trim() : "";
      const val = colon > 0 ? header.slice(colon + 1).trim() : "";
      if (!/^[A-Za-z0-9-]+$/.test(key)) throw new Error(`--header "${header}": write it as "Name: value".`);
      if (!HEADER_VALUE.test(val)) {
        throw new Error(
          `--header ${key}: the value must be a \${VAR} reference (like "Bearer \${TOKEN}"), so the secret stays in your environment and never sits in the file.`,
        );
      }
      options.headers[key] = val;
    } else if (word.startsWith("--")) {
      throw new Error(`Unknown option ${word}. Usage: ${ADD_USAGE}`);
    } else if (source === undefined) {
      source = word;
    } else {
      throw new Error(`Only one source at a time ("${source}", then "${word}"); server arguments go after --. Usage: ${ADD_USAGE}`);
    }
  }
  if (source === undefined) throw new Error(`Usage: ${ADD_USAGE}`);
  options.source = source;
  return options;
}
