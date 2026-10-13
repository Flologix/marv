import { load, save } from "./store.js";

/** Adds a user (name → { name, joined }); an existing name is an error. */
export function addUser(path, name, joined, cb) {
  load(path, (err, users) => {
    if (err) return cb(err);
    if (users[name]) return cb(new Error(`user ${name} already exists`));
    users[name] = { name, joined };
    save(path, users, (err2) => {
      if (err2) return cb(err2);
      cb(null, users[name]);
    });
  });
}

/** The user, or null. */
export function findUser(path, name, cb) {
  load(path, (err, users) => {
    if (err) return cb(err);
    cb(null, users[name] ?? null);
  });
}

/** Every user's name, sorted. */
export function listUsers(path, cb) {
  load(path, (err, users) => {
    if (err) return cb(err);
    cb(null, Object.keys(users).sort());
  });
}
