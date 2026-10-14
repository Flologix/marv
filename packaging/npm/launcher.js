#!/usr/bin/env node
// `marv` from npm: runs the prebuilt binary that npm installed with this package. Each platform's binary is its own
// package (an optionalDependency with `os` and `cpu` set, so npm installs only the one that fits), the way esbuild
// and biome ship. No postinstall script and no download: npm fetches and checks the binary like any package.
"use strict";

const { spawn } = require("node:child_process");

const PACKAGES = {
  "linux x64": "@flologixai/marv-linux-x64",
  "linux arm64": "@flologixai/marv-linux-arm64",
};

const key = `${process.platform} ${process.arch}`;
const pkg = PACKAGES[key];
if (!pkg) {
  console.error(`marv: there's no build for ${key}. Marv runs on Linux (x64 and arm64): its sandbox is bubblewrap.`);
  process.exit(1);
}
let bin;
try {
  bin = require.resolve(`${pkg}/bin/marv`);
} catch {
  console.error(`marv: ${pkg} isn't installed (optional dependencies were left out?). Reinstall: npm i -g @flologixai/marv`);
  process.exit(1);
}

const child = spawn(bin, process.argv.slice(2), { stdio: "inherit" });
// ctrl+c reaches the whole process group, marv included, and marv decides what it means (interrupt a reply, then
// exit): the launcher must not die first and leave it writing to a terminal its shell has taken back. Other
// signals sent to the launcher alone are passed on.
process.on("SIGINT", () => {});
for (const signal of ["SIGTERM", "SIGHUP"]) process.on(signal, () => child.kill(signal));
child.on("error", (err) => {
  console.error(`marv: couldn't start ${bin}: ${err.message}`);
  process.exit(1);
});
child.on("exit", (code, signal) => {
  if (signal) {
    process.removeAllListeners(signal);
    process.kill(process.pid, signal);
    return;
  }
  process.exit(code ?? 1);
});
