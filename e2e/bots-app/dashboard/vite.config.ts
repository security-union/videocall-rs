import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

import pkg from "./package.json" with { type: "json" };

/**
 * Vite config for the bots-app UX dashboard. Bound to 127.0.0.1 only
 * — never exposed over the network. In dev mode the `/api/*` prefix is
 * proxied to the Node sidecar (`bots-app dashboard`) which in turn
 * forwards to the phase-4 control API with the bearer token injected
 * server-side. The browser never sees the token. See ../README.md.
 *
 * The proxy target port (and the dashboard backend in general) is
 * supplied at run time via the `DASHBOARD_BACKEND_PORT` env var; we
 * default to 5174 here for the ergonomic case "operator runs
 * `bots-app dashboard` which spawns Vite with that env set."
 *
 * `__APP_VERSION__` is injected at build time from `package.json` via
 * Vite's `define` so the footer / About page can surface the dashboard
 * version without importing package.json into the React tree at
 * runtime. The replacement is a constant string baked into the bundle.
 */
const BACKEND_PORT = Number.parseInt(process.env.DASHBOARD_BACKEND_PORT ?? "5174", 10);

export default defineConfig({
  plugins: [react()],
  define: {
    __APP_VERSION__: JSON.stringify(pkg.version),
  },
  server: {
    host: "127.0.0.1",
    port: 5173,
    strictPort: false,
    proxy: {
      "/api": {
        target: `http://127.0.0.1:${BACKEND_PORT}`,
        changeOrigin: true,
      },
    },
  },
  build: {
    outDir: "dist",
    sourcemap: true,
    // Keep the dashboard bundle small — it's a control-plane UI, not
    // an app. No code-splitting today; if the bundle grows we'll
    // revisit.
    rollupOptions: {
      output: {
        manualChunks: undefined,
      },
    },
  },
});
