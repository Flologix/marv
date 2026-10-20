// Unit tests for the npm launcher's messages (#153): `node --test packaging/npm/`. The launcher itself runs only as
// the main module, so requiring it here starts nothing.
"use strict";

const test = require("node:test");
const assert = require("node:assert");
const { unsupported } = require("./launcher.js");

test("Linux x64 and arm64 have builds", () => {
  assert.strictEqual(unsupported("linux", "x64"), null);
  assert.strictEqual(unsupported("linux", "arm64"), null);
});

test("Windows points to WSL, with the README's link", () => {
  for (const arch of ["x64", "arm64"]) {
    const message = unsupported("win32", arch);
    assert.match(message, /install and run it inside WSL/);
    assert.match(message, /wsl --install/);
    assert.match(message, /npm install -g @flologixai\/marv/);
    assert.match(message, /https:\/\/github\.com\/FlologixAI\/marv#requirements/);
  }
});

test("macOS has no build yet", () => {
  assert.match(unsupported("darwin", "arm64"), /no macOS build yet/);
});

test("another platform says Linux", () => {
  assert.match(unsupported("freebsd", "x64"), /no build for freebsd x64\. Marv runs on Linux/);
  assert.match(unsupported("linux", "ia32"), /no build for linux ia32/);
});
