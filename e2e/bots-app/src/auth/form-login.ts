import { type Page } from "@playwright/test";

/**
 * Programmatic username/password login for identity providers that drive a
 * standard PKCE auth-code flow where the *application* orchestrates the
 * redirect and the operator only fills the login form (issue 2035, the
 * labsworkspace videocall identity-service is the reference target).
 *
 * Unlike `"jwt"` (server-secret cookie injection) and `"storage-state"`
 * (replay a previously-captured real session), `"form-login"` needs no
 * pre-captured state: the bot navigates to the meeting URL, the app kicks
 * off the OAuth redirect chain, and this helper detects the identity
 * provider's login page, types `BOT_EMAIL` / `BOT_PASSWORD` into the form,
 * submits it (the hidden PKCE `state` field rides along with the form
 * POST), and waits for the app to consume the `?code=&state=` callback and
 * re-establish its session. Control then returns to the existing join flow.
 *
 * Observed flow (captured from a real HAR against the labsworkspace target):
 *   app  → GET api/v1/oauth/provider-config
 *        → GET id…/oauth/authorize?(response_type,client_id,redirect_uri,
 *              scope,state,nonce,code_challenge,code_challenge_method)  307
 *        → GET id…/login?(state,client_id,scope)
 *        → POST id…/auth/login-form  {state,email,password}
 *              (application/x-www-form-urlencoded)                      303
 *        → GET app…/auth/callback?(code,state)
 *        → POST id…/oauth/token
 *
 * SECURITY: the password is NEVER logged. Only the (non-secret) email is
 * surfaced in diagnostics, and every URL we log — including on both failure
 * paths (login form never appears; app never settles back) — is stripped of
 * its query string (see `redactUrl`) so the single-use `code`/`state` in the
 * callback URL never lands in a log.
 *
 * TRUST MODEL (accepted by design): this is an internal e2e/load-testing tool,
 * not shipping client code. It runs on a controlled network against known test
 * targets using dedicated throwaway test accounts.
 *
 * As defense-in-depth against a mis-pointed run it refuses to type credentials
 * into a small set of KNOWN PUBLIC identity providers (Google, Microsoft,
 * Apple, …; see `isKnownPublicIdpHost`) — a denylist, not a positive allowlist.
 * That guard, plus the fact that `chooseAuthBackend` never auto-selects
 * form-login (it must be requested explicitly), is what keeps ambient
 * `BOT_EMAIL`/`BOT_PASSWORD` from being typed into e.g. Google's real login
 * form (PR #2082 blocker). It does NOT maintain a positive allowlist of
 * permitted identity origins, and the browser context ignores TLS errors (a
 * pre-existing setting for self-signed WebTransport / preview targets). Those
 * remaining gaps are acceptable for a trusted-network test tool; do NOT reuse
 * this helper for anything handling real user credentials without first adding
 * a positive identity-origin allowlist and enabling TLS verification.
 */

/**
 * Selector for the identity provider's email field. Primary is the
 * `name="email"` attribute observed on the labsworkspace `login-form`;
 * `type="email"` is the fallback if a future relabel drops the name.
 * Exported so the unit test drives the exact production selector.
 */
export const FORM_LOGIN_EMAIL_SELECTOR = 'input[name="email"], input[type="email"]';

/**
 * Selector for the identity provider's password field. Primary is the
 * `name="password"` attribute; `type="password"` is the fallback.
 */
export const FORM_LOGIN_PASSWORD_SELECTOR = 'input[name="password"], input[type="password"]';

/**
 * Selector for the login form's submit control. When present it is
 * clicked; otherwise the helper falls back to pressing Enter in the
 * password field (which submits a standard HTML form and carries the
 * hidden `state` field along).
 */
export const FORM_LOGIN_SUBMIT_SELECTOR = 'button[type="submit"], input[type="submit"]';

/**
 * Best-effort selector for an app-side "log in / sign in" trigger, used
 * ONLY as a fallback for the case where the app does not auto-redirect to
 * the identity provider and instead renders a button the user must click.
 * The reference target auto-drives the redirect, so this is defensive.
 * Text is matched case-insensitively as a substring by Playwright's
 * `:has-text()`.
 */
