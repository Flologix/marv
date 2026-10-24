import { format } from "../format.js";

// Converts square metres to acres.
export function areaAcre(value) {
  return format(value * 0.000247105, "acre");
}
