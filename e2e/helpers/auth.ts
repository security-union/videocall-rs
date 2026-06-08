import jwt from "jsonwebtoken";
import { BrowserContext } from "@playwright/test";

const JWT_SECRET = process.env.JWT_SECRET || "dev-jwt-secret-change-me";
const COOKIE_NAME = process.env.COOKIE_NAME || "session";

export function generateSessionToken(email: string, name: string, ttlSecs: number = 3600): string {
  const now = Math.floor(Date.now() / 1000);
  return jwt.sign(
    {
      sub: email,
      name: name,
      exp: now + ttlSecs,
      iat: now,
      iss: "videocall-meeting-backend",
    },
    JWT_SECRET,
    { algorithm: "HS256" },
  );
}

interface SessionCookieOptions {
  email?: string;
  name?: string;
  baseURL?: string;
}

/**
 * Deployment families whose session cookie is issued on a **leading-dot parent
 * domain** so it is shared across the `app.` (UI) and `api.` (meeting-api)
 * subdomains. Mirrors the server's `COOKIE_DOMAIN` for these hosts (confirmed
 * deployed value on HCL: `COOKIE_DOMAIN=.videocall.fnxlabs.com`).
 *
 * These are the same deployment families that use cookie-based JWT auth (cf.
 * `JWT_HOST_SUFFIXES` in `bots-app/src/auth/storage-state.ts`), but each entry
 * here is the family's actual shared-parent `COOKIE_DOMAIN` rather than the
 * auth-backend match suffix: conceptcar7's UI/API live at
 * `app./api.videocall.conceptcar7.com` (TLS wildcard `*.videocall.conceptcar7.com`),
 * so the parent cookie domain is `.videocall.conceptcar7.com`, NOT the broader
 * `.conceptcar7.com`. The parent is derived from the host by suffix match (no
 * single host is hardcoded as THE cookie domain), and the list is ordered
 * most-specific-first so a PR-preview host (`*.preview.videocall.fnxlabs.com`)
 * scopes to `.preview.videocall.fnxlabs.com`, not the broader
 * `.videocall.fnxlabs.com`.
 */
const PARENT_COOKIE_SUFFIXES: readonly string[] = [
  ".preview.videocall.fnxlabs.com",
  ".videocall.fnxlabs.com",
  ".videocall.conceptcar7.com",
];

/**
 * Resolve the cookie `domain` for a given host.
 *
 * Returns a **leading-dot parent domain** (e.g. `.videocall.fnxlabs.com`) when
 * `hostname` belongs to a known multi-subdomain deployment family, so the
 * injected session cookie reaches BOTH the `app.` UI host and the sibling
 * `api.` meeting-api host that the in-browser `/join` XHR targets. The leading
 * dot is what Playwright 1.58.2 requires for "apply to all subdomains" (see its
 * `addCookies` `domain` contract).
 *
 * Falls back to the **host itself** (host-only cookie) for anything else —
 * `localhost`, raw IPs, `app.videocall.rs`, Vercel previews, etc. — where there
 * is no `app.`/`api.` split (or where a stripped parent would be a public
 * suffix and the browser would reject the cookie). This preserves the existing
 * single-host behavior for local Playwright e2e and OSS targets.
 */
function resolveCookieDomain(hostname: string): string {
  for (const suffix of PARENT_COOKIE_SUFFIXES) {
    // `endsWith(suffix)` requires at least one label in front of the suffix
    // (the leading dot guarantees `hostname !== suffix.slice(1)`), so a bare
    // registrable domain like `videocall.fnxlabs.com` does not match and stays
    // host-only. A real deployment host is always `app.<suffix-without-dot>`.
    if (hostname.endsWith(suffix)) {
      return suffix;
    }
  }
  return hostname;
}

export async function injectSessionCookie(
  context: BrowserContext,
  opts: SessionCookieOptions = {},
): Promise<void> {
  const email = opts.email || "e2e-test@videocall.rs";
  const name = opts.name || "E2ETestUser";
  const resolvedURL = opts.baseURL || "http://localhost:80";

  const token = generateSessionToken(email, name);
  const url = new URL(resolvedURL);

  // Evict any pre-existing session cookie of the same name BEFORE injecting
  // this bot's per-identity cookie.
  //
  // When the JWT path also loads a captured SSO storage-state (HCL:
  // `<runDir>/auth/hcl-sso.json`), that state carries the operator's real
  // `<COOKIE_NAME>` app cookie (e.g. `videocall-session=jay.boyd@...`) on the
  // broad parent domain `.videocall.fnxlabs.com`. Playwright's `addCookies`
  // below writes our per-bot cookie on the bare host (`app.videocall.fnxlabs.com`),
  // which does NOT overwrite the parent-domain cookie — so both coexist and the
  // server reads the SSO identity, collapsing every bot onto a single `sub`.
  // That shared `sub` then collapses all publishers into one keyframe-limiter
  // bucket on the relay (keyed by target user-email), starving the bot tiles.
  //
  // Clearing by name across all domains/paths first guarantees the per-bot
  // identity injected below is the only `<COOKIE_NAME>` the server sees,
  // including the parent-domain SSO copy. In the no-SSO case (local / previews)
  // there is no pre-existing cookie, so this clear is a no-op.
  await context.clearCookies({ name: COOKIE_NAME });

  // Scope the cookie to the parent domain for multi-subdomain deployments so it
  // reaches the sibling `api.` meeting-api host that the in-browser `/join` XHR
  // targets — not just the `app.` UI host. Host-only for localhost / previews /
  // OSS (see `resolveCookieDomain`). `secure` mirrors the URL scheme so the
  // injected cookie matches the server's real `Secure` session cookie on https
  // targets and is still accepted on http://localhost.
  const cookieDomain = resolveCookieDomain(url.hostname);
  const secure = url.protocol === "https:";

  await context.addCookies([
    {
      name: COOKIE_NAME,
      value: token,
      domain: cookieDomain,
      path: "/",
      httpOnly: true,
      secure,
      sameSite: "Lax",
    },
  ]);
}
