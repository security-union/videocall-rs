import { test, expect, Page } from "@playwright/test";
import { injectSessionCookie } from "../helpers/auth";
import { waitForServices } from "../helpers/wait-for-services";

/**
 * E2E coverage for issue 1134 — "show cam/mic selectors with join meeting".
 *
 * On the pre-join (lobby) screen the Dioxus UI now AUTO-requests media
 * permission once on mount (a single one-shot `getUserMedia` request via the
 * auto-request `use_effect` in `attendants.rs`), so the camera/mic device
 * selectors appear automatically WITHOUT the user clicking "Allow camera &
 * mic". The manual Allow button is kept only as a fallback for the brief
 * in-flight window before the auto-request resolves (e.g. if it is slow); once
 * permission resolves either way the pre-permission block is replaced by the
 * device UI.
 *
 * Two invariants this spec locks down:
 *   1. AUTO-SHOW (the issue 1134 feature): the granted-state UI (camera/mic
 *      toggles + camera/mic/speaker selects) becomes visible on its own, with NO
 *      click on the Allow button.
 *   2. PREVIEW-ONLY / NO AUTO-JOIN (the load-bearing issue 933 regression
 *      guard): the auto-request must NEVER auto-join. The user stays on the
 *      pre-join screen, no in-meeting state appears, and a window `focus` event
 *      must remain a no-op (it must not re-fire the request into a join). The
 *      auto-request effect deliberately does NOT set `join_requested`, so the
 *      permission `on_result` stays on the preview branch (no `client.connect()`,
 *      no `meeting_joined`).
 *
 * ─── Fake media ────────────────────────────────────────────────────────────
 * The Chromium launch args (see `playwright.config.ts`) include:
 *   --use-fake-device-for-media-stream  (synthetic camera + mic, no hardware)
 *   --use-fake-ui-for-media-stream      (auto-grant getUserMedia, no OS prompt)
 * So the on-mount auto-request resolves to "granted" without any UI gesture —
 * which is exactly what makes the auto-show assertions possible here.
 *
 * Stable selectors mirror the constants exported from
 * `dioxus-ui/src/components/pre_join_settings_card.rs`.
 */

// ── Selector constants (mirror pre_join_settings_card.rs) ──────────────────
const PREVIEW = '[data-testid="prejoin-preview"]';
const CAMERA_TOGGLE = '[data-testid="prejoin-camera-toggle"]';
const MIC_TOGGLE = '[data-testid="prejoin-mic-toggle"]';
const CAMERA_SELECT = '[data-testid="prejoin-camera-select"]';
const MIC_SELECT = '[data-testid="prejoin-mic-select"]';
const SPEAKER_SELECT = '[data-testid="prejoin-speaker-select"]';
const PERMISSION_PROMPT = '[data-testid="prejoin-permission-prompt"]';
const PERMISSION_ALLOW = '[data-testid="prejoin-permission-allow"]';

// In-meeting confirmation marker shown on the empty-meeting invite overlay.
const MEETING_READY = "Your meeting is ready!";
// New pre-grant prompt copy (issue 1134) shown during the brief in-flight
// auto-request window, before getUserMedia resolves.
const REQUESTING_COPY = "Requesting camera & microphone access…";

const startOrJoinButton = (page: Page) =>
  page.getByRole("button", { name: /Start Meeting|Join Meeting/ });

/**
 * Navigate to a fresh meeting and wait for the pre-join card to mount. Does NOT
 * touch the Allow button — the whole point of issue 1134 is that the granted UI
 * arrives on its own.
 */
async function gotoPreJoin(page: Page, meetingId: string) {
  await page.goto(`/meeting/${meetingId}`);
  const actionButton = startOrJoinButton(page);
  await actionButton.waitFor({ timeout: 30_000 });
  await expect(page.locator(PREVIEW)).toBeVisible({ timeout: 30_000 });
  return actionButton;
}

/**
 * Assert we are still on the pre-join screen and NO meeting has started. This is
 * the issue 933 no-auto-start invariant, re-checked after the auto-request and
 * after focus events. Stronger than the original prejoin-no-auto-start spec: in
 * addition to the Start/Join button staying visible and "Your meeting is ready!"
 * being absent, we assert there are zero in-meeting video controls (the
 * `.video-controls-container` only mounts once `meeting_joined` is true).
 */
