import type { User } from "./types.ts";

export const greet = (user: User): string => `Hi ${user.name}`;
