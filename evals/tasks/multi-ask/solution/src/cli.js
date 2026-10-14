// The `report` command: `report <export.csv> [--top N] [--json]`.
import { parseCsv } from "./csv.js";
import { formatTable, topRows, total } from "./report.js";

const USAGE = "usage: report <export.csv> [--top N] [--json]";

/** Runs the command; returns the exit code. `io` reads files and writes output (tests pass their own). */
export function run(argv, io) {
  let file;
  let limit;
  let json = false;
  for (let i = 0; i < argv.length; i++) {
    const arg = argv[i];
    if (arg === "--top" || arg === "--limit") {
      if (arg === "--limit") io.err("warning: --limit is deprecated, use --top instead");
      limit = Number(argv[++i]);
      if (!Number.isInteger(limit) || limit < 1) {
        io.err(`${arg} takes a whole number, not "${argv[i]}"`);
        return 2;
      }
    } else if (arg === "--json") {
      json = true;
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
  const shown = topRows(rows, limit);
  const sum = total(rows);
  io.out(json ? JSON.stringify({ rows: shown, total: sum }, null, 2) : formatTable(shown, sum));
  return 0;
}
