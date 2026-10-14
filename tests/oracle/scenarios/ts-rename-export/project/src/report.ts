import { formatPrice, type Line } from "./index.ts";
import type { Formatter } from "./types.ts";

const formatters: Record<string, Formatter> = {
  plain: formatPrice,
  short: (cents, currency) => `${Math.round(cents / 100)} ${currency}`,
};

export function report(lines: Line[], style = "plain"): string[] {
  const format = formatters[style] ?? formatPrice;
  return lines.map((line) => `${line.name}: ${format(line.cents * line.qty, "EUR")}`);
}
