// GitHub links, read through GitHub's API instead of its HTML (which is mostly menus): a repository becomes
// its description, README and file list, so the model can explore it like the project (glob, then read_file);
// a file becomes its raw text. A token goes to the API host only.
import { ToolError } from "../types.ts";
import { get, httpError, redirectNote, type GetOptions, type Got, type Page } from "./fetch.ts";
import { numberedLine } from "../read-file.ts";

export type GithubLink =
  | { kind: "repo"; owner: string; repo: string }
  | { kind: "tree"; owner: string; repo: string; ref: string; dir: string }
  | { kind: "blob"; owner: string; repo: string; ref: string; path: string };

export interface GithubOptions extends GetOptions {
  /** GITHUB_TOKEN: 5,000 API requests an hour instead of 60, and private repositories. */
  token?: string;
  apiBase?: string;
  rawBase?: string;
}

interface RepoInfo {
  full_name: string;
  description: string | null;
  default_branch: string;
  stargazers_count?: number;
  language?: string | null;
  homepage?: string | null;
  archived?: boolean;
}
interface Tree {
  tree: { path: string; type: string; mode?: string }[];
  truncated?: boolean;
}
/** One entry of the contents API's folder listing. */
interface Entry {
  path: string;
  type: string;
}

const API = "https://api.github.com";
const RAW = "https://raw.githubusercontent.com";
const MAX_FILES = 500;
const MAX_README = 12_000;
/** The whole-repository tree can be big (a monorepo's is megabytes of JSON); past this we list the top level instead. */
const TREE_BYTES = 16 * 1024 * 1024;
/** github.com/<these>/… are GitHub's own pages, not repositories. */
const NOT_OWNERS = new Set([
  "about", "account", "apps", "codespaces", "collections", "copilot", "customer-stories", "dashboard", "enterprise", "events", "explore",
  "features", "issues", "join", "login", "marketplace", "new", "notifications", "organizations", "orgs", "pricing", "pulls", "readme",
  "resources", "search", "security", "settings", "signup", "site", "solutions", "sponsors", "stars", "team", "topics", "trending", "users", "watching",
]);
const SLASH_HINT = " If the branch name has a slash in it, the link is ambiguous: use the commit hash instead.";
const CONTROL = /[\u0000-\u001f\u007f]/;
// Names go into API paths carrying the user's token, so only what GitHub itself allows gets through: an
// owner or repo like "x%2F..%2Fuser%2Femails%3F" would otherwise steer the token to another endpoint.
const OWNER = /^[A-Za-z0-9_-]{1,39}$/;
const REPO = /^[A-Za-z0-9._-]{1,100}$/;

/** Each segment of a path percent-encoded, the slashes kept. */
const enc = (path: string) => path.split("/").map(encodeURIComponent).join("/");

/** A decoded path or folder segment we're willing to put in a URL. `?` and `#` are fine: `enc` escapes them. */
const safeSegment = (seg: string) => seg !== "" && seg !== "." && seg !== ".." && !seg.includes("/") && !CONTROL.test(seg);
/** A decoded branch, tag or commit: a slash is fine (`feature%2Fx`), it's encoded back when used. */
const safeRef = (ref: string) => ref.split("/").every(safeSegment);

/** The repository, folder or file a github.com link points to; null for anything else (issues, PRs…: read as a web page). */
export function parseGithub(url: URL): GithubLink | null {
  if (url.hostname !== "github.com" && url.hostname !== "www.github.com") return null;
  let parts: string[];
  try {
    // pathname keeps its escapes: decode each segment once, here, so nothing downstream encodes a "%" again.
    parts = url.pathname.split("/").filter(Boolean).map(decodeURIComponent);
  } catch {
    return null; // a malformed escape
  }
  const [owner, name, kind, ref, ...rest] = parts;
  if (!owner || !name || NOT_OWNERS.has(owner) || !OWNER.test(owner)) return null;
  const repo = name.replace(/\.git$/, "");
  if (!REPO.test(repo) || repo === "." || repo === "..") return null;
  if (kind === undefined) return { kind: "repo", owner, repo };
  if (!ref || !safeRef(ref) || !rest.every(safeSegment)) return null;
  if (kind === "tree") return { kind: "tree", owner, repo, ref, dir: rest.join("/") };
  if (kind === "blob" && rest.length) return { kind: "blob", owner, repo, ref, path: rest.join("/") };
  return null;
}

/** A repository's README as raw text, through the API (null: it has none). /mcp add reads it for a package name. */
export async function fetchReadme(owner: string, repo: string, options: GithubOptions = {}): Promise<string | null> {
  if (!OWNER.test(owner) || !REPO.test(repo)) throw new ToolError(`Not a GitHub repository: ${owner}/${repo}.`);
  const got = await api(`/repos/${owner}/${repo}/readme`, options, "application/vnd.github.raw");
  if (got.status === 404) return null;
  return got.body;
}

export function fetchGithub(link: GithubLink, options: GithubOptions = {}): Promise<Page> {
  if (link.kind === "repo") return overview(link, options);
  if (link.kind === "tree") return folder(link, options);
  return file(link, options);
}

