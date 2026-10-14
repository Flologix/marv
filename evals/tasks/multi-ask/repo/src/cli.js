// The `report` command: `report <export.csv> [--limit N]`.
import { parseCsv } from "./csv.js";
import { formatTable, topRows, total } from "./report.js";

const USAGE = "usage: report <export.csv> [--limit N]";

/** Runs the command; returns the exit code. `io` reads files and writes output (tests pass their own). */
export function run(argv, io) {
  let file;
  let limit;
  for (let i = 0; i < argv.length; i++) {
    const arg = argv[i];
    if (arg === "--limit") {
      limit = Number(argv[++i]);
      if (!Number.isInteger(limit) || limit < 1) {
        io.err(`--limit takes a whole number, not "${argv[i]}"`);
        return 2;
      }
    } else if (arg === "--help") {
      io.out(USAGE);
      return 0;
    } else if (arg.startsWith("--")) {
      io.err(`unknown option ${arg}\n${USAGE}`);
      return 2;
    } else file = arg;
  }
  if (!file) {
    io.err(USAGE);
    return 2;
  }
  const rows = parseCsv(io.readFile(file));
  io.out(formatTable(topRows(rows, limit), total(rows)));
  return 0;
}
