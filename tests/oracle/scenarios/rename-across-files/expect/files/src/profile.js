import { loadUser } from "./api.js";

export async function profileCard(id, deps) {
  const user = await loadUser(id, deps);
  return `${user.name} <${user.email}>`;
}
