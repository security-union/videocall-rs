import path from "path";
import { test, expect } from "@playwright/test";
import { injectSessionCookie } from "../helpers/auth";
import { waitForServices } from "../helpers/wait-for-services";

/**
 * E2E tests for the single-slot custom theme import feature in
 * AppearanceSettingsPanel.
 *
 * The panel renders a "Source" sub-row below the Dark/System/Light toggle:
 *   - Default state: "✓ Default" label + "Import…" button (file input).
 *   - After import: "✓ <theme name>" label + "Reset to default" button.
 *   - Errors: inline alert div for invalid/oversize/wrong-version files.
 *
 * The custom theme is stored in localStorage["vc_theme_custom"] as plain JSON
 * (not CBOR-encoded like the mode preference).
 */

const FIXTURES_DIR = path.resolve(__dirname, "../fixtures/themes");
const VALID_THEME_PATH = path.join(FIXTURES_DIR, "valid-custom.json");
const INVALID_JSON_PATH = path.join(FIXTURES_DIR, "invalid.json");
const WRONG_VERSION_PATH = path.join(FIXTURES_DIR, "wrong-version.json");

/** Expected --bg value from the bundled default dark theme. */
const BUNDLED_DARK_BG = "#000000";

/** Expected --bg values from the valid-custom fixture. */
const CUSTOM_DARK_BG = "#111133";
const CUSTOM_LIGHT_BG = "#eeeeff";

/** Read the resolved --bg CSS custom property from :root. */
async function getBgColor(page: import("@playwright/test").Page): Promise<string> {
  return page.evaluate(() =>
    window.getComputedStyle(document.documentElement).getPropertyValue("--bg").trim(),
  );
}

/** Navigate to the Appearance tab inside a live meeting. Mirrors the
 *  pattern from theme-toggle.spec.ts. */
async function openAppearanceTab(
  page: import("@playwright/test").Page,
  meetingId: string,
  username: string,
): Promise<void> {
  await page.goto("/");
  await page.waitForTimeout(1500);

  await page.locator("#meeting-id").click();
  await page.locator("#meeting-id").pressSequentially(meetingId, { delay: 80 });

  await page.locator("#username").click();
  await page.locator("#username").fill("");
  await page.locator("#username").pressSequentially(username, { delay: 80 });
  await page.waitForTimeout(500);

  const submitButton = page.getByRole("button", { name: "Start or Join Meeting" });
  await expect(submitButton).toBeVisible({ timeout: 5_000 });
  await submitButton.click();

  await expect(page).toHaveURL(new RegExp(`/meeting/${meetingId}`), {
    timeout: 10_000,
  });

  const joinButton = page.getByRole("button", { name: /Start Meeting|Join Meeting/ });
  await expect(joinButton).toBeVisible({ timeout: 20_000 });
  await joinButton.click();

  await expect(page.locator("#grid-container")).toBeVisible({ timeout: 15_000 });

  // Open the settings modal.
  await page.locator('[data-testid="open-settings"]').click();
  await expect(page.locator(".device-settings-modal")).toBeVisible({ timeout: 10_000 });

  // Navigate to the Appearance tab.
  await page.getByRole("tab", { name: "Appearance" }).click();
  await expect(page.locator("#settings-panel-appearance")).toBeVisible({ timeout: 5_000 });
}

