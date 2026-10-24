import { format } from "../format.js";

// Converts grams to kilograms.
export function massKg(value) {
  return format(value * 0.001, "kg");
}
