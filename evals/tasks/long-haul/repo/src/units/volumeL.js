// Converts millilitres to litres.
export function volumeL(value) {
  return `${Math.round(value * 0.001 * 100) / 100}L`;
}
