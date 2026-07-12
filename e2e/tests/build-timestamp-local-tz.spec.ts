import { test, expect } from "@playwright/test";
import { injectSessionCookie } from "../helpers/auth";
import { waitForServices } from "../helpers/wait-for-services";
import { setShowBuildGitInfoFlag } from "../helpers/show-build-git-info-config";

/**
 * E2E tests for issue #1789 — build timestamps rendered in the VIEWER'S LOCAL
 * TIMEZONE.
 *
 * Before #1789 the "Built" values on the About modal + diagnostics build-info
 * surfaces were the raw UTC RFC3339 timestamp with the `'T'` swapped for a space
 * (`build_datetime` in `dioxus-ui/src/constants.rs`) — e.g. `2026-05-25 12:00:00Z`.
 * #1789 routes those values through `build_datetime_local` ->
 * `format_datetime_zoned_seconds` (js_sys::Date + Intl.DateTimeFormat), which
 * converts the UTC instant into the reader's local zone and appends a short zone
 * label — e.g. (viewer in America/Los_Angeles) `May 25, 2026, 5:00:00 AM PDT`.
 * The home-page footer's "· built <date>" suffix likewise switched from
 * `build_date` (raw UTC calendar date) to `build_date_local` ->
 * `format_local_date_iso` (viewer-local `YYYY-MM-DD`).
 *
 * ## Why this is deterministic in CI despite "local timezone"
 *
 * Playwright emulates a fixed timezone + locale per context (`test.use` below),
 * so `Intl.DateTimeFormat(undefined, …)` inside the wasm resolves to that zone
 * regardless of the runner's own tz. Pinning `America/Los_Angeles` + `en-US`
 * makes the converted strings exact:
 *   - `2026-05-25T12:00:00Z` (12:00 UTC) -> `5:00:00 AM PDT` (UTC-7, DST active in May)
 *   - `2026-05-24T10:30:00Z` (10:30 UTC) -> `3:30:00 AM PDT`
 * The SERVER build rows are mocked with these known instants (see MOCK body),
 * so their "Built" cells are a fully deterministic discriminator: the assertions
 * below hold ONLY on the converted output and FAIL on the pre-#1789 raw-UTC form.
 *
 * ## What each assertion pins to the un-fixed code
 *
 * Reverting the site back to `build_datetime` / `build_date` renders the server
 * cell as `2026-05-25 12:00:00Z` (numeric date, `12:00:00`, trailing `Z`, no zone
 * label). So `PDT`, `May 25, 2026`, and `5:00:00` are all absent while `Z` /
 * `12:00:00` are present — every assertion flips. These are true regression
 * guards, not tautologies.
 *
 * NOTE: the assertions match the local FORMAT (converted time + zone label, no
 * trailing `Z`), NOT the exact rendered string. The space before the AM/PM
 * marker is a U+202F narrow no-break space in modern ICU (Chromium 145), so
 * substrings are chosen to stay on the regular-space side of it
 * (`May 25, 2026,`, `5:00:00`, `PDT`).
 */

// Mocked server versions with KNOWN UTC instants. In America/Los_Angeles these
// convert to PDT (DST active in May) at a DIFFERENT clock time than UTC, which is
// what makes them a discriminator for the tz conversion (not just a reformat).
const MOCK_VERSIONS_BODY = {
  components: [
    {
      service: "meeting-api",
      version: "1.2.3",
      git_sha: "deadbeefcafef00d",
      git_branch: "main",
      build_timestamp: "2026-05-25T12:00:00Z", // -> May 25, 2026, 5:00:00 AM PDT
    },
    {
      service: "websocket",
      version: "0.9.1",
      git_sha: "abc123456789",
      git_branch: "main",
      build_timestamp: "2026-05-24T10:30:00Z", // -> May 24, 2026, 3:30:00 AM PDT
    },
  ],
};

// Pin BOTH the timezone (so the UTC->local shift is a fixed, known value) AND the
// locale (so the short month name is "May" and the zone label / AM-PM are the
// en-US forms). Without both, "local timezone" would be non-deterministic in CI.
test.use({ timezoneId: "America/Los_Angeles", locale: "en-US" });