export const FORM_LOGIN_TRIGGER_SELECTOR = [
  'a:has-text("Sign in")',
  'button:has-text("Sign in")',
  'a:has-text("Log in")',
  'button:has-text("Log in")',
  'a:has-text("Login")',
  'button:has-text("Login")',
].join(", ");

/**
 * Wall-clock budget (ms) for the two network-bound waits: (1) the
 * identity login form appearing after the app's OAuth redirect chain, and
 * (2) the redirect back to the app after the credential POST. Generous
 * because the redirect chain crosses the app origin → identity origin →
 * back, each hop a real network round-trip that can exceed a second on a
 * high-latency link.
 */
export const FORM_LOGIN_TIMEOUT_MS = 20_000;

/**
 * Per-action timeout (ms) for the discrete fill / click / press steps
 * once the form element is already present. Shorter than the network
 * budget above because these act on an element that is already visible.
 */
export const FORM_LOGIN_ACTION_TIMEOUT_MS = 10_000;

/**
 * Credentials resolved from the environment for the form-login flow.
 */
export interface FormLoginCredentials {
  email: string;
  password: string;
}

/**
 * Read `BOT_EMAIL` / `BOT_PASSWORD` from an environment record. Returns
 * `null` (rather than throwing) when either is absent or empty so the
 * caller can produce a single, actionable error at the launch site.
 *
 * The email is trimmed (operators paste with trailing whitespace); the
 * password is taken verbatim (leading/trailing characters could be
 * significant and are not ours to alter). Neither value is logged here.
 *
 * Takes the env record as an argument (rather than reading
 * `process.env` directly) so it is unit-testable without mutating global
 * process state.
 */
export function resolveFormLoginCredentials(env: NodeJS.ProcessEnv): FormLoginCredentials | null {
  const email = env.BOT_EMAIL?.trim();
  const password = env.BOT_PASSWORD;
  if (!email || !password) return null;
  return { email, password };
}

/**
 * Strip the query string and hash from a URL for logging. The identity
 * callback URL carries the single-use auth `code` in its query, so we log
 * only origin + pathname. Returns a fixed placeholder on an unparseable
 * input rather than risking echoing a raw (possibly secret-bearing)
 * string.
 */
export function redactUrl(rawUrl: string): string {
  try {
    const url = new URL(rawUrl);
    return `${url.origin}${url.pathname}`;
  } catch {
    return "<unparseable-url>";
  }
}

/**
 * Registrable hosts of well-known PUBLIC identity providers. This is a
 * defense-in-depth DENYLIST (not a positive allowlist): the form-login flow
 * refuses to type `BOT_EMAIL` / `BOT_PASSWORD` into a login page served from
 * any of these, so a mis-pointed run (e.g. `--auth form-login` against a host
 * that actually uses real Google/Microsoft OAuth) can never send throwaway
 * test credentials to a real consumer/enterprise IdP.
 *
 * The reference target — the self-hosted labsworkspace videocall
 * identity-service on `*.labsworkspace.fnxlabs.com` — matches none of these,
 * so the guard is transparent for the intended use. Extend the list if a new
 * public provider ever becomes reachable; a positive allowlist is deliberately
 * NOT used because the set of valid *self-hosted* identity origins is
 * open-ended (see the module header's TRUST MODEL note).
 */
export const KNOWN_PUBLIC_IDP_HOSTS: readonly string[] = [
  "accounts.google.com",
  "accounts.google.co.uk",
  "login.microsoftonline.com",
  "login.microsoft.com",
  "login.live.com",
  "appleid.apple.com",
  "facebook.com",
  "github.com",
  "login.yahoo.com",
  "okta.com",
  "auth0.com",
];

/**
 * True when `host` is (or is a subdomain of) a known public identity
 * provider from {@link KNOWN_PUBLIC_IDP_HOSTS}. Matching is case-insensitive
 * and covers exact + subdomain hits (`accounts.google.com` and
 * `xyz.okta.com`), but never a same-suffix-different-registrable-domain
 * lookalike (`notgithub.com` does not match `github.com`).
 */
export function isKnownPublicIdpHost(host: string): boolean {
  const h = host.toLowerCase();
  return KNOWN_PUBLIC_IDP_HOSTS.some((d) => h === d || h.endsWith(`.${d}`));
}

