// Tests for the npm launcher (`node --test packaging/npm/launcher.test.js`): its messages (#153), and which platform
// package it runs (#167). The launcher itself runs only as the main module, so requiring it here starts nothing. The
// end-to-end tests run a copy of it with process.platform and process.arch replaced by a preload, and with no platform
// package installed, so what it says names the package it looked for. packaging/npm/test.sh runs it with a real binary.
"use strict";

const test = require("node:test");
const assert = require("node:assert");
const { spawnSync } = require("node:child_process");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const { unsupported } = require("./launcher.js");

const launcher = path.join(__dirname, "launcher.js");

// --- the messages ---

test("Linux and macOS, x64 and arm64, have builds", () => {
  for (const platform of ["linux", "darwin"]) {
    for (const arch of ["x64", "arm64"]) assert.strictEqual(unsupported(platform, arch), null, `${platform} ${arch}`);
  }
});

test("Windows points to WSL, with the README's link", () => {
  for (const arch of ["x64", "arm64"]) {
    const message = unsupported("win32", arch);
    assert.match(message, /install and run it inside WSL/);
    assert.match(message, /wsl --install/);
    assert.match(message, /npm install -g @flologixai\/marv/);
    assert.match(message, /https:\/\/github\.com\/Flologix\/marv#requirements/);
  }
});

test("another platform says Linux and macOS", () => {
  assert.match(unsupported("freebsd", "x64"), /no build for freebsd x64\. Marv runs on Linux and macOS/);
  assert.match(unsupported("linux", "ia32"), /no build for linux ia32/);
  assert.match(unsupported("darwin", "ia32"), /no build for darwin ia32/);
});

// --- the launcher end to end, on a faked platform ---

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

test("on Windows the launcher says to use WSL", () => {
  const r = run("win32", "x64");
  assert.strictEqual(r.status, 1);
  assert.match(r.stderr, /inside WSL/);
});

for (const [platform, arch] of [
  ["linux", "ia32"],
  ["freebsd", "x64"],
]) {
  test(`${platform} ${arch} has no build`, () => {
    const r = run(platform, arch);
    assert.strictEqual(r.status, 1);
    assert.match(r.stderr, new RegExp(`there's no build for ${platform} ${arch}`));
  });
}
