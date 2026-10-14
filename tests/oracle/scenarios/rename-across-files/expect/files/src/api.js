export async function loadUser(id, { client }) {
  if (!id) throw new Error("loadUser: id is required");
  const response = await client.get(`/users/${id}`);
  return response.data;
}
