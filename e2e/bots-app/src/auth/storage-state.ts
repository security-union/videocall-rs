import { existsSync } from "node:fs";
import { join } from "node:path";

/**
 * Auth backend = "how does this bot prove it's a logged-in user to the
 * server it's joining?"
 *
 * - `"jwt"` — mints a session JWT with a server-known secret and injects
 *   it as a cookie. Works for local dev, HCL daily, and PR previews
 *   (anywhere we control `JWT_SECRET`). Doesn't work for `app.videocall.rs`,
 *   which uses real Google OAuth.
 *
 * - `"storage-state"` — replays a previously-captured Playwright storage
 *   state (cookies + localStorage) from `bots-app login <account>`. The
 *   captured session represents a real Google-authenticated user. Works
 *   anywhere a real user can log in, including `app.videocall.rs`.
 */
export type AuthBackend = "jwt" | "storage-state";

/**
 * Hostnames where we can authenticate via JWT-cookie injection (we control
 * the server-side `JWT_SECRET`). Anything else falls back to the
 * storage-state path.
 */
const JWT_HOSTS = new Set<string>(["localhost", "127.0.0.1"]);

const JWT_HOST_SUFFIXES: readonly string[] = [
  ".videocall.fnxlabs.com",
  ".preview.videocall.fnxlabs.com",
  ".conceptcar7.com",
];

/**
 * Pick the auth backend for a given hostname. Honors an explicit override
 * (CLI `--auth`) when provided; otherwise auto-selects via the host list
 * above.
 */
export function chooseAuthBackend(hostname: string, override?: AuthBackend): AuthBackend {
  if (override) return override;
  if (JWT_HOSTS.has(hostname)) return "jwt";
  for (const suffix of JWT_HOST_SUFFIXES) {
    if (hostname.endsWith(suffix)) return "jwt";
  }
  return "storage-state";
}

/**
 * Conventional location for the captured storage-state file produced by
 * `bots-app login <account>`. The basename matches the participant /
 * account handle so `bots-app run --participant alice` can find
 * `run/auth/alice.json` without an extra flag.
 *
 * `runDir` is the same directory used by the asset-prep step
 * (`e2e/bots-app/run` by default). The auth files live in a sibling
 * `auth/` subdir.
 */
export function storageStatePath(runDir: string, account: string): string {
  return join(runDir, "auth", `${account}.json`);
}

/**
 * Resolve and validate that a storage-state file exists. Throws with a
 * human-readable message when the file is missing so the caller can
 * surface the right "run `bots-app login` first" guidance.
 */
export function requireStorageState(path: string): string {
  if (!existsSync(path)) {
    throw new Error(
      `storage-state file ${path} not found — run \`bots-app login <account>\` first to capture a Google session for this participant`,
    );
  }
  return path;
}
