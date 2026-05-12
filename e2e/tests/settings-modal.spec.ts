import { test, expect } from "@playwright/test";
import { injectSessionCookie } from "../helpers/auth";
import { waitForServices } from "../helpers/wait-for-services";

test.describe("Device settings modal", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test.beforeEach(async ({ context, baseURL }) => {
    await injectSessionCookie(context, { baseURL });
  });

  test("user can open settings modal and switch between Audio and Video sections", async ({
    page,
  }) => {
    const meetingId = `e2e_settings_${Date.now()}`;

    await page.goto("/");
    await page.waitForTimeout(1500);

    await page.locator("#meeting-id").click();
    await page.locator("#meeting-id").pressSequentially(meetingId, { delay: 80 });

    await page.locator("#username").click();
    await page.locator("#username").fill("");
    await page.locator("#username").pressSequentially("settings-user", { delay: 80 });
    await page.waitForTimeout(500);
    await page.locator("#username").press("Enter");

    await expect(page).toHaveURL(new RegExp(`/meeting/${meetingId}`), { timeout: 10_000 });

    // Enter the meeting
    const joinButton = page.getByRole("button", { name: /Start Meeting|Join Meeting/ });
    await expect(joinButton).toBeVisible({ timeout: 20_000 });
    await joinButton.click();

    await expect(page.locator("#grid-container")).toBeVisible({
      timeout: 15_000,
    });

    // Open settings modal from bottom toolbar gear
    await page.locator('[data-testid="open-settings"]').click();

    await expect(page.locator(".device-settings-modal")).toBeVisible({
      timeout: 10_000,
    });

    // Default section: Audio
    await expect(page.locator(".settings-nav-button.active")).toContainText("Audio");
    await expect(page.locator("#modal-audio-select")).toBeVisible();
    await expect(page.locator("#modal-speaker-select")).toBeVisible();
    await expect(page.locator("#modal-video-select")).toHaveCount(0);

    // Switch to Video
    await page.getByRole("tab", { name: "Video" }).click();

    await expect(page.locator(".settings-nav-button.active")).toContainText("Video");
    await expect(page.locator("#modal-video-select")).toBeVisible();
    await expect(page.locator("#modal-audio-select")).toHaveCount(0);
  });

  test("microphone dropdown closes while switching to the Video tab in one click", async ({
    page,
  }) => {
    const meetingId = `e2e_settings_dropdown_${Date.now()}`;

    await page.goto("/");
    await page.waitForTimeout(1500);

    await page.locator("#meeting-id").click();
    await page.locator("#meeting-id").pressSequentially(meetingId, { delay: 80 });

    await page.locator("#username").click();
    await page.locator("#username").fill("");
    await page.locator("#username").pressSequentially("dropdown-user", { delay: 80 });
    await page.waitForTimeout(500);
    await page.locator("#username").press("Enter");

    await expect(page).toHaveURL(new RegExp(`/meeting/${meetingId}`), { timeout: 10_000 });

    const joinButton = page.getByRole("button", { name: /Start Meeting|Join Meeting/ });
    await expect(joinButton).toBeVisible({ timeout: 20_000 });
    await joinButton.click();

    await expect(page.locator("#grid-container")).toBeVisible({ timeout: 15_000 });

    await page.locator('[data-testid="open-settings"]').click();
    await expect(page.locator(".device-settings-modal")).toBeVisible({ timeout: 10_000 });
    await expect(page.locator(".settings-nav-button.active")).toContainText("Audio");

    const microphoneSelect = page.locator("#modal-audio-select");
    const dropdownMenu = page.locator("#modal-audio-select + .glass-select-menu");
    const videoTab = page.getByRole("tab", { name: "Video" });

    await microphoneSelect.click();
    await expect(dropdownMenu).toBeVisible();

    await videoTab.click();

    await expect(dropdownMenu).toHaveCount(0);
    await expect(page.locator(".settings-nav-button.active")).toContainText("Video");
    await expect(page.locator("#settings-panel-video")).toBeVisible();
  });

  // FIXME(#727): Tests below have stale UI selectors after the Appearance
  // panel refactor. Need rewrite against current DOM. Tracked in #727.
  // FIXME(#727): Uses getByRole("button", { name: "Mute" }) which doesn't
  // match hidden tooltip spans. Unblock: switch to tooltip locator pattern.
  test.fixme("opening settings modal preserves current microphone and camera button states", async ({
    page,
  }) => {
    const meetingId = `e2e_settings_safe_refresh_${Date.now()}`;

    await page.goto("/");
    await page.waitForTimeout(1500);

    await page.locator("#meeting-id").click();
    await page.locator("#meeting-id").pressSequentially(meetingId, { delay: 80 });

    await page.locator("#username").click();
    await page.locator("#username").fill("");
    await page.locator("#username").pressSequentially("safe-refresh-user", { delay: 80 });
    await page.waitForTimeout(500);
    await page.locator("#username").press("Enter");

    await expect(page).toHaveURL(new RegExp(`/meeting/${meetingId}`), { timeout: 10_000 });

    const joinButton = page.getByRole("button", { name: /Start Meeting|Join Meeting/ });
    await expect(joinButton).toBeVisible({ timeout: 20_000 });
    await joinButton.click();

    await expect(page.locator("#grid-container")).toBeVisible({ timeout: 15_000 });

    const micMuteButton = page.getByRole("button", { name: "Mute" });
    const micUnmuteButton = page.getByRole("button", { name: "Unmute" });
    const camStopButton = page.getByRole("button", { name: "Stop Video" });
    const camStartButton = page.getByRole("button", { name: "Start Video" });

    const micStateBefore = (await micMuteButton.count()) > 0 ? "enabled" : "disabled";
    const camStateBefore = (await camStopButton.count()) > 0 ? "enabled" : "disabled";

    await expect(
      micStateBefore === "enabled" ? micMuteButton.first() : micUnmuteButton.first(),
    ).toBeVisible({ timeout: 10_000 });
    await expect(
      camStateBefore === "enabled" ? camStopButton.first() : camStartButton.first(),
    ).toBeVisible({ timeout: 10_000 });

    await page.locator('[data-testid="open-settings"]').click();
    await expect(page.locator(".device-settings-modal")).toBeVisible({ timeout: 10_000 });
    await expect(page.locator("#modal-audio-select")).toBeVisible({ timeout: 10_000 });

    await page.locator('[data-testid="open-settings"]').click();
    await expect(page.locator(".device-settings-modal")).toBeHidden({ timeout: 10_000 });

    const micStateAfter = (await micMuteButton.count()) > 0 ? "enabled" : "disabled";
    const camStateAfter = (await camStopButton.count()) > 0 ? "enabled" : "disabled";

    await expect(
      micStateAfter === "enabled" ? micMuteButton.first() : micUnmuteButton.first(),
    ).toBeVisible({ timeout: 10_000 });
    await expect(
      camStateAfter === "enabled" ? camStopButton.first() : camStartButton.first(),
    ).toBeVisible({ timeout: 10_000 });

    expect(micStateAfter).toBe(micStateBefore);
    expect(camStateAfter).toBe(camStateBefore);
  });

  // FIXME(#727): Appearance panel DOM changed in refactor — selectors for
  // glow controls, color swatches, and sliders need rewriting.
  test.fixme("user can open the Appearance section and adjust local glow controls", async ({
    page,
  }) => {
    const meetingId = `e2e_settings_appearance_${Date.now()}`;

    await page.goto("/");
    await page.waitForTimeout(1500);

    await page.locator("#meeting-id").click();
    await page.locator("#meeting-id").pressSequentially(meetingId, { delay: 80 });

    await page.locator("#username").click();
    await page.locator("#username").fill("");
    await page.locator("#username").pressSequentially("appearance-user", { delay: 80 });
    await page.waitForTimeout(500);
    await page.locator("#username").press("Enter");

    await expect(page).toHaveURL(new RegExp(`/meeting/${meetingId}`), { timeout: 10_000 });

    const joinButton = page.getByRole("button", { name: /Start Meeting|Join Meeting/ });
    await expect(joinButton).toBeVisible({ timeout: 20_000 });
    await joinButton.click();

    await expect(page.locator("#grid-container")).toBeVisible({
      timeout: 15_000,
    });

    await page.locator('[data-testid="open-settings"]').click();

    await expect(page.locator(".device-settings-modal")).toBeVisible({
      timeout: 10_000,
    });

    await page.getByRole("tab", { name: "Appearance" }).click();

    await expect(page.locator(".settings-nav-button.active")).toContainText("Appearance");
    await expect(page.locator("#settings-panel-appearance")).toBeVisible();
    await expect(
      page.getByText("Customize how speaking glows appear on your screen"),
    ).toBeVisible();

    const mintSwatch = page.locator('[aria-label="Select Mint Green glow"]');
    const cyanSwatch = page.locator('[aria-label="Select Cyan glow"]');
    const brightnessSlider = page.locator(".brightness-section .appearance-slider");
    const innerGlowSlider = page.locator(".inner-glow-section .appearance-slider");

    await expect(mintSwatch).toHaveAttribute("aria-pressed", "true");
    await expect(page.locator(".brightness-section .slider-value")).toHaveText("100%");
    await expect(page.locator(".inner-glow-section .slider-value")).toHaveText("100%");

    await cyanSwatch.click();

    await expect(cyanSwatch).toHaveAttribute("aria-pressed", "true");
    await expect(mintSwatch).toHaveAttribute("aria-pressed", "false");

    await brightnessSlider.fill("55");
    await innerGlowSlider.fill("40");

    await expect(page.locator(".brightness-section .slider-value")).toHaveText("55%");
    await expect(page.locator(".inner-glow-section .slider-value")).toHaveText("40%");

    // Preview tile uses fixed-intensity glow (no audio dependency).
    // After selecting Cyan (rgb 12, 175, 255) the style must contain those values.
    const previewTile = page.locator(".appearance-preview-area .preview-tile");
    await expect(previewTile).toBeVisible();
    // Decorative avatar silhouette SVG is rendered inside the preview tile
    await expect(previewTile.locator("svg")).toBeVisible();
    await expect(previewTile).toHaveAttribute("style", /rgba\(12, 175, 255/);
    // Pulsation CSS class is present (visual animation only — no timing assertions).
    await expect(previewTile).toHaveClass(/preview-tile-pulsing/);
  });

  // FIXME(#727): Stale selectors after Appearance panel refactor.
  test.fixme("brightness slider at 0 keeps panel functional and color selection works", async ({
    page,
  }) => {
    const meetingId = `e2e_brightness_zero_${Date.now()}`;

    await page.goto("/");
    await page.waitForTimeout(1500);

    await page.locator("#meeting-id").click();
    await page.locator("#meeting-id").pressSequentially(meetingId, { delay: 80 });

    await page.locator("#username").click();
    await page.locator("#username").fill("");
    await page.locator("#username").pressSequentially("brightness-zero-user", { delay: 80 });
    await page.waitForTimeout(500);
    await page.locator("#username").press("Enter");

    await expect(page).toHaveURL(new RegExp(`/meeting/${meetingId}`), { timeout: 10_000 });

    const joinButton = page.getByRole("button", { name: /Start Meeting|Join Meeting/ });
    await expect(joinButton).toBeVisible({ timeout: 20_000 });
    await joinButton.click();

    await expect(page.locator("#grid-container")).toBeVisible({ timeout: 15_000 });

    await page.locator('[data-testid="open-settings"]').click();
    await expect(page.locator(".device-settings-modal")).toBeVisible({ timeout: 10_000 });
    await page.getByRole("tab", { name: "Appearance" }).click();

    // Set brightness to 0
    const brightnessSlider = page.locator(".brightness-section .appearance-slider");
    await brightnessSlider.fill("0");
    await expect(page.locator(".brightness-section .slider-value")).toHaveText("0%");

    // Panel should remain fully functional: color swatches are still clickable
    const cyanSwatch = page.locator('[aria-label="Select Cyan glow"]');
    await cyanSwatch.click();
    await expect(cyanSwatch).toHaveAttribute("aria-pressed", "true");

    const magentaSwatch = page.locator('[aria-label="Select Magenta glow"]');
    await magentaSwatch.click();
    await expect(magentaSwatch).toHaveAttribute("aria-pressed", "true");
    await expect(cyanSwatch).toHaveAttribute("aria-pressed", "false");

    // Inner glow slider is still adjustable
    const innerGlowSlider = page.locator(".inner-glow-section .appearance-slider");
    await innerGlowSlider.fill("70");
    await expect(page.locator(".inner-glow-section .slider-value")).toHaveText("70%");

    // Preview tile uses fixed-intensity glow; with brightness at 0 the
    // outer alpha is 0 but the border-color still carries the selected
    // Magenta colour (rgb 255, 0, 191).
    const previewTile = page.locator(".appearance-preview-area .preview-tile");
    await expect(previewTile).toBeVisible();
    await expect(previewTile).toHaveAttribute("style", /rgba\(255, 0, 191/);
    await expect(previewTile).toHaveClass(/preview-tile-pulsing/);
  });

  // FIXME(#727): Stale selectors after Appearance panel refactor.
  test.fixme("appearance settings persist after reload and rejoin", async ({ page }) => {
    const meetingId = `e2e_settings_persist_${Date.now()}`;

    await page.goto("/");
    await page.waitForTimeout(1500);

    await page.locator("#meeting-id").click();
    await page.locator("#meeting-id").pressSequentially(meetingId, { delay: 80 });

    await page.locator("#username").click();
    await page.locator("#username").fill("");
    await page.locator("#username").pressSequentially("persist-user", { delay: 80 });
    await page.waitForTimeout(500);
    await page.locator("#username").press("Enter");

    await expect(page).toHaveURL(new RegExp(`/meeting/${meetingId}`), { timeout: 10_000 });

    const joinButton = page.getByRole("button", { name: /Start Meeting|Join Meeting/ });
    await expect(joinButton).toBeVisible({ timeout: 20_000 });
    await joinButton.click();

    await expect(page.locator("#grid-container")).toBeVisible({ timeout: 15_000 });

    await page.locator('[data-testid="open-settings"]').click();
    await expect(page.locator(".device-settings-modal")).toBeVisible({ timeout: 10_000 });
    await page.getByRole("tab", { name: "Appearance" }).click();

    const cyanSwatch = page.locator('[aria-label="Select Cyan glow"]');
    const brightnessSlider = page.locator(".brightness-section .appearance-slider");
    const innerGlowSlider = page.locator(".inner-glow-section .appearance-slider");
    const glowToggle = page.locator('.glow-switch input[type="checkbox"]');

    await cyanSwatch.click();
    await brightnessSlider.fill("55");
    await innerGlowSlider.fill("40");
    await glowToggle.uncheck();

    await expect(cyanSwatch).toHaveAttribute("aria-pressed", "true");
    await expect(page.locator(".brightness-section .slider-value")).toHaveText("55%");
    await expect(page.locator(".inner-glow-section .slider-value")).toHaveText("40%");
    await expect(glowToggle).not.toBeChecked();

    // Wait for the 300ms debounced save to flush to localStorage before reloading
    await page.waitForTimeout(500);

    await page.reload();

    const joinAgain = page.getByRole("button", { name: /Start Meeting|Join Meeting/ });
    if (await joinAgain.isVisible({ timeout: 5_000 }).catch(() => false)) {
      await joinAgain.click();
    }

    await expect(page.locator("#grid-container")).toBeVisible({ timeout: 15_000 });

    await page.locator('[data-testid="open-settings"]').click();
    await expect(page.locator(".device-settings-modal")).toBeVisible({ timeout: 10_000 });
    await page.getByRole("tab", { name: "Appearance" }).click();

    await expect(page.locator('[aria-label="Select Cyan glow"]')).toHaveAttribute(
      "aria-pressed",
      "true",
    );
    await expect(page.locator(".brightness-section .appearance-slider")).toHaveValue("55");
    await expect(page.locator(".inner-glow-section .appearance-slider")).toHaveValue("40");
    await expect(page.locator('.glow-switch input[type="checkbox"]')).not.toBeChecked();
  });

  test("Glow Color row includes + button that opens custom color popover", async ({ page }) => {
    const meetingId = `e2e_custom_color_popover_${Date.now()}`;

    await page.goto("/");
    await page.waitForTimeout(1500);

    await page.locator("#meeting-id").click();
    await page.locator("#meeting-id").pressSequentially(meetingId, { delay: 80 });

    await page.locator("#username").click();
    await page.locator("#username").fill("");
    await page.locator("#username").pressSequentially("custom-color-user", { delay: 80 });
    await page.waitForTimeout(500);
    await page.locator("#username").press("Enter");

    await expect(page).toHaveURL(new RegExp(`/meeting/${meetingId}`), { timeout: 10_000 });

    const joinButton = page.getByRole("button", { name: /Start Meeting|Join Meeting/ });
    await expect(joinButton).toBeVisible({ timeout: 20_000 });
    await joinButton.click();

    await expect(page.locator("#grid-container")).toBeVisible({ timeout: 15_000 });

    await page.locator('[data-testid="open-settings"]').click();
    await expect(page.locator(".device-settings-modal")).toBeVisible({ timeout: 10_000 });
    await page.getByRole("tab", { name: "Appearance" }).click();

    // The + button is visible inside the swatches row
    const addBtn = page.locator('[aria-label="Add custom color"]');
    await expect(addBtn).toBeVisible();

    // The + button renders an inline SVG icon (not a text glyph)
    await expect(addBtn.locator("svg")).toBeVisible();

    // Modal and backdrop are not yet visible
    await expect(page.locator(".custom-color-popover")).toHaveCount(0);
    await expect(page.locator(".custom-color-modal-overlay")).toHaveCount(0);

    // Click + opens the modal with input, color picker, Cancel, and Add button
    await addBtn.click();

    // Backdrop overlay is rendered behind the modal
    await expect(page.locator(".custom-color-modal-overlay")).toBeVisible();

    const popover = page.locator(".custom-color-popover");
    await expect(popover).toBeVisible();

    // Modal accessibility attributes
    await expect(popover).toHaveAttribute("role", "dialog");
    await expect(popover).toHaveAttribute("aria-modal", "true");
    await expect(popover).toHaveAttribute("aria-labelledby", "custom-color-modal-title");
    const title = popover.locator("#custom-color-modal-title");
    await expect(title).toBeVisible();
    await expect(title).toHaveText("Choose Custom Color");

    // Custom HSV picker: saturation/value square with proper ARIA
    const svSquare = popover.locator(".color-picker-sv-square");
    await expect(svSquare).toBeVisible();
    await expect(svSquare).toHaveAttribute("role", "application");
    await expect(svSquare).toHaveAttribute("tabindex", "0");
    await expect(svSquare.locator(".color-picker-sv-marker")).toBeVisible();

    // Hue slider with proper ARIA
    const hueTrack = popover.locator(".color-picker-hue-track");
    await expect(hueTrack).toBeVisible();
    await expect(hueTrack).toHaveAttribute("role", "slider");
    await expect(hueTrack).toHaveAttribute("aria-label", "Hue");
    await expect(hueTrack).toHaveAttribute("aria-valuemin", "0");
    await expect(hueTrack).toHaveAttribute("aria-valuemax", "360");
    await expect(hueTrack).toHaveAttribute("tabindex", "0");
    await expect(hueTrack.locator(".color-picker-hue-thumb")).toBeVisible();

    // Preview swatch is rendered alongside the picker
    await expect(popover.locator(".color-picker-preview-swatch")).toBeVisible();

    // Hex text input
    await expect(popover.locator(".custom-color-input")).toBeVisible();
    await expect(popover.locator(".custom-color-input")).toHaveAttribute("placeholder", "#RRGGBB");

    // Cancel and Add buttons
    await expect(popover.locator(".custom-color-cancel-btn")).toBeVisible();
    await expect(popover.locator(".custom-color-cancel-btn")).toHaveText("Cancel");
    await expect(popover.locator(".custom-color-add-btn")).toBeVisible();
    await expect(popover.locator(".custom-color-add-btn")).toHaveText("Add");
  });

  test("typing a valid hex in the text input updates the hue slider value", async ({ page }) => {
    const meetingId = `e2e_custom_color_picker_sync_${Date.now()}`;

    await page.goto("/");
    await page.waitForTimeout(1500);

    await page.locator("#meeting-id").click();
    await page.locator("#meeting-id").pressSequentially(meetingId, { delay: 80 });

    await page.locator("#username").click();
    await page.locator("#username").fill("");
    await page.locator("#username").pressSequentially("picker-sync-user", { delay: 80 });
    await page.waitForTimeout(500);
    await page.locator("#username").press("Enter");

    await expect(page).toHaveURL(new RegExp(`/meeting/${meetingId}`), { timeout: 10_000 });

    const joinButton = page.getByText(/Start Meeting|Join Meeting/);
    await expect(joinButton).toBeVisible({ timeout: 20_000 });
    await joinButton.click();

    await expect(page.locator("#grid-container")).toBeVisible({ timeout: 15_000 });

    await page.locator('[data-testid="open-settings"]').click();
    await expect(page.locator(".device-settings-modal")).toBeVisible({ timeout: 10_000 });
    await page.getByRole("tab", { name: "Appearance" }).click();

    await page.locator('[aria-label="Add custom color"]').click();

    const popover = page.locator(".custom-color-popover");
    await expect(popover).toBeVisible();

    const colorInput = popover.locator(".custom-color-input");
    const hueTrack = popover.locator(".color-picker-hue-track");

    // #12ABEF -> hue ~198 degrees
    await colorInput.fill("#12ABEF");
    await expect
      .poll(
        async () => {
          const v = await hueTrack.getAttribute("aria-valuenow");
          return v === null ? NaN : Number(v);
        },
        { timeout: 5_000 },
      )
      .toBeGreaterThanOrEqual(196);
    expect(Number(await hueTrack.getAttribute("aria-valuenow"))).toBeLessThanOrEqual(200);

    // #ff5500 -> hue ~20 degrees
    await colorInput.fill("#ff5500");
    await expect
      .poll(
        async () => {
          const v = await hueTrack.getAttribute("aria-valuenow");
          return v === null ? NaN : Number(v);
        },
        { timeout: 5_000 },
      )
      .toBeGreaterThanOrEqual(18);
    expect(Number(await hueTrack.getAttribute("aria-valuenow"))).toBeLessThanOrEqual(22);
  });

  test("hue slider supports keyboard navigation (ArrowDown increases hue)", async ({ page }) => {
    const meetingId = `e2e_custom_color_hue_kbd_${Date.now()}`;

    await page.goto("/");
    await page.waitForTimeout(1500);

    await page.locator("#meeting-id").click();
    await page.locator("#meeting-id").pressSequentially(meetingId, { delay: 80 });

    await page.locator("#username").click();
    await page.locator("#username").fill("");
    await page.locator("#username").pressSequentially("hue-kbd-user", { delay: 80 });
    await page.waitForTimeout(500);
    await page.locator("#username").press("Enter");

    await expect(page).toHaveURL(new RegExp(`/meeting/${meetingId}`), { timeout: 10_000 });

    const joinButton = page.getByText(/Start Meeting|Join Meeting/);
    await expect(joinButton).toBeVisible({ timeout: 20_000 });
    await joinButton.click();

    await expect(page.locator("#grid-container")).toBeVisible({ timeout: 15_000 });

    await page.locator('[data-testid="open-settings"]').click();
    await expect(page.locator(".device-settings-modal")).toBeVisible({ timeout: 10_000 });
    await page.getByRole("tab", { name: "Appearance" }).click();

    await page.locator('[aria-label="Add custom color"]').click();

    const popover = page.locator(".custom-color-popover");
    await expect(popover).toBeVisible();

    // Seed a known hue via the hex input so the test is deterministic.
    await popover.locator(".custom-color-input").fill("#ff5500");

    const hueTrack = popover.locator(".color-picker-hue-track");
    await expect(hueTrack).toHaveAttribute("aria-orientation", "vertical");
    await expect
      .poll(async () => Number(await hueTrack.getAttribute("aria-valuenow")), { timeout: 5_000 })
      .toBeGreaterThanOrEqual(18);

    const before = Number(await hueTrack.getAttribute("aria-valuenow"));

    await hueTrack.focus();
    await hueTrack.press("ArrowDown");

    await expect
      .poll(async () => Number(await hueTrack.getAttribute("aria-valuenow")), { timeout: 5_000 })
      .toBeGreaterThan(before);

    const after = Number(await hueTrack.getAttribute("aria-valuenow"));
    // ArrowDown should bump hue by ~1 degree on the vertical hue slider.
    expect(after - before).toBeGreaterThanOrEqual(1);
    expect(after - before).toBeLessThanOrEqual(3);
  });

  test("Cancel button closes the modal without adding a swatch", async ({ page }) => {
    const meetingId = `e2e_custom_color_cancel_${Date.now()}`;

    await page.goto("/");
    await page.waitForTimeout(1500);

    await page.locator("#meeting-id").click();
    await page.locator("#meeting-id").pressSequentially(meetingId, { delay: 80 });

    await page.locator("#username").click();
    await page.locator("#username").fill("");
    await page.locator("#username").pressSequentially("cancel-modal-user", { delay: 80 });
    await page.waitForTimeout(500);
    await page.locator("#username").press("Enter");

    await expect(page).toHaveURL(new RegExp(`/meeting/${meetingId}`), { timeout: 10_000 });

    const joinButton = page.getByText(/Start Meeting|Join Meeting/);
    await expect(joinButton).toBeVisible({ timeout: 20_000 });
    await joinButton.click();

    await expect(page.locator("#grid-container")).toBeVisible({ timeout: 15_000 });

    await page.locator('[data-testid="open-settings"]').click();
    await expect(page.locator(".device-settings-modal")).toBeVisible({ timeout: 10_000 });
    await page.getByRole("tab", { name: "Appearance" }).click();

    const swatchCountBefore = await page.locator(".color-swatches .color-swatch").count();

    await page.locator('[aria-label="Add custom color"]').click();

    const popover = page.locator(".custom-color-popover");
    await expect(popover).toBeVisible();

    // Type a syntactically valid hex but cancel before adding
    await popover.locator(".custom-color-input").fill("#ABCDEF");

    await popover.locator(".custom-color-cancel-btn").click();

    // Modal and backdrop are dismissed
    await expect(popover).toHaveCount(0);
    await expect(page.locator(".custom-color-modal-overlay")).toHaveCount(0);

    // No swatch was added
    const swatchCountAfter = await page.locator(".color-swatches .color-swatch").count();
    expect(swatchCountAfter).toBe(swatchCountBefore);
    await expect(page.locator('[aria-label*="Select custom glow #ABCDEF"]')).toHaveCount(0);

    // Focus returns to the add button
    const focusedElementId = await page.evaluate(() => document.activeElement?.id);
    expect(focusedElementId).toBe("add-custom-color-btn");
  });

  // FIXME(#694): Color picker feature tests — not yet validated against
  // current DOM. Unblock: run in headed mode to confirm selectors.
  test.fixme("custom color popover closes when clicking outside and focus returns to add button", async ({
    page,
  }) => {
    const meetingId = `e2e_popover_click_outside_${Date.now()}`;

    await page.goto("/");
    await page.waitForTimeout(1500);

    await page.locator("#meeting-id").click();
    await page.locator("#meeting-id").pressSequentially(meetingId, { delay: 80 });

    await page.locator("#username").click();
    await page.locator("#username").fill("");
    await page.locator("#username").pressSequentially("click-outside-user", { delay: 80 });
    await page.waitForTimeout(500);
    await page.locator("#username").press("Enter");

    await expect(page).toHaveURL(new RegExp(`/meeting/${meetingId}`), { timeout: 10_000 });

    const joinButton = page.getByRole("button", { name: /Start Meeting|Join Meeting/ });
    await expect(joinButton).toBeVisible({ timeout: 20_000 });
    await joinButton.click();

    await expect(page.locator("#grid-container")).toBeVisible({ timeout: 15_000 });

    await page.locator('[data-testid="open-settings"]').click();
    await expect(page.locator(".device-settings-modal")).toBeVisible({ timeout: 10_000 });
    await page.getByRole("tab", { name: "Appearance" }).click();

    // Open the custom color popover
    const addBtn = page.locator('[aria-label="Add custom color"]');
    await addBtn.click();

    const popover = page.locator(".custom-color-popover");
    await expect(popover).toBeVisible();

    // Click outside the dialog — clicking on the modal backdrop (away from the
    // dialog content) should dismiss the popover.
    await page.locator(".custom-color-modal-overlay").click({ position: { x: 5, y: 5 } });

    // Popover should be dismissed
    await expect(popover).toHaveCount(0);

    // Focus should return to the add button
    const focusedElementId = await page.evaluate(() => document.activeElement?.id);
    expect(focusedElementId).toBe("add-custom-color-btn");
  });

  // FIXME(#694): Color picker feature — Add button is disabled for invalid
  // input (by design), so the test flow of click-then-assert-error is wrong.
  // Unblock: assert button.toBeDisabled() instead of clicking it.
  test.fixme("invalid custom color input shows error and does not add swatch", async ({ page }) => {
    const meetingId = `e2e_custom_color_invalid_${Date.now()}`;

    await page.goto("/");
    await page.waitForTimeout(1500);

    await page.locator("#meeting-id").click();
    await page.locator("#meeting-id").pressSequentially(meetingId, { delay: 80 });

    await page.locator("#username").click();
    await page.locator("#username").fill("");
    await page.locator("#username").pressSequentially("invalid-color-user", { delay: 80 });
    await page.waitForTimeout(500);
    await page.locator("#username").press("Enter");

    await expect(page).toHaveURL(new RegExp(`/meeting/${meetingId}`), { timeout: 10_000 });

    const joinButton = page.getByRole("button", { name: /Start Meeting|Join Meeting/ });
    await expect(joinButton).toBeVisible({ timeout: 20_000 });
    await joinButton.click();

    await expect(page.locator("#grid-container")).toBeVisible({ timeout: 15_000 });

    await page.locator('[data-testid="open-settings"]').click();
    await expect(page.locator(".device-settings-modal")).toBeVisible({ timeout: 10_000 });
    await page.getByRole("tab", { name: "Appearance" }).click();

    const addBtn = page.locator('[aria-label="Add custom color"]');
    await addBtn.click();

    const popover = page.locator(".custom-color-popover");
    await expect(popover).toBeVisible();

    const colorInput = popover.locator(".custom-color-input");
    const addColorBtn = popover.locator(".custom-color-add-btn");

    // Count existing swatches before attempting invalid adds
    const swatchCountBefore = await page.locator(".color-swatches .color-swatch").count();

    // Test invalid: missing # prefix — Add button is disabled
    await colorInput.fill("123456");
    await expect(addColorBtn).toBeDisabled();
    await expect(popover).toBeVisible();

    // Test invalid: too short
    await colorInput.fill("#12");
    await expect(addColorBtn).toBeDisabled();
    await expect(popover).toBeVisible();

    // Test invalid: non-hex characters
    await colorInput.fill("#GGGGGG");
    await expect(addColorBtn).toBeDisabled();
    await expect(popover).toBeVisible();

    // Confirm no new swatch was added
    const swatchCountAfter = await page.locator(".color-swatches .color-swatch").count();
    expect(swatchCountAfter).toBe(swatchCountBefore);
  });

  // FIXME(#694): Color picker feature — not yet validated against current DOM.
  test.fixme("valid custom color adds swatch, selects it, and it can be reselected", async ({
    page,
  }) => {
    const meetingId = `e2e_custom_color_valid_${Date.now()}`;

    await page.goto("/");
    await page.waitForTimeout(1500);

    await page.locator("#meeting-id").click();
    await page.locator("#meeting-id").pressSequentially(meetingId, { delay: 80 });

    await page.locator("#username").click();
    await page.locator("#username").fill("");
    await page.locator("#username").pressSequentially("valid-color-user", { delay: 80 });
    await page.waitForTimeout(500);
    await page.locator("#username").press("Enter");

    await expect(page).toHaveURL(new RegExp(`/meeting/${meetingId}`), { timeout: 10_000 });

    const joinButton = page.getByRole("button", { name: /Start Meeting|Join Meeting/ });
    await expect(joinButton).toBeVisible({ timeout: 20_000 });
    await joinButton.click();

    await expect(page.locator("#grid-container")).toBeVisible({ timeout: 15_000 });

    await page.locator('[data-testid="open-settings"]').click();
    await expect(page.locator(".device-settings-modal")).toBeVisible({ timeout: 10_000 });
    await page.getByRole("tab", { name: "Appearance" }).click();

    const addBtn = page.locator('[aria-label="Add custom color"]');
    await addBtn.click();

    const popover = page.locator(".custom-color-popover");
    await expect(popover).toBeVisible();

    const colorInput = popover.locator(".custom-color-input");
    const addColorBtn = popover.locator(".custom-color-add-btn");

    // Add a valid custom color
    await colorInput.fill("#12ABEF");
    await addColorBtn.click();

    // Popover closes after successful add
    await expect(popover).toHaveCount(0);

    // A new custom swatch appears and is automatically selected
    const customSwatch = page.locator('[aria-label*="Select custom glow #12ABEF"]');
    await expect(customSwatch).toBeVisible();
    await expect(customSwatch).toHaveAttribute("aria-pressed", "true");

    // Previously selected preset (default Mint Green) is deselected
    await expect(page.locator('[aria-label="Select Mint Green glow"]')).toHaveAttribute(
      "aria-pressed",
      "false",
    );

    // Switch to a preset, then reselect the custom color
    const cyanSwatch = page.locator('[aria-label="Select Cyan glow"]');
    await cyanSwatch.click();
    await expect(cyanSwatch).toHaveAttribute("aria-pressed", "true");
    await expect(customSwatch).toHaveAttribute("aria-pressed", "false");

    await customSwatch.click();
    await expect(customSwatch).toHaveAttribute("aria-pressed", "true");
    await expect(cyanSwatch).toHaveAttribute("aria-pressed", "false");

    // Preview tile uses fixed-intensity glow — selecting custom #12ABEF
    // (rgb 18, 171, 239) must appear in the inline style.
    const previewTile = page.locator(".appearance-preview-area .preview-tile");
    await expect(previewTile).toBeVisible();
    await expect(previewTile).toHaveAttribute("style", /rgba\(18, 171, 239/);
    await expect(previewTile).toHaveClass(/preview-tile-pulsing/);
  });

  // FIXME(#694): Color picker feature — not yet validated against current DOM.
  test.fixme("custom color can be deleted via delete button", async ({ page }) => {
    const meetingId = `e2e_custom_color_delete_${Date.now()}`;

    await page.goto("/");
    await page.waitForTimeout(1500);

    await page.locator("#meeting-id").click();
    await page.locator("#meeting-id").pressSequentially(meetingId, { delay: 80 });

    await page.locator("#username").click();
    await page.locator("#username").fill("");
    await page.locator("#username").pressSequentially("delete-color-user", { delay: 80 });
    await page.waitForTimeout(500);
    await page.locator("#username").press("Enter");

    await expect(page).toHaveURL(new RegExp(`/meeting/${meetingId}`), { timeout: 10_000 });

    const joinButton = page.getByRole("button", { name: /Start Meeting|Join Meeting/ });
    await expect(joinButton).toBeVisible({ timeout: 20_000 });
    await joinButton.click();

    await expect(page.locator("#grid-container")).toBeVisible({ timeout: 15_000 });

    await page.locator('[data-testid="open-settings"]').click();
    await expect(page.locator(".device-settings-modal")).toBeVisible({ timeout: 10_000 });
    await page.getByRole("tab", { name: "Appearance" }).click();

    // Add a custom color
    const addBtn = page.locator('[aria-label="Add custom color"]');
    await addBtn.click();

    const popover = page.locator(".custom-color-popover");
    const colorInput = popover.locator(".custom-color-input");
    const addColorBtn = popover.locator(".custom-color-add-btn");

    await colorInput.fill("#FF5733");
    await addColorBtn.click();

    // Verify custom swatch was created and is selected
    const customSwatch = page.locator('[aria-label*="Select custom glow #FF5733"]').first();
    await expect(customSwatch).toBeVisible();
    await expect(customSwatch).toHaveAttribute("aria-pressed", "true");

    const swatchCountBefore = await page.locator(".color-swatches .color-swatch").count();

    // Hover over the custom swatch to reveal delete button and click it
    await customSwatch.hover();
    const deleteBtn = customSwatch.locator(".color-swatch-delete-btn");
    await expect(deleteBtn).toBeVisible();

    // Delete button renders an inline SVG × icon (not a text glyph)
    await expect(deleteBtn.locator("svg")).toBeVisible();

    await deleteBtn.click();

    // Verify swatch was deleted
    await expect(customSwatch).toHaveCount(0);
    const swatchCountAfter = await page.locator(".color-swatches .color-swatch").count();
    expect(swatchCountAfter).toBe(swatchCountBefore - 1);

    // Verify user is switched back to default color (Mint Green) after deletion
    const mintSwatch = page.locator('[aria-label="Select Mint Green glow"]');
    await expect(mintSwatch).toHaveAttribute("aria-pressed", "true");
  });

  // FIXME(#694): Color picker / glow toggle feature — not yet validated.
  test.fixme("glow toggle exists, defaults to ON, and can be switched off and on", async ({
    page,
  }) => {
    const meetingId = `e2e_glow_toggle_${Date.now()}`;

    await page.goto("/");
    await page.waitForTimeout(1500);

    await page.locator("#meeting-id").click();
    await page.locator("#meeting-id").pressSequentially(meetingId, { delay: 80 });

    await page.locator("#username").click();
    await page.locator("#username").fill("");
    await page.locator("#username").pressSequentially("glow-toggle-user", { delay: 80 });
    await page.waitForTimeout(500);
    await page.locator("#username").press("Enter");

    await expect(page).toHaveURL(new RegExp(`/meeting/${meetingId}`), { timeout: 10_000 });

    const joinButton = page.getByRole("button", { name: /Start Meeting|Join Meeting/ });
    await expect(joinButton).toBeVisible({ timeout: 20_000 });
    await joinButton.click();

    await expect(page.locator("#grid-container")).toBeVisible({ timeout: 15_000 });

    await page.locator('[data-testid="open-settings"]').click();
    await expect(page.locator(".device-settings-modal")).toBeVisible({ timeout: 10_000 });
    await page.getByRole("tab", { name: "Appearance" }).click();

    // The glow toggle section is visible with a "Glow" label
    const glowToggle = page.locator('.glow-switch input[type="checkbox"]');
    await expect(glowToggle).toBeVisible();

    // Default state: ON (checked)
    await expect(glowToggle).toBeChecked();

    // Switch OFF
    await glowToggle.uncheck();
    await expect(glowToggle).not.toBeChecked();

    // Switch back ON
    await glowToggle.check();
    await expect(glowToggle).toBeChecked();
  });

  // FIXME(#694): Color picker / glow toggle feature — not yet validated.
  test.fixme("preview tile loses glow when toggle is OFF and regains it when ON", async ({
    page,
  }) => {
    const meetingId = `e2e_glow_toggle_preview_${Date.now()}`;

    await page.goto("/");
    await page.waitForTimeout(1500);

    await page.locator("#meeting-id").click();
    await page.locator("#meeting-id").pressSequentially(meetingId, { delay: 80 });

    await page.locator("#username").click();
    await page.locator("#username").fill("");
    await page.locator("#username").pressSequentially("glow-preview-user", { delay: 80 });
    await page.waitForTimeout(500);
    await page.locator("#username").press("Enter");

    await expect(page).toHaveURL(new RegExp(`/meeting/${meetingId}`), { timeout: 10_000 });

    const joinButton = page.getByRole("button", { name: /Start Meeting|Join Meeting/ });
    await expect(joinButton).toBeVisible({ timeout: 20_000 });
    await joinButton.click();

    await expect(page.locator("#grid-container")).toBeVisible({ timeout: 15_000 });

    await page.locator('[data-testid="open-settings"]').click();
    await expect(page.locator(".device-settings-modal")).toBeVisible({ timeout: 10_000 });
    await page.getByRole("tab", { name: "Appearance" }).click();

    const glowToggle = page.locator('.glow-switch input[type="checkbox"]');
    const previewTile = page.locator(".appearance-preview-area .preview-tile");
    const panel = page.locator(".appearance-settings-panel");

    // Default ON — preview shows glow with default Mint Green (91, 207, 159)
    await expect(previewTile).toBeVisible();
    await expect(previewTile).toHaveAttribute("style", /rgba\(91, 207, 159/);
    await expect(panel).not.toHaveClass(/glow-disabled/);

    // Toggle OFF — preview shows no glow; panel gets glow-disabled class
    // which fades luminous effects on swatches, sliders, and preview animation
    await glowToggle.uncheck();
    await expect(previewTile).toHaveAttribute("style", /box-shadow:\s*none/);
    await expect(panel).toHaveClass(/glow-disabled/);

    // Toggle ON — glow returns with the same color; glow-disabled removed
    await glowToggle.check();
    await expect(previewTile).toHaveAttribute("style", /rgba\(91, 207, 159/);
    await expect(panel).not.toHaveClass(/glow-disabled/);
  });

  // FIXME(#694): Color picker / glow toggle feature — not yet validated.
  test.fixme("toggling glow off does not clear the selected color", async ({ page }) => {
    const meetingId = `e2e_glow_toggle_color_${Date.now()}`;

    await page.goto("/");
    await page.waitForTimeout(1500);

    await page.locator("#meeting-id").click();
    await page.locator("#meeting-id").pressSequentially(meetingId, { delay: 80 });

    await page.locator("#username").click();
    await page.locator("#username").fill("");
    await page.locator("#username").pressSequentially("glow-color-user", { delay: 80 });
    await page.waitForTimeout(500);
    await page.locator("#username").press("Enter");

    await expect(page).toHaveURL(new RegExp(`/meeting/${meetingId}`), { timeout: 10_000 });

    const joinButton = page.getByRole("button", { name: /Start Meeting|Join Meeting/ });
    await expect(joinButton).toBeVisible({ timeout: 20_000 });
    await joinButton.click();

    await expect(page.locator("#grid-container")).toBeVisible({ timeout: 15_000 });

    await page.locator('[data-testid="open-settings"]').click();
    await expect(page.locator(".device-settings-modal")).toBeVisible({ timeout: 10_000 });
    await page.getByRole("tab", { name: "Appearance" }).click();

    const glowToggle = page.locator('.glow-switch input[type="checkbox"]');
    const cyanSwatch = page.locator('[aria-label="Select Cyan glow"]');

    // Select Cyan
    await cyanSwatch.click();
    await expect(cyanSwatch).toHaveAttribute("aria-pressed", "true");

    // Toggle glow OFF — Cyan should still be selected
    await glowToggle.uncheck();
    await expect(cyanSwatch).toHaveAttribute("aria-pressed", "true");

    // Brightness and inner glow sliders retain their values
    await expect(page.locator(".brightness-section .slider-value")).toHaveText("100%");
    await expect(page.locator(".inner-glow-section .slider-value")).toHaveText("100%");

    // Toggle ON — preview restores Cyan glow (12, 175, 255)
    await glowToggle.check();
    const previewTile = page.locator(".appearance-preview-area .preview-tile");
    await expect(previewTile).toHaveAttribute("style", /rgba\(12, 175, 255/);
  });

  // FIXME(#694): Color picker feature — not yet validated against current DOM.
  test.fixme("custom color popover closes with Escape key and focus returns to add button", async ({
    page,
  }) => {
    const meetingId = `e2e_popover_escape_${Date.now()}`;

    await page.goto("/");
    await page.waitForTimeout(1500);

    await page.locator("#meeting-id").click();
    await page.locator("#meeting-id").pressSequentially(meetingId, { delay: 80 });

    await page.locator("#username").click();
    await page.locator("#username").fill("");
    await page.locator("#username").pressSequentially("escape-user", { delay: 80 });
    await page.waitForTimeout(500);
    await page.locator("#username").press("Enter");

    await expect(page).toHaveURL(new RegExp(`/meeting/${meetingId}`), { timeout: 10_000 });

    const joinButton = page.getByRole("button", { name: /Start Meeting|Join Meeting/ });
    await expect(joinButton).toBeVisible({ timeout: 20_000 });
    await joinButton.click();

    await expect(page.locator("#grid-container")).toBeVisible({ timeout: 15_000 });

    await page.locator('[data-testid="open-settings"]').click();
    await expect(page.locator(".device-settings-modal")).toBeVisible({ timeout: 10_000 });
    await page.getByRole("tab", { name: "Appearance" }).click();

    // Open the custom color popover
    const addBtn = page.locator('[aria-label="Add custom color"]');
    await addBtn.click();

    const popover = page.locator(".custom-color-popover");
    await expect(popover).toBeVisible();

    const colorInput = popover.locator(".custom-color-input");

    // Focus the input and press Escape
    await colorInput.click();
    await colorInput.press("Escape");

    // Popover should be dismissed
    await expect(popover).toHaveCount(0);

    // Focus should return to the add button
    const focusedElementId = await page.evaluate(() => document.activeElement?.id);
    expect(focusedElementId).toBe("add-custom-color-btn");
  });

  // FIXME(#694): Color picker feature — not yet validated against current DOM.
  test.fixme("custom color storage is capped at MAX_CUSTOM_COLORS (10)", async ({ page }) => {
    const meetingId = `e2e_custom_color_cap_${Date.now()}`;

    await page.goto("/");
    await page.waitForTimeout(1500);

    await page.locator("#meeting-id").click();
    await page.locator("#meeting-id").pressSequentially(meetingId, { delay: 80 });

    await page.locator("#username").click();
    await page.locator("#username").fill("");
    await page.locator("#username").pressSequentially("storage-cap-user", { delay: 80 });
    await page.waitForTimeout(500);
    await page.locator("#username").press("Enter");

    await expect(page).toHaveURL(new RegExp(`/meeting/${meetingId}`), { timeout: 10_000 });

    const joinButton = page.getByRole("button", { name: /Start Meeting|Join Meeting/ });
    await expect(joinButton).toBeVisible({ timeout: 20_000 });
    await joinButton.click();

    await expect(page.locator("#grid-container")).toBeVisible({ timeout: 15_000 });

    await page.locator('[data-testid="open-settings"]').click();
    await expect(page.locator(".device-settings-modal")).toBeVisible({ timeout: 10_000 });
    await page.getByRole("tab", { name: "Appearance" }).click();

    // Add custom colors up to the limit
    const colorHexValues = [
      "#FF0000",
      "#FF7700",
      "#FFFF00",
      "#00FF00",
      "#00FFFF",
      "#0077FF",
      "#0000FF",
      "#7700FF",
      "#FF00FF",
      "#FF77FF",
    ];

    const addBtn = page.locator('[aria-label="Add custom color"]');

    // Add up to 10 colors
    for (let i = 0; i < colorHexValues.length; i++) {
      const shouldAddBeVisible = await addBtn.isVisible({ timeout: 1_000 }).catch(() => false);

      if (!shouldAddBeVisible) {
        // If + button is hidden, all slots are full
        break;
      }

      await addBtn.click();
      const popover = page.locator(".custom-color-popover");
      await expect(popover).toBeVisible();

      const colorInput = popover.locator(".custom-color-input");
      const addColorBtn = popover.locator(".custom-color-add-btn");

      await colorInput.fill(colorHexValues[i]);
      await addColorBtn.click();

      // Wait for popover to close
      await expect(popover).toHaveCount(0);

      // Small delay to ensure storage update
      await page.waitForTimeout(300);
    }

    // Verify the add button is now hidden (no more room)
    await expect(addBtn).toHaveCount(0);

    // Verify persisted values by checking localStorage directly
    const storedColors = await page.evaluate(() => {
      const stored = localStorage.getItem("vc_appearance_custom_colors") ?? "";
      return stored
        .split(",")
        .map((hex) => hex.trim())
        .filter((hex) => hex.length > 0);
    });

    expect(storedColors.length).toBeLessThanOrEqual(10);

    // Reload page and verify cap is still enforced
    await page.reload();

    const joinAgain = page.getByRole("button", { name: /Start Meeting|Join Meeting/ });
    if (await joinAgain.isVisible({ timeout: 5_000 }).catch(() => false)) {
      await joinAgain.click();
    }

    await expect(page.locator("#grid-container")).toBeVisible({ timeout: 15_000 });

    await page.locator('[data-testid="open-settings"]').click();
    await expect(page.locator(".device-settings-modal")).toBeVisible({ timeout: 10_000 });
    await page.getByRole("tab", { name: "Appearance" }).click();

    // After reload, verify stored colors are still capped at 10
    const storedColorsAfterReload = await page.evaluate(() => {
      const stored = localStorage.getItem("vc_appearance_custom_colors") ?? "";
      return stored
        .split(",")
        .map((hex) => hex.trim())
        .filter((hex) => hex.length > 0);
    });

    expect(storedColorsAfterReload.length).toBeLessThanOrEqual(10);

    // Verify UI reflects the cap: add button should still be hidden
    await expect(page.locator('[aria-label="Add custom color"]')).toHaveCount(0);
  });
});
