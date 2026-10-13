import { theme } from "./theme.ts";

export function banner(): string {
  return `Theme: ${theme()}`;
}
