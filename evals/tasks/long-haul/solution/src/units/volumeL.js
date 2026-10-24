import { format } from "../format.js";

// Converts millilitres to litres.
export function volumeL(value) {
  return format(value * 0.001, "L");
}
