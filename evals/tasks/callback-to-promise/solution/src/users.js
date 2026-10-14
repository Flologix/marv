import { load, save } from "./store.js";

/** Adds a user (name → { name, joined }); an existing name is an error. */
export async function addUser(path, name, joined) {
  const users = await load(path);
  if (users[name]) throw new Error(`user ${name} already exists`);
  users[name] = { name, joined };
  await save(path, users);
  return users[name];
}

/** The user, or null. */
export async function findUser(path, name) {
  const users = await load(path);
  return users[name] ?? null;
}

/** Every user's name, sorted. */
export async function listUsers(path) {
  return Object.keys(await load(path)).sort();
}