async function overview({ owner, repo }: { owner: string; repo: string }, options: GithubOptions): Promise<Page> {
  const base = `/repos/${owner}/${repo}`;
  const got = await api(base, options);
  if (got.status === 404) throw notFound(`${owner}/${repo}`, options);
  const info = json<RepoInfo>(got);
  const branch = info.default_branch;
  const name = `${owner}/${repo}`;
  const readme = await api(`${base}/readme`, options, "application/vnd.github.raw");

  // The whole tree in one request when it's small enough; otherwise the top level only, which works at any size.
  let files: string | undefined;
  let readmePath = "README.md";
  const pickReadme = (paths: string[]) => (readmePath = paths.find((p) => /^readme(\.|$)/i.test(p)) ?? readmePath);
  let listed: Tree | undefined;
  let empty = false;
  try {
    const tree = await api(`${base}/git/trees/${encodeURIComponent(branch)}?recursive=1`, options, undefined, { maxBytes: options.maxBytes ?? TREE_BYTES, allow: [409] });
    if (tree.status === 404 || tree.status === 409) empty = true; // 409: "Git Repository is empty."
    else {
      const parsed = json<Tree>(tree);
      if (!parsed.truncated) listed = parsed;
    }
  } catch (err) {
    if (!(err instanceof ToolError && /is larger than/.test(err.message))) throw err;
  }
  if (empty) files = "(Empty repository.)";
  else if (listed) {
    const entries = listed.tree.filter((e) => e.type === "blob" || e.type === "commit");
    pickReadme(entries.filter((e) => e.type === "blob").map((e) => e.path));
    files = listing(entries.map((e) => mark(e.path, e.type, e.mode)), name, branch);
  } else {
    const top = await entriesOf(await api(`${base}/contents?ref=${encodeURIComponent(branch)}`, options));
    pickReadme(top.filter((e) => e.type === "file").map((e) => e.path));
    files = listing(top.map((e) => mark(e.path, e.type)), name, branch, `The repository is too big to list whole: these are its top-level entries; list a folder with web_fetch github.com/${name}/tree/${encodeURIComponent(branch)}/<folder>.`);
  }

  let readmeText = readme.body.trim();
  if (readmeText.length > MAX_README) {
    readmeText = `${readmeText.slice(0, MAX_README)}\n\n(README cut: read all of it with web_fetch github.com/${name}/blob/${encodeURIComponent(branch)}/${enc(readmePath)})`;
  }
  const facts = [`Default branch: ${branch}`, info.language, info.stargazers_count !== undefined && `★ ${info.stargazers_count}`, info.homepage, info.archived && "archived"];
  const text = [
    `# ${info.full_name}`,
    info.description ?? "",
    facts.filter(Boolean).join(" · "),
    readme.status === 404 ? "(No README.)" : `## README\n\n${readmeText}`,
    files,
  ];
  return { url: `https://github.com/${name}`, title: info.full_name, text: text.filter(Boolean).join("\n\n") };
}

/** Folders come from the contents API, which lists one level at any repository size (the recursive tree can be truncated). */
async function folder(link: { owner: string; repo: string; ref: string; dir: string }, options: GithubOptions): Promise<Page> {
  const { owner, repo, ref, dir } = link;
  const got = await api(`/repos/${owner}/${repo}/contents${dir ? `/${enc(dir)}` : ""}?ref=${encodeURIComponent(ref)}`, options);
  if (got.status === 404) throw notFound(dir ? `${dir} at ${ref} in ${owner}/${repo}` : `${owner}/${repo} at ${ref}`, options, SLASH_HINT);
  const data = json<unknown>(got);
  if (!Array.isArray(data)) throw new ToolError(`${dir} is a file: read it with web_fetch github.com/${owner}/${repo}/blob/${encodeURIComponent(ref)}/${enc(dir)}.`);
  return folderPage(link, data as Entry[]);
}

function folderPage({ owner, repo, ref, dir }: { owner: string; repo: string; ref: string; dir: string }, entries: Entry[]): Page {
  const where = dir ? `${owner}/${repo}/${dir}` : `${owner}/${repo}`;
  return {
    url: `https://github.com/${owner}/${repo}/tree/${encodeURIComponent(ref)}${dir ? `/${enc(dir)}` : ""}`,
    title: `${where} at ${ref}`,
    text: listing(entries.map((e) => mark(e.path, e.type)), `${owner}/${repo}`, ref),
  };
}

