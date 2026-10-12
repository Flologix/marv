// Integer ranges. Ranges are half-open: they include start and exclude end.

/** The integers from start up to, but not including, end, counting by step (which may be negative). */
export function range(start, end, step = 1) {
  if (step === 0) throw new RangeError("step must not be 0");
  const out = [];
  if (step > 0) {
    for (let i = start; i < end; i += step) out.push(i);
  } else {
    for (let i = start; i > end; i += step) out.push(i);
  }
  return out;
}

/** Whether n lies in [start, end). */
export function contains(start, end, n) {
  return n >= start && n < end;
}
