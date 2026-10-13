// Builds the report: the rows by amount, largest first, and the total over the whole file.

/** The sum of every row's amount. */
export function total(rows) {
  let sum = 0;
  for (const row of rows) sum += row.amount;
  return sum;
}

/** The rows to show, largest amount first; `limit` keeps only that many. */
export function topRows(rows, limit) {
  const sorted = [...rows].sort((a, b) => b.amount - a.amount);
  return limit === undefined ? sorted : sorted.slice(0, limit);
}

/** The report as a text table. */
export function formatTable(rows, sum) {
  const width = Math.max(4, ...rows.map((r) => r.name.length));
  const lines = rows.map((r) => `${r.name.padEnd(width)}  ${String(r.amount).padStart(8)}`);
  return [...lines, `${"Total".padEnd(width)}  ${String(sum).padStart(8)}`].join("\n");
}
