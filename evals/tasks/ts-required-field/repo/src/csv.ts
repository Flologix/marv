import { makeUser } from "./factory.ts";
import type { User } from "./types.ts";

/** "id,name" lines to users. */
export function importUsers(csv: string): User[] {
  return csv
    .trim()
    .split("\n")
    .map((line) => {
      const [id = "", name = ""] = line.split(",");
      return makeUser(id, name);
    });
}
