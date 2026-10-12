// glob: **
//
// A file that lost more than half of its committed lines. Sprint 1's #35 worker truncated src/tools/bash.ts from 301
// to 72 lines with one write_file (meaning to change a few lines): most of the file was gone, and nothing said so.
// Compared with git's HEAD, so an intended rewrite is reported once (the baseline then holds it) and new files aren't.
//
// A check for Marv's own repository (src/checks): prints one JSON line per violation, {file, line, message, why}.
const MIN_LINES = 20;
const WHY =
  "A write that drops most of a file is usually a write_file that meant to change a few lines and sent only those. " +
  "If the rewrite was intended, go on; otherwise restore what was lost (git diff shows it) and change the file with edit_file.";

const git = (...args: string[]) => Bun.spawnSync(["git", "--no-optional-locks", ...args], { stdout: "pipe", stderr: "pipe" });
const lines = (text: string) => (text === "" ? 0 : text.replace(/\n$/, "").split("\n").length);

// No repository or no commit yet: nothing to compare with.
if (git("rev-parse", "--verify", "-q", "HEAD").exitCode === 0) {
  const diff = git("diff", "--numstat", "--no-renames", "-z", "HEAD");
  if (diff.exitCode !== 0) throw new Error(`git diff failed: ${diff.stderr.toString().trim()}`);
  for (const entry of diff.stdout.toString().split("\0")) {
    const [added, removed, file] = entry.split("\t");
    if (!file || added === "-" || Number(removed) <= Number(added)) continue; // binary, or it didn't shrink
    const file_ = Bun.file(file);
    if (!(await file_.exists())) continue; // deleted: not a write
    const committed = git("show", `HEAD:${file}`);
    if (committed.exitCode !== 0) continue;
    const before = lines(committed.stdout.toString());
    const now = lines(await file_.text());
    if (before < MIN_LINES || now * 2 >= before) continue;
    console.log(JSON.stringify({ file, message: `shrank to less than half of its ${before} committed lines`, why: WHY }));
  }
}
