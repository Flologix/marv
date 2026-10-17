import react from "@vitejs/plugin-react";
import { defineConfig } from "vite";

// Tauri runs the dev server on a fixed port and watches src-tauri itself.
export default defineConfig({
  plugins: [react()],
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
    host: "127.0.0.1",
    watch: { ignored: ["**/src-tauri/**"] },
  },
});