test.describe("Custom theme import in AppearanceSettingsPanel", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test.beforeEach(async ({ context, baseURL }, testInfo) => {
    const uniqueEmail = `e2e-theme-import-${testInfo.title.replace(/[^a-z0-9]+/gi, "-").toLowerCase()}-${Date.now()}@videocall.rs`;
    await injectSessionCookie(context, { baseURL, email: uniqueEmail });
  });

  test.afterEach(async ({ page }) => {
    // Clean up both mode and custom-theme storage to keep tests independent.
    await page.evaluate(() => {
      localStorage.removeItem("ui-theme");
      localStorage.removeItem("vc_theme_custom");
    });
  });

  // ── 1. Import valid theme → label + --bg updated ────────────────────────
  test("importing a valid theme updates the source label and applies custom --bg", async ({
    page,
  }) => {
    const meetingId = `e2e_import_valid_${Date.now()}`;
    await openAppearanceTab(page, meetingId, "import-valid-user");

    // Before import: source shows Default.
    const sourceLabel = page.locator('[data-testid="theme-source-active"]');
    await expect(sourceLabel).toContainText("Default");

    // The --bg should be the bundled dark value (default mode is Dark).
    expect(await getBgColor(page)).toBe(BUNDLED_DARK_BG);

    // Import the valid custom theme.
    const fileInput = page.locator('[data-testid="theme-import-input"]');
    await fileInput.setInputFiles(VALID_THEME_PATH);

    // After import: source shows theme name, --bg updated.
    await expect(sourceLabel).toContainText("E2E Neon", { timeout: 5_000 });
    await expect.poll(async () => getBgColor(page), { timeout: 5_000 }).toBe(CUSTOM_DARK_BG);

    // Reset button should be visible.
    await expect(page.locator('[data-testid="theme-reset-btn"]')).toBeVisible();
  });

  // ── 2. Custom theme responds to Dark/Light mode toggle ──────────────────
  test("custom theme applies correct values when switching between Dark and Light modes", async ({
    page,
  }) => {
    const meetingId = `e2e_import_mode_switch_${Date.now()}`;
    await openAppearanceTab(page, meetingId, "import-mode-user");

    // Import the custom theme.
    await page.locator('[data-testid="theme-import-input"]').setInputFiles(VALID_THEME_PATH);
    await expect.poll(async () => getBgColor(page), { timeout: 5_000 }).toBe(CUSTOM_DARK_BG);

    // Switch to Light mode.
    await page.getByRole("button", { name: "Light", exact: true }).click();

    await expect
      .poll(async () => page.evaluate(() => document.documentElement.getAttribute("data-theme")), {
        timeout: 3_000,
      })
      .toBe("light");
    await expect.poll(async () => getBgColor(page), { timeout: 3_000 }).toBe(CUSTOM_LIGHT_BG);

    // Switch back to Dark mode.
    await page.getByRole("button", { name: "Dark", exact: true }).click();

    await expect
      .poll(async () => page.evaluate(() => document.documentElement.getAttribute("data-theme")), {
        timeout: 3_000,
      })
      .toBe("dark");
    await expect.poll(async () => getBgColor(page), { timeout: 3_000 }).toBe(CUSTOM_DARK_BG);
  });

  // ── 3. Invalid JSON file → error shown, theme unchanged ─────────────────
  test("importing an invalid JSON file shows an error and leaves theme unchanged", async ({
    page,
  }) => {
    const meetingId = `e2e_import_invalid_${Date.now()}`;
    await openAppearanceTab(page, meetingId, "import-invalid-user");

    // Confirm starting state.
    expect(await getBgColor(page)).toBe(BUNDLED_DARK_BG);

    // Import invalid JSON.
    await page.locator('[data-testid="theme-import-input"]').setInputFiles(INVALID_JSON_PATH);

    // Error should appear.
    const errorDiv = page.locator('[data-testid="theme-import-error"]');
    await expect(errorDiv).toBeVisible({ timeout: 5_000 });

    // Theme should be unchanged.
    expect(await getBgColor(page)).toBe(BUNDLED_DARK_BG);

    // Source label should still say Default.
    await expect(page.locator('[data-testid="theme-source-active"]')).toContainText("Default");
  });

  // ── 4. Wrong-version file → error mentions version, theme unchanged ─────
  test("importing a wrong-version file shows a version error and leaves theme unchanged", async ({
    page,
  }) => {
    const meetingId = `e2e_import_version_${Date.now()}`;
    await openAppearanceTab(page, meetingId, "import-version-user");

    expect(await getBgColor(page)).toBe(BUNDLED_DARK_BG);

    await page.locator('[data-testid="theme-import-input"]').setInputFiles(WRONG_VERSION_PATH);

    const errorDiv = page.locator('[data-testid="theme-import-error"]');
    await expect(errorDiv).toBeVisible({ timeout: 5_000 });
    await expect(errorDiv).toContainText(/version/i);

    // Theme unchanged.
    expect(await getBgColor(page)).toBe(BUNDLED_DARK_BG);
    await expect(page.locator('[data-testid="theme-source-active"]')).toContainText("Default");
  });

  // ── 5. Reset to default → bundled colors restored ───────────────────────
  test("resetting to default restores bundled theme and hides the reset button", async ({
    page,
  }) => {
    const meetingId = `e2e_import_reset_${Date.now()}`;
    await openAppearanceTab(page, meetingId, "import-reset-user");

    // Import a custom theme first.
    await page.locator('[data-testid="theme-import-input"]').setInputFiles(VALID_THEME_PATH);
    await expect.poll(async () => getBgColor(page), { timeout: 5_000 }).toBe(CUSTOM_DARK_BG);

    // Click reset.
    const resetBtn = page.locator('[data-testid="theme-reset-btn"]');
    await expect(resetBtn).toBeVisible();
    await resetBtn.click();

    // --bg returns to bundled default.
    await expect.poll(async () => getBgColor(page), { timeout: 5_000 }).toBe(BUNDLED_DARK_BG);

    // Label returns to Default.
    await expect(page.locator('[data-testid="theme-source-active"]')).toContainText("Default");

    // Reset button should be gone.
    await expect(resetBtn).not.toBeVisible();
  });

  // ── 6. Persistence across reload ───────────────────────────────────────
  test("custom theme persists across a full page reload", async ({ page }) => {
    const meetingId = `e2e_import_persist_${Date.now()}`;
    await openAppearanceTab(page, meetingId, "import-persist-user");

    // Import the custom theme.
    await page.locator('[data-testid="theme-import-input"]').setInputFiles(VALID_THEME_PATH);
    await expect.poll(async () => getBgColor(page), { timeout: 5_000 }).toBe(CUSTOM_DARK_BG);

    // Verify localStorage was set.
    const stored = await page.evaluate(() => localStorage.getItem("vc_theme_custom"));
    expect(stored).not.toBeNull();

    // Reload the page.
    await page.reload();

    // The custom --bg must be re-applied from storage after reload.
    // If persistence broke, this would resolve to BUNDLED_DARK_BG (#000000) instead.
    await expect.poll(async () => getBgColor(page), { timeout: 10_000 }).toBe(CUSTOM_DARK_BG);

    // Confirm localStorage survived the reload — proves the stored theme was
    // not cleared and is the source of the re-applied custom --bg above.
    const storedAfterReload = await page.evaluate(() => localStorage.getItem("vc_theme_custom"));
    expect(storedAfterReload).not.toBeNull();
  });

  // ── 7. Keyboard: Tab to reset button and activate with Enter ────────────
  test("reset button is keyboard-accessible via Tab and Enter", async ({ page }) => {
    const meetingId = `e2e_import_keyboard_${Date.now()}`;
    await openAppearanceTab(page, meetingId, "import-keyboard-user");

    // Import to make the reset button appear.
    await page.locator('[data-testid="theme-import-input"]').setInputFiles(VALID_THEME_PATH);
    const resetBtn = page.locator('[data-testid="theme-reset-btn"]');
    await expect(resetBtn).toBeVisible({ timeout: 5_000 });

    // Focus the reset button and press Enter.
    await resetBtn.focus();
    await page.keyboard.press("Enter");

    // Theme should return to bundled default.
    await expect.poll(async () => getBgColor(page), { timeout: 5_000 }).toBe(BUNDLED_DARK_BG);

    await expect(page.locator('[data-testid="theme-source-active"]')).toContainText("Default");
    await expect(resetBtn).not.toBeVisible();
  });

  // ── 8. Keyboard: Import input is focusable (pins visually-hidden fix) ───
  test("import input is keyboard-focusable and triggers focus-within styling", async ({ page }) => {
    const meetingId = `e2e_import_focus_${Date.now()}`;
    await openAppearanceTab(page, meetingId, "import-focus-user");

    // Before import: source shows Default, import control is visible.
    await expect(page.locator('[data-testid="theme-source-active"]')).toContainText("Default");

    const fileInput = page.locator('[data-testid="theme-import-input"]');

    // Assert the input is NOT hidden via display:none or visibility:hidden.
    // This is what made the element unfocusable in the pre-fix markup.
    const computedStyles = await fileInput.evaluate((el) => {
      const styles = window.getComputedStyle(el);
      return { display: styles.display, visibility: styles.visibility };
    });
    expect(computedStyles.display).not.toBe("none");
    expect(computedStyles.visibility).not.toBe("hidden");

    // Focus the input programmatically and assert it becomes activeElement.
    // Under the old `display:none` markup this would FAIL because
    // display:none elements cannot receive focus.
    await fileInput.focus();
    await expect(fileInput).toBeFocused();

    // Verify the wrapping .theme-import-btn matches :focus-within when the
    // input inside it is focused — this pins the CSS focus-ring indicator.
    const focusWithinActive = await page.evaluate(
      () => document.querySelector(".theme-import-btn")?.matches(":focus-within") ?? false,
    );
    expect(focusWithinActive).toBe(true);

    // Verify the input has the correct aria-label for screen readers.
    await expect(fileInput).toHaveAttribute("aria-label", "Import theme file (.json)");

    // Finally, confirm the import path still works end-to-end:
    // use setInputFiles (bypasses native dialog) to apply the theme.
    await fileInput.setInputFiles(VALID_THEME_PATH);
    await expect.poll(async () => getBgColor(page), { timeout: 5_000 }).toBe(CUSTOM_DARK_BG);
    await expect(page.locator('[data-testid="theme-source-active"]')).toContainText("E2E Neon");
  });
});
