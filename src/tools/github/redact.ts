// A second line of defense behind the github tool's refusals (#47): whatever gh prints goes to the model, the
// transcript and the trajectory, so GitHub's token shapes are replaced before it reaches any of them. The refusals
// keep gh from printing a token (auth, config, --verbose); this catches one that turns up anyway (a token pasted
// into an issue, a CI log that echoed one).

/** Personal, OAuth, user-to-server, server-to-server and refresh tokens; fine-grained PATs; installation tokens. */
const TOKENS = /\bgh[pousr]_[A-Za-z0-9]{36,}|\bgithub_pat_[A-Za-z0-9_]{22,}|\bv1\.[0-9a-f]{40}\b/g;

export const REDACTED = "[redacted token]";

/** The text with every token replaced, and how many there were. */
export function redact(text: string): { text: string; count: number } {
  let count = 0;
  const replaced = text.replace(TOKENS, () => {
    count++;
    return REDACTED;
  });
  return { text: replaced, count };
}
