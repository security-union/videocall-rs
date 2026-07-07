import { describe, it, expect } from "vitest";

import { PARENT_COOKIE_SUFFIXES, resolveCookieDomain } from "../../helpers/auth";

/**
 * Pins the full host -> cookie-`domain` resolution table used by
 * `injectSessionCookie` (e2e/helpers/auth.ts). Each row is a regression guard:
 *
 * - Parent-domain rows fail if `resolveCookieDomain` is reverted to the
 *   pre-#1130 `return url.hostname` behavior (which scoped the cookie host-only
 *   and so never reached the sibling `api.` meeting-api host).
 * - The `pr1.preview.videocall.fnxlabs.com` row fails if the suffix table is
 *   reordered so `.videocall.fnxlabs.com` precedes
 *   `.preview.videocall.fnxlabs.com`, proving the most-specific-first ordering.
 * - The bare-`videocall.fnxlabs.com` row pins the `endsWith` "at least one label
 *   in front" subtlety: a registrable domain must NOT match the leading-dot
 *   suffix and stays host-only.
 */
describe("resolveCookieDomain", () => {
  it("maps known multi-subdomain families to their leading-dot parent domain", () => {
    // HCL daily (Google OAuth): app.+api. share `.videocall.fnxlabs.com`.
    expect(resolveCookieDomain("app.videocall.fnxlabs.com")).toBe(".videocall.fnxlabs.com");
    expect(resolveCookieDomain("api.videocall.fnxlabs.com")).toBe(".videocall.fnxlabs.com");
    // conceptcar7 (Okta PKCE): scopes to `.videocall.conceptcar7.com`, NOT the
    // broader `.conceptcar7.com`.
    expect(resolveCookieDomain("app.videocall.conceptcar7.com")).toBe(".videocall.conceptcar7.com");
  });

  it("scopes PR-preview hosts to the most-specific parent (most-specific-first ordering)", () => {
    // Must resolve to `.preview.videocall.fnxlabs.com` and NOT collapse to the
    // broader `.videocall.fnxlabs.com` — guards the suffix-table ordering.
    expect(resolveCookieDomain("pr1.preview.videocall.fnxlabs.com")).toBe(
      ".preview.videocall.fnxlabs.com",
    );
  });

  it("leaves a bare registrable domain host-only (endsWith needs a label in front)", () => {
    // The leading-dot suffix must NOT match the registrable domain itself.
    expect(resolveCookieDomain("videocall.fnxlabs.com")).toBe("videocall.fnxlabs.com");
  });

  it("falls back to host-only for localhost, raw IPs, and OSS targets", () => {
    expect(resolveCookieDomain("localhost")).toBe("localhost");
    expect(resolveCookieDomain("127.0.0.1")).toBe("127.0.0.1");
    expect(resolveCookieDomain("app.videocall.rs")).toBe("app.videocall.rs");
  });

  it("orders the suffix table most-specific-first", () => {
    // Independent of the resolution rows above, pin the invariant directly: the
    // `.preview.` entry must precede the broader `.videocall.fnxlabs.com` so the
    // first `endsWith` match for a preview host is the most-specific parent.
    const previewIdx = PARENT_COOKIE_SUFFIXES.indexOf(".preview.videocall.fnxlabs.com");
    const broadIdx = PARENT_COOKIE_SUFFIXES.indexOf(".videocall.fnxlabs.com");
    expect(previewIdx).toBeGreaterThanOrEqual(0);
    expect(broadIdx).toBeGreaterThanOrEqual(0);
    expect(previewIdx).toBeLessThan(broadIdx);
  });
});
