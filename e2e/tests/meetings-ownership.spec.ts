import { test, expect, chromium, Browser, BrowserContext, Page } from "@playwright/test";
import { generateSessionToken } from "../helpers/auth";
import { waitForServices } from "../helpers/wait-for-services";
import { createMeeting, joinMeeting, deleteAllOwnedMeetings } from "../helpers/meeting-api";

const COOKIE_NAME = process.env.COOKIE_NAME || "session";

const BROWSER_ARGS = [
  "--ignore-certificate-errors",
  "--origin-to-force-quic-on=127.0.0.1:4433",
  "--use-fake-device-for-media-stream",
  "--use-fake-ui-for-media-stream",
  "--disable-gpu",
];

// ---------------------------------------------------------------------------
// Selectors
// ---------------------------------------------------------------------------

const MEETINGS_SECTION = ".meetings-list-container";
const MEETINGS_LIST_ROWS = ".meetings-list-container .meeting-item";
const OWNER_ICON = ".meeting-owner-icon";
const EDIT_BTN = ".meeting-edit-btn";
const DELETE_BTN = ".meeting-delete-btn";
const TOOLTIP_VISIBLE = "#meeting-info-tooltip-global.is-visible";
const TOOLTIP = "#meeting-info-tooltip-global";

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/**
 * Build a fresh authenticated browser context for the given identity.
 *
 * Mirrors the pattern in `auth.ts::injectSessionCookie` but is parameterised
 * over the launched browser instance so the two-browser tests can run two
 * independent identities side-by-side without sharing storage state.
 */
async function createAuthenticatedContext(
  browser: Browser,
  email: string,
  name: string,
  uiURL: string,
): Promise<BrowserContext> {
  const context = await browser.newContext({
    baseURL: uiURL,
    ignoreHTTPSErrors: true,
  });
  const token = generateSessionToken(email, name);
  const url = new URL(uiURL);
  await context.addCookies([
    {
      name: COOKIE_NAME,
      value: token,
      domain: url.hostname,
      path: "/",
      httpOnly: true,
      secure: false,
      sameSite: "Lax",
    },
  ]);
  return context;
}

/**
 * Wait for the merged meetings section to settle (loading spinner gone)
 * and at least the expected row count to render.
 */
