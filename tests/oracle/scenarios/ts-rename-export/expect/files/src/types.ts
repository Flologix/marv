import type { formatMoney } from "./format.ts";

/** Anything that turns cents into text the same way. */
export type Formatter = typeof formatMoney;
