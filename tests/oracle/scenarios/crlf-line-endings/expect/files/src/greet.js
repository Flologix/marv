// Greetings for the welcome screen.
export function greet(name) {
  const trimmed = (name ?? "").trim();
  return `Hello, ${trimmed || "stranger"}!`;
}

export function greetAll(names) {
  return names.map(greet).join("\n");
}
