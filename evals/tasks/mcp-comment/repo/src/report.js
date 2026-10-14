// The task report: how many tasks are done, per owner.
//
//   main(["tasks.json"], readFile) → the text table

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

/** argv: the tasks file's path. readFile(path) gives its text. */
export function main(argv, readFile) {
  const [file] = argv;
  if (!file) throw new Error("usage: report <tasks.json>");
  return formatText(summarize(JSON.parse(readFile(file))));
}
