import { format } from "../format.js";

// Converts seconds to minutes.
export function timeMin(value) {
  return format(value * 0.016666666666666666, "min");
}
