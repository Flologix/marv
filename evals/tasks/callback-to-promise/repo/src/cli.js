// `store <command> …` over the files in a data folder.
import { join } from "node:path";
import { increment } from "./counter.js";
import { summary } from "./report.js";
import { setSetting } from "./settings.js";
import { addUser } from "./users.js";

export const files = (dir) => ({ users: join(dir, "users.json"), settings: join(dir, "settings.json"), counters: join(dir, "counters.json") });

/** Runs one command; calls back with the line to print. */
export function main(dir, argv, cb) {
  const f = files(dir);
  const [command, ...args] = argv;
  if (command === "add-user") return addUser(f.users, args[0], args[1] ?? "today", (err, user) => (err ? cb(err) : cb(null, `added ${user.name}`)));
  if (command === "visit") return increment(f.counters, "visits", (err, n) => (err ? cb(err) : cb(null, `visit ${n}`)));
  if (command === "set") return setSetting(f.settings, args[0], args[1], (err) => (err ? cb(err) : cb(null, `${args[0]} = ${args[1]}`)));
  if (command === "report") return summary(f, cb);
  cb(new Error(`unknown command ${command}`));
}
