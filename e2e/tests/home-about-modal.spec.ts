import { readFileSync } from "node:fs";
import path from "node:path";
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
 * The client version is read from `dioxus-ui/Cargo.toml` at spec load time
 * so the assertion automatically tracks the running build — no hand-bumping
 * required when the crate version changes.
 *
 * The server-versions response is mocked via `page.route` so this spec
 * doesn't depend on `meeting-api` reporting a specific set of services in
 * the docker-compose stack — only that the UI correctly renders whatever
 * the endpoint returns.
 */

function readClientVersionFromCargoToml(): string {
  const cargoTomlPath = path.resolve(__dirname, "../../dioxus-ui/Cargo.toml");
  const text = readFileSync(cargoTomlPath, "utf8");
  // Match the first `version = "X.Y.Z"` in the [package] section.  The
  // dioxus-ui Cargo.toml puts [package] at the top, so the first match
  // is the crate version (a later `version = "..."` in [dependencies]
  // would only be reached if the file is reordered, in which case the
  // version regex below still validates the shape).
  const match = text.match(/^version\s*=\s*"(\d+\.\d+\.\d+)"/m);
  if (!match) {
    throw new Error(
      `Could not parse version from ${cargoTomlPath}; expected a 'version = "X.Y.Z"' line.`,
    );
  }
  return match[1];
}

const CLIENT_VERSION = readClientVersionFromCargoToml();

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

  test("dialog autofocuses on open and Escape closes it", async ({ page }) => {
    await page.goto("/");

    await page.locator('[data-testid="about-footer-link"]').click();
    const modal = page.locator('[data-testid="about-modal"]');
    const dialog = page.locator('[data-testid="about-modal-dialog"]');
    await expect(modal).toBeVisible({ timeout: 5_000 });

    // The inner `.card-apple` is the dialog (role="dialog", tabindex="0")
    // and `onmounted` calls `set_focus(true)` so keyboard-only users can
    // dismiss with Escape immediately — without any manual focus step.
    await expect(dialog).toBeFocused({ timeout: 3_000 });

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
