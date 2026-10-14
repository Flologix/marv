// Durations in seconds, for people.

/** 5400 gives "1h30m", 45 gives "45s", 0 gives "0s". */
export function formatDuration(seconds) {
  const h = Math.floor(seconds / 3600);
  const m = Math.floor((seconds % 3600) / 60);
  const s = seconds % 60;
  const parts = [h && `${h}h`, m && `${m}m`, s && `${s}s`].filter(Boolean);
  return parts.length ? parts.join("") : "0s";
}
