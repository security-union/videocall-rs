import { afterEach, describe, expect, it, vi } from "vitest";

import type { Page } from "@playwright/test";

import {
  FORM_LOGIN_EMAIL_SELECTOR,
  FORM_LOGIN_PASSWORD_SELECTOR,
  FORM_LOGIN_SUBMIT_SELECTOR,
  FORM_LOGIN_TRIGGER_SELECTOR,
  isKnownPublicIdpHost,
  performFormLogin,
  redactUrl,
  resolveFormLoginCredentials,
} from "./form-login";

const APP_BASE = "https://app.videocall.labsworkspace.fnxlabs.com";
const SECRET_PASSWORD = "hunter2-DO-NOT-LOG";
const EMAIL = "bot@example.test";

interface MockLocator {
  waitFor: ReturnType<typeof vi.fn>;
  fill: ReturnType<typeof vi.fn>;
  press: ReturnType<typeof vi.fn>;
  isVisible: ReturnType<typeof vi.fn>;
  click: ReturnType<typeof vi.fn>;
}

function mockLocator(over: Partial<MockLocator> = {}): MockLocator {
  return {
    waitFor: vi.fn().mockResolvedValue(undefined),
    fill: vi.fn().mockResolvedValue(undefined),
    press: vi.fn().mockResolvedValue(undefined),
    isVisible: vi.fn().mockResolvedValue(true),
    click: vi.fn().mockResolvedValue(undefined),
    ...over,
  };
}

/** A promise that never settles — models an element that never appears. */
const pending = (): Promise<never> => new Promise<never>(() => {});

function makePage(opts: {
  email?: MockLocator;
  password?: MockLocator;
  submit?: MockLocator;
  trigger?: MockLocator;
  currentUrl?: string;
  waitForURL?: ReturnType<typeof vi.fn>;
}): {
  page: Page;
  email: MockLocator;
  password: MockLocator;
  submit: MockLocator;
  trigger: MockLocator;
  locator: ReturnType<typeof vi.fn>;
  waitForURL: ReturnType<typeof vi.fn>;
} {
  const email = opts.email ?? mockLocator();
  const password = opts.password ?? mockLocator();
  const submit = opts.submit ?? mockLocator();
  // By default the app auto-redirects to the identity form, so the
  // app-side login trigger never appears (its wait never settles).
  const trigger = opts.trigger ?? mockLocator({ waitFor: vi.fn(pending) });

  const byId: Record<string, MockLocator> = {
    [FORM_LOGIN_EMAIL_SELECTOR]: email,
    [FORM_LOGIN_PASSWORD_SELECTOR]: password,
    [FORM_LOGIN_SUBMIT_SELECTOR]: submit,
    [FORM_LOGIN_TRIGGER_SELECTOR]: trigger,
  };
  const locator = vi.fn((sel: string) => {
    const l = byId[sel];
    if (!l) throw new Error(`unexpected selector passed to page.locator: ${sel}`);
    return { first: () => l };
  });
  const waitForURL = opts.waitForURL ?? vi.fn().mockResolvedValue(undefined);
  const page = {
    locator,
    waitForURL,
    url: vi.fn(() => opts.currentUrl ?? `${APP_BASE}/meeting/bottest`),
  };
  return { page: page as unknown as Page, email, password, submit, trigger, locator, waitForURL };
}

describe("resolveFormLoginCredentials", () => {
  it("returns trimmed email + verbatim password when both are set", () => {
    expect(
      resolveFormLoginCredentials({ BOT_EMAIL: "  bot@x.test ", BOT_PASSWORD: " pw " }),
    ).toEqual({ email: "bot@x.test", password: " pw " });
  });

  it("returns null when the email is missing or blank", () => {
    expect(resolveFormLoginCredentials({ BOT_PASSWORD: "pw" })).toBeNull();
    expect(resolveFormLoginCredentials({ BOT_EMAIL: "   ", BOT_PASSWORD: "pw" })).toBeNull();
  });

  it("returns null when the password is missing or empty", () => {
    // Mutation guard: dropping the `!password` check makes this non-null.
    expect(resolveFormLoginCredentials({ BOT_EMAIL: "bot@x.test" })).toBeNull();
    expect(resolveFormLoginCredentials({ BOT_EMAIL: "bot@x.test", BOT_PASSWORD: "" })).toBeNull();
  });
});

