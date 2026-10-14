// The github tool's classification (#47): what gh argv runs freely, what asks, what's never run. Read as gh reads
// it: the command path first, then flags (`--flag=value`, `-Rvalue` and `-R value` alike).
import { describe, expect, test } from "bun:test";
import { classify, originHost } from "../src/tools/github/allow.ts";

const at = (args: string[], host = "github.com") => classify(args, { host });
const kindOf = (args: string[], host?: string) => at(args, host).kind;

describe("classify: reads run freely", () => {
  const reads = [
    ["pr", "list"],
    ["pr", "view", "12"],
    ["pr", "view", "12", "--json", "title,body", "--comments"],
    ["pr", "checks", "12"],
    ["pr", "diff", "12"],
    ["pr", "status"],
    ["issue", "list", "--state", "open", "-L", "5"],
    ["issue", "view", "3"],
    ["issue", "status"],
    ["run", "list"],
    ["run", "view", "123", "--log-failed"],
    ["run", "view", "123", "--log"],
    ["workflow", "list"],
    ["workflow", "view", "ci.yml"],
    ["repo", "view"],
    ["repo", "view", "owner/repo"],
    ["release", "list"],
    ["release", "view", "v1.0"],
    ["label", "list"],
    ["search", "issues", "auth bug"],
    ["search", "prs", "--author", "me"],
    ["search", "repos", "marv"],
    ["search", "code", "token"],
    ["search", "commits", "fix"],
    ["status"],
    ["pr", "view", "auth"], // a positional, not the auth command
  ];
  for (const args of reads) test(args.join(" "), () => expect(at(args)).toMatchObject({ kind: "read" }));
});

describe("classify: writes ask, scoped by command", () => {
  const writes = [
    "pr comment",
    "pr create",
    "pr edit",
    "pr close",
    "pr reopen",
    "pr merge",
    "pr review",
    "pr ready",
    "issue comment",
    "issue create",
    "issue edit",
    "issue close",
    "issue reopen",
    "release create",
    "release edit",
    "run rerun",
    "run cancel",
    "workflow run",
    "label create",
  ];
  for (const path of writes) {
    test(path, () => expect(at([...path.split(" "), "1"])).toMatchObject({ kind: "write", scope: `github:${path}` }));
  }
});