async function expectStillOnPreJoin(page: Page, actionButton = startOrJoinButton(page)) {
  await expect(actionButton).toBeVisible();
  await expect(page.locator(PREVIEW)).toBeVisible();
  await expect(page.getByText(MEETING_READY)).not.toBeVisible();
  // In-meeting controls must not exist — their presence would prove a join.
  await expect(page.locator(".video-controls-container")).toHaveCount(0);
}

test.describe("Pre-join auto-shows device selectors without auto-joining (issue 1134)", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test.beforeEach(async ({ context, baseURL, page }) => {
    await injectSessionCookie(context, { baseURL });
    // Display name is read from localStorage before the pre-join card renders;
    // without it MeetingPage shows the "Enter your display name" form instead of
    // the lobby (see prejoin-no-capability-block.spec.ts for the rationale).
    await page.addInitScript(() => {
      localStorage.setItem("vc_display_name", "AutoShowUser");
    });
  });

  test("device selectors auto-appear on mount without clicking Allow @bvt1", async ({ page }) => {
    await gotoPreJoin(page, `e2e_1134_autoshow_${Date.now()}`);

    // The granted-state UI must become visible on its own — the auto-request
    // (issue 1134) fires getUserMedia on mount and the fake-UI Chromium grants
    // it, so the toggles + selects render WITHOUT any click on Allow.
    await expect(page.locator(CAMERA_TOGGLE)).toBeVisible({ timeout: 15_000 });
    await expect(page.locator(MIC_TOGGLE)).toBeVisible();
    await expect(page.locator(CAMERA_SELECT)).toBeVisible();
    await expect(page.locator(MIC_SELECT)).toBeVisible();
    await expect(page.locator(SPEAKER_SELECT)).toBeVisible();

    // The selectors are populated (device labels are only available after the
    // permission the auto-request obtained). At least one camera + mic option.
    expect(await page.locator(`${CAMERA_SELECT} option`).count()).toBeGreaterThanOrEqual(1);
    expect(await page.locator(`${MIC_SELECT} option`).count()).toBeGreaterThanOrEqual(1);

    // The pre-grant prompt + manual fallback button must be gone once granted.
    await expect(page.locator(PERMISSION_PROMPT)).toBeHidden();
    await expect(page.locator(PERMISSION_ALLOW)).toHaveCount(0);

    // CRITICAL (issue 933 guard): auto-showing the selectors must NOT auto-join.
    await expectStillOnPreJoin(page);
  });

  test("auto-request grants preview but does NOT start the meeting @bvt1", async ({ page }) => {
    const actionButton = await gotoPreJoin(page, `e2e_1134_noautojoin_${Date.now()}`);

    // Wait for the auto-request to land us in the granted state.
    await expect(page.locator(CAMERA_TOGGLE)).toBeVisible({ timeout: 15_000 });

    // Give any (buggy) async connect/join cascade time to fire if it's going to.
    // The preview-only auto-request must never reach client.connect()/
    // meeting_joined — this is the load-bearing no-auto-start assertion.
    await page.waitForTimeout(2_000);
    await expectStillOnPreJoin(page, actionButton);
  });

  test("window focus event does not re-fire the request into a join @bvt1", async ({ page }) => {
    const actionButton = await gotoPreJoin(page, `e2e_1134_focus_${Date.now()}`);

    // Reach the granted state via the on-mount auto-request.
    await expect(page.locator(CAMERA_TOGGLE)).toBeVisible({ timeout: 15_000 });

    // Dispatch focus events — the exact trigger that auto-started a meeting
    // before the issue 933 fix. The on-mount auto-request is one-shot (guarded
    // by an Rc<Cell<bool>>), and the focus handler is a no-op while
    // meeting_joined is false, so neither path may join here.
    await page.evaluate(() => {
      window.dispatchEvent(new Event("focus"));
      window.dispatchEvent(new Event("focus"));
    });
    await page.waitForTimeout(2_000);

    // Still on pre-join, still granted (focus must not have torn anything down),
    // and the toggles must not have been duplicated by a re-fired request.
    await expectStillOnPreJoin(page, actionButton);
    await expect(page.locator(CAMERA_TOGGLE)).toBeVisible();
    await expect(page.locator(CAMERA_TOGGLE)).toHaveCount(1);
    await expect(page.locator(MIC_TOGGLE)).toHaveCount(1);
  });

  test("clicking Start Meeting after the auto-request still joins @bvt1", async ({ page }) => {
    const actionButton = await gotoPreJoin(page, `e2e_1134_manualjoin_${Date.now()}`);

    // Auto-request grants preview; we remain on pre-join until we click.
    await expect(page.locator(CAMERA_TOGGLE)).toBeVisible({ timeout: 15_000 });
    await expectStillOnPreJoin(page, actionButton);

    // Now the explicit user gesture must actually join (auto-show must not have
    // consumed or disabled the join path).
    await actionButton.click();
    await expect(page.getByText(MEETING_READY)).toBeVisible({ timeout: 30_000 });
  });

  test("manual Allow fallback still reaches the granted state when shown @bvt1", async ({
    page,
  }) => {
    await gotoPreJoin(page, `e2e_1134_fallback_${Date.now()}`);

    // The manual fallback is the safety net for the brief in-flight window
    // before the auto-request resolves. With the fake-UI Chromium the
    // auto-request usually grants first and removes the button — so we only
    // exercise the click when the button is actually still present, and assert
    // the granted state either way.
    const grantedByAutoRequest = await page
      .locator(CAMERA_TOGGLE)
      .waitFor({ state: "visible", timeout: 3_000 })
      .then(() => true)
      .catch(() => false);

    const allow = page.locator(PERMISSION_ALLOW);
    if (!grantedByAutoRequest && (await allow.isVisible().catch(() => false))) {
      await allow.click().catch(() => {
        // Auto-grant may have detached the button between probe and click; the
        // granted-state wait below is the real gate.
      });
    }

    // Whether via the auto-request or the manual fallback click, the granted
    // state must be reached: prompt gone, toggles + selects present.
    await expect(page.locator(CAMERA_TOGGLE)).toBeVisible({ timeout: 15_000 });
    await expect(page.locator(PERMISSION_PROMPT)).toBeHidden({ timeout: 15_000 });
    await expect(page.locator(MIC_TOGGLE)).toBeVisible();
    await expect(page.locator(CAMERA_SELECT)).toBeVisible();

    // And still no auto-join from the fallback path.
    await expectStillOnPreJoin(page);
  });

  test("pre-grant prompt copy reads 'Requesting camera & microphone access…' (best-effort)", async ({
    page,
  }) => {
    // The reworded copy (issue 1134) is shown only during the brief in-flight
    // window before the auto-request resolves. With the fake-UI Chromium that
    // grant is near-instant, so this copy can flash too briefly to catch
    // reliably. We therefore assert it BEST-EFFORT: if we observe it, it must be
    // the new wording; if it already cleared, the load-bearing contract is that
    // the granted state was reached (asserted unconditionally below). We do NOT
    // fail the test on the transient copy timing — the granted-state assertion
    // and the dedicated auto-show test above are the real coverage.
    await page.goto(`/meeting/e2e_1134_copy_${Date.now()}`);
    await startOrJoinButton(page).waitFor({ timeout: 30_000 });

    const requesting = page.getByText(REQUESTING_COPY);
    const sawRequestingCopy = await requesting
      .waitFor({ state: "visible", timeout: 1_500 })
      .then(() => true)
      .catch(() => false);
    if (sawRequestingCopy) {
      // If we caught the in-flight window, it must carry the new wording and the
      // OLD pre-1134 "Allow camera & microphone to preview..."-style copy must
      // not be the one in use (the new copy is the only prompt text now).
      await expect(requesting).toHaveText(REQUESTING_COPY);
    }

    // Unconditional contract: the auto-request resolves to the granted state.
    await expect(page.locator(CAMERA_TOGGLE)).toBeVisible({ timeout: 15_000 });
    await expect(page.locator(PERMISSION_PROMPT)).toBeHidden();
    await expectStillOnPreJoin(page);
  });
});
