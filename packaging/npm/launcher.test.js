// Tests for launcher.js's choice of platform package (node --test packaging/npm/launcher.test.js). The launcher runs
// with process.platform and process.arch replaced by a preload, and with no platform package installed, so what it
// says names the package it looked for. packaging/npm/test.sh runs it with a real binary.
"use strict";

const { test } = require("node:test");
const assert = require("node:assert");
const { spawnSync } = require("node:child_process");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");

const launcher = path.join(__dirname, "launcher.js");

function run(platform, arch) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "marv-launcher-"));
  try {
    // A copy outside the repository, so no node_modules on the way up can hold a platform package.
    fs.copyFileSync(launcher, path.join(dir, "marv.js"));
    const preload = path.join(dir, "fake-platform.js");
    fs.writeFileSync(
      preload,
      `Object.defineProperty(process, "platform", { value: ${JSON.stringify(platform)} });\n` +
        `Object.defineProperty(process, "arch", { value: ${JSON.stringify(arch)} });\n`,
    );
    return spawnSync(process.execPath, ["--require", preload, path.join(dir, "marv.js"), "--version"], {
      encoding: "utf8",
    });
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
}

for (const [platform, arch, pkg] of [
  ["linux", "x64", "@flologixai/marv-linux-x64"],
  ["linux", "arm64", "@flologixai/marv-linux-arm64"],
  ["darwin", "x64", "@flologixai/marv-darwin-x64"],
  ["darwin", "arm64", "@flologixai/marv-darwin-arm64"],
]) {
  test(`${platform} ${arch} runs ${pkg}`, () => {
    const r = run(platform, arch);
    assert.strictEqual(r.status, 1);
    assert.match(r.stderr, new RegExp(`${pkg} isn't installed`));
  });
}

for (const [platform, arch] of [
  ["win32", "x64"],
  ["linux", "ia32"],
  ["freebsd", "x64"],
]) {
  test(`${platform} ${arch} has no build`, () => {
    const r = run(platform, arch);
    assert.strictEqual(r.status, 1);
    assert.match(r.stderr, new RegExp(`there's no build for ${platform} ${arch}`));
  });
}
