import { format } from "../format.js";

// Converts seconds to hours.
export function timeH(value) {
  return format(value * 0.0002777777777777778, "h");
}
