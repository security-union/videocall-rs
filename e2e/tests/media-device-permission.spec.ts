import { test, expect, Page } from "@playwright/test";
import { injectSessionCookie } from "../helpers/auth";
import { waitForServices } from "../helpers/wait-for-services";
import { installGetUserMediaMock, setGumFail, getGumCalls } from "../helpers/media-mock";

/**
 * E2E coverage for the camera/mic device-permission fix.
 *
 * Three related bugs were fixed and are regression-guarded here:
 *
 *  1. NO REASON SHOWN for a blocked device, especially `NotReadableError`
 *     (device held by another app, e.g. still open in Google Meet), and
 *     ESPECIALLY for the IN-MEETING failure path — which previously rendered
 *     NOTHING. The encoder's `getUserMedia` rejection now emits a CLASSIFIED
 *     permission-error callback, so the in-meeting "Device access problem"
 *     modal shows the specific DeviceInUse copy.
 *
 *  2. THE CONTROL BUTTON DEADLOCKED: it became HTML-`disabled` after any error,
 *     so the only way to retry was to leave and rejoin. The `disabled: !available`
 *     was REMOVED from both MicButton and CameraButton — the button now stays
 *     clickable (the `!available` state is shown via a `.device-warning` badge)
 *     and clicking it re-attempts acquisition.
 *
 *  3. NO AUTO-RECOVERY: if the other app released the device, this app never
 *     noticed. A background retry loop now re-probes ANY `DeviceInUse` device
 *     (unconditionally — no "user wants it on" gate, since a background probe
 *     can only ever CLEAR the error, never auto-start capture) and, on success,
 *     clears the error and auto-closes the modal — all with NO user action, and
 *     without starting capture (that still requires an explicit click).
 *
 * ─── How the failure is injected ───────────────────────────────────────────
 * The Chromium fake-device flags only ever RESOLVE getUserMedia, so a rejection
 * is injected via `installGetUserMediaMock` (helpers/media-mock.ts): an init
 * script that wraps `navigator.mediaDevices.getUserMedia` and, once a test opts
 * a side into failure with `setGumFail`, rejects with a chosen `DOMException`
 * name (`NotReadableError` → DeviceInUse, `NotAllowedError` → PermissionDenied).
 * By default the mock passes through to the real fake device, so the pre-join
 * preview and the join permission probe both succeed and the test can enter the
 * meeting BEFORE flipping a device into the blocked state.
 *
 * Stable selectors: `camera-toggle-button` and `mic-toggle-button` on the
 * in-meeting controls, and `device-warning-modal` on the modal overlay.
 */

// ── Selector constants ─────────────────────────────────────────────────────
// The pre-join card root — always mounted regardless of grant state — is the
// reliable "card is ready" signal (mirrors gotoPreJoin in
// prejoin-device-preview.spec.ts). The camera TOGGLE, by contrast, only renders
// in the granted branch of pre_join_settings_card.rs and is not reliably visible
// in the owner "Start Meeting" layout, so it is NOT used as the readiness gate.
const PREJOIN_PREVIEW = '[data-testid="prejoin-preview"]';
const CAMERA_BTN = '[data-testid="camera-toggle-button"]';
const MIC_BTN = '[data-testid="mic-toggle-button"]';
const MODAL = '[data-testid="device-warning-modal"]';
// The inner dialog surface. `.modal-overlay.in-meeting .modal-window` in
// global.css must give this a SOLID, theme-aware background so its actionable
// error text is readable over the translucent in-meeting backdrop + live video.
const MODAL_WINDOW = `${MODAL} .modal-window`;

const MEETING_READY = "Your meeting is ready!";

/**
 * Alpha channel of a CSS computed color. `getComputedStyle().backgroundColor`
 * renders a fully-opaque fill as `rgb(...)` (implicit alpha 1) and any
 * translucency as `rgba(..., a)` with an explicit sub-1 alpha; `transparent`
 * renders as `rgba(0, 0, 0, 0)`. Returns 1 for the opaque `rgb(...)` form.
 */
