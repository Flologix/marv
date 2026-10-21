// Formats a converted value the same way everywhere: two decimals, a comma between thousands, then the unit.
export function format(value, unit) {
  const [whole, decimals] = Math.abs(value).toFixed(2).split(".");
  const grouped = whole.replace(/\B(?=(\d{3})+(?!\d))/g, ",");
  return `${value < 0 ? "-" : ""}${grouped}.${decimals} ${unit}`;
}
