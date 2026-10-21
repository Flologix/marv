#!/usr/bin/env node
// `marv` from npm: runs the prebuilt binary that npm installed with this package. Each platform's binary is its own
// package (an optionalDependency with `os` and `cpu` set, so npm installs only the one that fits), the way esbuild
// and biome ship. No postinstall script and no download: npm fetches and checks the binary like any package.
"use strict";

const { spawn } = require("node:child_process");

const PACKAGES = {
  "linux x64": "@flologixai/marv-linux-x64",
  "linux arm64": "@flologixai/marv-linux-arm64",
  "darwin x64": "@flologixai/marv-darwin-x64",
  "darwin arm64": "@flologixai/marv-darwin-arm64",
};

// Where the README says how to run Marv on Windows (in WSL).
const WSL_HELP = "https://github.com/FlologixAI/marv#requirements";

/** Why there's no build to run on `platform`/`arch`, or null when there is one. */
function unsupported(platform, arch) {
  if (PACKAGES[`${platform} ${arch}`]) return null;
  // #153: Windows itself has no build, but Marv runs in WSL with its sandbox (bubblewrap works there).
  if (platform === "win32") {
    return (
      "marv: Marv doesn't run on Windows itself: install and run it inside WSL. In PowerShell run `wsl --install` " +
      "(it sets up Ubuntu), then in WSL: npm install -g @flologixai/marv, and run marv there. See " +
      WSL_HELP
    );
  }
  return `marv: there's no build for ${platform} ${arch}. Marv runs on Linux and macOS (x64 and arm64).`;
}

function main() {
  const why = unsupported(process.platform, process.arch);
  if (why) {
    console.error(why);
    process.exit(1);
  }
  const pkg = PACKAGES[`${process.platform} ${process.arch}`];
  let bin;
  try {
    bin = require.resolve(`${pkg}/bin/marv`);
  } catch {
    console.error(`marv: ${pkg} isn't installed (optional dependencies were left out?). Reinstall: npm i -g @flologixai/marv`);
    process.exit(1);
  }
  run(bin);
}

function run(bin) {
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
}

if (require.main === module) main();

module.exports = { unsupported, PACKAGES };
