// The eval runner's command line (evals/run.ts). Pure: parsing only, so tests can import it without the runner
// starting runs (run.ts is a script: importing it runs the runs).

/** The command line, parsed. `--report`'s value is the file names after it, up to the next flag. */
export interface Args {
  verify: boolean;
  report: string[];
  models?: string[];
  tasks?: string[];
  label?: string;
  repeat: number;
  budget: number;
  concurrency: number;
  timeout: number;
  maxSteps: number;
  /** Raw: only "on" and "off" mean anything, and the runner rejects the rest. */
  diagnostics?: string;
  /** A JSON file of MCP servers (Claude Code's format, `{ "mcpServers": … }`) connected in every run. */
  mcp?: string;
  /** Raw: auto, deferred or eager (#14); the runner rejects the rest. */
  mcpTools?: string;
}

export function parseArgs(argv: string[]): Args {
  const option = (name: string) => {
    const at = argv.indexOf(`--${name}`);
    return at >= 0 ? argv[at + 1] : undefined;
  };
  const flag = (name: string) => argv.includes(`--${name}`);
  const list = (value?: string) => value?.split(",").map((s) => s.trim()).filter(Boolean);
  const number = (name: string, fallback: number) => Number(option(name) ?? fallback);
  // The report's files, up to the next flag: `--label x` after them isn't part of the file list (its value would be).
  const at = argv.indexOf("--report");
  const rest = at >= 0 ? argv.slice(at + 1) : [];
  const end = rest.findIndex((a) => a.startsWith("--"));
  return {
    verify: flag("verify"),
    report: rest.slice(0, end >= 0 ? end : rest.length),
    models: list(option("models")),
    tasks: list(option("tasks")),
    label: option("label"),
    repeat: number("repeat", 1),
    budget: number("budget", 1),
    concurrency: number("concurrency", 4),
    timeout: number("timeout", 300),
    maxSteps: number("max-steps", 50),
    diagnostics: option("diagnostics"),
    mcp: option("mcp"),
    mcpTools: option("mcp-tools"),
  };
}
