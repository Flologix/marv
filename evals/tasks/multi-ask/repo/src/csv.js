// Reads the sales export: a header line, then `name,amount` per line.

/** The rows of a CSV export, without its header line. Blank lines are skipped. */
export function parseCsv(text) {
  const lines = text.split(/\r?\n/).filter((line) => line.trim() !== "");
  return lines.slice(1).map((line) => {
    const [name, amount] = line.split(",");
    return { name: name.trim(), amount: Number(amount) };
  });
}
