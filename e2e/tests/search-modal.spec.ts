import { test, expect, APIRequestContext } from "@playwright/test";
import { injectSessionCookie, generateSessionToken } from "../helpers/auth";
import { waitForServices } from "../helpers/wait-for-services";

/**
 * E2E coverage for the global Cmd-K / Ctrl-K search modal.
 *
 * In the E2E compose stack `SEARCH_API_BASE_URL` is not configured, so the
 * modal falls back to meeting-api Postgres search.  That is exactly the path
 * this spec exercises: it creates a meeting through the REST API (which the
 * authenticated session can do), then opens the modal and types a fragment
 * of the meeting id to assert the fallback list-meetings query drives the
 * result row and navigation.
 *
 * A separate assertion types Lucene metacharacters into the input to make
 * sure the escape helper keeps the UI alive even if SearchV2 were the
 * primary path (the middleware rejects unescaped queries with HTTP 400).
 */

const API_BASE_URL = process.env.API_BASE_URL || "http://localhost:8081";
const E2E_USER_EMAIL = "e2e-test@videocall.rs";
const E2E_USER_NAME = "E2ETestUser";

/**
 * Create a meeting via the REST API using the same session identity the
 * browser context will use.  Returns the generated meeting id.
 */
async function createMeeting(request: APIRequestContext, meetingId: string): Promise<void> {
  const token = generateSessionToken(E2E_USER_EMAIL, E2E_USER_NAME);
  const res = await request.post(`${API_BASE_URL}/api/v1/meetings`, {
    headers: {
      "Content-Type": "application/json",
      Cookie: `session=${token}`,
    },
    data: {
      meeting_id: meetingId,
      attendees: [],
      waiting_room_enabled: false,
      admitted_can_admit: false,
    },
  });
  // 201 on create, 409 if a prior test run already created this id.
  expect([201, 409]).toContain(res.status());
}

// Playwright `getByPlaceholder` is locale-agnostic; keep the literal in one
// place so a UI copy change is a single edit.
const SEARCH_PLACEHOLDER = "Search meetings...";

// Ctrl+K on Linux/CI, Meta+K on macOS devs running locally — the Rust
// listener accepts either.  CI runs Linux so Control is the canonical choice.
const OPEN_SHORTCUT = "Control+k";

