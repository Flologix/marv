import { formatMoney, type Line } from "./index.ts";
import type { Formatter } from "./types.ts";

const formatters: Record<string, Formatter> = {
  plain: formatMoney,
  short: (cents, currency) => `${Math.round(cents / 100)} ${currency}`,
};

export function report(lines: Line[], style = "plain"): string[] {
  const format = formatters[style] ?? formatMoney;
  return lines.map((line) => `${line.name}: ${format(line.cents * line.qty, "EUR")}`);
}
