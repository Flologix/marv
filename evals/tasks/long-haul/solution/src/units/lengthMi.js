import { format } from "../format.js";

// Converts meters to miles.
export function lengthMi(value) {
  return format(value * 0.000621371, "mi");
}
