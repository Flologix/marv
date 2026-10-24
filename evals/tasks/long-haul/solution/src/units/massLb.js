import { format } from "../format.js";

// Converts grams to pounds.
export function massLb(value) {
  return format(value * 0.00220462, "lb");
}
