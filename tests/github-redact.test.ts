import { expect, test } from "bun:test";
import { redact } from "../src/tools/github/redact.ts";

const A36 = "a".repeat(30) + "B1c2D3";

test("replaces each GitHub token shape and counts them", () => {
  for (const prefix of ["ghp_", "gho_", "ghu_", "ghs_", "ghr_"]) {
    expect(redact(`token ${prefix}${A36} here`)).toEqual({ text: "token [redacted token] here", count: 1 });
  }
  expect(redact(`github_pat_${"x".repeat(22)}_${"y".repeat(59)}`)).toEqual({ text: "[redacted token]", count: 1 });
  expect(redact(`v1.${"0123456789abcdef".repeat(2)}01234567`)).toEqual({ text: "[redacted token]", count: 1 });
  expect(redact(`a ghp_${A36}\nb gho_${A36}`).count).toBe(2);
});

test("leaves look-alikes and short strings alone", () => {
  for (const text of [`ghx_${A36}`, "ghp_short", "github_pat_short", "v1.abc", "the ghp_ prefix", `xghp_${A36}`]) {
    expect(redact(text)).toEqual({ text, count: 0 });
  }
});
