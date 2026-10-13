// `store <command> …` over the files in a data folder.
import { join } from "node:path";
import { increment } from "./counter.js";
import { summary } from "./report.js";
import { setSetting } from "./settings.js";
import { addUser } from "./users.js";

export const files = (dir) => ({ users: join(dir, "users.json"), settings: join(dir, "settings.json"), counters: join(dir, "counters.json") });

/** Runs one command; returns the line to print. */
export async function main(dir, argv) {
  const f = files(dir);
  const [command, ...args] = argv;
  if (command === "add-user") return `added ${(await addUser(f.users, args[0], args[1] ?? "today")).name}`;
  if (command === "visit") return `visit ${await increment(f.counters, "visits")}`;
  if (command === "set") {
    await setSetting(f.settings, args[0], args[1]);
    return `${args[0]} = ${args[1]}`;
  }
  if (command === "report") return summary(f);
  throw new Error(`unknown command ${command}`);
}
