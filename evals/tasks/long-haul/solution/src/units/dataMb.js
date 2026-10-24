import { format } from "../format.js";

// Converts bytes to megabytes.
export function dataMb(value) {
  return format(value * 1e-06, "MB");
}