describe("classify: refused", () => {
  const refused: [string[], RegExp][] = [
    [["auth", "token"], /never run/],
    [["auth", "status", "--show-token"], /never run/],
    [["auth", "login"], /never run/],
    [["-h", "auth", "token"], /command first/],
    [["--hostname", "x", "auth", "token"], /command first/],
    [["config", "get", "oauth_token"], /never run/],
    [["extension", "exec", "x"], /never run/],
    [["ext", "exec", "x"], /isn't something/],
    [["alias", "set", "x", "y"], /never run/],
    [["myalias"], /isn't something/],
    [["secret", "list"], /never run/],
    [["variable", "list"], /never run/],
    [["ssh-key", "list"], /never run/],
    [["gpg-key", "list"], /never run/],
    [["codespace", "list"], /isn't run/],
    [["gist", "view", "1"], /isn't run/],
    [["org", "list"], /isn't run/],
    [["browse"], /isn't run/],
    [["pr", "view", "12", "--web"], /browser/],
    [["pr", "view", "12", "-w"], /browser/],
    [["issue", "list", "--web=true"], /browser/],
    [["pr", "create", "-fw"], /separately/],
    [["run", "download", "1"], /download/],
    [["release", "download", "v1"], /download/],
    [["repo", "clone", "o/r"], /git outside the sandbox/],
    [["pr", "checkout", "12"], /git outside the sandbox/],
    [["repo", "create", "x"], /isn't run/],
    [["repo", "delete", "o/r"], /isn't run/],
    [["repo", "fork"], /isn't run/],
    [["repo", "edit"], /isn't run/],
    [["repo", "sync"], /isn't run/],
    [["pr"], /isn't something/],
    [["pr", "frobnicate"], /isn't something/],
    [[], /at least/],
  ];
  for (const [args, reason] of refused) {
    test(args.join(" ") || "(empty)", () => {
      const result = at(args);
      expect(result.kind).toBe("refused");
      if (result.kind === "refused") expect(result.reason).toMatch(reason);
    });
  }
  test("an unknown word lists what runs", () => {
    const result = at(["frob"]);
    expect(result.kind === "refused" && result.reason).toContain("pr view");
  });
  test("auth's reason says why", () => {
    expect(at(["auth", "token"])).toEqual({ kind: "refused", reason: expect.stringContaining("Marv keeps the token away from the conversation") });
  });
});

describe("classify: gh api", () => {
  const cases: [string[], ReturnType<typeof kindOf>, string?][] = [
    [["api", "repos/o/r/pulls"], "read"],
    [["api", "-X", "GET", "repos/o/r/pulls"], "read"],
    [["api", "--method=GET", "repos/o/r/pulls"], "read"],
    [["api", "--method", "get", "repos/o/r/pulls"], "read"],
    [["api", "-XGET", "repos/o/r"], "read"],
    [["api", "repos/{owner}/{repo}/pulls", "--paginate", "--jq", ".[].title"], "read"],
    [["api", "repos/o/r", "-q", ".name", "-i", "--slurp", "--cache", "1h", "-t", "{{.name}}", "--paginate"], "read"],
    [["api", "-H", "Accept: application/vnd.github.raw", "repos/o/r/contents/x"], "read"],
    [["api", "--header=X-GitHub-Api-Version: 2022-11-28", "repos/o/r"], "read"],
    [["api", "repos/o/r/issues?state=open"], "read"],
    [["api", "-X", "POST", "repos/o/r/issues"], "write", "github:api POST"],
    [["api", "--method", "delete", "repos/o/r"], "write", "github:api DELETE"],
    [["api", "repos/o/r/issues", "-f", "title=x"], "write", "github:api POST"],
    [["api", "repos/o/r/issues", "--raw-field=title=x"], "write", "github:api POST"],
    [["api", "repos/o/r/issues", "-F", "x=@/etc/passwd"], "write", "github:api POST"],
    [["api", "-X", "GET", "search/issues", "-f", "q=x"], "write", "github:api GET"],
    [["api", "graphql", "-f", "query={viewer{login}}"], "write", "github:api graphql"],
    [["api", "graphql"], "write", "github:api graphql"],
    [["api", "repos/o/r/actions/secrets"], "sensitive", "github:api sensitive"],
    [["api", "repos/o/r/actions/variables"], "sensitive", "github:api sensitive"],
    [["api", "repos/o/r/keys"], "sensitive", "github:api sensitive"],
    [["api", "repos/o/r/hooks"], "sensitive", "github:api sensitive"],
    [["api", "user/installations"], "sensitive", "github:api sensitive"],
    [["api", "repos/o/r/invitations"], "sensitive", "github:api sensitive"],
    [["api", "user/emails"], "sensitive", "github:api sensitive"],
    [["api", "/user/emails"], "sensitive", "github:api sensitive"],
    [["api", "authorizations"], "sensitive", "github:api sensitive"],
    [["api", "repos/o/r/actions/runners/registration-token"], "sensitive", "github:api sensitive"],
    [["api", "repos/o/r/Secrets"], "sensitive", "github:api sensitive"],
  ];
  for (const [args, kind, scope] of cases) {
    test(args.join(" "), () => {
      const result = at(args);
      expect(result.kind).toBe(kind);
      if (scope) expect(result).toMatchObject({ scope });
    });
  }
  const refused: [string[], RegExp][] = [
    [["api", "--input", "f", "repos/o/r/issues"], /--input/],
    [["api", "--input=f", "repos/o/r/issues"], /--input/],
    [["api", "--verbose", "repos/o/r"], /--verbose/],
    [["api", "-H", "X-HTTP-Method-Override: DELETE", "repos/o/r"], /header/i],
    [["api", "-H", "Authorization: token x", "repos/o/r"], /header/i],
    [["api", "--preview", "x", "repos/o/r"], /--preview/],
    [["api"], /endpoint/],
    [["api", "repos/o/r", "user"], /one endpoint/],
    [["api", "https://evil.example/x"], /another host|path/],
    [["api", "//evil.example/x"], /path/],
    [["api", "repos/../user/emails"], /path/],
    [["api", "repos/o/r/secre%74s"], /path/],
    [["api", "-iX", "POST", "repos/o/r"], /separately/],
  ];
  for (const [args, reason] of refused) {
    test(`refused: ${args.join(" ")}`, () => {
      const result = at(args);
      expect(result.kind).toBe("refused");
      if (result.kind === "refused") expect(result.reason).toMatch(reason);
    });
  }
});

describe("classify: hosts", () => {
  test("--hostname only for the project's host", () => {
    expect(kindOf(["api", "--hostname", "ghe.corp", "repos/o/r"], "ghe.corp")).toBe("read");
    expect(kindOf(["api", "--hostname=ghe.corp", "repos/o/r"], "ghe.corp")).toBe("read");
    expect(kindOf(["api", "--hostname", "ghe.corp", "repos/o/r"], "github.com")).toBe("refused");
    expect(kindOf(["api", "--hostname", "GitHub.com", "repos/o/r"], "github.com")).toBe("read");
  });
  test("-R with a host part only for the project's host; owner/repo anywhere on it", () => {
    expect(kindOf(["pr", "list", "-R", "other/repo"])).toBe("read");
    expect(kindOf(["pr", "list", "--repo=other/repo"])).toBe("read");
    expect(kindOf(["pr", "list", "-Rother/repo"])).toBe("read");
    expect(kindOf(["pr", "list", "-R", "other.host/o/r"])).toBe("refused");
    expect(kindOf(["pr", "list", "-Rother.host/o/r"])).toBe("refused");
    expect(kindOf(["pr", "list", "--repo", "other.host/o/r"])).toBe("refused");
    expect(kindOf(["pr", "list", "-R", "ghe.corp/o/r"], "ghe.corp")).toBe("read");
    expect(kindOf(["pr", "list", "-R", "https://other.host/o/r"])).toBe("refused");
    expect(kindOf(["pr", "list", "-R", "https://github.com/o/r"])).toBe("read");
    expect(kindOf(["pr", "list", "-R", "a/b/c/d"])).toBe("refused");
    expect(kindOf(["pr", "merge", "1", "-R", "other.host/o/r"])).toBe("refused");
  });
  test("a URL argument names its host", () => {
    expect(kindOf(["pr", "view", "https://github.com/o/r/pull/1"])).toBe("read");
    expect(kindOf(["pr", "view", "https://other.host/o/r/pull/1"])).toBe("refused");
    expect(kindOf(["issue", "view", "git@other.host:o/r"])).toBe("refused");
    expect(kindOf(["repo", "view", "other.host/o/r"])).toBe("refused");
    expect(kindOf(["repo", "view", "github.com/o/r"])).toBe("read");
  });
  test("without a known host, anything naming one is refused", () => {
    expect(classify(["pr", "list", "-R", "github.com/o/r"], { host: null }).kind).toBe("refused");
    expect(classify(["pr", "list", "-R", "o/r"], { host: null }).kind).toBe("read");
  });
  test("after --, everything is positional", () => {
    expect(kindOf(["search", "issues", "--", "--web"])).toBe("read");
  });
});

describe("originHost", () => {
  test("reads the host as gh would", () => {
    expect(originHost("https://github.com/o/r.git")).toBe("github.com");
    expect(originHost("git@ghe.corp:o/r.git")).toBe("ghe.corp");
    expect(originHost("ssh://git@GHE.corp:22/o/r")).toBe("ghe.corp");
    expect(originHost("https://user:pw@ghe.corp/o/r")).toBe("ghe.corp");
    expect(originHost("/local/path")).toBeNull();
    expect(originHost("")).toBeNull();
  });
});
