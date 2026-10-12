// The task report: how many tasks are done, per owner.
//
//   main(["tasks.json"], readFile)           → the text table
//   main(["--json", "tasks.json"], readFile) → the summary as JSON

/** Counts tasks ({ title, owner, done }) in total and per owner. */
export function summarize(tasks) {
  const owners = {};
  for (const task of tasks) {
    const owner = (owners[task.owner] ??= { total: 0, done: 0 });
    owner.total++;
    if (task.done) owner.done++;
  }
  const done = tasks.filter((t) => t.done).length;
  return { total: tasks.length, done, open: tasks.length - done, owners };
}

/** The summary as a table, one line per owner. */
export function formatText(summary) {
  const lines = [`${summary.done}/${summary.total} done, ${summary.open} open`];
  for (const [owner, counts] of Object.entries(summary.owners)) lines.push(`  ${owner.padEnd(10)} ${counts.done}/${counts.total}`);
  return lines.join("\n");
}

/** argv: the tasks file's path, and --json for JSON output. readFile(path) gives its text. */
export function main(argv, readFile) {
  const json = argv.includes("--json");
  const [file] = argv.filter((a) => a !== "--json");
  if (!file) throw new Error("usage: report [--json] <tasks.json>");
  const summary = summarize(JSON.parse(readFile(file)));
  return json ? JSON.stringify(summary, null, 2) : formatText(summary);
}
