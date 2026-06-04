/**
 * Global compile-time constants injected by Vite's `define`. See
 * `vite.config.ts` (and the mirrored entry in `vitest.config.ts` so
 * the same value is available under jsdom). `__APP_VERSION__` is
 * sourced from `package.json#version` and baked into the bundle as
 * a string literal at build time — there is no runtime import of
 * package.json.
 */
declare const __APP_VERSION__: string;