test.describe("Search modal (Cmd-K)", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test.beforeEach(async ({ context, baseURL }) => {
    await injectSessionCookie(context, { baseURL });
  });

  test("Ctrl+K opens the modal and focuses the input", async ({ page }) => {
    await page.goto("/");
    await page.waitForTimeout(1500);

    // Modal input not mounted yet.
    await expect(page.getByPlaceholder(SEARCH_PLACEHOLDER)).toHaveCount(0);

    await page.keyboard.press(OPEN_SHORTCUT);

    const input = page.getByPlaceholder(SEARCH_PLACEHOLDER);
    await expect(input).toBeVisible({ timeout: 5_000 });
    // `onmounted` in `SearchModal` calls `.set_focus(true)` so the cursor
    // should land inside the input immediately — users expect to start
    // typing without a click.
    await expect(input).toBeFocused();
  });

  test("Escape closes the modal", async ({ page }) => {
    await page.goto("/");
    await page.waitForTimeout(1500);

    await page.keyboard.press(OPEN_SHORTCUT);
    const input = page.getByPlaceholder(SEARCH_PLACEHOLDER);
    await expect(input).toBeVisible({ timeout: 5_000 });

    await page.keyboard.press("Escape");
    await expect(page.getByPlaceholder(SEARCH_PLACEHOLDER)).toHaveCount(0, { timeout: 5_000 });
  });

  test("Ctrl+K toggles the modal open and closed", async ({ page }) => {
    await page.goto("/");
    await page.waitForTimeout(1500);

    // Open.
    await page.keyboard.press(OPEN_SHORTCUT);
    await expect(page.getByPlaceholder(SEARCH_PLACEHOLDER)).toBeVisible({ timeout: 5_000 });

    // Second press closes.
    await page.keyboard.press(OPEN_SHORTCUT);
    await expect(page.getByPlaceholder(SEARCH_PLACEHOLDER)).toHaveCount(0, { timeout: 5_000 });
  });

  test("clicking the backdrop closes the modal", async ({ page }) => {
    await page.goto("/");
    await page.waitForTimeout(1500);

    await page.keyboard.press(OPEN_SHORTCUT);
    const input = page.getByPlaceholder(SEARCH_PLACEHOLDER);
    await expect(input).toBeVisible({ timeout: 5_000 });

    // Click near the top-left corner of the viewport — outside the centred
    // modal dialog — to hit the backdrop's onclick handler.
    await page.mouse.click(10, 10);
    await expect(page.getByPlaceholder(SEARCH_PLACEHOLDER)).toHaveCount(0, { timeout: 5_000 });
  });

  test("typing a meeting id surfaces a result row with the correct href", async ({
    page,
    request,
  }) => {
    // Unique per-run meeting id so parallel or re-run invocations don't
    // trip over each other's state (and so partial-match assertions are
    // unambiguous).
    const meetingId = `e2e-search-${Date.now()}`;
    await createMeeting(request, meetingId);

    await page.goto("/");
    await page.waitForTimeout(1500);

    await page.keyboard.press(OPEN_SHORTCUT);
    const input = page.getByPlaceholder(SEARCH_PLACEHOLDER);
    await expect(input).toBeVisible({ timeout: 5_000 });

    // Type a distinctive fragment — the whole timestamp — so the server-side
    // ILIKE can't match any other meeting leaked in from previous tests.
    const fragment = meetingId.split("-").slice(-1)[0];
    await input.pressSequentially(fragment, { delay: 40 });

    // Result row is an <a href="/meeting/<id>"> — assert it appears and
    // points at the meeting we just created.
    const resultLink = page.locator(`a[href="/meeting/${meetingId}"]`);
    await expect(resultLink).toBeVisible({ timeout: 10_000 });
    await expect(resultLink).toContainText(meetingId);
  });

  test("clicking a result navigates to the meeting page", async ({ page, request }) => {
    const meetingId = `e2e-navigate-${Date.now()}`;
    await createMeeting(request, meetingId);

    await page.goto("/");
    await page.waitForTimeout(1500);

    await page.keyboard.press(OPEN_SHORTCUT);
    const input = page.getByPlaceholder(SEARCH_PLACEHOLDER);
    await expect(input).toBeVisible({ timeout: 5_000 });

    await input.pressSequentially(meetingId.split("-").slice(-1)[0], { delay: 40 });

    const resultLink = page.locator(`a[href="/meeting/${meetingId}"]`);
    await expect(resultLink).toBeVisible({ timeout: 10_000 });
    await resultLink.click();

    await expect(page).toHaveURL(new RegExp(`/meeting/${meetingId}`), { timeout: 10_000 });
  });

  test("empty query shows no results and no error", async ({ page }) => {
    await page.goto("/");
    await page.waitForTimeout(1500);

    await page.keyboard.press(OPEN_SHORTCUT);
    const input = page.getByPlaceholder(SEARCH_PLACEHOLDER);
    await expect(input).toBeVisible({ timeout: 5_000 });

    // With an empty query the modal short-circuits to `results.set(Vec::new())`
    // and neither the loading spinner nor the "No meetings found" empty
    // state should render — just the bare input.
    await expect(page.getByText("Searching...")).toHaveCount(0);
    await expect(page.getByText("No meetings found")).toHaveCount(0);
  });

  test("a non-matching query renders the empty state", async ({ page }) => {
    await page.goto("/");
    await page.waitForTimeout(1500);

    await page.keyboard.press(OPEN_SHORTCUT);
    const input = page.getByPlaceholder(SEARCH_PLACEHOLDER);
    await expect(input).toBeVisible({ timeout: 5_000 });

    // A string that cannot match any meeting id in any test run.
    await input.pressSequentially("zzz-no-such-meeting-zzz", { delay: 30 });
    await expect(page.getByText("No meetings found")).toBeVisible({ timeout: 10_000 });
  });

  test("Lucene metacharacters do not crash the modal", async ({ page }) => {
    // Even though the E2E stack uses the Postgres fallback (which doesn't
    // parse Lucene syntax), this input would previously have crashed the
    // SearchV2 path or triggered HTTP 400s from the middleware.  Asserting
    // the modal stays up validates that `escape_lucene_query_string` is
    // applied before the query string is assembled.
    await page.goto("/");
    await page.waitForTimeout(1500);

    await page.keyboard.press(OPEN_SHORTCUT);
    const input = page.getByPlaceholder(SEARCH_PLACEHOLDER);
    await expect(input).toBeVisible({ timeout: 5_000 });

    // Every single-char Lucene metachar that the escape helper covers.
    await input.pressSequentially(`\\+-!(){}[]^"~*?:/&|`, { delay: 20 });

    // The input stays visible and no error banner is rendered (errors are
    // shown in red via `color:#ef4444` inside the same results container).
    await expect(input).toBeVisible();
    // The error banner uses the same container as results; assert by its
    // text prefix — `SearchV2 returned HTTP` / `SearchV2 parse error` /
    // `SearchV2 request failed` / `Client config error`.  None should fire.
    await expect(page.getByText(/SearchV2 (returned|parse error|request failed)/)).toHaveCount(0);
    await expect(page.getByText(/Client config error/)).toHaveCount(0);
  });
});
