// Retry helpers for provider adapters.
//
// Both OpenAICompatProvider and OllamaProvider need to retry when a network
// connection drops mid-stream, or the server returns a temporary error
// (502, 503, 504). These functions are the shared pieces of that logic.

/** Maximum number of retries (including the original attempt). */
export const MAX_ATTEMPTS = 3;
/** Backoff delays in seconds: 1 s, 4 s, 10 s. */
export const BACKOFFS = [1, 4, 10];
/** Multiplies every retry wait (backoffs and Retry-After). Tests set it small, so they don't wait in real time. */
export const retryTiming = { scale: 1 };

/**
 * Whether an HTTP status is worth retrying: a temporary server error (502, 503, 504)
 * or a rate limit (429).
 */
export function retriableHttpStatus(status: number): boolean {
  return status === 429 || status === 502 || status === 503 || status === 504;
}

/**
 * Whether an error looks like a network-level failure that retrying may fix:
 * connection refused or reset, socket closed, DNS failure, timeout.
 */
export function isNetworkError(err: unknown): boolean {
  const message = (err as Error).message ?? "";
  return /connection (refused|reset|closed|dropped)/i.test(message) ||
    /socket/.test(message) ||
    /timed?[-\s]?out/i.test(message) ||
    /dns|resolve|eai_again|enotfound/i.test(message) ||
    /name or service not known/i.test(message) ||
    /could not resolve/i.test(message) ||
    /transport|econnrefused|econnreset|enetunreach|ehostunreach/i.test(message) ||
    // Bun's fetch errors
    /unable to connect/i.test(message) ||
    /connection failed/i.test(message) ||
    /the socket connection was closed/i.test(message);
}

/**
 * Sleeps for `ms` milliseconds, but resolves early when the signal is aborted.
 * Returns `true` if aborted (caller should return without an error), `false` otherwise.
 */
export function sleepAbortable(ms: number, signal?: AbortSignal): Promise<boolean> {
  return new Promise((resolve) => {
    if (signal?.aborted) { resolve(true); return; }
    const timer = setTimeout(() => resolve(false), ms * retryTiming.scale);
    signal?.addEventListener("abort", () => {
      clearTimeout(timer);
      resolve(true);
    }, { once: true });
  });
}

/**
 * Parses a Retry-After header value. Returns a whole number of seconds (capped at 30)
 * if the value is a positive integer; returns undefined for HTTP dates, decimals or junk.
 */
export function parseRetryAfter(value: string | null): number | undefined {
  if (!value) return undefined;
  const trimmed = value.trim();
  const seconds = parseInt(trimmed, 10);
  if (Number.isFinite(seconds) && seconds > 0 && String(seconds) === trimmed) {
    return Math.min(seconds, 30);
  }
  return undefined;
}