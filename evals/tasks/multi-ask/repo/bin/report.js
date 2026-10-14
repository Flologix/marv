#!/usr/bin/env bun
import { readFileSync } from "node:fs";
import { run } from "../src/cli.js";

const io = {
  readFile: (path) => readFileSync(path, "utf8"),
  out: (text) => process.stdout.write(`${text}\n`),
  err: (text) => process.stderr.write(`${text}\n`),
};
process.exit(run(process.argv.slice(2), io));
