// The GitHub docs (README's "Connect GitHub" section) and the example skill are instructions a fresh user follows,
// so they're tested like data (tests/ci.test.ts does the same for the workflow file): every config the section
// shows must parse the way Marv parses MCP config, and the skill must load with the skill loader, so a broken
// frontmatter or a config Marv would reject fails here instead of in front of a user (issue #27).
import { describe, expect, test } from "bun:test";
import { copyFile, mkdir, mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { parseMcpServers } from "../src/mcp/config.ts";
import { loadSkills } from "../src/skills.ts";

const ROOT = join(import.meta.dir, "..");
const readme = () => Bun.file(join(ROOT, "README.md")).text();

/** The ```json blocks of one README section (## Heading … up to the next ## ). */
function jsonBlocks(text: string, heading: string): string[] {
  const start = text.indexOf(`## ${heading}`);
  if (start < 0) throw new Error(`README has no "## ${heading}" section`);
  const section = text.slice(start, text.indexOf("\n## ", start + 1));
  return [...section.matchAll(/```json\n([\s\S]*?)```/g)].map((m) => m[1]!);
}

describe("README's Connect GitHub section", () => {
  test("every config it shows parses as MCP config, reading the token from the environment", async () => {
    const blocks = jsonBlocks(await readme(), "Connect GitHub");
    expect(blocks.length).toBeGreaterThanOrEqual(2);
    let docker = 0;
    for (const block of blocks) {
      const config = JSON.parse(block) as { mcpServers: Record<string, unknown> };
      const { servers, problems } = parseMcpServers(config.mcpServers, { MARV_GITHUB_TOKEN: "token", MARV_GITHUB_CLIENT_SECRET: "secret" });
      expect(problems).toEqual([]);
      expect(servers).toHaveLength(1);
      expect(servers[0]!.name).toBe("github");
      const transport = servers[0]!.transport;
      if (transport.type === "http") {
        // The OAuth login (#18): the hosted server, your own app's client, the secret from the environment.
        expect(transport).toMatchObject({ url: "https://api.githubcopilot.com/mcp/", oauth: { clientSecret: "secret", callbackPort: 33418 } });
        expect(servers[0]!.reads).toEqual(["MARV_GITHUB_CLIENT_SECRET"]);
        continue;
      }
      expect(transport).toMatchObject({
        command: "docker",
        env: { GITHUB_PERSONAL_ACCESS_TOKEN: "token" }, // ${MARV_GITHUB_TOKEN} was filled in
      });
      expect(transport.args?.join(" ")).toContain("ghcr.io/github/github-mcp-server");
      expect(servers[0]!.reads).toEqual(["MARV_GITHUB_TOKEN"]);
      if (docker++ > 0) expect(transport.args?.join(" ")).toContain("GITHUB_TOOLSETS");
    }
    expect(docker).toBeGreaterThanOrEqual(2);
  });

  test("a missing token is reported naming the variable (what /mcp shows when the export is forgotten)", async () => {
    const config = JSON.parse(jsonBlocks(await readme(), "Connect GitHub")[0]!) as { mcpServers: Record<string, unknown> };
    const { servers, problems } = parseMcpServers(config.mcpServers, {});
    expect(servers).toEqual([]);
    expect(problems[0]).toContain("MARV_GITHUB_TOKEN");
  });

  test("links the example skill", async () => {
    expect(await readme()).toContain("examples/skills/github/SKILL.md");
  });
});

describe("README's Add an MCP server section", () => {
  test("the login example parses, its client secret read from the environment (#18)", async () => {
    const blocks = jsonBlocks(await readme(), "Add an MCP server");
    expect(blocks).toHaveLength(1);
    const config = JSON.parse(blocks[0]!) as { mcpServers: Record<string, unknown> };
    const { servers, problems } = parseMcpServers(config.mcpServers, { EXAMPLE_CLIENT_SECRET: "s" });
    expect(problems).toEqual([]);
    expect(servers[0]!.transport).toMatchObject({ type: "http", oauth: { clientId: "abc123", clientSecret: "s", callbackPort: 33418, scope: "read write" } });
  });
});

describe("the github example skill", () => {
  test("loads with the skill loader (broken frontmatter would fail here)", async () => {
    const home = await mkdtemp(join(tmpdir(), "marv-github-skill-"));
    try {
      await mkdir(join(home, ".marv", "skills", "github"), { recursive: true });
      await copyFile(join(ROOT, "examples", "skills", "github", "SKILL.md"), join(home, ".marv", "skills", "github", "SKILL.md"));
      const { skills, problems } = await loadSkills({ home });
      expect(problems).toEqual([]);
      const skill = skills.find((s) => s.name === "github")!;
      expect(skill.description).toContain("GitHub");
      expect(skill.body).toContain("mcp__github__");
      expect(skill.body).toContain("Never ask for or print tokens");
      expect(skill.body).toContain('merge_method: "merge"');
    } finally {
      await rm(home, { recursive: true, force: true });
    }
  });
});
