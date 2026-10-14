import { greet } from "./greet.ts";

const owner = { id: "root", name: "Root" };

export const welcomeGuest = (): string => greet({ id: "guest", name: "Guest" });
export const welcomeOwner = (): string => greet(owner);
