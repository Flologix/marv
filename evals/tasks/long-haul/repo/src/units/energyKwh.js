// Converts joules to kWh.
export function energyKwh(value) {
  return "kWh " + (value * 2.7777777777777776e-07).toFixed(2);
}
