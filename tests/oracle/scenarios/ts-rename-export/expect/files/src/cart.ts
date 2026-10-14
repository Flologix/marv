import { formatMoney } from "./format.ts";

export interface Line {
  name: string;
  cents: number;
  qty: number;
}

export function cartTotal(lines: Line[], currency = "EUR"): string {
  return formatMoney(lines.reduce((sum, line) => sum + line.cents * line.qty, 0), currency);
}
