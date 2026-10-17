export const tokens = (n: number) =>
  n >= 1_000_000 ? `${(n / 1_000_000).toFixed(1)}M` : n >= 1000 ? `${(n / 1000).toFixed(n >= 100_000 ? 0 : 1)}k` : String(n);

/** "now", "5m", "3h", "2d", or a date. */
export function ago(seconds: number): string {
  const s = Date.now() / 1000 - seconds;
  if (s < 60) return "now";
  if (s < 3600) return `${Math.floor(s / 60)}m`;
  if (s < 86400) return `${Math.floor(s / 3600)}h`;
  if (s < 86400 * 7) return `${Math.floor(s / 86400)}d`;
  return new Date(seconds * 1000).toLocaleDateString(undefined, { month: "short", day: "numeric" });
}