function cssAlpha(color: string): number {
  const m = color.match(/^rgba?\(([^)]+)\)$/);
  if (!m) return 1;
  const parts = m[1].split(",").map((s) => s.trim());
  return parts.length === 4 ? parseFloat(parts[3]) : 1;
}
// Exact in-meeting copy for the DeviceInUse case (attendants.rs
// render_single_device_error). The leading device name is substituted per side.
const CAMERA_IN_USE_COPY =
  "Camera is being used by another application. Close whatever else is using it and it will reconnect automatically.";
const MIC_IN_USE_COPY =
  "Microphone is being used by another application. Close whatever else is using it and it will reconnect automatically.";
const CAMERA_BLOCKED_COPY = "Camera is blocked in your browser.";

/**
 * Navigate directly to a fresh meeting (owner flow → "Start Meeting"), let the
 * pre-join auto-grant settle (mock passes through so the fake device grants),
 * then start the meeting and wait until the in-meeting camera control renders.
 */
async function gotoAndJoin(page: Page, meetingId: string): Promise<void> {
  await page.goto(`/meeting/${meetingId}`);
  const action = page.getByRole("button", { name: /Start Meeting|Join Meeting/ });
  await action.waitFor({ timeout: 30_000 });

  // The pre-join screen auto-requests getUserMedia on mount (#1134); with the
  // pass-through mock the fake-UI Chromium auto-grants, so the granted device UI
  // appears on its own — no click required. We gate ONLY on the always-mounted
  // pre-join card root being visible (mirrors gotoPreJoin in
  // prejoin-device-preview.spec.ts, which waits on the preview root, not the
  // camera toggle). We deliberately do NOT click the "Allow camera & mic"
  // fallback button: the #1134 auto-grant detaches it the instant it resolves,
  // so a click that races the grant hangs until the test timeout looking for the
  // now-detached element. The granted state is not needed to start the meeting —
  // clicking Start triggers acquisition itself.
  await expect(page.locator(PREJOIN_PREVIEW)).toBeVisible({ timeout: 30_000 });

  await action.click();

  // In-meeting: the action bar renders the camera toggle, and the empty-meeting
  // lobby shows the invite overlay.
  await expect(page.getByText(MEETING_READY)).toBeVisible({ timeout: 30_000 });
  await expect(page.locator(CAMERA_BTN)).toBeVisible({ timeout: 30_000 });
}

