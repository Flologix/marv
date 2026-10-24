import { format } from "../format.js";

// Converts metres per second to km/h.
export function speedKmh(value) {
  return format(value * 3.6, "km/h");
}
