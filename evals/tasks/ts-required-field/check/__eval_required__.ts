import type { User } from "./src/types.ts";

// @ts-expect-error: email is required, so a User without one doesn't typecheck.
export const noEmail: User = { id: "x", name: "x" };
