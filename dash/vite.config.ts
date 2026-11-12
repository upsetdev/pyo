import tailwindcss from "@tailwindcss/vite";
import { tanstackStart } from "@tanstack/react-start/plugin/vite";
import viteReact from "@vitejs/plugin-react";
import { fileURLToPath } from "node:url";
import { defineConfig, loadEnv } from "vite";

// The dashboard is a static single-page app. Everything server-side is the
// Rust API (crates/pyo-api), which serves the built files in production.
// In dev, Vite forwards /api and /install to a running API, set in the
// repository's .env (next to .env.example).
export default defineConfig(({ mode }) => {
  const env = loadEnv(mode, fileURLToPath(new URL("..", import.meta.url)), "");
  const api = env.PYO_API_URL || "http://127.0.0.1:8071";

  return {
    server: {
      port: 5173,
      proxy: {
        // Only /api and /api/*: /api-keys is a dashboard page.
        "^/api(/|$)": { target: api, changeOrigin: false, ws: true },
        "/install": { target: api, changeOrigin: false },
        // OAuth discovery for MCP clients (oauth.rs).
        "/.well-known/oauth-": { target: api, changeOrigin: false },
        "/.well-known/openid-configuration": { target: api, changeOrigin: false },
      },
    },
    resolve: { tsconfigPaths: true },
    plugins: [tailwindcss(), tanstackStart({ spa: { enabled: true } }), viteReact()],
  };
});
