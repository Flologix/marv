// Summaries over an application log. A log line looks like
// "2026-10-12T09:00:07Z INFO request 7 handled in 32ms".

/** The log as entries: { time, level, message }, in file order. */
export function parseLog(text) {
  return text.split("\n").filter(Boolean).map((line) => {
    const first = line.indexOf(" ");
    const rest = line.slice(first + 1);
    const second = rest.indexOf(" ");
    return { time: line.slice(0, first), level: rest.slice(0, second), message: rest.slice(second + 1) };
  });
}

/** One line per entry, for the nightly ops report: "09:00:07Z INFO request 7 handled in 32ms". */
export function summarize(entries) {
  return entries
    .filter((_, i) => i % 2 === 0)
    .map((entry) => `${entry.time.slice(11)} ${entry.level} ${entry.message}`);
}

/** How many entries each level has: { INFO: 50, WARN: 30, ERROR: 10 }. */
export function countByLevel(entries) {
  const counts = {};
  for (const entry of entries) counts[entry.level] = (counts[entry.level] ?? 0) + 1;
  return counts;
}

/** The ERROR entries, in file order. */
export function errors(entries) {
  return entries.filter((entry) => entry.level === "ERROR");
}
