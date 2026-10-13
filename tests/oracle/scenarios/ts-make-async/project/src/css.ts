import { theme } from "./theme.ts";

export const isDark = (): boolean => theme() === "dark";
