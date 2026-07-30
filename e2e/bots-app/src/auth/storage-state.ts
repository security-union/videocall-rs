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
 *
 * - `"none"` — skip auth entirely; the context launches with no
 *   pre-injected cookie or storage state. Works only when the target
 *   meeting allows guest joining (no session cookie required to land
 *   on `/meeting/<id>`). Useful for testing guest-flow UX, and the
 *   default surface when the meeting URL is for a public/no-auth
 *   deployment.
 *
 * - `"form-login"` — drive the identity provider's username/password
 *   login form at launch (issue 2035). Needs no pre-captured state:
 *   the bot navigates to the meeting URL, the app kicks off its PKCE
 *   redirect chain, and the bot fills `BOT_EMAIL` / `BOT_PASSWORD` into
 *   the identity login form (see `src/auth/form-login.ts`). Works for
 *   any target whose identity provider exposes a plain login form the
 *   app drives (the labsworkspace videocall identity-service is the
 *   reference target). This backend is **never auto-selected** — it must
 *   be requested explicitly (`--auth form-login` or `auth: form-login`
 *   in a meeting config) so ambient `BOT_EMAIL` / `BOT_PASSWORD` can
 *   never silently route real credentials into a third-party login form
 *   (e.g. Google on `app.videocall.rs`). See #2035 / PR #2082 review.
 */
export type AuthBackend = "jwt" | "storage-state" | "none" | "form-login";

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
 * (CLI `--auth` or a meeting-config `auth:`) when provided; otherwise
 * auto-selects between `"jwt"` (host is in the JWT list) and
 * `"storage-state"` (everything else).
 *
 * `"form-login"` is intentionally NOT part of the auto-selection: it is
 * only ever reached via an explicit `override`. This is the fix for the
 * PR #2082 blocker — an earlier revision auto-selected `"form-login"` for
 * any non-JWT host whenever `BOT_EMAIL` + `BOT_PASSWORD` were present in
 * the environment, which meant ambient credentials could be typed into a
 * third-party login form (e.g. Google on `app.videocall.rs`) with no flag
 * and no config change. Requiring an explicit opt-in removes that footgun;
 * `src/auth/form-login.ts` adds a second, defense-in-depth guard that
 * refuses to type into a known public IdP even when explicitly requested.
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
 * Basename of the default HCL SSO state file. SSO is captured separately
 * from per-account OAuth state because it lives in a *parent* domain
 * (`*.fnxlabs.com`) and is shared across all bots — a single
 * `bots-app sso-login` capture serves every participant for the lifetime
 * of the HCL SSO session.
 */
export const DEFAULT_SSO_STATE_BASENAME = "hcl-sso.json";

/**
 * Conventional path to the captured SSO state — `<runDir>/auth/hcl-sso.json`.
 */
export function defaultSsoStatePath(runDir: string): string {
  return join(runDir, "auth", DEFAULT_SSO_STATE_BASENAME);
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
