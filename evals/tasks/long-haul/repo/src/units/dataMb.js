// Converts bytes to megabytes.
export function dataMb(value) {
  return `${Math.round(value * 1e-06 * 100) / 100}MB`;
}
