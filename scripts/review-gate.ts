// Runs Open Code Review (https://github.com/alibaba/open-code-review, the
// `ocr` command) on this branch's changes and fails when it reports a finding
// that's serious enough to block a push. It's the same gate for every author
// (you, Claude Code, Marv, any other agent), because it runs from git's
// pre-push hook (.githooks/pre-push), not from any one agent's settings.
//
//   bun run review                      review HEAD against origin/main
//   bun run review --base sprint-3      against another branch
//   bun run review --block critical     block only on critical findings
//
// The hook runs it with --pre-push: git then passes the refs being pushed on
// stdin, and each pushed commit is reviewed against origin/main.
//
// Needs `ocr` on PATH (`npm i -g @alibaba-group/open-code-review`) and a
// configured model in ~/.opencodereview/config.json (`ocr config provider`).
// Skip once with `git push --no-verify` or `REVIEW_GATE=off git push`; without `ocr` installed it warns and passes.
//
// It fails closed once ocr is there: a review that didn't finish, or printed
// something unreadable, blocks the push, as does a finding whose severity it
// doesn't know.
import { spawnSync } from "node:child_process";
import { appendFileSync, existsSync, mkdirSync, readFileSync } from "node:fs";
import { join } from "node:path";

type Finding = { path: string; start_line?: number; end_line?: number; content: string; severity?: string; category?: string };
type Result = { status?: string; message?: string; comments?: Finding[] | null; warnings?: unknown[] };

const SEVERITIES = ["critical", "high", "medium", "low"];

const fail = (message: string): never => {
  console.error(`review-gate: ${message}`);
  process.exit(1);
};

// --- Command-line arguments ---
const args = process.argv.slice(2);
const option = (name: string, fallback: string): string => {
  const at = args.indexOf(name);
  if (at < 0) return fallback;
  const value = args[at + 1];
  if (!value || value.startsWith("--")) fail(`${name} needs a value`);
  args.splice(at, 2);
  return value!;
};
const base = option("--base", "origin/main");
const blocking = new Set(option("--block", "critical,high").split(","));
for (const s of blocking) if (!SEVERITIES.includes(s)) fail(`unknown severity "${s}" in --block (use ${SEVERITIES.join(", ")})`);
const prePush = args.includes("--pre-push");

const git = (...a: string[]): string => {
  const run = spawnSync("git", a, { encoding: "utf8" });
  if (run.status !== 0) fail(`git ${a.join(" ")} failed: ${(run.stderr ?? "").trim()}`);
  return (run.stdout ?? "").trim();
};

// --- Which commits to review ---
// From the hook: every pushed ref, except deletions (all-zero sha) and tags.
// Otherwise: HEAD. No branch refs on stdin is not an error: that's a push of
// tags only, or of branch deletions, which add no code to review.
const ZERO = /^0+$/;
if (prePush && process.stdin.isTTY) fail("--pre-push reads the pushed refs from stdin; git's pre-push hook passes them");
const commits = prePush
  ? readFileSync(0, "utf8")
      .split("\n")
      .map((line) => line.split(" "))
      // Each line is "<local ref> <local sha> <remote ref> <remote sha>". What lands on a branch is what counts: the
      // remote ref, so `git push origin HEAD:refs/heads/fix` (a local ref of HEAD) is reviewed too.
      .filter(([, localSha, remoteRef]) => remoteRef?.startsWith("refs/heads/") && localSha && !ZERO.test(localSha))
      .map(([, localSha]) => localSha!)
  : [git("rev-parse", "HEAD")];

if (commits.length === 0) process.exit(0);
// An explicit opt-out, for one push (`REVIEW_GATE=off git push`) or a machine that doesn't review.
if (process.env.REVIEW_GATE === "off") {
  console.error("review-gate: skipped (REVIEW_GATE=off).");
  process.exit(0);
}

// Without ocr there's no review to fail: `bun install` turns the hook on for everyone, and a fresh clone or a
// contributor without ocr must still be able to push. Said loudly; a review that ran and failed still blocks.
const version = spawnSync("ocr", ["--version"], { encoding: "utf8" });
if (version.error || version.status !== 0) {
  console.error("review-gate: `ocr` isn't installed, so this push isn't reviewed. Install it with `npm i -g @alibaba-group/open-code-review`, then `ocr config provider`.");
  process.exit(0);
}
// The first line ("open-code-review v1.12.13 (…)"): the cache file is read line by line, so a key must be one line.
const ocrVersion = (version.stdout ?? "").trim().split("\n")[0]!.trim();
if (spawnSync("git", ["rev-parse", "--verify", "--quiet", `${base}^{commit}`]).status !== 0) fail(`base "${base}" isn't a commit here (fetch it, or pass --base)`);

