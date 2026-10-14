import { loadUser } from "./api.js";

// Both users, for the admin's side-by-side view.
export async function compareUsers(a, b, deps) {
  const first = await loadUser(a, deps);
  const second = await loadUser(b, deps);
  return { first, second, sameTeam: first.team === second.team };
}
