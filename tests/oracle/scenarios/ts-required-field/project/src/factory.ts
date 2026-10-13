import type { User } from "./types.ts";

export function makeUser(id: string, name: string): User {
  return { id, name };
}
