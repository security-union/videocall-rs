import { test, expect } from "@playwright/test";
import { injectSessionCookie } from "../helpers/auth";
import { waitForServices } from "../helpers/wait-for-services";

/**
 * E2E tests for the "About" modal on the homepage (issue 785).
 *
 * The homepage renders a thin footer link ("About videocall-ui vX.Y.Z") below
 * the hero card.  Clicking the link opens a glass-backdrop modal that lists
 * the running client build (CARGO_PKG_VERSION + GIT_SHA + BUILD_TIMESTAMP)
 * and the server-side components returned by `GET /api/v1/versions`.
 *
 * The client version literal is intentionally embedded here as a string
 * mirror of `dioxus-ui/Cargo.toml#version`.  If you bump the crate version,
 * update this constant in the same commit so the assertion keeps tracking
 * the running build.
 *
 * The server-versions response is mocked via `page.route` so this spec
 * doesn't depend on `meeting-api` reporting a specific set of services in
 * the docker-compose stack — only that the UI correctly renders whatever
 * the endpoint returns.
 */

const CLIENT_VERSION = "1.1.42";

const MOCK_VERSIONS_BODY = {
  components: [
    {
      service: "meeting-api",
      version: "1.2.3",
      git_sha: "deadbeefcafef00d",
      git_branch: "main",
      build_timestamp: "2026-05-25T12:00:00Z",
    },
    {
      service: "websocket",
      version: "0.9.1",
      git_sha: "abc123456789",
      git_branch: "main",
      build_timestamp: "2026-05-24T10:30:00Z",
    },
  ],
};

test.describe("Homepage About modal", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test.beforeEach(async ({ context, baseURL, page }) => {
    await injectSessionCookie(context, { baseURL });
    // Mock both potential meeting-api hosts (preview slot URL or localhost)
    // so the modal renders the same predictable rows regardless of where
    // the stack is wired.
    await page.route("**/api/v1/versions", async (route) => {
      await route.fulfill({
        status: 200,
        contentType: "application/json",
        body: JSON.stringify(MOCK_VERSIONS_BODY),
      });
    });
  });

  test("footer link is visible on the homepage with the client version", async ({ page }) => {
    await page.goto("/");

    const aboutLink = page.locator('[data-testid="about-footer-link"]');
    await expect(aboutLink).toBeVisible({ timeout: 10_000 });
    await expect(aboutLink).toHaveText(new RegExp(`About videocall-ui v${CLIENT_VERSION}`));
  });

  test("clicking the footer link opens the About modal with client + server rows", async ({
    page,
  }) => {
    await page.goto("/");

    await page.locator('[data-testid="about-footer-link"]').click();

    const modal = page.locator('[data-testid="about-modal"]');
    await expect(modal).toBeVisible({ timeout: 5_000 });

    // Heading
    await expect(modal.getByRole("heading", { name: "About", level: 3 })).toBeVisible();

    // Client section — version row carries the crate version.
    await expect(modal).toContainText(`v${CLIENT_VERSION}`);
    await expect(modal).toContainText("videocall-ui");

    // Server section rows from the mocked response.
    await expect(modal).toContainText("meeting-api");
    await expect(modal).toContainText("1.2.3");
    // Short SHA (first 7 chars) of "deadbeefcafef00d".
    await expect(modal).toContainText("deadbee");
    await expect(modal).toContainText("websocket");
  });

  test("Escape closes the About modal", async ({ page }) => {
    await page.goto("/");

    await page.locator('[data-testid="about-footer-link"]').click();
    const modal = page.locator('[data-testid="about-modal"]');
    await expect(modal).toBeVisible({ timeout: 5_000 });

    // The backdrop owns the keydown handler; focus it so the key event
    // reaches the dialog rather than the page body.
    await modal.focus();
    await page.keyboard.press("Escape");

    await expect(modal).toBeHidden({ timeout: 3_000 });
  });

  test("clicking outside the card closes the About modal", async ({ page }) => {
    await page.goto("/");

    await page.locator('[data-testid="about-footer-link"]').click();
    const modal = page.locator('[data-testid="about-modal"]');
    await expect(modal).toBeVisible({ timeout: 5_000 });

    // Click the backdrop area (the modal element itself, outside the
    // card-apple inner div which stops propagation).  Position 5,5 is at
    // the very top-left of the fixed overlay, well outside the centred
    // card.
    await modal.click({ position: { x: 5, y: 5 } });

    await expect(modal).toBeHidden({ timeout: 3_000 });
  });

  test("Close button dismisses the About modal", async ({ page }) => {
    await page.goto("/");

    await page.locator('[data-testid="about-footer-link"]').click();
    const modal = page.locator('[data-testid="about-modal"]');
    await expect(modal).toBeVisible({ timeout: 5_000 });

    await modal.getByRole("button", { name: "Close About dialog" }).click();
    await expect(modal).toBeHidden({ timeout: 3_000 });
  });

  test("modal shows an error message when the versions endpoint fails", async ({
    page,
    context,
    baseURL,
  }) => {
    // Override the per-test 200 mock with a 500 for this case only.
    // `route` matchers are first-match; we need to remove the previous
    // handler before adding the failing one.
    await page.unroute("**/api/v1/versions");
    await page.route("**/api/v1/versions", async (route) => {
      await route.fulfill({ status: 500, body: "internal error" });
    });
    // The beforeEach injected the cookie before page.route was reset; that
    // doesn't matter for this assertion, but make linters happy.
    void context;
    void baseURL;

    await page.goto("/");
    await page.locator('[data-testid="about-footer-link"]').click();

    const modal = page.locator('[data-testid="about-modal"]');
    await expect(modal).toBeVisible({ timeout: 5_000 });

    // Client rows still render even when server fetch fails.
    await expect(modal).toContainText(`v${CLIENT_VERSION}`);
    // Server-side area shows an error.
    await expect(modal).toContainText(/Couldn't reach the server/i);
  });
});
