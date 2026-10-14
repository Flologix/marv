// Runs Marv's bash tool once, as yolo runs it (no one asked), and prints the result as JSON. The guard tests start
// it as a child process with HOME pointing at a decoy home folder: Bun reads HOME once, when the process starts, so
// the tool's homedir() (the folder the sandbox hides) can only be a decoy in a fresh process.
//   bun tests/guards/fixtures/bash-tool.ts <project root> <command>
import { runTool } from "../../../src/tools/index.ts";

const [root, command] = process.argv.slice(2);
if (!root || !command) throw new Error("usage: bash-tool.ts <root> <command>");
const result = await runTool({ id: "g1", name: "bash", arguments: JSON.stringify({ command }) }, { root, sandbox: true, yolo: true });
console.log(JSON.stringify(result));