test.describe("Build timestamps render in the viewer's local timezone (issue #1789)", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test.beforeEach(async ({ context, baseURL, page }) => {
    await injectSessionCookie(context, { baseURL });
    await page.route("**/api/v1/versions", async (route) => {
      await route.fulfill({
        status: 200,
        contentType: "application/json",
        body: JSON.stringify(MOCK_VERSIONS_BODY),
      });
    });
  });

  test("About modal server 'Built' cells show the mocked UTC instants converted to local zone", async ({
    page,
    context,
  }) => {
    // Pin showBuildGitInfo ON so the server table layout is the deterministic
    // 4-col `--git` form (Service / Version / Commit / Built). The Built value is
    // identical regardless of this flag; pinning only fixes the column layout so
    // the "last mono cell == Built" locator below is unambiguous. Must run before
    // the first navigation so the first /config.local.js is patched.
    await setShowBuildGitInfoFlag(context, "true");
    await page.goto("/");

    await page.locator('[data-testid="about-footer-link"]').click();

    const modal = page.locator('[data-testid="about-modal"]');
    await expect(modal).toBeVisible({ timeout: 5_000 });

    // Anchor on the Server section and wait for the mocked rows to render before
    // measuring the Built cell (presence-before-measurement).
    const serverSection = modal.locator('section[aria-labelledby="about-server-heading"]');
    await expect(serverSection).toContainText("meeting-api");

    // The "Built" cell is the LAST `.about-modal-value--mono` span in the data
    // row (Version, Commit, Built are the mono cells; the Service cell is
    // `--strong`, not `--mono`). Robust whether or not the Commit column is shown.
    const meetingApiBuilt = serverSection
      .locator(".about-modal-row")
      .filter({ hasText: "meeting-api" })
      .locator(".about-modal-value--mono")
      .last();

    // Positive discriminators — present ONLY in the converted local rendering.
    await expect(meetingApiBuilt).toContainText("PDT"); // short local zone label
    await expect(meetingApiBuilt).toContainText("May 25, 2026"); // en-US MMM D, YYYY
    await expect(meetingApiBuilt).toContainText("5:00:00"); // 12:00Z shifted to 05:00 PDT
    // Negative discriminators — present ONLY in the pre-#1789 raw-UTC form.
    await expect(meetingApiBuilt).not.toContainText("12:00:00"); // the raw UTC hour is gone
    await expect(meetingApiBuilt).not.toContainText("Z"); // no trailing UTC 'Z'

    // A SECOND instant, so the conversion isn't a single-value coincidence:
    // 10:30Z -> 3:30 PDT for the websocket row.
    const websocketBuilt = serverSection
      .locator(".about-modal-row")
      .filter({ hasText: "websocket" })
      .locator(".about-modal-value--mono")
      .last();
    await expect(websocketBuilt).toContainText("PDT");
    await expect(websocketBuilt).toContainText("3:30:00"); // 10:30Z shifted to 03:30 PDT
    await expect(websocketBuilt).not.toContainText("10:30:00");
    await expect(websocketBuilt).not.toContainText("Z");
  });

  test("About modal client 'Built' row is converted to local-zone format (not raw UTC)", async ({
    page,
    context,
  }) => {
    // The client "Built" value is `build_datetime_local(env!("BUILD_TIMESTAMP"))`
    // — BUILD_TIMESTAMP is baked at COMPILE time, so its exact value is unknown at
    // spec-write time. We therefore assert the local FORMAT, not a specific date:
    // a converted rendering carries a month name + a Pacific zone label and NO
    // trailing `Z`; the pre-#1789 raw form (`YYYY-MM-DD HH:MM:SSZ`) has none of
    // those. (This relies on the e2e image baking a REAL timestamp, not the
    // build.rs `"unknown"` sentinel — the same dependency the existing #1480
    // footer test already leans on and which passes in CI.)
    await setShowBuildGitInfoFlag(context, "true");
    await page.goto("/");

    await page.locator('[data-testid="about-footer-link"]').click();

    const modal = page.locator('[data-testid="about-modal"]');
    await expect(modal).toBeVisible({ timeout: 5_000 });

    const clientSection = modal.locator('section[aria-labelledby="about-client-heading"]');
    // The Built row is the client `.about-modal-row` whose `.about-modal-label`
    // is "Built"; its value is the row's `.about-modal-value--mono` span.
    const clientBuilt = clientSection
      .locator(".about-modal-row")
      .filter({ hasText: "Built" })
      .locator(".about-modal-value--mono");
    await expect(clientBuilt).toBeVisible();

    // Local zone label (PDT in summer, PST in winter — the build date's season is
    // unknown, so accept either). Absent from the raw-UTC form.
    await expect(clientBuilt).toContainText(/P[DS]T/);
    // en-US Intl "MMM D, YYYY" prefix (short month name + day + 4-digit year).
    // The raw form starts with a numeric `YYYY-MM-DD`, so this never matches it.
    await expect(clientBuilt).toContainText(/[A-Z][a-z]{2} \d{1,2}, \d{4}/);
    // No trailing UTC 'Z' — the raw form always carries one.
    await expect(clientBuilt).not.toContainText("Z");
  });

  test("home footer '· built <date>' stays a bare local YYYY-MM-DD with no zone hint", async ({
    page,
  }) => {
    // #1789 switched the footer suffix from `build_date` (raw UTC date) to
    // `build_date_local` (viewer-local date). Both render `YYYY-MM-DD`, so the
    // SHAPE is unchanged — this test guards that the switch did not regress the
    // footer format or accidentally append a zone label (the footer is date-only
    // by design; the zone label lives only on the About/diagnostics time surfaces).
    //
    // HONEST LIMITATION: we CANNOT assert the local-vs-UTC calendar-day flip here.
    // That flip only occurs for a near-midnight-UTC build instant, and
    // BUILD_TIMESTAMP's time-of-day is unknown at spec-write time — so a
    // deterministic day-flip assertion is impossible on this surface. The
    // local-vs-UTC correctness of the date is host-tested at the
    // `build_date_local_with` seam in constants.rs
    // (`build_date_local_uses_local_calendar_date_not_utc`); here we only pin the
    // rendered shape.
    await page.goto("/");

    const aboutLink = page.locator('[data-testid="about-footer-link"]');
    await expect(aboutLink).toBeVisible({ timeout: 10_000 });
    // U+00B7 MIDDLE DOT + literal "built" + a YYYY-MM-DD date that ENDS the text
    // (the `$` anchor asserts nothing — e.g. a zone token — trails the date).
    await expect(aboutLink).toHaveText(/· built \d{4}-\d{2}-\d{2}$/);
  });
});
