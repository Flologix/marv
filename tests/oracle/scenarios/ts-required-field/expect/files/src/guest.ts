import { greet } from "./greet.ts";

const owner = { id: "root", name: "Root", email: "root@example.com" };

export const welcomeGuest = (): string => greet({ id: "guest", name: "Guest", email: "guest@example.com" });
export const welcomeOwner = (): string => greet(owner);
