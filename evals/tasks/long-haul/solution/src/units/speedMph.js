import { format } from "../format.js";

// Converts metres per second to mph.
export function speedMph(value) {
  return format(value * 2.23694, "mph");
}
