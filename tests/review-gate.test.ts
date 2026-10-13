import { afterEach, beforeEach, expect, test } from "bun:test";
import { chmodSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

// scripts/review-gate.ts against a fake `ocr` that:
// - prints a multi-line --version (as the real one does)
// - records the --from/--to of each review call in a log file
// - accepts env overrides to sleep on review or to fail with review_round_failed
const GATE = join(import.meta.dir, "..", "scripts", "review-gate.ts");
let dir: string;
let repo: string;
let bin: string;

// Build a fake ocr script. Uses regular strings (not backtick templates) so
// shell variable references ($from, $to, $OCR_SLEEP_S) aren't treated as
// TypeScript interpolations.
const fakeOcrScript = (d: string): string =>
  "#!/bin/sh\n" +
  'if [ "$1" = "--version" ]; then printf \'open-code-review v1.0.0 (abc)\\nbuilt at: 2026-01-01\\nhttps://example.com\\n\'; exit 0; fi\n' +
  "# Called as: ocr review --from <base> --to <sha> --format json --audience agent\n" +
  'from=""; to=""\n' +
  'while [ $# -gt 0 ]; do\n' +
  '  case "$1" in\n' +
  '    --from) from="$2"; shift 2 ;;\n' +
  '    --to) to="$2"; shift 2 ;;\n' +
  '    *) shift ;;\n' +
  '  esac\n' +
  "done\n" +
  'echo "from:${from} to:${to}" >> "' + join(d, "reviews") + '"\n' +
  "# Simulate a hang when OCR_SLEEP_S is set\n" +
  'if [ -n "${OCR_SLEEP_S}" ]; then sleep "${OCR_SLEEP_S}"; fi\n' +
  "# Simulate review_round_failed on first call for this commit when OCR_FAIL_ONCE is set\n" +
  'retry_ctrl="' + join(d, "retry_ctrl") + '"\n' +
  'if [ -n "${OCR_FAIL_ONCE}" ] && [ -f "${retry_ctrl}" ]; then\n' +
  "  echo '{\"status\":\"complete\",\"comments\":[],\"warnings\":[{\"type\":\"review_round_failed\",\"message\":\"model timeout on a batch\"}]}'\n" +
  "  exit 0\n" +
  "fi\n" +
  "echo '{\"status\":\"complete\",\"comments\":[]}'\n";

beforeEach(() => {
  dir = mkdtempSync(join(tmpdir(), "marv-gate-"));
  repo = join(dir, "repo");
  bin = join(dir, "bin");
  const sh = (cmd: string[], cwd = dir) => {
    const r = Bun.spawnSync(cmd, { cwd, env: { ...process.env, GIT_CONFIG_GLOBAL: "/dev/null", GIT_CONFIG_NOSYSTEM: "1" } });
    if (r.exitCode !== 0) throw new Error(r.stderr.toString());
  };
  sh(["mkdir", "-p", repo, bin]);
  const git = (...a: string[]) => sh(["git", "-c", "user.email=t@t", "-c", "user.name=t", "-c", "commit.gpgSign=false", ...a], repo);
  git("init", "-q", "-b", "main");
  writeFileSync(join(repo, "a.txt"), "a\n");
  git("add", ".");
  git("commit", "-qm", "base");
  git("checkout", "-qb", "work");
  writeFileSync(join(repo, "a.txt"), "b\n");
  git("commit", "-qam", "change");
});
afterEach(() => rmSync(dir, { recursive: true, force: true }));

// --- Helpers ---

const writeOcr = () => {
  writeFileSync(join(bin, "ocr"), fakeOcrScript(dir));
  chmodSync(join(bin, "ocr"), 0o755);
};

const gate = (env: Record<string, string> = {}) =>
  Bun.spawnSync(["bun", GATE, "--base", "main"], { cwd: repo, env: { ...process.env, PATH: `${bin}:${process.env.PATH}`, REVIEW_GATE: "", ...env } });

const reviewsLog = (): string[] => {
  const p = join(dir, "reviews");
  const content = readFileSync(p, "utf8").trim();
  if (!content) return [];
  return content.split("\n");
};

// Every test uses a "work" branch with one commit ahead of main.
// Some add a second commit; we set up the fake ocr per test.
test("a commit that passed is not reviewed again (ocr's multi-line --version included)", () => {
  writeOcr();
  expect(gate().exitCode).toBe(0);
  expect(gate().exitCode).toBe(0);
  expect(reviewsLog().length).toBe(1);
});

test("incremental review on a branch: new commit after a passed commit reviews only what's new", () => {
  writeOcr();
  // First push: the first commit is new, should be reviewed against base
  expect(gate().exitCode).toBe(0);
  const firstLogs = reviewsLog();
  expect(firstLogs.length).toBe(1);

  // Add a second commit on the branch
  const git = (...a: string[]) => {
    const r = Bun.spawnSync(
      ["git", "-c", "user.email=t@t", "-c", "user.name=t", "-c", "commit.gpgSign=false", ...a],
      { cwd: repo, env: { ...process.env, GIT_CONFIG_GLOBAL: "/dev/null", GIT_CONFIG_NOSYSTEM: "1" } },
    );
    if (r.exitCode !== 0) throw new Error(r.stderr.toString());
  };
  writeFileSync(join(repo, "a.txt"), "c\n");
  git("commit", "-qam", "second change");

  // Second push: should review only the new commit, with --from set to the first commit
  writeOcr();
  expect(gate().exitCode).toBe(0);
  const secondLogs = reviewsLog();
  expect(secondLogs.length).toBe(2);

  // The second review should use the first commit as --from (not "main")
  const secondReview = secondLogs[1]!;
  expect(secondReview).toMatch(/^from:[0-9a-f]{7,40} to:/);
  // The from value should not equal "main" — it should be a commit hash
  expect(secondReview).not.toContain("from:main");
  // It should review what's new: from=<first commit> to=<second commit>
  const firstReview = secondLogs[0]!;
  // Actually: first review is from:main to:<first>, second should be from:<first> to:<second>
  const firstTo = firstReview.match(/to:(\S+)/)![1];
  const secondFrom = secondReview.match(/from:(\S+)/)![1];
  const secondTo = secondReview.match(/to:(\S+)/)![1];
  expect(secondFrom).toBe(firstTo); // second reviews from the first passed commit
  expect(secondTo).not.toBe(firstTo); // to a newer commit

  // A third commit: with two passed ancestors, the review starts at the newer one (the smallest range).
  writeFileSync(join(repo, "a.txt"), "d\n");
  git("commit", "-qam", "third change");
  writeOcr();
  expect(gate().exitCode).toBe(0);
  const thirdReview = reviewsLog()[2]!;
  expect(thirdReview.match(/from:(\S+)/)![1]).toBe(secondTo);
});

test("overall timeout kills ocr and fails closed", () => {
  writeOcr();
  // The fake ocr won't sleep by default, so this test writes a separate ocr
  // that sleeps for 5 seconds, and we set REVIEW_GATE_TIMEOUT_S to 2.
  writeFileSync(
    join(bin, "ocr"),
    "#!/bin/sh\n" +
    'if [ "$1" = "--version" ]; then printf \'open-code-review v1.0.0 (abc)\\nbuilt at: 2026-01-01\\nhttps://example.com\\n\'; exit 0; fi\n' +
    "# Hang for 5 seconds, recording args first\n" +
    'echo "from:fake to:fake" >> "' + join(dir, "reviews") + '"\n' +
    "sleep 5\n" +
    "echo '{\"status\":\"complete\",\"comments\":[]}'\n",
  );
  chmodSync(join(bin, "ocr"), 0o755);

  const result = gate({ REVIEW_GATE_TIMEOUT_S: "2" });
  expect(result.exitCode).toBe(1);
  const stderr = result.stderr.toString();
  expect(stderr).toContain("timed out");
  expect(stderr).toContain("review-gate:");
});

test("retries once on review_round_failed warning, succeeds on retry", () => {
  // The fake ocr fails the first review call with review_round_failed,
  // then succeeds on the second call (detected by retry_ctrl file
  // disappearing after the first attempt).
  writeFileSync(join(dir, "retry_ctrl"), "1\n");
  writeFileSync(
    join(bin, "ocr"),
    "#!/bin/sh\n" +
    'if [ "$1" = "--version" ]; then printf \'open-code-review v1.0.0 (abc)\\nbuilt at: 2026-01-01\\nhttps://example.com\\n\'; exit 0; fi\n' +
    'from=""; to=""\n' +
    'while [ $# -gt 0 ]; do\n' +
    '  case "$1" in\n' +
    '    --from) from="$2"; shift 2 ;;\n' +
    '    --to) to="$2"; shift 2 ;;\n' +
    '    *) shift ;;\n' +
    '  esac\n' +
    "done\n" +
    'echo "from:${from} to:${to}" >> "' + join(dir, "reviews") + '"\n' +
    'if [ -f "' + join(dir, "retry_ctrl") + '" ]; then\n' +
    "  # This is the first review for this commit: fail with model timeout\n" +
    '  rm -f "' + join(dir, "retry_ctrl") + '"\n' +
    "  echo '{\"status\":\"complete\",\"comments\":[],\"warnings\":[{\"type\":\"review_round_failed\",\"message\":\"model timeout on a batch\"}]}'\n" +
    "  exit 0\n" +
    "fi\n" +
    "echo '{\"status\":\"complete\",\"comments\":[]}'\n",
  );
  chmodSync(join(bin, "ocr"), 0o755);

  const result = gate();
  expect(result.exitCode).toBe(0);
  const stderr = result.stderr.toString();
  expect(stderr).toContain("passed");

  // The review should have been run twice: once failing, once succeeding
  expect(reviewsLog().length).toBe(2);
  // Both reviews should be for the same commit (same from/to)
  expect(reviewsLog()[0]).toBe(reviewsLog()[1]);
});

test("mixed findings with retry: only model timeouts are retried", () => {
  // A warning that isn't review_round_failed should NOT be retried — fail closed.
  writeFileSync(
    join(bin, "ocr"),
    "#!/bin/sh\n" +
    'if [ "$1" = "--version" ]; then printf \'open-code-review v1.0.0 (abc)\\nbuilt at: 2026-01-01\\nhttps://example.com\\n\'; exit 0; fi\n' +
    'echo "from:x to:y" >> "' + join(dir, "reviews") + '"\n' +
    "echo '{\"status\":\"complete\",\"comments\":[],\"warnings\":[{\"type\":\"parse_error\",\"message\":\"could not parse file.py\"}]}'\n",
  );
  chmodSync(join(bin, "ocr"), 0o755);

  const result = gate();
  expect(result.exitCode).toBe(1);
  const stderr = result.stderr.toString();
  expect(stderr).toContain("couldn't be reviewed");
  // Only 1 review attempt (no retry)
  expect(reviewsLog().length).toBe(1);
});
test("a retry that completes with a file ocr couldn't review still blocks, and isn't cached as passed", () => {
  writeFileSync(join(dir, "retry_ctrl"), "1\n");
  writeFileSync(
    join(bin, "ocr"),
    "#!/bin/sh\n" +
      'if [ "$1" = "--version" ]; then printf \'open-code-review v1.0.0 (abc)\\nbuilt at: 2026-01-01\\nhttps://example.com\\n\'; exit 0; fi\n' +
      'echo review >> "' + join(dir, "reviews") + '"\n' +
      'if [ -f "' + join(dir, "retry_ctrl") + '" ]; then\n' +
      '  rm -f "' + join(dir, "retry_ctrl") + '"\n' +
      "  echo '{\"status\":\"complete\",\"comments\":[],\"warnings\":[{\"type\":\"review_round_failed\",\"message\":\"model timeout\"}]}'\n" +
      "  exit 0\n" +
      "fi\n" +
      // The retry completes, but a file couldn't be reviewed (not a model timeout).
      "echo '{\"status\":\"complete\",\"comments\":[],\"warnings\":[{\"type\":\"file_failed\",\"message\":\"x.ts\"}]}'\n",
  );
  chmodSync(join(bin, "ocr"), 0o755);
  expect(gate().exitCode).toBe(1);
  // Not cached: the next push reviews again.
  expect(gate().exitCode).toBe(1);
  expect(reviewsLog().length).toBe(3);
});

test("REVIEW_GATE_FULL=1 reviews the whole branch even after an earlier pass", () => {
  writeOcr();
  expect(gate().exitCode).toBe(0);
  writeFileSync(join(repo, "a.txt"), "e\n");
  Bun.spawnSync(["git", "-c", "user.email=t@t", "-c", "user.name=t", "-c", "commit.gpgSign=false", "commit", "-qam", "more"], {
    cwd: repo,
    env: { ...process.env, GIT_CONFIG_GLOBAL: "/dev/null", GIT_CONFIG_NOSYSTEM: "1" },
  });
  expect(gate({ REVIEW_GATE_FULL: "1" }).exitCode).toBe(0);
  const mainSha = Bun.spawnSync(["git", "rev-parse", "main"], { cwd: repo }).stdout.toString().trim();
  expect(reviewsLog().at(-1)).toContain(`from:${mainSha} `); // the merge-base, which is main here
  // And a commit that already passed is reviewed again, in full.
  const before = reviewsLog().length;
  expect(gate({ REVIEW_GATE_FULL: "1" }).exitCode).toBe(0);
  expect(reviewsLog().length).toBe(before + 1);
});

test("eval tasks' starting repos are excluded from the review (their bugs are planted)", () => {
  writeFileSync(
    join(bin, "ocr"),
    "#!/bin/sh\n" +
      'if [ "$1" = "--version" ]; then printf \'open-code-review v1.0.0 (abc)\\n\'; exit 0; fi\n' +
      'echo "$@" >> "' + join(dir, "reviews") + '"\n' +
      "echo '{\"status\":\"complete\",\"comments\":[]}'\n",
  );
  chmodSync(join(bin, "ocr"), 0o755);
  expect(gate().exitCode).toBe(0);
  expect(reviewsLog()[0]).toContain("--exclude evals/tasks/*/repo/**");
});

test("a branch behind its base is reviewed from the merge-base, not the base's tip (the base's new work isn't its diff)", () => {
  writeOcr();
  const run = (...a: string[]) =>
    Bun.spawnSync(["git", "-c", "user.email=t@t", "-c", "user.name=t", "-c", "commit.gpgSign=false", ...a], {
      cwd: repo,
      env: { ...process.env, GIT_CONFIG_GLOBAL: "/dev/null", GIT_CONFIG_NOSYSTEM: "1" },
    }).stdout.toString().trim();
  const mergeBase = run("merge-base", "main", "work");
  run("checkout", "-q", "main");
  writeFileSync(join(repo, "other.txt"), "another branch's work, merged into the base\n");
  run("add", ".");
  run("commit", "-qm", "base moves on");
  run("checkout", "-q", "work");
  expect(gate().exitCode).toBe(0);
  expect(reviewsLog()[0]).toStartWith(`from:${mergeBase} `);
});
