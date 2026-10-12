// Durations in seconds, for people.

/** 5400 gives "1h30m", 45 gives "45s", 0 gives "0s". */
export function formatDuration(seconds) {
  const h = Math.floor(seconds / 3600);
  const m = Math.floor((seconds % 3600) / 60);
  const s = seconds % 60;
  const parts = [h && `${h}h`, m && `${m}m`, s && `${s}s`].filter(Boolean);
  return parts.length ? parts.join("") : "0s";
}

/**
 * The inverse of formatDuration: "1h30m" gives 5400, "45s" gives 45, "2h5s" gives 7205. Units h, m and s, each at
 * most once and in that order; whitespace around the whole is ignored. Anything else ("", "5", "1m1h", "1x")
 * throws a RangeError.
 */
export function parseDuration(text) {
  const match = /^(?:(\d+)h)?(?:(\d+)m)?(?:(\d+)s)?$/.exec(String(text).trim());
  if (!match || !(match[1] || match[2] || match[3])) throw new RangeError(`Not a duration: "${text}"`);
  const [, h = "0", m = "0", s = "0"] = match;
  return Number(h) * 3600 + Number(m) * 60 + Number(s);
}
