export async function fetchUserData(id, { client }) {
  if (!id) throw new Error("fetchUserData: id is required");
  const response = await client.get(`/users/${id}`);
  return response.data;
}
