import { formatMoney } from "./format.ts";

export const total = (cents: number) => formatMoney(cents);
