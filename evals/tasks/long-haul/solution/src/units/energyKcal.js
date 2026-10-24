import { format } from "../format.js";

// Converts joules to kcal.
export function energyKcal(value) {
  return format(value * 0.0002390057361376673, "kcal");
}
