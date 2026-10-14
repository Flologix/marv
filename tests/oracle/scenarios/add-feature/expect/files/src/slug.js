/**
 * Turns a title into a URL slug: "Hello, World!" gives "hello-world".
 * Lowercase ASCII letters and digits only; everything else separates words.
 * Options: separator (default "-"); maxLength: the slug has at most that many
 * characters, cut at a word boundary (whole words only, no separator at the end).
 */
export function slugify(text, options = {}) {
  // "Crème brûlée" → "creme-brulee": NFD splits é into e + ◌́, and the marks are dropped.
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
