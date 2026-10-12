import { expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { match } from "./src/router.js";

const raw = readFileSync("data/routes.json", "utf8");
const parsed = JSON.parse(raw);
// What the file held when the task started. The check folder is copied into the agent's workspace after the run, so
// its own copy of the original (shipped beside this test) is what the agent's changes are compared against.
const before = JSON.parse(readFileSync(new URL("./routes-before.json", import.meta.url), "utf8")).routes;

const lines = raw.trimEnd().split("\n");

test("the new route works, and only with its methods", () => {
  expect(match("GET", "/tenants/4/invites")).toEqual({ handler: "tenantInvites", params: { id: "4" } });
  expect(match("POST", "/tenants/4/invites")).toEqual({ handler: "tenantInvites", params: { id: "4" } });
  expect(match("DELETE", "/tenants/4/invites")).toBeNull();
});

test("the file is in the required shape: 2-space indent, one route object per line", () => {
  expect(lines[0]).toBe("{");
  expect(lines[1]).toBe('  "routes": [');
  expect(lines.at(-2)).toBe("  ]");
  expect(lines.at(-1)).toBe("}");
  const routeLines = lines.slice(2, -2);
  expect(routeLines).toHaveLength(parsed.routes.length);
  for (const [i, line] of routeLines.entries()) {
    expect(line.startsWith('    { "path"')).toBe(true);
    expect(JSON.parse(line.trim().replace(/,$/, ""))).toEqual(parsed.routes[i]);
  }
});

test("every route's fields are in the order path, handler, methods", () => {
  for (const line of lines.slice(2, -2)) {
    const route = JSON.parse(line.trim().replace(/,$/, ""));
    expect(Object.keys(route)).toEqual(["path", "handler", "methods"]);
  }
});

test("nothing was lost, reordered or changed (plus the new route)", () => {
  expect(parsed.routes.filter((r) => r.path !== "/tenants/:id/invites")).toEqual(before);
  expect(parsed.routes.at(-1)).toEqual({ path: "/tenants/:id/invites", handler: "tenantInvites", methods: ["GET", "POST"] });
});

test("a sample of the old routes still resolves", () => {
  expect(match("GET", "/area0/home")).toEqual({ handler: "home", params: {} });
  expect(match("PATCH", "/orgs/7/userPosts/12")).toEqual({ handler: "userPostsItem", params: { orgId: "7", itemId: "12" } });
  expect(match("GET", "/spaces/3/teamMembers/9")).toEqual({ handler: "teamMembersItem", params: { spaceId: "3", itemId: "9" } });
  expect(match("GET", "/area19/4/settings")).toEqual({ handler: "audit", params: { id: "4" } });
  expect(match("GET", "/orgs/7/nope")).toBeNull();
});
