import { expect, test } from "bun:test";
import { handleCreate, handleDelete, handleUpdate } from "./src/handlers.js";

const store = () => ({ removed: [], create: (b) => ({ ...b, id: 1 }), update: (id, b) => ({ ...b, id }), remove(id) { this.removed.push(id); } });
const user = { name: "u", isAdmin: false };
const admin = { name: "a", isAdmin: true };

test("delete needs an admin", () => {
  const s = store();
  expect(handleDelete({ user, params: { id: 7 } }, s)).toEqual({ status: 403, body: { error: "admins only" } });
  expect(s.removed).toEqual([]);
  expect(handleDelete({ user: admin, params: { id: 7 } }, s)).toEqual({ status: 204, body: null });
  expect(s.removed).toEqual([7]);
  expect(handleDelete({ params: { id: 7 } }, s).status).toBe(401);
});
test("create and update don't", () => {
  expect(handleCreate({ user, body: { a: 1 } }, store())).toEqual({ status: 201, body: { a: 1, id: 1 } });
  expect(handleUpdate({ user, params: { id: 2 }, body: { a: 1 } }, store())).toEqual({ status: 200, body: { a: 1, id: 2 } });
  expect(handleCreate({ body: {} }, store()).status).toBe(401);
});