async function waitForMeetingsRowCount(page: Page, expected: number): Promise<void> {
  await expect(page.locator(`${MEETINGS_SECTION} .meetings-loading`)).toHaveCount(0, {
    timeout: 15_000,
  });
  await expect(page.locator(MEETINGS_LIST_ROWS)).toHaveCount(expected, { timeout: 10_000 });
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/**
 * Two-browser ownership regression guard.
 *
 * The original bug: the home page meeting list inferred ownership from the
 * row's host email rather than from a server-supplied flag. Two distinct
 * authenticated identities looking at the same meeting could each see
 * themselves as "the owner" — User B (a non-owner who had been admitted)
 * would see the gold owner icon, the edit button, and the delete button
 * on a meeting User A actually owns.
 *
 * The fix: the home page now calls `GET /api/v1/meetings/feed`, which
 * returns a server-computed `is_owner` boolean per row. The UI gates the
 * inline gold star (`.meeting-owner-icon`), the edit / delete buttons,
 * and the tooltip's "Owner" line on that flag exclusively.
 *
 * This spec drives two real Chromium instances with distinct session
 * cookies and asserts the gating end-to-end. The corresponding backend
 * regression test
 * (`meeting-api/tests/list_feed_tests.rs::test_two_identities_disjoint_is_owner_for_same_meeting`)
 * covers the wire boundary; this spec covers the UI binding.
 */
test.describe("Meeting list ownership gating (two-browser regression)", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test("owner sees owner-only affordances; non-owner does not", async ({ baseURL }) => {
    test.setTimeout(120_000);

    const uiURL = baseURL || "http://localhost:3001";
    const ts = Date.now();
    const meetingId = `e2e_ownership_${ts}`;
    const userAEmail = `ownership-a-${ts}@videocall.rs`;
    const userAName = "OwnershipUserA";
    const userBEmail = `ownership-b-${ts}@videocall.rs`;
    const userBName = "OwnershipUserB";

    const browserA = await chromium.launch({ args: BROWSER_ARGS });
    const browserB = await chromium.launch({ args: BROWSER_ARGS });

    try {
      // Seed via the meeting-api: User A creates the meeting, User A joins
      // (activating it), then User B joins and gets auto-admitted (waiting
      // room is off so the join sets `admitted_at` immediately). The
      // resulting DB state is the canonical "two identities, one meeting,
      // one owner" shape that motivated the regression.
      await createMeeting(userAEmail, userAName, {
        meetingId,
        waitingRoomEnabled: false,
      });
      await joinMeeting(userAEmail, userAName, meetingId, userAName);
      const userBJoin = await joinMeeting(userBEmail, userBName, meetingId, userBName);
      // Hard sanity-check the seed: any other status would mean User B
      // never landed in the merged feed, making the non-owner assertions
      // below silently vacuous.
      expect(userBJoin.status, "User B must auto-admit on join (waiting room is off)").toBe(
        "admitted",
      );

      // ── User A: should see the row with owner-only affordances ────────
      const ctxA = await createAuthenticatedContext(browserA, userAEmail, userAName, uiURL);
      const pageA = await ctxA.newPage();
      await pageA.goto("/");
      await pageA.waitForTimeout(1500);
      await waitForMeetingsRowCount(pageA, 1);

      const rowA = pageA.locator(MEETINGS_LIST_ROWS).filter({ hasText: meetingId });
      await expect(rowA).toHaveCount(1);

      // Gold-star icon present + accessible "Owner" label.
      const iconA = rowA.locator(OWNER_ICON);
      await expect(iconA).toHaveCount(1);
      await expect(iconA).toBeVisible();
      await expect(iconA).toHaveAttribute("aria-label", "Owner");

      // Owner-only buttons present.
      await expect(rowA.locator(EDIT_BTN)).toHaveCount(1);
      await expect(rowA.locator(DELETE_BTN)).toHaveCount(1);

      // Hover the row → tooltip portal carries the literal "Owner" copy.
      await rowA.locator(".meeting-item-content").first().hover();
      await expect(pageA.locator(TOOLTIP_VISIBLE)).toBeVisible({ timeout: 2_000 });
      await expect(pageA.locator(TOOLTIP)).toContainText("Owner");
      // Move the pointer off the row so the tooltip is hidden again before
      // we navigate away — keeps the page state predictable for cleanup.
      await pageA.mouse.move(0, 0);

      // ── User B: same row, but ZERO owner affordances ──────────────────
      const ctxB = await createAuthenticatedContext(browserB, userBEmail, userBName, uiURL);
      const pageB = await ctxB.newPage();
      await pageB.goto("/");
      await pageB.waitForTimeout(1500);
      await waitForMeetingsRowCount(pageB, 1);

      const rowB = pageB.locator(MEETINGS_LIST_ROWS).filter({ hasText: meetingId });
      await expect(rowB).toHaveCount(1);

      // No gold-star icon anywhere in the non-owner row.
      await expect(rowB.locator(OWNER_ICON)).toHaveCount(0);
      // No owner-only buttons.
      await expect(rowB.locator(EDIT_BTN)).toHaveCount(0);
      await expect(rowB.locator(DELETE_BTN)).toHaveCount(0);

      // Hover the row → tooltip is visible but does NOT contain "Owner".
      // This is the most direct UI signal that the `is_owner` flag is
      // gating tooltip composition correctly for non-owners.
      await rowB.locator(".meeting-item-content").first().hover();
      await expect(pageB.locator(TOOLTIP_VISIBLE)).toBeVisible({ timeout: 2_000 });
      const tooltipB = pageB.locator(TOOLTIP);
      // Expect the tooltip's text to NOT contain "Owner". We use a regex
      // negation against the rendered textContent because Playwright's
      // matchers don't have a direct "not.toContainText" semantic that
      // survives partial async settle.
      const tooltipBText = (await tooltipB.textContent()) ?? "";
      expect(tooltipBText).not.toMatch(/Owner/);
      // Active-meeting metadata rows must still render — we're asserting
      // ownership lines are absent, not the whole tooltip.
      expect(tooltipBText).toMatch(/Started on|Last active on/);
      expect(tooltipBText).toMatch(/Duration/);
    } finally {
      // Best-effort cleanup so seeded state doesn't bleed into future
      // runs. Failures here are tolerated by the helper.
      try {
        await deleteAllOwnedMeetings(userAEmail, userAName);
      } catch {
        // ignore — the harness will pave over leftovers on next run
      }
      await browserA.close();
      await browserB.close();
    }
  });
});
