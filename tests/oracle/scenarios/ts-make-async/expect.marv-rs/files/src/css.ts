import { theme } from "./theme.ts";

export const isDark = async (): Promise<boolean> => (await theme()) === "dark";
