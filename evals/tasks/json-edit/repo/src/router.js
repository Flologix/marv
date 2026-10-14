import { readFileSync } from "node:fs";

const { routes } = JSON.parse(readFileSync(new URL("../data/routes.json", import.meta.url), "utf8"));

// The route for a method and path, with its :params, or null.
export function match(method, path) {
  const parts = path.split("/").filter(Boolean);
  for (const route of routes) {
    const pattern = route.path.split("/").filter(Boolean);
    if (pattern.length !== parts.length || !route.methods.includes(method)) continue;
    const params = {};
    if (pattern.every((p, i) => (p.startsWith(":") ? ((params[p.slice(1)] = parts[i]), true) : p === parts[i]))) {
      return { handler: route.handler, params };
    }
  }
  return null;
}
