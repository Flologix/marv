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
//
// A push times out (REVIEW_GATE_TIMEOUT_S, default 1200 = 20 minutes) so a
// hung ocr session can't hang git push; it fails closed with a clear message.
//
// When the only problem is a review_round_failed warning (a model timeout on
// one batch), the review is retried once before blocking.
import { spawnSync } from "node:child_process";
import { appendFileSync, existsSync, mkdirSync, readFileSync } from "node:fs";
import { join } from "node:path";

type Finding = { path: string; start_line?: number; end_line?: number; content: string; severity?: string; category?: string };
type Result = { status?: string; message?: string; comments?: Finding[] | null; warnings?: unknown[] };
type Warning = { type?: string; message?: string };
// Eval tasks' starting repos hold bugs planted on purpose (the model is meant to find them): never findings.
const EXCLUDE = "evals/tasks/*/repo/**";
/** ocr warnings that say nothing went unreviewed (comment_refiled: a comment moved to the file it describes). */
const BENIGN_WARNINGS = new Set(["comment_refiled"]);

const SEVERITIES = ["critical", "high", "medium", "low"];
const TIMEOUT_S = Number(process.env.REVIEW_GATE_TIMEOUT_S ?? "1200");

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
const passedLines = existsSync(passedFile) ? readFileSync(passedFile, "utf8").split("\n") : [];
const passed = new Set(passedLines);

// For incremental review: we store parsed pass records so we can find the
// newest passed commit on the same branch that's an ancestor of the sha being
// pushed. Each line: "<sha> <merge-base> <blocking> <ocrVersion>".
type PassRecord = { sha: string; mergeBase: string; blocking: string; version: string };
const parseRecord = (line: string): PassRecord | undefined => {
  const parts = line.trim().split(" ");
  if (parts.length < 4) return undefined;
  return { sha: parts[0]!, mergeBase: parts[1]!, blocking: parts[2]!, version: parts.slice(3).join(" ") };
};

const keyParts = (sha: string) => ({ sha, blocking: [...blocking].sort().join(","), version: ocrVersion });

const key = (sha: string) => `${sha} ${git("merge-base", base, sha)} ${[...blocking].sort().join(",")} ${ocrVersion}`;

// Run ocr review for a commit and return { blocked, failed, warnings }.
// blocked: there were blocking findings
// failed: the review itself failed (non-zero exit, unreadable, parse error, non-timeout warnings)
// modelTimeout: true when the only warnings are review_round_failed
interface ReviewOutcome {
  blocked: boolean;
  failed: boolean;
  modelTimeout: boolean;
  findings: Finding[];
  warnings: Warning[];
  result: Result | undefined;
}
const runReview = (from: string, to: string): ReviewOutcome => {
  const run = spawnSync("ocr", ["review", "--from", from, "--to", to, "--format", "json", "--audience", "agent", "--exclude", EXCLUDE], {
    encoding: "utf8",
    // No stdin: run in the background, the gate's own stdin may be a closed pipe, and ocr died of it (exit 141).
    stdio: ["ignore", "pipe", "pipe"],
    maxBuffer: 64 * 1024 * 1024,
    timeout: TIMEOUT_S * 1000,
  });
  let result: Result | undefined;
  try {
    result = JSON.parse(run.stdout ?? "") as Result;
  } catch {
    // handled below: no result is a failed review
  }

  // Check for a timeout (Bun sends SIGTERM when timeout expires)
  if (run.signal === "SIGTERM") {
    console.error(`review-gate: ocr timed out after ${TIMEOUT_S}s (REVIEW_GATE_TIMEOUT_S) while reviewing ${to.slice(0, 8)} (from ${from.slice(0, 8)}).`);
    return { blocked: false, failed: true, modelTimeout: false, findings: [], warnings: [], result: undefined };
  }

  if (run.status !== 0 || !result || result.status !== "complete") {
    // Fail closed: a review that didn't run isn't a review that passed.
    console.error(`review-gate: ocr failed (exit ${run.status}, status ${result?.status ?? "unreadable"}):\n${(run.stderr ?? "").trim()}`);
    return { blocked: false, failed: true, modelTimeout: false, findings: [], warnings: [], result };
  }

  const findings = (result.comments ?? []) as Finding[];
  // Notes about how ocr filed its output, not files it couldn't review: they don't hold a commit back.
  const warnings = ((result.warnings ?? []) as Warning[]).filter((w) => !BENIGN_WARNINGS.has(w.type ?? ""));

  // An unknown severity blocks: the gate shouldn't pass what it can't rank.
  const blocks = (f: Finding) => blocking.has(f.severity ?? "") || !SEVERITIES.includes(f.severity ?? "");
  const blockers = findings.filter(blocks);

  // Check whether the only warnings are model timeouts (review_round_failed)
  let modelTimeout = false;
  if (warnings.length > 0) {
    modelTimeout = warnings.every((w) => w.type === "review_round_failed");
  }

  return { blocked: blockers.length > 0, failed: false, modelTimeout, findings, warnings, result };
};

const printFindings = (findings: Finding[], blockers: Finding[]) => {
  for (const f of [...blockers, ...findings.filter((f) => !blockers.includes(f))]) {
    const lines = f.start_line ? `:${f.start_line}${f.end_line && f.end_line !== f.start_line ? `-${f.end_line}` : ""}` : "";
    const mark = blockers.includes(f) ? "BLOCKING" : "note";
    console.log(`\n[${mark} · ${f.severity ?? "?"} · ${f.category ?? "?"}] ${f.path}${lines}\n${f.content}`);
  }
};