describe("redactUrl", () => {
  it("strips the query string and hash (so the OAuth code never lands in a log)", () => {
    expect(redactUrl(`${APP_BASE}/auth/callback?code=SECRET_CODE&state=xyz#frag`)).toBe(
      `${APP_BASE}/auth/callback`,
    );
  });

  it("returns a fixed placeholder for an unparseable URL", () => {
    expect(redactUrl("not a url")).toBe("<unparseable-url>");
  });
});

describe("performFormLogin", () => {
  afterEach(() => {
    vi.restoreAllMocks();
  });

  it("fills the identity form with the exact production selectors and submits", async () => {
    const { page, email, password, submit, locator, waitForURL } = makePage({});

    await performFormLogin({
      page,
      email: EMAIL,
      password: SECRET_PASSWORD,
      appBaseUrl: APP_BASE,
      meetingId: "bottest",
      label: "bot",
    });

    // Pins that the fill used the production selector CONSTANTS. NOTE: the mock
    // page is keyed by these same exported constants, so this does NOT catch a
    // selector VALUE that no longer matches the real login form — only an e2e
    // run against the live identity form guards that.
    expect(locator).toHaveBeenCalledWith(FORM_LOGIN_EMAIL_SELECTOR);
    expect(locator).toHaveBeenCalledWith(FORM_LOGIN_PASSWORD_SELECTOR);

    // The right value went into the right field.
    expect(email.fill).toHaveBeenCalledWith(EMAIL, expect.anything());
    expect(password.fill).toHaveBeenCalledWith(SECRET_PASSWORD, expect.anything());

    // Submit button present -> clicked; Enter fallback NOT used.
    expect(submit.click).toHaveBeenCalledTimes(1);
    expect(password.press).not.toHaveBeenCalled();

    // Waited for the app to consume the callback.
    expect(waitForURL).toHaveBeenCalledTimes(1);
  });

  it("NEVER logs the password (neither the value nor via a logged URL)", async () => {
    const logSpy = vi.spyOn(console, "log").mockImplementation(() => {});
    const warnSpy = vi.spyOn(console, "warn").mockImplementation(() => {});
    const errorSpy = vi.spyOn(console, "error").mockImplementation(() => {});

    // Land on a callback URL that carries a secret code, to also prove
    // the logged URL is redacted.
    const { page } = makePage({
      currentUrl: `${APP_BASE}/auth/callback?code=SECRET_CODE&state=abc`,
    });

    await performFormLogin({
      page,
      email: EMAIL,
      password: SECRET_PASSWORD,
      appBaseUrl: APP_BASE,
      meetingId: "bottest",
      label: "bot",
    });

    const allLogged = [...logSpy.mock.calls, ...warnSpy.mock.calls, ...errorSpy.mock.calls]
      .flat()
      .map((a) => String(a))
      .join(" ⋮ ");

    expect(allLogged).not.toContain(SECRET_PASSWORD);
    // Redaction guard: the raw single-use code must not be logged either.
    expect(allLogged).not.toContain("SECRET_CODE");
  });

  it("falls back to pressing Enter when there is no submit button", async () => {
    const submit = mockLocator({ isVisible: vi.fn().mockResolvedValue(false) });
    const { page, password } = makePage({ submit });

    await performFormLogin({
      page,
      email: EMAIL,
      password: SECRET_PASSWORD,
      appBaseUrl: APP_BASE,
      meetingId: "bottest",
      label: "bot",
    });

    expect(submit.click).not.toHaveBeenCalled();
    expect(password.press).toHaveBeenCalledWith("Enter", expect.anything());
  });

  it("clicks an app-side login trigger before filling when the app does not auto-redirect", async () => {
    // The email field only becomes visible on the SECOND wait — after the
    // trigger is clicked. The first (race) wait for the email field stays
    // pending so the trigger arm wins the race.
    const email = mockLocator({
      waitFor: vi.fn().mockReturnValueOnce(pending()).mockResolvedValue(undefined),
    });
    const trigger = mockLocator(); // its waitFor resolves immediately -> wins the race
    const { page } = makePage({ email, trigger });

    await performFormLogin({
      page,
      email: EMAIL,
      password: SECRET_PASSWORD,
      appBaseUrl: APP_BASE,
      meetingId: "bottest",
      label: "bot",
    });

    // Mutation guard: remove the trigger-click branch and this fails.
    expect(trigger.click).toHaveBeenCalledTimes(1);
    expect(email.fill).toHaveBeenCalledWith(EMAIL, expect.anything());
  });

  it("throws (with no secret) when neither the form nor a login trigger appears", async () => {
    const reject = (): Promise<never> => Promise.reject(new Error("TimeoutError"));
    const email = mockLocator({ waitFor: vi.fn(reject) });
    const trigger = mockLocator({ waitFor: vi.fn(reject) });
    const { page } = makePage({ email, trigger });

    await expect(
      performFormLogin({
        page,
        email: EMAIL,
        password: SECRET_PASSWORD,
        appBaseUrl: APP_BASE,
        meetingId: "bottest",
        label: "bot",
      }),
    ).rejects.toThrow(/identity login form did not appear/);
  });

  it("only treats landing on the /meeting/<id> path as success (not /auth/callback or /)", async () => {
    const { page, waitForURL } = makePage({});
    await performFormLogin({
      page,
      email: EMAIL,
      password: SECRET_PASSWORD,
      appBaseUrl: APP_BASE,
      meetingId: "bottest",
      label: "bot",
    });

    const predicate = waitForURL.mock.calls[0][0] as (url: URL) => boolean;

    // Settled on the meeting path -> success (query is allowed).
    expect(predicate(new URL(`${APP_BASE}/meeting/bottest`))).toBe(true);
    expect(predicate(new URL(`${APP_BASE}/meeting/bottest?netsim=good_wifi`))).toBe(true);

    // Regression lock for the live-validated defect: the OAuth callback
    // hop must NOT count as success — returning here let the caller's
    // hang-up detector misread the callback→meeting navigation. Reverting
    // the phase-3 predicate to `!pathname.startsWith("/login")` flips this
    // to `true` and the test fails.
    expect(predicate(new URL(`${APP_BASE}/auth/callback?code=x&state=y`))).toBe(false);

    // A transient bounce through `/` must NOT count either — we wait for
    // the meeting path specifically.
    expect(predicate(new URL(`${APP_BASE}/`))).toBe(false);

    // Still on the identity origin -> not yet.
    expect(predicate(new URL("https://id.labsworkspace.fnxlabs.com/login?state=z"))).toBe(false);

    // A DIFFERENT meeting id on the app origin is not our meeting.
    expect(predicate(new URL(`${APP_BASE}/meeting/other-room`))).toBe(false);
  });

  it("refuses to type credentials into a known public IdP (blocker guard)", async () => {
    // Phase 1 detects a login form, but the page is a real public IdP.
    // PR #2082 blocker: a mis-pointed run must not send throwaway creds to
    // e.g. Google. The URL carries a `state`; only the host may surface.
    const { page, email, password } = makePage({
      currentUrl: "https://accounts.google.com/signin/v2?state=SECRET_STATE&client_id=abc",
    });

    let err: Error | undefined;
    try {
      await performFormLogin({
        page,
        email: EMAIL,
        password: SECRET_PASSWORD,
        appBaseUrl: APP_BASE,
        meetingId: "bottest",
        label: "bot",
      });
    } catch (e) {
      err = e as Error;
    }

    // Mutation guard: delete the isKnownPublicIdpHost check and the flow
    // proceeds to fill — these three assertions all flip.
    expect(err).toBeDefined();
    expect(err?.message).toMatch(/refusing to enter credentials/);
    expect(err?.message).toContain("accounts.google.com");
    // Refused BEFORE any credential was typed, and the redacted message
    // leaks neither the password nor the raw `state`.
    expect(email.fill).not.toHaveBeenCalled();
    expect(password.fill).not.toHaveBeenCalled();
    expect(err?.message).not.toContain(SECRET_PASSWORD);
    expect(err?.message).not.toContain("SECRET_STATE");
  });

  it("redacts the URL on the phase-3 'did not settle' failure path", async () => {
    // A rejected credential leaves the page on the identity origin and
    // waitForURL times out. Playwright's TimeoutError embeds the raw URL
    // (with the single-use `state`); the catch must re-throw it redacted.
    const { page, email, password } = makePage({
      currentUrl: "https://id.labsworkspace.fnxlabs.com/login?state=SECRET_STATE_VALUE&client_id=x",
      waitForURL: vi
        .fn()
        .mockRejectedValue(
          new Error(
            'TimeoutError: navigated to "https://id.labsworkspace.fnxlabs.com/login?state=SECRET_STATE_VALUE"',
          ),
        ),
    });

    let err: Error | undefined;
    try {
      await performFormLogin({
        page,
        email: EMAIL,
        password: SECRET_PASSWORD,
        appBaseUrl: APP_BASE,
        meetingId: "bottest",
        label: "bot",
      });
    } catch (e) {
      err = e as Error;
    }

    // The self-hosted identity host is NOT a public IdP, so we got past the
    // guard and actually attempted the login.
    expect(email.fill).toHaveBeenCalledTimes(1);
    expect(password.fill).toHaveBeenCalledTimes(1);
    // Mutation guard: remove the catch/redact around waitForURL and the raw
    // Playwright TimeoutError (carrying the `state`) propagates — the last
    // two assertions fail.
    expect(err?.message).toMatch(/did not settle on/);
    expect(err?.message).not.toContain("SECRET_STATE_VALUE");
    expect(err?.message).not.toContain("?state=");
  });

  it("redacts the URL when the form never appears after clicking the login trigger", async () => {
    // Trigger wins the race; after the click the email field still never
    // appears, so the post-click waitFor rejects. Its error must be redacted
    // (form-login.ts:228 fix) — the raw URL carries a `state`.
    const email = mockLocator({
      waitFor: vi
        .fn()
        .mockReturnValueOnce(pending())
        .mockRejectedValue(
          new Error(
            'TimeoutError: navigated to "https://id.labsworkspace.fnxlabs.com/login?state=SECRET_TRIGGER_STATE"',
          ),
        ),
    });
    const trigger = mockLocator(); // resolves immediately -> wins the race
    const { page } = makePage({
      email,
      trigger,
      currentUrl: "https://id.labsworkspace.fnxlabs.com/login?state=SECRET_TRIGGER_STATE",
    });

    let err: Error | undefined;
    try {
      await performFormLogin({
        page,
        email: EMAIL,
        password: SECRET_PASSWORD,
        appBaseUrl: APP_BASE,
        meetingId: "bottest",
        label: "bot",
      });
    } catch (e) {
      err = e as Error;
    }

    expect(trigger.click).toHaveBeenCalledTimes(1);
    expect(err?.message).toMatch(/did not appear after clicking/);
    expect(err?.message).not.toContain("SECRET_TRIGGER_STATE");
  });
});

