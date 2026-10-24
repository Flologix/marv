import { format } from "../format.js";

// Converts millilitres to US gallons.
export function volumeGal(value) {
  return format(value * 0.000264172, "gal");
}