/**
 * Drive the identity provider's login form for a bot using
 * username/password credentials, then wait for the app to re-establish
 * its session.
 *
 * Preconditions: `page` has already navigated to the meeting URL (the app
 * origin) and the app has begun — or is about to begin — the OAuth
 * redirect chain. This helper is tolerant of both orderings: it waits for
 * the identity login form to appear (auto-redirect case) OR for an
 * app-side login trigger to appear first (click-to-login case), clicking
 * the trigger before waiting for the form.
 *
 * Postcondition on success: the top-frame URL has settled on the app's
 * `/meeting/<id>` path (NOT merely the app origin). We deliberately wait
 * past the intermediate `/auth/callback?code=…` hop — and any transient
 * bounce through `/` — so that by the time the caller installs its
 * `/meeting/<id>` hang-up detector, the callback→meeting navigation has
 * already completed and cannot be misread as a manual hang-up. Resolving
 * merely on "back on the app origin" was the defect observed in live
 * validation against the labsworkspace target: `performFormLogin` returned
 * while the URL was still `/auth/callback`, and the subsequent
 * callback→`/meeting/<id>` navigation tripped the hang-up detector.
 *
 * Throws (with no secret in the message) when the login form never
 * appears, or when the app never redirects back within the budget (the
 * usual cause of the latter is a rejected credential — the identity page
 * re-renders the form with an inline error and stays on the identity
 * origin).
 *
 * The password is NEVER logged.
 */
