import { getSetting } from "./settings.js";
import { listUsers } from "./users.js";
import { load } from "./store.js";

/** "3 users (ana, bo, cy); 12 visits", titled by the `title` setting. */
export function summary({ users, settings, counters }, cb) {
  getSetting(settings, "title", "Report", (err, title) => {
    if (err) return cb(err);
    listUsers(users, (err2, names) => {
      if (err2) return cb(err2);
      load(counters, (err3, counts) => {
        if (err3) return cb(err3);
        cb(null, `${title}: ${names.length} users (${names.join(", ")}); ${counts.visits ?? 0} visits`);
      });
    });
  });
}
