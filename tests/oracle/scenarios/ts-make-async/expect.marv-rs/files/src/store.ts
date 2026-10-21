// A pretend key-value store.
const data = new Map<string, string>([["settings", JSON.stringify({ theme: "dark", verbose: true })]]);

export async function read(key: string): Promise<string | undefined> {
  await Promise.resolve(); // a real store would wait for the disk or the network here
  return data.get(key);
}
