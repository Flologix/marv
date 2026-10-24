import { format } from "../format.js";

// Converts square metres to hectares.
export function areaHa(value) {
  return format(value * 0.0001, "ha");
}