describe("isKnownPublicIdpHost", () => {
  it("matches known public IdPs exactly and as subdomains", () => {
    expect(isKnownPublicIdpHost("accounts.google.com")).toBe(true);
    expect(isKnownPublicIdpHost("login.microsoftonline.com")).toBe(true);
    expect(isKnownPublicIdpHost("appleid.apple.com")).toBe(true);
    expect(isKnownPublicIdpHost("github.com")).toBe(true);
    // Case-insensitive + subdomain of a listed registrable host.
    expect(isKnownPublicIdpHost("ACCOUNTS.GOOGLE.COM")).toBe(true);
    expect(isKnownPublicIdpHost("mytenant.okta.com")).toBe(true);
  });

  it("does NOT match the self-hosted identity target or lookalikes", () => {
    // The reference form-login target must pass the guard.
    expect(isKnownPublicIdpHost("id.labsworkspace.fnxlabs.com")).toBe(false);
    expect(isKnownPublicIdpHost("app.videocall.labsworkspace.fnxlabs.com")).toBe(false);
    // Same-suffix-different-registrable-domain lookalikes must NOT match —
    // guards against a naive `includes`/`endsWith` without the dot boundary.
    expect(isKnownPublicIdpHost("notgithub.com")).toBe(false);
    expect(isKnownPublicIdpHost("accounts.google.com.evil.test")).toBe(false);
  });
});