const isAncestor = (maybeAncestor: string, sha: string): boolean => {
  const run = spawnSync("git", ["merge-base", "--is-ancestor", maybeAncestor, sha]);
  return run.status === 0;
};

let failed = false;
for (const sha of new Set(commits)) {
  // REVIEW_GATE_FULL=1 reviews again what passed before (the full-sprint run before a PR).
  if (process.env.REVIEW_GATE_FULL !== "1" && passed.has(key(sha))) continue;

  // Determine the review range: <from>..<sha>
  //
  // What a pass record means: "this commit passed the gate", not "ocr reviewed origin/main..sha in one go". With an
  // earlier pass on the branch, only the commits since it are reviewed (ocr still sees each changed file whole), so
  // the chain of passes covers every change once. That's the trade #49 makes on purpose: a small fix on a big branch
  // costs a small review. Whole-branch review still happens where it's cheap to wait for it: the sprint PR (Greptile)
  // and the full-sprint run before the PR (REVIEW_GATE_FULL=1 reviews from the base whatever passed before).
  // If there's a passed commit on this branch (an ancestor of sha) with the
  // same merge-base, blocking and ocr version, review only what's new since
  // that commit.
  const mergeBase = git("merge-base", base, sha);
  const kp = keyParts(sha);
  let from: string = mergeBase; // the base may have moved on since the branch left it: review only the branch's own work
  // Newest first: the passed file is appended to, so its last matching line is the latest pass.
  for (const line of process.env.REVIEW_GATE_FULL === "1" ? [] : [...passedLines].reverse()) {
    const rec = parseRecord(line);
    if (
      rec && rec.blocking === kp.blocking &&
      rec.version === kp.version && rec.mergeBase === mergeBase &&
      isAncestor(rec.sha, sha)
    ) {
      from = rec.sha;
      break; // the newest passed ancestor: the smallest range still covering everything new
    }
  }

  console.error(`review-gate: reviewing ${sha.slice(0, 8)} against ${from.slice(0, 8)} (base ${base}) with Open Code Review…`);

  // First attempt
  const outcome = runReview(from, sha);

  // Print findings for display
  const blockers = outcome.findings.filter(
    (f) => blocking.has(f.severity ?? "") || !SEVERITIES.includes(f.severity ?? ""),
  );
  printFindings(outcome.findings, blockers);

  // Retry once when the only problem is a model timeout on a batch (modelTimeout: warnings, all review_round_failed)
  if (!outcome.failed && outcome.modelTimeout && blockers.length === 0) {
    // (A first attempt that already found blockers blocks: a retry that happens to find none mustn't pass it.)
    {
      console.error(`review-gate: ${sha.slice(0, 8)} had model-timeout warnings only, retrying…`);
      const retry = runReview(from, sha);
      const retryBlockers = retry.findings.filter(
        (f) => blocking.has(f.severity ?? "") || !SEVERITIES.includes(f.severity ?? ""),
      );
      printFindings(retry.findings, retryBlockers);

      // A retry passes only when it finished cleanly: no warnings of any kind (a file it couldn't review is a review
      // that didn't finish, as on the first attempt) and no blockers.
      if (!retry.failed && retry.warnings.length === 0) {
        if (retryBlockers.length === 0) {
          console.error(`review-gate: ${sha.slice(0, 8)} passed on retry (${retry.findings.length} non-blocking note(s)).`);
          mkdirSync(cacheDir, { recursive: true });
          appendFileSync(passedFile, `${key(sha)}\n`);
          continue; // Don't mark as failed
        }
        // Still has blockers — fall through to block
        console.error(
          `\nreview-gate: ${retryBlockers.length} blocking finding(s) in ${sha.slice(0, 8)} (${[...blocking].join("/")}, or unranked). ` +
            "Fix them (or explain in the PR why they're wrong) and push again.",
        );
        failed = true;
        continue;
      }
      // The retry also had warnings or failed: block, saying what it said.
      if (retry.failed) console.error(`review-gate: the retry of ${sha.slice(0, 8)} failed too (see above), so it isn't passed; push again.`);
      if (!retry.failed) {
        console.error(`review-gate: ${retry.warnings.length} file(s) couldn't be reviewed on retry, so ${sha.slice(0, 8)} isn't passed; see \`ocr session list\`, then push again.`);
        for (const w of retry.warnings) console.error(`  ${JSON.stringify(w).slice(0, 300)}`);
      }
      failed = true;
      continue;
    }
  }

  if (outcome.failed) {
    failed = true;
    continue;
  }

  // Blockers first, so a review with both says what to fix; then whatever it couldn't review.
  if (outcome.blocked) {
    console.error(
      `\nreview-gate: ${blockers.length} blocking finding(s) in ${sha.slice(0, 8)} (${[...blocking].join("/")}, or unranked). ` +
        "Fix them (or explain in the PR why they're wrong) and push again.",
    );
    failed = true;
  }
  if (outcome.warnings.length) {
    console.error(`review-gate: ${outcome.warnings.length} file(s) couldn't be reviewed, so ${sha.slice(0, 8)} isn't passed; see \`ocr session list\`, then push again.`);
    for (const w of outcome.warnings) console.error(`  ${JSON.stringify(w).slice(0, 300)}`);
    failed = true;
  }
  if (outcome.blocked || outcome.warnings.length) continue;
  console.error(`review-gate: ${sha.slice(0, 8)} passed (${outcome.findings.length} non-blocking note(s)).`);
  mkdirSync(cacheDir, { recursive: true });
  appendFileSync(passedFile, `${key(sha)}\n`);
}
process.exit(failed ? 1 : 0);