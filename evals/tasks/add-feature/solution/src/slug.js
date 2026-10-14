export function slugify(text, options = {}) {
  const { separator = "-", maxLength } = options;
  const words = text.normalize("NFD").replace(/[\u0300-\u036f]/g, "").toLowerCase().split(/[^a-z0-9]+/).filter(Boolean);
  if (maxLength === undefined) return words.join(separator);
  let slug = "";
  for (const word of words) {
    const next = slug ? slug + separator + word : word;
    if (next.length > maxLength) break;
    slug = next;
  }
  return slug;
}