// --- Already-passed commits ---
// A commit that passed once passes again: pushing it a second time (a retry, a
// push to another remote) doesn't pay for a second review. The key holds the
// commit, where base was then (by sha, so a moved base reviews again), the
// blocking severities and ocr's version. The list lives in the shared .git
// folder, so every worktree sees it; a lost line under concurrent pushes only
// costs a second review.
const cacheDir = join(git("rev-parse", "--path-format=absolute", "--git-common-dir"), "ocr-review");
const passedFile = join(cacheDir, "passed");
const passed = new Set(existsSync(passedFile) ? readFileSync(passedFile, "utf8").split("\n") : []);
const key = (sha: string) => `${sha} ${git("merge-base", base, sha)} ${[...blocking].sort().join(",")} ${ocrVersion}`;

let failed = false;
for (const sha of new Set(commits)) {
  if (passed.has(key(sha))) continue;
  console.error(`review-gate: reviewing ${sha.slice(0, 8)} against ${base} with Open Code Review…`);
  // --from <base> --to <sha> reviews what the commit adds since it left base
  // (the merge-base), like a PR diff. --audience agent keeps stdout one JSON document.
  const run = spawnSync("ocr", ["review", "--from", base, "--to", sha, "--format", "json", "--audience", "agent"], {
    encoding: "utf8",
    maxBuffer: 64 * 1024 * 1024,
  });
  let result: Result | undefined;
  try {
    result = JSON.parse(run.stdout ?? "") as Result;
  } catch {
    // handled below: no result is a failed review
  }
  if (run.status !== 0 || !result || result.status !== "complete") {
    // Fail closed: a review that didn't run isn't a review that passed.
    console.error(`review-gate: ocr failed (exit ${run.status}, status ${result?.status ?? "unreadable"}):\n${(run.stderr ?? "").trim()}`);
    failed = true;
    continue;
  }
  const findings = result.comments ?? [];
  // An unknown severity blocks: the gate shouldn't pass what it can't rank.
  const blocks = (f: Finding) => blocking.has(f.severity ?? "") || !SEVERITIES.includes(f.severity ?? "");
  const blockers = findings.filter(blocks);

  // Every finding is printed, blocking ones first, in a form an agent can act
  // on: where it is, how bad it is, and what to change.
  for (const f of [...blockers, ...findings.filter((f) => !blockers.includes(f))]) {
    const lines = f.start_line ? `:${f.start_line}${f.end_line && f.end_line !== f.start_line ? `-${f.end_line}` : ""}` : "";
    const mark = blockers.includes(f) ? "BLOCKING" : "note";
    console.log(`\n[${mark} · ${f.severity ?? "?"} · ${f.category ?? "?"}] ${f.path}${lines}\n${f.content}`);
  }
  // A file ocr couldn't review is a review that didn't finish: it blocks, and the commit isn't cached as passed.
  if (result.warnings?.length) {
    console.error(`review-gate: ${result.warnings.length} file(s) couldn't be reviewed, so ${sha.slice(0, 8)} isn't passed; see \`ocr session list\`, then push again.`);
    // What ocr said, so a passing glitch (a model timeout) can be told from a file it can't review at all.
    for (const w of result.warnings) console.error(`  ${JSON.stringify(w).slice(0, 300)}`);
    failed = true;
    continue;
  }

  if (blockers.length) {
    console.error(
      `\nreview-gate: ${blockers.length} blocking finding(s) in ${sha.slice(0, 8)} (${[...blocking].join("/")}, or unranked). ` +
        "Fix them (or explain in the PR why they're wrong) and push again.",
    );
    failed = true;
  } else {
    console.error(`review-gate: ${sha.slice(0, 8)} passed (${findings.length} non-blocking note(s)).`);
    mkdirSync(cacheDir, { recursive: true });
    appendFileSync(passedFile, `${key(sha)}\n`);
  }
}
process.exit(failed ? 1 : 0);
