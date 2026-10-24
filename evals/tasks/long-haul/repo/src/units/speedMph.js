// Converts metres per second to mph.
export function speedMph(value) {
  return `${Math.round(value * 2.23694 * 100) / 100}mph`;
}
