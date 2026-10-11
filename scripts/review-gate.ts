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
// configured model (`ocr config provider`, or OPENROUTER_API_KEY with
// `ocr config set providers.openrouter...`). Skip once with `git push --no-verify`.
import { spawnSync } from "node:child_process";
import { appendFileSync, existsSync, mkdirSync, readFileSync } from "node:fs";
import { join } from "node:path";

type Finding = { path: string; start_line?: number; end_line?: number; content: string; severity?: string; category?: string };
type Result = { status: string; message?: string; comments: Finding[]; warnings?: unknown[] };

// --- Command-line arguments ---
const args = process.argv.slice(2);
const option = (name: string, fallback: string): string => {
  const at = args.indexOf(name);
  return at >= 0 ? args.splice(at, 2)[1] ?? fallback : fallback;
};
const base = option("--base", "origin/main");
const blocking = new Set(option("--block", "critical,high").split(","));
const prePush = args.includes("--pre-push");

const git = (...a: string[]): string => spawnSync("git", a, { encoding: "utf8" }).stdout.trim();

// --- Which commits to review ---
// From the hook: every pushed ref, except deletions (all-zero sha) and tags.
// Otherwise: HEAD.
const ZERO = /^0+$/;
const commits = prePush
  ? readFileSync(0, "utf8")
      .split("\n")
      .map((line) => line.split(" "))
      .filter(([localRef, localSha]) => localRef?.startsWith("refs/heads/") && localSha && !ZERO.test(localSha))
      .map(([, localSha]) => localSha!)
  : [git("rev-parse", "HEAD")];

// --- Already-passed commits ---
// A commit that passed once passes again: pushing it a second time (a retry, a
// push to another remote) doesn't pay for a second review. The list lives in the
// shared .git folder, so every worktree sees it.
const cacheDir = join(git("rev-parse", "--path-format=absolute", "--git-common-dir"), "ocr-review");
const passedFile = join(cacheDir, "passed");
const passed = new Set(existsSync(passedFile) ? readFileSync(passedFile, "utf8").split("\n") : []);
const key = (sha: string) => `${sha} ${base} ${[...blocking].sort().join(",")}`;

if (spawnSync("ocr", ["--version"]).error) {
  console.error("review-gate: `ocr` isn't installed. Install it with `npm i -g @alibaba-group/open-code-review`, then `ocr config provider`.");
  process.exit(1);
}

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
  if (run.status !== 0) {
    // Fail closed: a review that didn't run isn't a review that passed.
    console.error(`review-gate: ocr failed (exit ${run.status}):\n${run.stderr.trim()}`);
    failed = true;
    continue;
  }
  const result = JSON.parse(run.stdout) as Result;
  const findings = result.comments ?? [];
  const blockers = findings.filter((f) => blocking.has(f.severity ?? ""));

  // Every finding is printed, blocking ones first, in a form an agent can act
  // on: where it is, how bad it is, and what to change.
  for (const f of [...blockers, ...findings.filter((f) => !blockers.includes(f))]) {
    const lines = f.start_line ? `:${f.start_line}${f.end_line && f.end_line !== f.start_line ? `-${f.end_line}` : ""}` : "";
    const mark = blockers.includes(f) ? "BLOCKING" : "note";
    console.log(`\n[${mark} · ${f.severity ?? "?"} · ${f.category ?? "?"}] ${f.path}${lines}\n${f.content}`);
  }
  if (result.warnings?.length) console.error(`review-gate: ${result.warnings.length} file(s) couldn't be reviewed; see \`ocr session list\`.`);

  if (blockers.length) {
    console.error(
      `\nreview-gate: ${blockers.length} ${[...blocking].join("/")} finding(s) in ${sha.slice(0, 8)}. ` +
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
