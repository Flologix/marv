export function mean(values) {
  if (values.length === 0) return NaN;
  let total = 0;
  for (const v of values) total += v;
  return total / values.length;
}

export function median(values) {
  if (values.length === 0) return NaN;
  const mid = Math.floor(values.length / 2);
  return values[mid];
}
