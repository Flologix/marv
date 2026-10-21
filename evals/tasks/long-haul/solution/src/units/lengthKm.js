import { format } from "../format.js";

// Converts meters to kilometres.
export function lengthKm(value) {
  return format(value * 0.001, "km");
}
