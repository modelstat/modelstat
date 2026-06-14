import { crx } from "@crxjs/vite-plugin";
import tailwind from "@tailwindcss/vite";
import react from "@vitejs/plugin-react";
import { resolve } from "node:path";
import { defineConfig } from "vite";
import manifest from "./manifest.json" with { type: "json" };

export default defineConfig({
  // @crxjs discovers entry points from manifest.json:
  //   - background.service_worker
  //   - content_scripts[].js
  //   - action.default_popup
  //   - options_page
  //   - web_accessible_resources[].resources (our main-world + offscreen)
  // Don't re-declare them via rollupOptions.input; that produces
  // duplicate chunks and breaks HMR.
  plugins: [react(), tailwind(), crx({ manifest })],
  resolve: {
    alias: { "@": resolve(__dirname, "src") },
  },
  build: {
    target: "chrome116",
    sourcemap: true,
    // Keep chunk names short for a smaller CWS package.
    rollupOptions: {
      // The offscreen document is referenced via chrome.offscreen.
      // createDocument() — crxjs doesn't see it, so we declare it here
      // to guarantee vite emits it into dist/.
      input: {
        offscreen: resolve(__dirname, "src/offscreen/index.html"),
      },
      output: {
        chunkFileNames: "assets/[name]-[hash].js",
        assetFileNames: "assets/[name]-[hash][extname]",
      },
    },
  },
  server: {
    port: 5174,
    strictPort: true,
    hmr: { port: 5175 },
  },
});
