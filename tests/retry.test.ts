import { describe, expect, test } from "bun:test";
import { isNetworkError, parseRetryAfter, retriableHttpStatus, sleepAbortable } from "../src/provider/retry.ts";

describe("retry helpers (#36)", () => {
  test("Retry-After: whole seconds are used, capped at 30; dates and junk fall back to the backoff", () => {
    expect(parseRetryAfter("5")).toBe(5);
    expect(parseRetryAfter("3600")).toBe(30);
    expect(parseRetryAfter("Wed, 21 Oct 2026 07:28:00 GMT")).toBeUndefined();
    expect(parseRetryAfter("1.5")).toBeUndefined();
    expect(parseRetryAfter("0")).toBeUndefined();
    expect(parseRetryAfter(null)).toBeUndefined();
  });

  test("which failures are worth retrying", () => {
    expect([429, 502, 503, 504].every(retriableHttpStatus)).toBe(true);
    expect([400, 401, 404, 500].some(retriableHttpStatus)).toBe(false);
    expect(isNetworkError(new Error("The socket connection was closed unexpectedly."))).toBe(true);
    expect(isNetworkError(new Error("Unable to connect. Is the computer able to access the url?"))).toBe(true);
    expect(isNetworkError(new Error("Invalid API key"))).toBe(false);
  });

  test("an aborted wait ends at once and says so", async () => {
    const controller = new AbortController();
    const start = Date.now();
    const waiting = sleepAbortable(10_000, controller.signal);
    controller.abort();
    expect(await waiting).toBe(true);
    expect(Date.now() - start).toBeLessThan(100);
  });
});
