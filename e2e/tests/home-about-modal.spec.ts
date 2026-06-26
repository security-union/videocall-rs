import { readFileSync } from "node:fs";
import path from "node:path";
import { test, expect } from "@playwright/test";
import { injectSessionCookie } from "../helpers/auth";
import { waitForServices } from "../helpers/wait-for-services";
import { setShowBuildGitInfoFlag } from "../helpers/show-build-git-info-config";

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
    // Non-anchored RegExp → matches as a substring, so the ungated
    // " \u{00b7} built <date>" suffix (issue #1480) appended after the version
    // does NOT break this prefix assertion.
    await expect(aboutLink).toHaveText(new RegExp(`About videocall-ui v${CLIENT_VERSION}`));
  });

  test("footer shows the ungated ' \u{00b7} built <YYYY-MM-DD>' build-date suffix (issue #1480)", async ({
    page,
  }) => {
    // Issue #1480: the home footer reads
    //   `About videocall-ui v<version> \u{00b7} built <YYYY-MM-DD>`
    // The " \u{00b7} built <date>" suffix is UNGATED (shown regardless of
    // showBuildGitInfo) — it is the build TIMESTAMP compacted to its calendar
    // date (`build_date(env!("BUILD_TIMESTAMP"))` in pages/home.rs). It is
    // omitted entirely only when build.rs emitted the `"unknown"` sentinel. The
    // E2E stack builds with a real timestamp, so the suffix is present.
    await page.goto("/");

    const aboutLink = page.locator('[data-testid="about-footer-link"]');
    await expect(aboutLink).toBeVisible({ timeout: 10_000 });
    // U+00B7 MIDDLE DOT separator + a literal "built" + a YYYY-MM-DD-shaped date.
    await expect(aboutLink).toHaveText(/· built \d{4}-\d{2}-\d{2}/);
  });

  test("clicking the footer link opens the About modal with client + server rows", async ({
    page,
    context,
  }) => {
    // Issue #1480: pin showBuildGitInfo ON EXPLICITLY (rather than leaning on the
    // committed config.js default) so this test's commit/branch assertions are
    // self-documenting and survive a future flip of the committed default. Must
    // run before the first navigation so the very first /config.js is patched.
    await setShowBuildGitInfoFlag(context, "true");
    await page.goto("/");

    await page.locator('[data-testid="about-footer-link"]').click();

    const modal = page.locator('[data-testid="about-modal"]');
    await expect(modal).toBeVisible({ timeout: 5_000 });

    // Heading
    await expect(modal.getByRole("heading", { name: "About", level: 3 })).toBeVisible();

    // Client section — version row carries the crate version.
    await expect(modal).toContainText(`v${CLIENT_VERSION}`);
    await expect(modal).toContainText("videocall-ui");

    // Issue #1480 — Client section: with showBuildGitInfo truthy the Commit AND
    // Branch rows are present (each an `.about-modal-row` whose `.about-modal-label`
    // is the row name), and the always-shown Built row is present. These labels
    // pin the gated rows by their stable label text, independent of the (volatile)
    // SHA / branch / timestamp VALUES the build compiles in.
    const clientLabels = modal.locator(".about-modal-row .about-modal-label");
    await expect(clientLabels.filter({ hasText: "Commit" })).toHaveCount(1);
    await expect(clientLabels.filter({ hasText: "Branch" })).toHaveCount(1);
    await expect(clientLabels.filter({ hasText: "Built" })).toHaveCount(1);

    // Server section rows from the mocked response.
    await expect(modal).toContainText("meeting-api");
    await expect(modal).toContainText("1.2.3");
    // Short SHA (first 7 chars) of "deadbeefcafef00d" — present ONLY because the
    // server table's gated Commit column is shown (showBuildGitInfo truthy).
    await expect(modal).toContainText("deadbee");
    await expect(modal).toContainText("websocket");

    // Issue #1480 — Server table: the gated `Commit` header span is present and
    // the rows use the base 4-col `.about-modal-row` (NOT the 3-col
    // `--server-nogit` modifier). The `Built` header is always present.
    const serverHeader = modal.locator(".about-modal-row--header");
    await expect(serverHeader).toContainText("Commit");
    await expect(serverHeader).toContainText("Built");
    await expect(modal.locator(".about-modal-row--server-nogit")).toHaveCount(0);
  });

  test("About modal HIDES commit/branch when showBuildGitInfo is falsey, but keeps version + built (issue #1480)", async ({
    page,
    context,
  }) => {
    // Issue #1480 HIDDEN path — the production-style default: github info
    // (commit + branch) FAILS CLOSED. We force showBuildGitInfo OFF for this
    // context only (the committed e2e config.js ships it "true"), exercising the
    // exact gating a production config.js that omits the key would produce. The
    // rows/columns are rsx-`if`-gated, so they are REMOVED from the DOM (not
    // CSS-hidden) — asserted via toHaveCount(0).
    await setShowBuildGitInfoFlag(context, "false");
    await page.goto("/");

    await page.locator('[data-testid="about-footer-link"]').click();

    const modal = page.locator('[data-testid="about-modal"]');
    await expect(modal).toBeVisible({ timeout: 5_000 });

    // Version + Built (the ALWAYS-shown rows) remain.
    await expect(modal).toContainText(`v${CLIENT_VERSION}`);
    await expect(modal).toContainText("videocall-ui");
    const clientLabels = modal.locator(".about-modal-row .about-modal-label");
    await expect(clientLabels.filter({ hasText: "Built" })).toHaveCount(1);

    // Client section: Commit + Branch rows are GONE from the DOM.
    await expect(clientLabels.filter({ hasText: "Commit" })).toHaveCount(0);
    await expect(clientLabels.filter({ hasText: "Branch" })).toHaveCount(0);

    // Server table still renders (the mocked response is reused), but with the
    // 3-col `--server-nogit` modifier and NO Commit column. Wait for the server
    // section to finish loading by anchoring on a known mocked service row, then
    // assert the gated Commit header span is absent while Built is still present.
    await expect(modal).toContainText("meeting-api");
    const serverHeader = modal.locator(".about-modal-row--header");
    await expect(serverHeader).toContainText("Built");
    await expect(serverHeader).not.toContainText("Commit");
    // The short SHA of the mocked git_sha is NOT rendered (no server Commit col).
    await expect(modal).not.toContainText("deadbee");
    // Header + every data row carry the 3-col nogit modifier (header + 2 rows).
    await expect(modal.locator(".about-modal-row--server-nogit")).toHaveCount(3);
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
