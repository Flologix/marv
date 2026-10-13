import { getSetting } from "./settings.js";
import { listUsers } from "./users.js";
import { load } from "./store.js";

/** "3 users (ana, bo, cy); 12 visits", titled by the `title` setting. */
export async function summary({ users, settings, counters }) {
  const title = await getSetting(settings, "title", "Report");
  const names = await listUsers(users);
  const counts = await load(counters);
  return `${title}: ${names.length} users (${names.join(", ")}); ${counts.visits ?? 0} visits`;
}
