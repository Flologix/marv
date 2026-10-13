import { formatPrice } from "./format.ts";

export const total = (cents: number) => formatPrice(cents);
