import { defineConfig } from "vite";
import preact from "@preact/preset-vite";

// In development (`bun run dev`) the server starts vite and passes its API's address.
export default defineConfig({
  plugins: [preact()],
  server: {
    proxy: { "/api": process.env.LOCUM_VIEWER_API ?? "http://127.0.0.1:5171" },
  },
  build: { outDir: "dist", emptyOutDir: true, chunkSizeWarningLimit: 1000 },
});
