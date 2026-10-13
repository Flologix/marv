import { theme } from "./theme.ts";

export async function banner(): Promise<string> {
  return `Theme: ${await theme()}`;
}