async function file({ owner, repo, ref, path }: { owner: string; repo: string; ref: string; path: string }, options: GithubOptions): Promise<Page> {
  // With a token, through the API (private repositories work); without one, from raw.githubusercontent.com,
  // which doesn't count toward the 60-an-hour limit. The raw request gets no headers: no token there.
  const got = options.token
    ? await api(`/repos/${owner}/${repo}/contents/${enc(path)}?ref=${encodeURIComponent(ref)}`, options, "application/vnd.github.raw")
    : await get(`${options.rawBase ?? RAW}/${owner}/${repo}/${enc(ref)}/${enc(path)}`, { ...options, headers: undefined });
  refuseRedirect(got);
  if (got.status === 404) throw notFound(`${path} at ${ref} in ${owner}/${repo}`, options, SLASH_HINT);
  if (got.status < 200 || got.status >= 300) throw httpError(got);
  // A folder answers with its listing (JSON) instead of raw text, even when raw was asked for.
  if (options.token && /^application\/json/i.test(got.headers.get("content-type") ?? "")) {
    const data = json<unknown>(got);
    if (Array.isArray(data)) return folderPage({ owner, repo, ref, dir: path }, data as Entry[]);
  }
  const lines = got.body.replace(/\n$/, "").split("\n");
  return {
    url: `https://github.com/${owner}/${repo}/blob/${encodeURIComponent(ref)}/${enc(path)}`,
    title: path,
    text: lines.map((line, i) => numberedLine(i + 1, line)).join("\n"),
  };
}

/** A redirect that wasn't followed says where it leads, like fetchPage's, instead of a bare "302 Found". */
function refuseRedirect(got: Got): void {
  const note = redirectNote(got);
  if (note) throw new ToolError(note);
}

/**
 * A GitHub API request. A 404 (and any status in `allow`) is returned, since the caller knows what it means;
 * other errors are thrown.
 */
async function api(path: string, options: GithubOptions, accept = "application/vnd.github+json", extra: { maxBytes?: number; allow?: number[] } = {}): Promise<Got> {
  const headers: Record<string, string> = { accept, "x-github-api-version": "2022-11-28" };
  if (options.token) headers.authorization = `Bearer ${options.token}`;
  const got = await get(`${options.apiBase ?? API}${path}`, { ...options, headers, ...(extra.maxBytes ? { maxBytes: extra.maxBytes } : {}) });
  refuseRedirect(got);
  if (got.status === 403 || got.status === 429) {
    const wait = got.headers.get("retry-after");
    if (wait && /^\d+$/.test(wait)) throw new ToolError(`GitHub asks to wait ${wait} s (secondary rate limit).`);
    if (got.headers.get("x-ratelimit-remaining") === "0") throw rateLimited(got, options);
  }
  if ((got.status >= 200 && got.status < 300) || got.status === 404 || extra.allow?.includes(got.status)) return got;
  throw failed(got);
}

/** An error status with GitHub's own explanation, when its body has one. */
function failed(got: Got): ToolError {
  let message = "";
  try {
    const m = (JSON.parse(got.body) as { message?: unknown }).message;
    if (typeof m === "string") message = m.slice(0, 300);
  } catch {
    // not JSON: the status alone
  }
  return new ToolError(`${got.status} ${got.statusText || "error"} at ${got.url}${message ? `: ${message}` : ""}.`);
}

function json<T>(got: Got): T {
  try {
    return JSON.parse(got.body) as T;
  } catch {
    throw new ToolError("GitHub's API answered with something that isn't JSON (a proxy or captive portal?).");
  }
}

async function entriesOf(got: Got): Promise<Entry[]> {
  const data = json<unknown>(got);
  if (!Array.isArray(data)) throw new ToolError(`GitHub's API answered with something unexpected at ${got.url}.`);
  return data as Entry[];
}

function rateLimited(got: Got, { token }: GithubOptions): ToolError {
  const reset = Number(got.headers.get("x-ratelimit-reset"));
  const until = reset ? ` until ${new Date(reset * 1000).toLocaleTimeString()}` : "";
  return new ToolError(
    token
      ? `GitHub's API limit for your token is used up${until}.`
      : `GitHub's API limit (60 requests an hour without a token) is used up${until}. Set GITHUB_TOKEN for 5,000 an hour.`,
  );
}

const notFound = (what: string, { token }: GithubOptions, hint = "") =>
  new ToolError(`Not found on GitHub: ${what}.${hint}${token ? "" : " If it's a private repository, set GITHUB_TOKEN."}`);

/** How `ls` would show an entry: folders with a slash, the odd ones named. */
function mark(path: string, type: string, mode?: string): string {
  if (type === "dir" || type === "tree") return `${path}/`;
  if (type === "submodule" || type === "commit") return `${path} (submodule)`;
  if (type === "symlink" || mode === "120000") return `${path} (symlink)`;
  return path;
}

function listing(lines: string[], repo: string, ref: string, note = ""): string {
  const shown = lines.slice(0, MAX_FILES);
  const r = encodeURIComponent(ref);
  const cut = lines.length > MAX_FILES ? `\n(Showing ${shown.length} of ${lines.length} files: list a folder with web_fetch github.com/${repo}/tree/${r}/<folder>.)` : "";
  const heading = note ? "## Files (top level)" : `## Files (${lines.length})`;
  return `${heading}\n\n${note ? `${note}\n\n` : ""}${shown.join("\n")}${cut}\n\nRead a file with web_fetch github.com/${repo}/blob/${r}/<path>.`;
}
