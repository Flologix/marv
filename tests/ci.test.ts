// The CI workflow (.github/workflows/ci.yml) is data GitHub reads; these checks keep it honest: it must trigger on
// pull requests and pushes to main, run the three documented commands, make the sandbox tests really run (bwrap
// installed and Ubuntu 24.04's unprivileged-userns restriction lifted, with the availability said in the job
// summary), and cache installs by bun.lock. CI can't run CI from here: this only checks the workflow's content.
import { readFileSync } from "node:fs";
import { join } from "node:path";
import { describe, expect, test } from "bun:test";

const ROOT = join(import.meta.dir, "..");
const yml = () => readFileSync(join(ROOT, ".github", "workflows", "ci.yml"), "utf8");

describe("the CI workflow", () => {
  test("runs on pull requests and pushes to main", () => {
    // Bun's YAML parser keeps `on` a string (some read it as the boolean true); accept either spelling, and require
    // the file to quote it so a YAML 1.1 parser can't turn the triggers off.
    const parsed = Bun.YAML.parse(yml()) as Record<string, unknown> | null;
    const triggers = (parsed?.["on"] ?? parsed?.["true"]) as Record<string, unknown> | undefined;
    expect(triggers).toBeDefined();
    expect(Object.keys(triggers!).sort()).toEqual(["pull_request", "push"]);
    expect(triggers!["push"]).toEqual({ branches: ["main"] });
  });

  test("runs the documented commands, in that order", () => {
    const text = yml();
    const order = ["bun install", "bun run typecheck", "bun test", "bun run eval --verify"].map((command) => text.indexOf(command));
    expect(order).not.toContain(-1);
    expect(order).toEqual([...order].sort((a, b) => a - b));
  });

  test("makes the sandbox tests run: bwrap, the userns sysctl, and the availability in the summary", () => {
    const text = yml();
    // Ubuntu 24.04 runners refuse unprivileged user namespaces (AppArmor), which bwrap needs.
    expect(text).toContain("apt-get install -y bubblewrap");
    expect(text).toContain("kernel.apparmor_restrict_unprivileged_userns=0");
    expect(text).toContain("GITHUB_STEP_SUMMARY");
    // The summary is written even when a step before it failed, so a red run still says whether the sandbox ran.
    expect(text).toContain("if: always()");
  });

  test("covers what the runners must provide for a few tests: git, python3, and node for the tsc launcher", () => {
    const text = yml();
    expect(text).toContain("node --version");
    expect(text).toContain("python3 --version");
    expect(text).toContain("git --version");
  });

  test("pins ubuntu-24.04 (the sysctl workaround is that image's) and sets up Bun via oven-sh/setup-bun", () => {
    const text = yml();
    expect(text).toContain("runs-on: ubuntu-24.04");
    expect(text).toContain("oven-sh/setup-bun");
  });

  test("fails before the tests when bwrap can't start a sandbox, so a green run never skipped the sandbox tests", () => {
    const text = yml();
    const steps = text.split("\n");
    const require = steps.findIndex((line) => line.includes("name: Require a working sandbox"));
    const tests = steps.findIndex((line) => line.includes("run: bun test"));
    expect(require).toBeGreaterThan(-1);
    expect(require).toBeLessThan(tests);
    expect(steps[require + 1]).toContain("bwrap ");
  });

  test("pins every action to a full commit hash, so a moved tag can't change what runs", () => {
    const text = yml();
    const uses = [...text.matchAll(/uses:\s*(\S+)/g)].map((m) => m[1]!);
    expect(uses.length).toBeGreaterThan(0);
    for (const action of uses) expect(action).toMatch(/@[0-9a-f]{40}$/);
  });

  test("caches installs keyed on bun.lock", () => {
    const text = yml();
    expect(text).toContain("actions/cache");
    expect(text).toContain("hashFiles('bun.lock')");
    expect(text).toContain("~/.bun/install/cache");
  });
});