export async function performFormLogin(args: {
  page: Page;
  email: string;
  password: string;
  /** The app origin, e.g. `https://app.videocall.labsworkspace.fnxlabs.com`. */
  appBaseUrl: string;
  /**
   * Meeting id parsed from the meeting URL (e.g. `bottest`). Used to wait
   * for the app to settle on the `/meeting/<id>` path after the OAuth
   * callback before returning — see phase 3. Must match the id the caller
   * derives for its own `/meeting/<id>` hang-up detector so the two agree
   * on "in the meeting".
   */
  meetingId: string;
  /** Log prefix (participant or participant@idshort). */
  label: string;
  /** Override the network budget; defaults to {@link FORM_LOGIN_TIMEOUT_MS}. */
  timeoutMs?: number;
}): Promise<void> {
  const { page, email, password, appBaseUrl, meetingId, label } = args;
  const timeout = args.timeoutMs ?? FORM_LOGIN_TIMEOUT_MS;
  const appHost = new URL(appBaseUrl).host;

  const emailInput = page.locator(FORM_LOGIN_EMAIL_SELECTOR).first();
  const passwordInput = page.locator(FORM_LOGIN_PASSWORD_SELECTOR).first();
  const submitButton = page.locator(FORM_LOGIN_SUBMIT_SELECTOR).first();
  const loginTrigger = page.locator(FORM_LOGIN_TRIGGER_SELECTOR).first();

  // ── Phase 1: reach the identity login form ──────────────────────────
  // Race the auto-redirect (email field appears on its own) against an
  // app-side login trigger (button the user must click first). Each arm
  // catches its own timeout to null so the winner is whichever real
  // element appears first; if neither appears both settle to null around
  // the budget and we throw.
  console.log(
    `[${label}] form-login: waiting for the identity login form (or an app-side login trigger)`,
  );
  const detected = await Promise.race([
    emailInput
      .waitFor({ state: "visible", timeout })
      .then(() => "form" as const)
      .catch(() => null),
    loginTrigger
      .waitFor({ state: "visible", timeout })
      .then(() => "trigger" as const)
      .catch(() => null),
  ]);

  if (detected === "trigger") {
    console.log(`[${label}] form-login: clicking app-side login trigger`);
    await loginTrigger.click({ timeout: FORM_LOGIN_ACTION_TIMEOUT_MS }).catch(() => {
      // A trigger that vanished (the app auto-redirected in the meantime)
      // is fine — the form wait below is the real gate.
    });
    try {
      await emailInput.waitFor({ state: "visible", timeout });
    } catch {
      // Playwright's own TimeoutError call-log can embed the raw current
      // URL (with the PKCE `state` in its query). Re-throw with the URL
      // redacted so the "never log a raw URL, including on the failure
      // path" contract in the module header holds here too.
      throw new Error(
        `form-login: identity login form did not appear after clicking the app-side login trigger ` +
          `within ${timeout}ms (current page: ${redactUrl(page.url())})`,
      );
    }
  } else if (detected === null) {
    throw new Error(
      `form-login: identity login form did not appear within ${timeout}ms (current page: ${redactUrl(page.url())})`,
    );
  }

  // ── Guard: refuse to type into a known public IdP ───────────────────
  // Defense-in-depth for a mis-pointed run: we are now on the identity
  // origin's login page and about to fill real credentials. If that origin
  // is a well-known public provider (Google, Microsoft, …), a
  // misconfiguration has us about to send throwaway test creds to a real
  // consumer/enterprise IdP — refuse. Only the (non-secret) host is named;
  // the URL is still redacted. See the module header's TRUST MODEL note.
  const currentHost = (() => {
    try {
      return new URL(page.url()).host;
    } catch {
      return "";
    }
  })();
  if (isKnownPublicIdpHost(currentHost)) {
    throw new Error(
      `form-login: refusing to enter credentials — the login page host "${currentHost}" is a known public ` +
        `identity provider. form-login is only for the self-hosted identity-service target; a match here means ` +
        `the meeting URL points at a real-OAuth host (use --auth storage-state instead).`,
    );
  }

  // ── Phase 2: fill + submit (password NEVER logged) ──────────────────
  console.log(`[${label}] form-login: filling credentials for ${email}`);
  await emailInput.fill(email, { timeout: FORM_LOGIN_ACTION_TIMEOUT_MS });
  await passwordInput.fill(password, { timeout: FORM_LOGIN_ACTION_TIMEOUT_MS });

  const submitVisible = await submitButton.isVisible({ timeout: 2_000 }).catch(() => false);
  if (submitVisible) {
    await submitButton.click({ timeout: FORM_LOGIN_ACTION_TIMEOUT_MS });
  } else {
    // Standard HTML forms submit on Enter and carry the hidden `state`
    // field along with the POST.
    await passwordInput.press("Enter", { timeout: FORM_LOGIN_ACTION_TIMEOUT_MS });
  }

  // ── Phase 3: wait for the app to land back on the meeting path ──────
  // Success is signalled by the top-frame URL settling on the app's
  // `/meeting/<id>` path — NOT merely the app origin. The OAuth callback
  // first lands on `app…/auth/callback?code=…`, and the app may briefly
  // bounce through `/` before restoring `/meeting/<id>`. Resolving on
  // either of those (the earlier behavior) returned control to the caller
  // while the app was still navigating, and the caller's `/meeting/<id>`
  // hang-up detector then misread the callback→meeting navigation as a
  // manual hang-up (observed in live validation). Waiting for the meeting
  // path itself closes that race.
  //
  // Cross-origin safety: we are on the IDENTITY origin at this point
  // (phase 1 waited for its login form; phase 2 submitted it), so the
  // initial pre-redirect `/meeting/<id>` page is already gone — this
  // predicate cannot resolve on it. The `host === appHost` guard is
  // belt-and-suspenders (the meeting path only exists on the app origin).
  const meetingPathPrefix = `/meeting/${meetingId}`;
  console.log(
    `[${label}] form-login: submitted — waiting for the app to settle on ${meetingPathPrefix}`,
  );
  try {
    await page.waitForURL(
      (url) => url.host === appHost && url.pathname.startsWith(meetingPathPrefix),
      { timeout },
    );
  } catch {
    // A rejected credential leaves the page on the identity origin; Playwright's
    // own TimeoutError embeds the raw current URL (with the single-use PKCE
    // `state` in its query). Re-throw with the URL redacted so the "never log a
    // raw URL" contract in the module header holds on the failure path too.
    throw new Error(
      `form-login: app did not settle on ${meetingPathPrefix} within ${timeout}ms ` +
        `(current page: ${redactUrl(page.url())}) — usually a rejected credential`,
    );
  }
  console.log(
    `[${label}] form-login: session established (${redactUrl(page.url())}) — continuing join`,
  );
}
