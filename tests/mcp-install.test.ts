import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { join } from "node:path";
import { displayOf } from "../src/mcp/config.ts";
import { readmeCandidates, resolveGithub } from "../src/mcp/install/github.ts";
import { resolveAdd } from "../src/mcp/install/index.ts";
import { resolveNpm } from "../src/mcp/install/npm.ts";
import { approvalFor, httpProposal, type Proposal, type ResolveDeps } from "../src/mcp/install/proposal.ts";
import { resolvePypi } from "../src/mcp/install/pypi.ts";
import { resolveRegistry, searchRegistry } from "../src/mcp/install/registry.ts";
import { parseAddArgs, parseSource, serverName } from "../src/mcp/install/source.ts";
import { get } from "../src/tools/web/fetch.ts";

describe("parseSource", () => {
  test.each([
    ["https://github.com/acme/weather-mcp", { kind: "github", owner: "acme", repo: "weather-mcp" }],
    ["https://github.com/acme/weather-mcp/tree/main/src", { kind: "github", owner: "acme", repo: "weather-mcp" }],
    ["https://mcp.linear.app/mcp", { kind: "http", url: "https://mcp.linear.app/mcp" }],
    ["http://localhost:8080/mcp", { kind: "http", url: "http://localhost:8080/mcp" }],
    ["http://127.0.0.1:8080/mcp", { kind: "http", url: "http://127.0.0.1:8080/mcp" }],
    ["http://[::1]:8080/mcp", { kind: "http", url: "http://[::1]:8080/mcp" }],
    ["@modelcontextprotocol/server-filesystem", { kind: "npm", name: "@modelcontextprotocol/server-filesystem" }],
    ["weather", { kind: "npm", name: "weather" }],
    ["npm:weather@1.2.0", { kind: "npm", name: "weather", version: "1.2.0" }],
    ["@scope/x@2.0.0", { kind: "npm", name: "@scope/x", version: "2.0.0" }],
    ["pypi:mcp-server-fetch@2025.4.7", { kind: "pypi", name: "mcp-server-fetch", version: "2025.4.7" }],
    ["pypi:mcp-server-fetch", { kind: "pypi", name: "mcp-server-fetch" }],
    ["io.github.acme/weather", { kind: "registry", name: "io.github.acme/weather" }],
  ])("%s", (input, expected) => expect(parseSource(input)).toEqual(expected as never));

  test("plain http is for loopback only (a header would travel in clear)", () => {
    expect(() => parseSource("http://example.com/mcp")).toThrow(/https/);
  });
  test("not a package, URL or registry name", () => {
    expect(() => parseSource("Not A Package!")).toThrow(/npm package, a URL/);
    expect(() => parseSource("ftp://example.com/x")).toThrow(/npm package, a URL/);
    expect(() => parseSource("a/b/c")).toThrow(/npm package, a URL/);
    expect(() => parseSource("npm:Bad Name")).toThrow(/npm package, a URL/);
  });
  test("a GitHub link that isn't a repository", () => {
    expect(() => parseSource("https://github.com/acme")).toThrow(/isn't a GitHub repository/);
    expect(() => parseSource("https://github.com/settings/profile")).toThrow(/isn't a GitHub repository/);
  });
});

describe("serverName", () => {
  test.each([
    ["@modelcontextprotocol/server-filesystem", "filesystem"],
    ["mcp-server-fetch", "fetch"],
    ["weather-mcp", "weather"],
    ["https://mcp.linear.app/mcp", "linear"],
    ["io.github.acme/weather", "weather"],
    ["https://github.com/acme/weather-mcp", "weather"],
    ["my.odd__name_", "my_odd_name"],
    ["mcp-server", "server"],
  ])("%s → %s", (from, name) => expect(serverName(from)).toBe(name));
});

describe("parseAddArgs", () => {
  test("--name, --env, --header, and server args after --", () => {
    expect(parseAddArgs('weather --name wx --env API_KEY --header "Authorization: Bearer ${TOKEN}" -- --units metric')).toEqual({
      source: "weather",
      name: "wx",
      env: ["API_KEY"],
      headers: { Authorization: "Bearer ${TOKEN}" },
      args: ["--units", "metric"],
    });
  });
  test("a secret value is refused: only ${VAR} references are written", () => {
    expect(() => parseAddArgs("weather --env API_KEY=sk-123")).toThrow(/\$\{API_KEY\}/);
    expect(() => parseAddArgs('weather --header "Authorization: Bearer sk-123"')).toThrow(/\$\{/);
    expect(() => parseAddArgs('weather --header "Authorization: Bearer sk-123 ${X}"')).toThrow(/\$\{/);
  });
  test("usage errors", () => {
    expect(() => parseAddArgs("")).toThrow(/Usage/);
    expect(() => parseAddArgs("a b")).toThrow(/Only one source/);
    expect(() => parseAddArgs("a --what")).toThrow(/Unknown option/);
    expect(() => parseAddArgs("a --name")).toThrow(/needs a value/);
    expect(() => parseAddArgs("a --name a__b")).toThrow(/server name/);
    expect(() => parseAddArgs('a --header "x')).toThrow(/isn't closed/);
  });
});

// The APIs, served locally from fixtures written from their documented (and real) shapes: no test reaches the network.
const FIXTURES = join(import.meta.dir, "fixtures", "mcp-install");
const fixture = (name: string) => readFileSync(join(FIXTURES, name), "utf8");
const registryServers = JSON.parse(fixture("registry-servers.json")) as Record<string, unknown>;
let server: ReturnType<typeof Bun.serve>;
const requests: string[] = [];
let searchBody: string | undefined;

beforeAll(() => {
  server = Bun.serve({
    port: 0,
    fetch(req) {
      const url = new URL(req.url);
      requests.push(`${url.pathname}${url.search}`);
      const json = (body: string) => new Response(body, { headers: { "content-type": "application/json" } });
      const path = decodeURIComponent(url.pathname);
      if (path === "/npm/@modelcontextprotocol/server-filesystem") return json(fixture("npm-server-filesystem.json"));
      if (path === "/npm/weather-mcp") return json(fixture("npm-weather-mcp.json"));
      if (path === "/pypi/pypi/mcp-server-fetch/json") return json(fixture("pypi-mcp-server-fetch.json"));
      if (path === "/registry/v0/servers") return json(searchBody ?? fixture("registry-search.json"));
      const entry = /^\/registry\/v0\/servers\/(.+)\/versions\/latest$/.exec(path);
      if (entry && registryServers[entry[1]!]) return json(JSON.stringify(registryServers[entry[1]!]));
      return new Response('{"error":"Not found"}', { status: 404, headers: { "content-type": "application/json" } });
    },
  });
});
afterAll(() => server.stop(true));

const NOW = new Date("2026-10-12T12:00:00Z");
const deps = (over: Partial<ResolveDeps> = {}): ResolveDeps => ({
  get: (url, o) => get(url, { ...o, allowPrivate: true }),
  npmRegistry: `${server.url.origin}/npm`,
  pypi: `${server.url.origin}/pypi`,
  registry: `${server.url.origin}/registry`,
  now: () => NOW,
  ...over,
});
const none = { env: [], args: [] };
const text = (p: Proposal) => [...p.facts, ...p.warnings].join("\n");

describe("npm", () => {
  test("latest is pinned into args, with the facts to judge it by", async () => {
    const p = await resolveNpm({ kind: "npm", name: "@modelcontextprotocol/server-filesystem" }, { args: ["${MARV_PROJECT_DIR}"], env: [] }, deps());
    expect(p.name).toBe("filesystem");
    expect(p.entry).toEqual({ command: "npx", args: ["-y", "@modelcontextprotocol/server-filesystem@2026.1.14", "${MARV_PROJECT_DIR}"] });
    expect(text(p)).toMatch(/2026\.1\.14, published 2026-01-14 by jspahrsummers/);
    expect(text(p)).toMatch(/github\.com\/modelcontextprotocol\/servers \(as the package states it/);
    expect(p.warnings).toEqual([]);
  });
  test("a requested version is used as given", async () => {
    const p = await resolveNpm({ kind: "npm", name: "@modelcontextprotocol/server-filesystem", version: "2025.8.21" }, none, deps());
    expect(p.entry).toMatchObject({ args: ["-y", "@modelcontextprotocol/server-filesystem@2025.8.21"] });
  });
  test("a version that doesn't exist is an error, never a fallback", async () => {
    await expect(resolveNpm({ kind: "npm", name: "@modelcontextprotocol/server-filesystem", version: "9.9.9" }, none, deps())).rejects.toThrow(/9\.9\.9/);
    await expect(resolveNpm({ kind: "npm", name: "@modelcontextprotocol/server-filesystem", version: "^2026" }, none, deps())).rejects.toThrow(/exact version/);
  });
  test("a package that doesn't exist", async () => {
    await expect(resolveNpm({ kind: "npm", name: "nope" }, none, deps())).rejects.toThrow(/no npm package named nope/);
  });
  test("deprecated, brand-new and command-less packages are flagged; stranger-written text is made printable", async () => {
    const p = await resolveNpm({ kind: "npm", name: "weather-mcp" }, none, deps());
    expect(p.warnings.join("\n")).toMatch(/deprecated: Use @acme\/weather instead/);
    expect(p.warnings.join("\n")).toMatch(/first published 2 days ago/);
    expect(p.warnings.join("\n")).toMatch(/declares no command/);
    expect(p.facts.join("\n")).not.toContain("\u001b");
  });
  test("--env becomes ${NAME}", async () => {
    const p = await resolveNpm({ kind: "npm", name: "weather-mcp" }, { args: [], env: ["API_KEY"] }, deps());
    expect(p.name).toBe("weather");
    expect(p.entry).toMatchObject({ env: { API_KEY: "${API_KEY}" } });
  });
});

describe("pypi", () => {
  test("uvx <pkg>@<version>", async () => {
    const p = await resolvePypi({ kind: "pypi", name: "mcp-server-fetch" }, none, deps());
    expect(p.name).toBe("fetch");
    expect(p.entry).toEqual({ command: "uvx", args: ["mcp-server-fetch@2025.4.7"] });
    expect(text(p)).toMatch(/mcp-server-fetch 2025\.4\.7, uploaded 2025-04-07/);
    expect(text(p)).toMatch(/github\.com\/modelcontextprotocol\/servers\/tree\/main\/src\/fetch/);
  });
  test("a yanked version is flagged; a missing one is an error", async () => {
    const p = await resolvePypi({ kind: "pypi", name: "mcp-server-fetch", version: "2025.1.14" }, none, deps());
    expect(p.warnings.join("\n")).toMatch(/yanked: broken build/);
    await expect(resolvePypi({ kind: "pypi", name: "mcp-server-fetch", version: "1.2.3" }, none, deps())).rejects.toThrow(/no version 1\.2\.3/);
    await expect(resolvePypi({ kind: "pypi", name: "nope" }, none, deps())).rejects.toThrow(/no PyPI package/);
  });
});

describe("http", () => {
  test("no fetch, headers as given (already checked to be ${VAR})", () => {
    expect(httpProposal({ kind: "http", url: "https://mcp.linear.app/mcp" }, { name: "linear", headers: { Authorization: "Bearer ${LINEAR_TOKEN}" } }).entry).toEqual({
      type: "http",
      url: "https://mcp.linear.app/mcp",
      headers: { Authorization: "Bearer ${LINEAR_TOKEN}" },
    });
  });
  test("through resolveAdd: nothing is fetched", async () => {
    const before = requests.length;
    const p = (await resolveAdd(parseAddArgs("https://mcp.linear.app/mcp"), deps())) as Proposal;
    expect(p).toMatchObject({ name: "linear", entry: { type: "http", url: "https://mcp.linear.app/mcp" } });
    expect(requests.length).toBe(before);
  });
  test("options that don't fit the source are refused", async () => {
    await expect(resolveAdd(parseAddArgs("https://mcp.linear.app/mcp --env X"), deps())).rejects.toThrow(/--header/);
    await expect(resolveAdd(parseAddArgs('weather-mcp --header "Authorization: Bearer ${X}"'), deps())).rejects.toThrow(/--env/);
  });
});

describe("the MCP Registry", () => {
  test("search: latest, active entries only, at most 10", async () => {
    searchBody = undefined;
    const hits = await searchRegistry("files", deps());
    expect(hits.map((h) => h.name)).toEqual(["io.modelcontextprotocol/filesystem", "com.pulsemcp/remote-filesystem"]);
    expect(hits[0]).toEqual({
      name: "io.modelcontextprotocol/filesystem",
      description: "Secure file operations with configurable access controls.",
      version: "2026.1.14",
      repository: "github.com/modelcontextprotocol/servers",
    });
    expect(requests.at(-1)).toBe("/registry/v0/servers?search=files&version=latest&limit=30");
    const many = Array.from({ length: 14 }, (_, i) => ({ server: { name: `com.example/s${i}`, version: "1.0.0" } }));
    searchBody = JSON.stringify({ servers: many });
    expect(await searchRegistry("s", deps())).toHaveLength(10);
    searchBody = "<html>proxy</html>";
    await expect(searchRegistry("s", deps())).rejects.toThrow(/doesn't understand/);
    searchBody = undefined;
  });

  test("an npm package goes through the npm resolver at the registry's version", async () => {
    const p = await resolveRegistry({ kind: "registry", name: "io.modelcontextprotocol/filesystem" }, { env: [], args: ["/data"] }, deps());
    expect(p.name).toBe("filesystem");
    expect(p.entry).toEqual({
      command: "npx",
      args: ["-y", "@modelcontextprotocol/server-filesystem@2025.8.21", "--stdio", "--mode", "safe", "/data"],
      env: { FS_TOKEN: "${FS_TOKEN}", FS_LEVEL: "${FS_LEVEL:-info}" },
    });
    expect(text(p)).toMatch(/2025\.8\.21, published 2025-08-21/); // npm's facts
    expect(text(p)).toMatch(/registry +io\.modelcontextprotocol\/filesystem 2025\.8\.21 \(namespace verified: the publisher proved it controls modelcontextprotocol\.io\)/);
    expect(text(p)).toMatch(/optional +FS_DEBUG/);
  });

  test("a required argument with no value must come after --", async () => {
    await expect(resolveRegistry({ kind: "registry", name: "io.modelcontextprotocol/filesystem" }, none, deps())).rejects.toThrow(/<root_dir> \(The folder it may read\).*after --/);
  });

  test("a pypi package (an OCI one listed first is skipped)", async () => {
    const p = await resolveRegistry({ kind: "registry", name: "io.github.acme/fetch" }, { env: ["EXTRA"], args: [] }, deps());
    expect(p.entry).toEqual({ command: "uvx", args: ["mcp-server-fetch@2025.4.7"], env: { EXTRA: "${EXTRA}" } });
    expect(text(p)).toMatch(/signed in to GitHub as acme/);
  });

  test("a remote: streamable-http, its headers as ${VAR} references", async () => {
    const p = await resolveRegistry({ kind: "registry", name: "ai.smithery/github" }, none, deps());
    expect(p.entry).toEqual({
      type: "http",
      url: "https://server.smithery.ai/github/mcp",
      headers: { Authorization: "Bearer ${SMITHERY_API_KEY}", "X-Api-Key": "${X_API_KEY}" },
    });
  });

  test("Docker only, deprecated, unknown", async () => {
    await expect(resolveRegistry({ kind: "registry", name: "io.github.acme/docker-only" }, none, deps())).rejects.toThrow(
      "Docker images aren't supported yet; the entry says: docker run ghcr.io/acme/docker-only:1.0.0",
    );
    await expect(resolveRegistry({ kind: "registry", name: "com.example/deprecated" }, none, deps())).rejects.toThrow(/deprecated/);
    await expect(resolveRegistry({ kind: "registry", name: "com.example/nope" }, none, deps())).rejects.toThrow(/\/mcp search/);
  });
});

describe("GitHub: the README nominates, the resolver decides", () => {
  test("takes only the package name or URL from a README's mcpServers block", () => {
    const md = "```json\n{\"mcpServers\":{\"wx\":{\"command\":\"npx\",\"args\":[\"-y\",\"weather-mcp\",\"--evil\"],\"env\":{\"WX_KEY\":\"sk-x\"}}}}\n```";
    expect(readmeCandidates(md)).toEqual([{ source: { kind: "npm", name: "weather-mcp" }, envHints: ["WX_KEY"] }]);
  });
  test("uvx and https entries; docker and local builds are reported, not used", () => {
    const md = [
      "# Weather",
      "```json\n{\"mcpServers\":{\"a\":{\"command\":\"uvx\",\"args\":[\"mcp-server-fetch\"]}}}\n```",
      "```jsonc\n{\"servers\":{\"b\":{\"type\":\"http\",\"url\":\"https://mcp.example.com/mcp\"}}}\n```",
      "```json\n{\"mcpServers\":{\"c\":{\"command\":\"docker\",\"args\":[\"run\",\"-i\",\"acme/wx\"]}}}\n```",
      "```json\n{\"command\":\"node\",\"args\":[\"build/index.js\"]}\n```",
      "```json\nnot json\n```",
      "```json\n{\"mcpServers\":{\"d\":{\"type\":\"http\",\"url\":\"http://example.com/mcp\"}}}\n```",
    ].join("\n\n");
    expect(readmeCandidates(md)).toEqual([
      { source: { kind: "pypi", name: "mcp-server-fetch" }, envHints: [] },
      { source: { kind: "http", url: "https://mcp.example.com/mcp" }, envHints: [] },
      { unsupported: "docker run -i acme/wx" },
      { unsupported: "node build/index.js" },
      { unsupported: "http://example.com/mcp" },
    ]);
  });

  const readme = (md: string | null) => async () => md;
  const repo = { kind: "github", owner: "acme", repo: "weather-mcp" } as const;
  const block = (servers: unknown) => "```json\n" + JSON.stringify({ mcpServers: servers }) + "\n```";

  test("one candidate → its proposal; a repository mismatch comes first", async () => {
    const md = block({ wx: { command: "npx", args: ["-y", "weather-mcp", "--evil"], env: { WX_KEY: "sk-x" } } });
    const p = (await resolveGithub(repo, none, deps({ readme: readme(md) }))) as Proposal;
    expect(p.entry).toEqual({ command: "npx", args: ["-y", "weather-mcp@0.2.0"] }); // --evil and sk-x dropped
    expect(p.facts[0]).toMatch(/^mismatch +the package says its repository is github\.com\/someone-else\/weather, not github\.com\/acme\/weather-mcp/);
    expect(p.warnings[0]).toMatch(/someone-else/);
    expect(text(p)).toMatch(/WX_KEY: not written unless you pass --env WX_KEY/);
    expect(JSON.stringify(p)).not.toContain("sk-x");
  });
  test("a matching repository isn't flagged", async () => {
    const p = (await resolveGithub({ kind: "github", owner: "modelcontextprotocol", repo: "servers" }, none, deps({ readme: readme(block({ f: { command: "npx", args: ["-y", "@modelcontextprotocol/server-filesystem"] } })) }))) as Proposal;
    expect(p.facts[0]).toMatch(/^github +the README of github\.com\/modelcontextprotocol\/servers names @modelcontextprotocol\/server-filesystem/);
    expect(p.warnings).toEqual([]);
  });
  test("several → the commands to choose from", async () => {
    const md = block({ a: { command: "npx", args: ["-y", "weather-mcp"] }, b: { command: "uvx", args: ["mcp-server-fetch"] } });
    expect(await resolveGithub(repo, none, deps({ readme: readme(md) }))).toEqual({ choices: ["/mcp add weather-mcp", "/mcp add pypi:mcp-server-fetch"] });
  });
  test("none → an error naming what was found", async () => {
    const md = block({ c: { command: "docker", args: ["run", "acme/wx"] } });
    await expect(resolveGithub(repo, none, deps({ readme: readme(md) }))).rejects.toThrow(/`docker run acme\/wx`: that needs building or Docker/);
    await expect(resolveGithub(repo, none, deps({ readme: readme(null) }))).rejects.toThrow(/has no README/);
  });
});

describe("the approval", () => {
  const proposal: Proposal = {
    name: "filesystem",
    entry: { command: "npx", args: ["-y", "@modelcontextprotocol/server-filesystem@2026.1.14", "${MARV_PROJECT_DIR}"], env: { GITHUB_TOKEN: "${GITHUB_TOKEN}", HOME_DIR: "${HOME_DIR}" } },
    facts: ["npm      @modelcontextprotocol/server-filesystem 2026.1.14"],
    warnings: ["brand new"],
  };
  test("shows the command and the exact JSON written, every variable read, and never offers 'don't ask again'", () => {
    const { request, json } = approvalFor(proposal, { env: { GITHUB_TOKEN: "x" }, projectServers: ["filesystem"] });
    expect(json).toBe(JSON.stringify(proposal.entry));
    expect(request.preview.command).toBe(displayOf(proposal.entry));
    expect(request.preview.text).toContain(JSON.stringify(proposal.entry));
    expect(request.preview.text).toContain("npm      @modelcontextprotocol/server-filesystem 2026.1.14");
    expect(request.preview.text).toMatch(/reads +GITHUB_TOKEN \(set\)/);
    expect(request.preview.text).toMatch(/reads +HOME_DIR \(not set: it won't start until you set it and restart Marv\)/);
    expect(request.preview.text).not.toMatch(/reads +MARV_PROJECT_DIR/);
    expect(request.preview.text).toMatch(/this project's \.mcp\.json also has a server "filesystem": it will be ignored/);
    expect(request.preview.warning).toBe("brand new");
    expect(request.once).toBe(true);
    expect(approvalFor(proposal, { env: {} }).request.scope.key).not.toBe(request.scope.key);
  });
  test("an entry the screen can't show as written is refused", () => {
    for (const bad of ["a\u009bb", "a\u202eb"]) {
      expect(() => approvalFor({ ...proposal, entry: { command: "npx", args: [bad] } }, { env: {} })).toThrow(/control characters/);
    }
    // C0 controls are escaped by JSON itself: shown and written the same.
    expect(approvalFor({ ...proposal, entry: { command: "npx", args: ["a\u001bb"] } }, { env: {} }).json).toContain("\\u001b");
  });
  test("a remote server: its URL and headers, no command", () => {
    const remote = httpProposal({ kind: "http", url: "https://mcp.linear.app/mcp" }, { name: "linear", headers: { Authorization: "Bearer ${LINEAR_TOKEN}" } });
    const { request } = approvalFor(remote, { env: {} });
    expect(request.preview.command).toBeUndefined();
    expect(request.preview.text).toContain("https://mcp.linear.app/mcp · headers Authorization: Bearer ${LINEAR_TOKEN}");
    expect(request.preview.text).toMatch(/reads +LINEAR_TOKEN \(not set/);
  });
});
