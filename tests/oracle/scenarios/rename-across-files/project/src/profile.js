import { fetchUserData } from "./api.js";

export async function profileCard(id, deps) {
  const user = await fetchUserData(id, deps);
  return `${user.name} <${user.email}>`;
}
