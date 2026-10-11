// Turning HTTP and connection failures into messages the user can act on.
// Shared by every adapter.

/** Upstream detail kept from a wrapped error: enough to act on, not a dump. */
const MAX_RAW = 400;

/** An error object as providers send it: OpenAI-style, with OpenRouter's wrapper for upstream failures. */
export interface ErrorObject {
  message?: string;
  metadata?: { provider_name?: string; raw?: unknown };
}

/**
 * The message of an error object. OpenRouter wraps a failure upstream as "Provider returned error", with the
 * provider's own message in metadata.raw: that's the part that says what went wrong.
 */
export function describeError(error: ErrorObject): string {
  const message = error.message ?? "unknown error";
  const raw = error.metadata?.raw;
  if (raw === undefined || raw === null || raw === "") return message;
  const detail = typeof raw === "string" ? raw : JSON.stringify(raw);
  const who = error.metadata?.provider_name ? `${error.metadata.provider_name}: ` : "";
  return `${message}: ${who}${detail.length > MAX_RAW ? `${detail.slice(0, MAX_RAW)}…` : detail}`;
}

/** Pulls the message out of an error body ({"error":{"message":…}} or Ollama's {"error":"…"}). */
export async function errorMessage(response: Response): Promise<string> {
  const text = await response.text().catch(() => "");
  // A gateway's or proxy's HTML error page (a 502 from Cloudflare) says nothing useful: drop it.
  if (/text\/html/i.test(response.headers.get("content-type") ?? "") || /^\s*</.test(text)) return "";
  try {
    const body = JSON.parse(text);
    if (typeof body.error === "string") return body.error;
    return body.error ? describeError(body.error) : text;
  } catch {
    return (text.length > MAX_RAW ? `${text.slice(0, MAX_RAW)}…` : text) || response.statusText;
  }
}

/** What a bad key or an unknown model tells the user to change: the TUI's commands, or the SDK's options. */
export interface ErrorHints {
  key: string;
  model: string;
}
export const TUI_HINTS: ErrorHints = { key: "Run /setup to enter a new one.", model: "Pick another with /model." };
export const SDK_HINTS: ErrorHints = {
  key: "Pass another provider.apiKey to createSession() or configure().",
  model: "Pass another provider.model to createSession() or configure().",
};

/** Turns a status code into something the user can act on. */
export function httpError(status: number, detail: string, label: string, model: string, hints = TUI_HINTS): string {
  const reason = (() => {
    switch (status) {
      case 401:
      case 403:
        return `${label} rejected the API key. ${hints.key}`;
      case 402:
        return `Your ${label} account is out of credits.`;
      case 404:
        return `Model "${model}" wasn't found on ${label}. ${hints.model}`;
      case 429:
        return `Rate limited by ${label}. Wait a moment and try again.`;
      default:
        return status >= 500 ? `${label} is having trouble (${status}). Try again shortly.` : `${label} returned ${status}.`;
    }
  })();
  return detail ? `${reason}\n${detail}` : reason;
}

export function unreachable(label: string, baseUrl: string, err: unknown, hint?: string): string {
  return `Can't reach ${label} at ${new URL(baseUrl).origin}.${hint ? ` ${hint}` : ""} (${(err as Error).message})`;
}
