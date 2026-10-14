// Starts one background process with Marv's ProcessManager and then just waits, so the guard test can SIGKILL this
// process (Marv dying hard, with no chance to clean up) and check that nothing it started survives.
//   bun tests/guards/fixtures/process-parent.ts <project root> <runtime dir> <command>
import { ProcessManager } from "../../../src/processes.ts";

const [root, runtimeDir, command] = process.argv.slice(2);
if (!root || !runtimeDir || !command) throw new Error("usage: process-parent.ts <root> <runtime dir> <command>");
const manager = new ProcessManager({ root, runtimeDir, startWaitMs: 300 });
await manager.start({ command, name: "survivor" });
console.log("started");
setInterval(() => {}, 1000);
