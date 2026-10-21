import { format } from "../format.js";

// Converts joules to kWh.
export function energyKwh(value) {
  return format(value * 2.7777777777777776e-07, "kWh");
}
