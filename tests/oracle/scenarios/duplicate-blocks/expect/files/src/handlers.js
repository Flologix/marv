export function handleCreate(req, store) {
  if (!req.user) {
    return { status: 401, body: { error: "not signed in" } };
  }
  const item = store.create(req.body);
  return { status: 201, body: item };
}

export function handleUpdate(req, store) {
  if (!req.user) {
    return { status: 401, body: { error: "not signed in" } };
  }
  const item = store.update(req.params.id, req.body);
  return { status: 200, body: item };
}

export function handleDelete(req, store) {
  if (!req.user) {
    return { status: 401, body: { error: "not signed in" } };
  }
  if (!req.user.isAdmin) {
    return { status: 403, body: { error: "admins only" } };
  }
  store.remove(req.params.id);
  return { status: 204, body: null };
}