test.describe("In-meeting device-permission handling", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test.beforeEach(async ({ context, baseURL, page }) => {
    await injectSessionCookie(context, { baseURL });
    // The getUserMedia override must be in place before the app boots.
    await installGetUserMediaMock(page);
    // Display name is read from localStorage before the pre-join card renders.
    await page.addInitScript(() => {
      localStorage.setItem("vc_display_name", "DevicePermUser");
    });
  });

  test("in-meeting NotReadableError shows the specific 'in use by another application' reason", async ({
    page,
  }) => {
    await gotoAndJoin(page, `e2e_perm_inuse_${Date.now()}`);

    // The camera is now held by another application: every video getUserMedia
    // rejects with NotReadableError.
    await setGumFail(page, { errorName: "NotReadableError", video: -1 });

    const camera = page.locator(CAMERA_BTN);
    // Bug #2 precondition: the control is clickable (never HTML-disabled).
    await expect(camera).not.toBeDisabled();

    // Turn the camera ON → the encoder's getUserMedia rejects in-meeting.
    await camera.click();

    // Bug #1 regression: the in-meeting failure now renders the SPECIFIC
    // DeviceInUse reason (previously the in-meeting path rendered nothing).
    const modal = page.locator(MODAL);
    await expect(modal).toBeVisible({ timeout: 15_000 });
    await expect(modal).toContainText(CAMERA_IN_USE_COPY);

    // Bug #2 regression: after the failure the button is STILL not disabled;
    // the unavailable state is conveyed via the error class / warning badge.
    await expect(camera).not.toBeDisabled();
    await expect(camera).toHaveClass(/\berror\b/);
    await expect(camera.locator(".device-warning")).toBeVisible();
  });

  test("blocked camera button stays clickable and re-attempts acquisition on click", async ({
    page,
  }) => {
    await gotoAndJoin(page, `e2e_perm_retry_${Date.now()}`);

    // Use NotAllowedError (→ PermissionDenied): a permanently blocked case that
    // is NOT auto-retried, so the ONLY new getUserMedia calls are the ones our
    // clicks trigger — clean attribution for "clicking re-attempts".
    await setGumFail(page, { errorName: "NotAllowedError", video: -1 });

    const camera = page.locator(CAMERA_BTN);
    const modal = page.locator(MODAL);

    // First attempt fails and raises the modal with the blocked-in-browser copy.
    await camera.click();
    await expect(modal).toBeVisible({ timeout: 15_000 });
    await expect(modal).toContainText(CAMERA_BLOCKED_COPY);

    // Bug #2 regression: the button did NOT become disabled after the failure.
    await expect(camera).not.toBeDisabled();

    // Dismiss the modal so the control is clickable again, then prove a SECOND
    // click issues a fresh getUserMedia (the deadlock fix: previously the button
    // was disabled and clicking did nothing — leave-and-rejoin was the only way).
    await page.locator(MODAL).getByRole("button", { name: "Ok" }).click();
    await expect(modal).toBeHidden();
    await expect(camera).not.toBeDisabled();

    // PermissionDenied is NOT auto-retried and the encoder is torn down once the
    // failure sets video_enabled=false, so the video call count is stable here —
    // any further increase is attributable to the click below. Settle briefly to
    // let the first attempt's encoder-restart burst finish before snapshotting.
    await page.waitForTimeout(1_500);
    const before = (await getGumCalls(page)).video;
    await camera.click();
    // A new video getUserMedia call fired because of the click.
    await expect
      .poll(async () => (await getGumCalls(page)).video, { timeout: 5_000 })
      .toBeGreaterThan(before);
    // The user-initiated retry re-surfaces the modal (it failed again).
    await expect(modal).toBeVisible({ timeout: 15_000 });
  });

  test("auto-recovery: a released device clears the error and becomes available again, but capture does NOT auto-start", async ({
    page,
  }) => {
    await gotoAndJoin(page, `e2e_perm_recover_${Date.now()}`);

    // Camera held by another app: always fail for now.
    await setGumFail(page, { errorName: "NotReadableError", video: -1 });

    const camera = page.locator(CAMERA_BTN);
    const modal = page.locator(MODAL);

    // Turn the camera ON → fails → DeviceInUse modal, and (crucially) the user
    // still WANTS the camera on, which arms the background auto-retry loop.
    await camera.click();
    await expect(modal).toBeVisible({ timeout: 15_000 });
    await expect(modal).toContainText(CAMERA_IN_USE_COPY);
    await expect(camera).toHaveClass(/\berror\b/);

    // The other application RELEASES the device: getUserMedia now succeeds.
    // No test click follows — the background recovery probe must be fully
    // hands-off, and (per the fix) must NOT start capture on its own.
    await setGumFail(page, { video: 0 });

    // The background retry (~4s base, with backoff) re-probes request_video_only
    // and succeeds. Per the fix, a background recovery probe ONLY clears the
    // blocked-state error — it does NOT set a pending-enable, so capture stays
    // OFF. The button therefore returns to the plain "off" state: error cleared
    // (device available again) but not "active" (no silent auto-start). Starting
    // capture is a fresh, explicit user action, not a surprise side effect of the
    // other app releasing the device (a privacy concern).
    await expect(camera).toHaveClass(/\boff\b/, { timeout: 25_000 });
    await expect(camera).not.toHaveClass(/\berror\b/);
    await expect(camera).not.toHaveClass(/\bactive\b/);
    // should_auto_close_device_warning: the modal the user left open closes
    // itself once BOTH sides are error-free again — independent of pending-enable.
    await expect(modal).toBeHidden({ timeout: 25_000 });

    // The button is genuinely usable again (not merely cosmetically cleared):
    // an explicit user click now DOES start capture, because getUserMedia
    // succeeds and the manual-click path sets the pending-enable.
    await camera.click();
    await expect(camera).toHaveClass(/\bactive\b/, { timeout: 15_000 });
    await expect(camera).not.toHaveClass(/\berror\b/);
  });

  test("camera blocked at initial join still recovers in-meeting once released", async ({
    page,
  }) => {
    // Regression guard for the initial-JOIN-time blocked-device path. Distinct
    // from the recovery test above, which joins CLEANLY (gotoAndJoin) and only
    // then blocks the camera via an in-meeting click. Here the camera is blocked
    // at the VERY FIRST permission probe — the moment the user clicks Join. The
    // background auto-retry loop arms itself off the `video_error` signal alone
    // (unconditional `should_auto_retry`), so a device blocked at join time
    // recovers hands-off. This variant has the pre-join camera toggle ON; the
    // companion test below covers the toggle-OFF variant that the original
    // intent-gated fix left stuck "blocked forever".
    //
    // The saved pre-join preference has the camera ON, so the join probe
    // REQUESTS video. Set before navigation so `load_preferred_camera_on()`
    // reads it as the pre-join card mounts.
    await page.addInitScript(() => {
      localStorage.setItem("vc_prejoin_camera_on", "true");
    });

    await page.goto(`/meeting/e2e_perm_join_block_${Date.now()}`);
    const action = page.getByRole("button", { name: /Start Meeting|Join Meeting/ });
    await action.waitFor({ timeout: 30_000 });
    await expect(page.locator(PREJOIN_PREVIEW)).toBeVisible({ timeout: 30_000 });

    // Block the camera BEFORE the join click: the first join-time getUserMedia
    // probe rejects with NotReadableError → DeviceInUse. This is the exact
    // regression path (blocked at JOIN, not via a later in-meeting click).
    await setGumFail(page, { errorName: "NotReadableError", video: -1 });

    await action.click();

    // The pre-join failure modal surfaces the specific DeviceInUse copy.
    const modal = page.locator(MODAL);
    await expect(modal).toBeVisible({ timeout: 15_000 });
    await expect(modal).toContainText(CAMERA_IN_USE_COPY);

    // Dismiss with "Ok" → join the meeting anyway with the camera left off
    // (issue #959 intended behavior — the device stays off, the user is in).
    await modal.getByRole("button", { name: "Ok" }).click();

    const camera = page.locator(CAMERA_BTN);
    await expect(page.getByText(MEETING_READY)).toBeVisible({ timeout: 30_000 });
    await expect(camera).toBeVisible({ timeout: 30_000 });
    // Joined with the camera failed at join → the in-meeting button shows the
    // error badge (class "off error").
    await expect(camera).toHaveClass(/\berror\b/);

    // The blocking app RELEASES the device: getUserMedia now succeeds. NO click
    // follows — the background auto-retry loop, armed off the `video_error`
    // signal alone, must recover hands-off. Per the no-auto-enable fix, recovery
    // ONLY clears the error; it must NOT auto-start capture, so the button lands
    // on plain "off".
    await setGumFail(page, { video: 0 });

    await expect(camera).toHaveClass(/\boff\b/, { timeout: 25_000 });
    await expect(camera).not.toHaveClass(/\berror\b/);
    await expect(camera).not.toHaveClass(/\bactive\b/);
    // Any modal left open auto-closes once both sides are error-free again.
    await expect(modal).toBeHidden({ timeout: 25_000 });
  });

  test("camera blocked at initial join with the pre-join toggle OFF still recovers hands-off, with NO camera-button click", async ({
    page,
  }) => {
    // THE EXACT RESIDUAL BUG this fix closes. The user does NOT intend to turn
    // the camera on: the pre-join camera toggle is OFF. But the join-time
    // permission probe ALWAYS probes video (a video-only getUserMedia, issued
    // regardless of the toggle — see MediaDeviceAccess::request), so a camera
    // held by another app is still detected and flagged DeviceInUse at join.
    //
    // The earlier, narrower fix seeded the retry loop's `video_want_on` intent
    // from the RAW pre-join toggle. With the toggle OFF that seed was `false`,
    // so `should_auto_retry(false, DeviceInUse)` stayed false, the loop never
    // armed, and the camera warning badge sat there FOREVER unless the user
    // manually clicked the camera button (which finally set the intent). That is
    // precisely the reported gap: "user just joins and camera button hasn't been
    // used yet → badge never clears on its own."
    //
    // The current fix drops the intent gate entirely: retry arms off the
    // `video_error` signal alone. This test sets the toggle OFF, blocks the
    // camera at join, joins anyway (camera stays off), and NEVER clicks the
    // camera button — asserting the badge clears purely from the background loop
    // once the block is released. On the intent-gated code this assertion times
    // out (the loop never arms), which is this test's mutation sensitivity.
    //
    // Toggle OFF is the default (load_preferred_camera_on → false), but set it
    // explicitly so the test does not silently depend on that default.
    await page.addInitScript(() => {
      localStorage.setItem("vc_prejoin_camera_on", "false");
    });

    await page.goto(`/meeting/e2e_perm_join_block_off_${Date.now()}`);
    const action = page.getByRole("button", { name: /Start Meeting|Join Meeting/ });
    await action.waitFor({ timeout: 30_000 });
    await expect(page.locator(PREJOIN_PREVIEW)).toBeVisible({ timeout: 30_000 });

    // Block the camera BEFORE the join click: the join-time video-only probe
    // rejects with NotReadableError → DeviceInUse, even though the toggle is OFF.
    await setGumFail(page, { errorName: "NotReadableError", video: -1 });

    await action.click();

    // The join-failure modal surfaces the specific DeviceInUse copy for the
    // camera (the probe flagged it regardless of the OFF toggle).
    const modal = page.locator(MODAL);
    await expect(modal).toBeVisible({ timeout: 15_000 });
    await expect(modal).toContainText(CAMERA_IN_USE_COPY);

    // Dismiss with "Ok" → join with the camera off (as always for a blocked
    // camera at join — unrelated to this fix).
    await modal.getByRole("button", { name: "Ok" }).click();

    const camera = page.locator(CAMERA_BTN);
    await expect(page.getByText(MEETING_READY)).toBeVisible({ timeout: 30_000 });
    await expect(camera).toBeVisible({ timeout: 30_000 });
    // In-meeting: camera off, but flagged with the error badge from the join
    // probe (class "off error").
    await expect(camera).toHaveClass(/\berror\b/);

    // The blocking app RELEASES the device: getUserMedia now succeeds. CRITICAL:
    // no camera-button click happens anywhere in this test. Recovery must come
    // PURELY from the background auto-retry loop, which — post-fix — arms off the
    // `video_error` signal with no intent gate.
    await setGumFail(page, { video: 0 });

    // The badge clears on its own once the loop's next probe succeeds. Recovery
    // ONLY clears the error (no auto-start), so the button lands on plain "off":
    // never "active" (no silent capture), never still "error".
    await expect(camera).toHaveClass(/\boff\b/, { timeout: 25_000 });
    await expect(camera).not.toHaveClass(/\berror\b/);
    await expect(camera).not.toHaveClass(/\bactive\b/);
    // The modal left open auto-closes once both sides are error-free again.
    await expect(modal).toBeHidden({ timeout: 25_000 });
  });

  test("in-meeting mic NotReadableError shows the specific reason and keeps the mic button clickable", async ({
    page,
  }) => {
    await gotoAndJoin(page, `e2e_perm_mic_${Date.now()}`);

    // The microphone is held by another application.
    await setGumFail(page, { errorName: "NotReadableError", audio: -1 });

    const mic = page.locator(MIC_BTN);
    await expect(mic).not.toBeDisabled();

    // Turn the mic ON → the microphone encoder's getUserMedia rejects.
    await mic.click();

    const modal = page.locator(MODAL);
    await expect(modal).toBeVisible({ timeout: 15_000 });
    await expect(modal).toContainText(MIC_IN_USE_COPY);

    // Bug #2 regression for the mic control: still not disabled after failure.
    await expect(mic).not.toBeDisabled();
    await expect(mic).toHaveClass(/\berror\b/);
    await expect(mic.locator(".device-warning")).toBeVisible();
  });

  // ─── Item 1 (a11y): Escape dismisses the device-warning modal ──────────────
  //
  // The dialog now carries role="dialog"/aria-modal + an Escape handler that
  // calls the SAME on_dismiss the "Ok" button uses. A permanently blocked
  // (NotAllowedError → PermissionDenied) camera raises the modal and is NOT
  // auto-retried, so it stays up until the user dismisses it — here via the
  // keyboard. Mutation sensitivity: remove the onkeydown/Escape handler in
  // render_device_warning_modal and this times out (the modal never hides).
  test("Escape dismisses the in-meeting device-warning modal", async ({ page }) => {
    await gotoAndJoin(page, `e2e_perm_escape_${Date.now()}`);

    await setGumFail(page, { errorName: "NotAllowedError", video: -1 });

    const camera = page.locator(CAMERA_BTN);
    const modal = page.locator(MODAL);

    await camera.click();
    await expect(modal).toBeVisible({ timeout: 15_000 });
    await expect(modal).toContainText(CAMERA_BLOCKED_COPY);

    // The dialog announces itself and is the focus target on open, so Escape is
    // delivered to its onkeydown handler without any prior click inside.
    const dialog = modal.getByRole("dialog");
    await expect(dialog).toHaveAttribute("aria-modal", "true");

    await page.keyboard.press("Escape");

    // Escape closes the modal exactly like clicking "Ok".
    await expect(modal).toBeHidden({ timeout: 5_000 });
    // The control remains clickable afterward (no deadlock).
    await expect(camera).not.toBeDisabled();
  });

  // ─── Item 2: the in-meeting warning does NOT fully black out the live call ──
  //
  // Pre-join uses the opaque `.modal-overlay`; the in-meeting call site adds the
  // `.in-meeting` modifier which swaps the backdrop to the semi-transparent
  // `--overlay-backdrop` token so remote peers / the still-working device stay
  // visible-but-dimmed. Mutation sensitivity: drop the `.in-meeting` class at the
  // in-meeting call site (or revert the CSS) and the computed backdrop reverts to
  // fully opaque `rgb(0, 0, 0)`, failing the alpha assertion below.
  test("in-meeting device-warning overlay is translucent, not fully opaque black", async ({
    page,
  }) => {
    await gotoAndJoin(page, `e2e_perm_overlay_${Date.now()}`);

    await setGumFail(page, { errorName: "NotAllowedError", video: -1 });

    const camera = page.locator(CAMERA_BTN);
    const modal = page.locator(MODAL);

    await camera.click();
    await expect(modal).toBeVisible({ timeout: 15_000 });

    // The in-meeting call site tags the overlay with `.in-meeting`.
    await expect(modal).toHaveClass(/\bin-meeting\b/);

    // The computed backdrop is the semi-transparent token (rgba with alpha < 1),
    // NOT the opaque `rgb(0, 0, 0)` the pre-join overlay uses. getComputedStyle
    // renders a sub-1 alpha as an `rgba(...)` string, so an opaque backdrop would
    // read as `rgb(0, 0, 0)` and fail both assertions.
    const bg = await modal.evaluate((el) => getComputedStyle(el).backgroundColor);
    expect(bg).not.toBe("rgb(0, 0, 0)");
    expect(bg).toMatch(/^rgba\(0,\s*0,\s*0,\s*0?\.\d+\)$/);

    // ── Regression guard for the readability bug ──────────────────────────────
    // Because the overlay backdrop above is translucent AND sits over a live,
    // blurred call, the inner `.modal-window` MUST supply its own SOLID surface
    // (via `.modal-overlay.in-meeting .modal-window` → --surface-elevated) so the
    // actionable error copy is readable regardless of the video luminance behind
    // it. Without that rule `.modal-window` has no matching background and
    // computes `rgba(0, 0, 0, 0)` (transparent) — failing the alpha assertion.
    // Mutation sensitivity: delete the `.modal-overlay.in-meeting .modal-window`
    // rule and `windowBg` reverts to `rgba(0, 0, 0, 0)`, breaking both asserts.
    const windowBg = await page
      .locator(MODAL_WINDOW)
      .evaluate((el) => getComputedStyle(el).backgroundColor);
    // Fully opaque (alpha === 1), and NOT the see-through overlay backdrop.
    expect(cssAlpha(windowBg)).toBe(1);
    expect(windowBg).not.toBe("rgba(0, 0, 0, 0)");
    expect(windowBg).not.toBe(bg);
    // Default (no `ui-theme`) is the dark palette → --surface-elevated #2c2c2e.
    expect(windowBg).toBe("rgb(44, 44, 46)");
  });

  // ─── Same solid-surface guard, LIGHT theme ─────────────────────────────────
  //
  // The readability failure is content- AND theme-dependent: in light theme
  // --text-primary is near-black (#1a1a1a) and, without a solid window, would
  // render dark-on-dimmed-dark video. --surface-elevated is theme-aware
  // (#f9f9fb light / #2c2c2e dark), so this asserts the light-theme window is a
  // solid LIGHT surface — proving the fix adapts and did not hardcode a color.
  // Theme is selected the same way theme-init.spec.ts does: seed
  // localStorage["ui-theme"] before boot so apply_and_save_theme() applies it on
  // mount. Mutation sensitivity: revert to a hardcoded dark `background: #1c1c1e`
  // (the `.modal-content` mistake CLAUDE.md warns against) and this fails.
  test("in-meeting device-warning modal window has a solid light surface in light theme", async ({
    page,
  }) => {
    await page.addInitScript(() => localStorage.setItem("ui-theme", "light"));

    await gotoAndJoin(page, `e2e_perm_window_light_${Date.now()}`);

    // Confirm the light palette is actually active before asserting its token.
    await expect
      .poll(() => page.evaluate(() => document.documentElement.getAttribute("data-theme")))
      .toBe("light");

    await setGumFail(page, { errorName: "NotAllowedError", video: -1 });

    await page.locator(CAMERA_BTN).click();
    await expect(page.locator(MODAL)).toBeVisible({ timeout: 15_000 });

    const windowBg = await page
      .locator(MODAL_WINDOW)
      .evaluate((el) => getComputedStyle(el).backgroundColor);
    // Solid (alpha 1) and the light-theme token --surface-elevated #f9f9fb.
    expect(cssAlpha(windowBg)).toBe(1);
    expect(windowBg).toBe("rgb(249, 249, 251)");
  });

  // ─── Extended-duration: the 60s backoff plateau does NOT wedge ─────────────
  //
  // The other auto-recovery tests above wait only ~25s, which covers just the
  // first 2-3 backoff tiers (4s → 8s → 16s). A REAL manual test reported the
  // camera badge still showing 5 MINUTES after the blocking app was released —
  // deep into the 60s-plateau region no existing spec reaches. This test drives
  // the retry loop across the whole backoff ladder and asserts it keeps issuing
  // getUserMedia probes at the stable ~60s plateau cadence, then still recovers.
  //
  // The retry loop probes `request_video_only` on an exponential schedule with a
  // 4s base: probes at ~4s, 12s, 28s, 60s, 120s, 180s … (gaps of 4, 8, 16, 32,
  // 60, 60 … seconds). `getGumCalls().video` is the observable — each background
  // probe is one video-requesting getUserMedia call. `expect.poll` passes as soon
  // as the threshold is met, so tiers that arrive early don't waste wall time.
  //
  // The DETERMINISTIC 10-minute proof lives in the Rust unit test
  // `retry_backoff_never_wedges_past_the_plateau` (drives the exact production
  // `retry_tick_decision` for 150 ticks). This E2E validates the SAME property
  // end-to-end through the real browser `setInterval` + `getUserMedia` wiring —
  // catching any breakage that only manifests once the loop is live in-page.
  //
  // Untagged (no @bvt): too long for per-PR CI. Runs against the local docker
  // e2e stack / a scoped dispatch. See CLAUDE.md "Change Acceptance Criteria".
  test("auto-recovery keeps probing across the full backoff plateau and still recovers", async ({
    page,
  }) => {
    // Backoff crosses two 60s plateau probes (~120s) plus a recovery wait; give
    // generous headroom over Playwright's 30s default.
    test.setTimeout(300_000);

    await gotoAndJoin(page, `e2e_perm_plateau_${Date.now()}`);

    // Camera held indefinitely by another app.
    await setGumFail(page, { errorName: "NotReadableError", video: -1 });

    const camera = page.locator(CAMERA_BTN);
    const modal = page.locator(MODAL);

    // Turn the camera ON → fails → DeviceInUse modal, and the user still WANTS it
    // on, which arms the background auto-retry loop for the camera side.
    await camera.click();
    await expect(modal).toBeVisible({ timeout: 15_000 });
    await expect(modal).toContainText(CAMERA_IN_USE_COPY);
    await expect(camera).toHaveClass(/\berror\b/);

    // Baseline video-probe count AFTER the initial (combined) request settled, so
    // the deltas below count ONLY background `request_video_only` retry probes.
    const baseline = (await getGumCalls(page)).video;
    const videoProbesSinceBaseline = async (): Promise<number> =>
      (await getGumCalls(page)).video - baseline;

    // Tiers ~4s + ~12s + ~28s → at least 3 background probes by ~45s.
    await expect
      .poll(videoProbesSinceBaseline, { timeout: 45_000, intervals: [2_000] })
      .toBeGreaterThanOrEqual(3);

    // First plateau probe (~60s tier) → at least 4 by ~75s.
    await expect
      .poll(videoProbesSinceBaseline, { timeout: 40_000, intervals: [2_000] })
      .toBeGreaterThanOrEqual(4);

    // SECOND plateau probe (~120s tier): the coverage no ~25s spec provides. It
    // proves the loop KEEPS probing at a steady ~60s cadence AFTER the gap caps —
    // i.e. it does not wedge once deep in the plateau, the exact "5 minutes and
    // the badge never cleared" failure mode from the manual report.
    await expect
      .poll(videoProbesSinceBaseline, { timeout: 80_000, intervals: [3_000] })
      .toBeGreaterThanOrEqual(5);

    // Still blocked all this time → badge still shown, capture never auto-started.
    await expect(camera).toHaveClass(/\berror\b/);
    await expect(camera).not.toHaveClass(/\bactive\b/);

    // The other app RELEASES the device: the next plateau probe (within ~60s)
    // succeeds and clears the error hands-off. Per the no-auto-start rule the
    // camera returns to plain "off" (error cleared) — NOT "active".
    await setGumFail(page, { video: 0 });
    // The REAL recovery signal is the error class CLEARING. Because capture never
    // auto-starts (video_enabled stays false), the class is only ever "off error"
    // (blocked) or "off" (recovered) — "off" is present in BOTH, so a bare
    // toHaveClass(/\boff\b/) resolves instantly and waits for nothing. Deep in the
    // 60s plateau the next recovery probe can be up to ~60s away, so the wait must
    // live on the error-cleared assertion with plateau-sized headroom. Playwright's
    // 10s default is far too short here — that timeout/assertion mismatch is what
    // flaked this test, not the retry loop (which the plateau probes above prove
    // keeps firing at a steady ~60s cadence).
    await expect(camera).not.toHaveClass(/\berror\b/, { timeout: 70_000 });
    // Recovery confirmed: still plain "off" (error cleared) and never "active"
    // (the no-auto-start rule). Both already hold now — cheap follow-up checks.
    await expect(camera).toHaveClass(/\boff\b/);
    await expect(camera).not.toHaveClass(/\bactive\b/);
    // The modal the user left open closes itself once both sides are error-free.
    await expect(modal).toBeHidden({ timeout: 10_000 });
  });
});
