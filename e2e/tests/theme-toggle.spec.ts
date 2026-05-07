import { test, expect } from "@playwright/test";
import { injectSessionCookie } from "../helpers/auth";
import { waitForServices } from "../helpers/wait-for-services";

/**
 * E2E tests for the theme icon buttons in AppearanceSettingsPanel.
 *
 * The panel renders three icon buttons ("Dark", "System", "Light") inside the
 * Appearance section of the device-settings modal.  Clicking a button:
 *   1. Updates the ThemePreferenceCtx signal.
 *   2. Triggers apply_and_save_theme(), which writes localStorage["ui-theme"]
 *      and sets html[data-theme].
 *
 * IMPORTANT: `apply_and_save_theme()` uses `dioxus_sdk_storage::LocalStorage::set`
 * which CBOR+zlib+hex-encodes the stored value.  Do NOT seed or assert
 * localStorage with plain strings like "light" or "dark" — they will not be
 * decoded by the CBOR-aware `load_theme_from_storage()` reader.  Use the UI
 * toggle to write values and assert `html[data-theme]` for correctness.
 *
 * Tests navigate into a live meeting, open the settings modal, switch to the
 * Appearance tab, and then exercise the theme buttons.
 */

/** Navigate to the home page, create a meeting, join it, and open the
 *  Appearance tab in the settings modal.  Returns the page ready for
 *  theme-pill interactions. */
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
  await page.locator("#username").press("Enter");

  await expect(page).toHaveURL(new RegExp(`/meeting/${meetingId}`), {
    timeout: 10_000,
  });

  const joinButton = page.getByText(/Start Meeting|Join Meeting/);
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

type ModalStyleProbe = {
  modalTextColor: string;
  modalBackdropFilter: string;
  activeNavColor: string;
};

type MobileModalStyleProbe = {
  overlayBackgroundColor: string;
  modalBackdropFilter: string;
};

async function readModalStyleProbe(
  page: import("@playwright/test").Page,
): Promise<ModalStyleProbe> {
  return page.evaluate(() => {
    const modal = document.querySelector<HTMLElement>(
      ".device-settings-modal.settings-modal, .settings-modal",
    );
    if (!modal) {
      throw new Error("Settings modal not found for style probe");
    }

    const activeNav = document.querySelector<HTMLElement>(".settings-nav-button.active");
    if (!activeNav) {
      throw new Error("Active settings nav button not found for style probe");
    }

    const modalStyles = window.getComputedStyle(modal);
    const activeNavStyles = window.getComputedStyle(activeNav);

    return {
      modalTextColor: modalStyles.color,
      modalBackdropFilter: modalStyles.backdropFilter,
      activeNavColor: activeNavStyles.color,
    };
  });
}

async function readMobileModalStyleProbe(
  page: import("@playwright/test").Page,
): Promise<MobileModalStyleProbe> {
  return page.evaluate(() => {
    const overlay = document.querySelector<HTMLElement>(".device-settings-modal-overlay");
    if (!overlay) {
      throw new Error("Settings modal overlay not found for mobile style probe");
    }

    const modal = document.querySelector<HTMLElement>(
      ".device-settings-modal.settings-modal, .settings-modal",
    );
    if (!modal) {
      throw new Error("Settings modal not found for mobile style probe");
    }

    const overlayStyles = window.getComputedStyle(overlay);
    const modalStyles = window.getComputedStyle(modal);

    return {
      overlayBackgroundColor: overlayStyles.backgroundColor,
      modalBackdropFilter: modalStyles.backdropFilter,
    };
  });
}

function parseRgb(value: string): { r: number; g: number; b: number } {
  const match = value.match(/rgba?\((\d+),\s*(\d+),\s*(\d+)/i);
  if (!match) {
    throw new Error(`Unsupported color format: ${value}`);
  }

  return {
    r: Number(match[1]),
    g: Number(match[2]),
    b: Number(match[3]),
  };
}

function luminance(value: string): number {
  const { r, g, b } = parseRgb(value);
  return 0.2126 * r + 0.7152 * g + 0.0722 * b;
}

test.describe("Theme toggle icon buttons in AppearanceSettingsPanel", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test.beforeEach(async ({ context, baseURL }) => {
    await injectSessionCookie(context, { baseURL });
  });

  test.afterEach(async ({ page }) => {
    // Clean up persisted theme so tests remain independent.
    await page.evaluate(() => localStorage.removeItem("ui-theme"));
  });

  // ── 1. Default selection ────────────────────────────────────────────────
  test("Dark icon button is active by default when no theme is stored", async ({ page }) => {
    const meetingId = `e2e_theme_default_${Date.now()}`;

    // Ensure no stored preference.
    await page.goto("/");
    await page.evaluate(() => localStorage.removeItem("ui-theme"));

    await openAppearanceTab(page, meetingId, "theme-default-user");

    const darkButton = page.getByRole("button", { name: "Dark" });
    const lightButton = page.getByRole("button", { name: "Light" });
    const systemButton = page.getByRole("button", { name: "System" });

    await expect(darkButton).toBeVisible();
    await expect(lightButton).toBeVisible();
    await expect(systemButton).toBeVisible();

    // The active button has the theme-icon-button--active class.
    const darkClass = await darkButton.getAttribute("class");
    const lightClass = await lightButton.getAttribute("class");

    expect(darkClass).toContain("theme-icon-button--active");
    expect(lightClass).not.toContain("theme-icon-button--active");
  });

  // ── 2. Clicking "Light" pill ────────────────────────────────────────────
  test("clicking Light pill sets html[data-theme]=light and persists to localStorage", async ({
    page,
  }) => {
    const meetingId = `e2e_theme_light_${Date.now()}`;

    await openAppearanceTab(page, meetingId, "theme-light-user");

    await page.getByRole("button", { name: "Light" }).click();

    // html[data-theme] must be updated immediately.
    const theme = await page.evaluate(() => document.documentElement.getAttribute("data-theme"));
    expect(theme).toBe("light");

    const lightBgImage = await page.evaluate(
      () => window.getComputedStyle(document.documentElement).backgroundImage,
    );
    expect(lightBgImage).toContain("theme_v2.png");

    // Only invite window copy should switch to dark text in Light mode.
    const inviteHeadingColor = await page
      .locator("#invite-overlay h4")
      .evaluate((el) => window.getComputedStyle(el).color);
    expect(inviteHeadingColor).toBe("rgb(58, 58, 60)");

    // localStorage must be persisted by apply_and_save_theme.  The value is
    // CBOR+zlib+hex-encoded by the SDK, so we only verify it is non-null.
    const stored = await page.evaluate(() => localStorage.getItem("ui-theme"));
    expect(stored).not.toBeNull();

    // The active button has the theme-icon-button--active class.
    const lightClass = await page.getByRole("button", { name: "Light" }).getAttribute("class");
    const darkClass = await page.getByRole("button", { name: "Dark" }).getAttribute("class");
    expect(lightClass).toContain("theme-icon-button--active");
    expect(darkClass).not.toContain("theme-icon-button--active");

    // Light theme must keep the frosted modal treatment while adapting toward
    // darker foreground text for readability on the lighter surface.
    const lightProbe = await readModalStyleProbe(page);
    expect(lightProbe.modalBackdropFilter).toContain("blur");
    expect(luminance(lightProbe.modalTextColor)).toBeLessThan(90);
    expect(luminance(lightProbe.activeNavColor)).toBeLessThan(90);
  });

  // ── 3. Clicking "Dark" pill after switching to Light ────────────────────
  test("clicking Dark pill sets html[data-theme]=dark and persists to localStorage", async ({
    page,
  }) => {
    const meetingId = `e2e_theme_dark_${Date.now()}`;

    // Do NOT seed localStorage with a plain string — the SDK decoder is
    // CBOR-aware and will not recognise it.  Instead, click Light via the UI
    // first so that apply_and_save_theme writes a proper CBOR-encoded value.
    await openAppearanceTab(page, meetingId, "theme-dark-user");

    await page.getByRole("button", { name: "Light" }).click();
    await expect(async () =>
      expect(await page.evaluate(() => document.documentElement.getAttribute("data-theme"))).toBe(
        "light",
      ),
    ).toPass({ timeout: 3_000 });

    const lightProbeBeforeDarkToggle = await readModalStyleProbe(page);

    // Now toggle back to Dark.
    await page.getByRole("button", { name: "Dark" }).click();

    const theme = await page.evaluate(() => document.documentElement.getAttribute("data-theme"));
    expect(theme).toBe("dark");

    const darkBgImage = await page.evaluate(
      () => window.getComputedStyle(document.documentElement).backgroundImage,
    );
    expect(darkBgImage).not.toContain("theme_v2.png");

    // The stored value is CBOR+zlib+hex-encoded; assert non-null only.
    const stored = await page.evaluate(() => localStorage.getItem("ui-theme"));
    expect(stored).not.toBeNull();

    const darkClass = await page.getByRole("button", { name: "Dark" }).getAttribute("class");
    const lightClass = await page.getByRole("button", { name: "Light" }).getAttribute("class");
    expect(darkClass).toContain("theme-icon-button--active");
    expect(lightClass).not.toContain("theme-icon-button--active");

    // Dark theme should remain dark-adaptive while preserving frosted blur.
    const darkProbe = await readModalStyleProbe(page);
    expect(darkProbe.modalBackdropFilter).toContain("blur");
    expect(luminance(darkProbe.modalTextColor)).toBeGreaterThan(
      luminance(lightProbeBeforeDarkToggle.modalTextColor) + 80,
    );
    expect(luminance(darkProbe.activeNavColor)).toBeGreaterThan(
      luminance(lightProbeBeforeDarkToggle.activeNavColor) + 80,
    );
  });

  // ── 4. Persistence across page reload ───────────────────────────────────
  test("light theme persists across a full page reload", async ({ page }) => {
    const meetingId = `e2e_theme_persist_${Date.now()}`;

    await openAppearanceTab(page, meetingId, "theme-persist-user");

    // Switch to light via the UI so that apply_and_save_theme writes a
    // properly CBOR+zlib+hex-encoded value via the SDK.
    await page.getByRole("button", { name: "Light" }).click();

    // The stored value is CBOR-encoded; assert it is non-null (SDK wrote something).
    const storedBeforeReload = await page.evaluate(() => localStorage.getItem("ui-theme"));
    expect(storedBeforeReload).not.toBeNull();

    // Reload the same meeting page.
    await page.reload();

    // initialize_document_theme() uses load_theme_from_storage() (CBOR-aware)
    // and must re-apply the stored preference.
    const themeAfterReload = await page.evaluate(() =>
      document.documentElement.getAttribute("data-theme"),
    );
    expect(themeAfterReload).toBe("light");

    const lightBgImageAfterReload = await page.evaluate(
      () => window.getComputedStyle(document.documentElement).backgroundImage,
    );
    expect(lightBgImageAfterReload).toContain("theme_v2.png");

    const inviteHeadingColorAfterReload = await page
      .locator("#invite-overlay h4")
      .evaluate((el) => window.getComputedStyle(el).color);
    expect(inviteHeadingColorAfterReload).toBe("rgb(58, 58, 60)");

    // The CBOR-encoded blob is preserved across reload; confirm still non-null.
    const storedAfterReload = await page.evaluate(() => localStorage.getItem("ui-theme"));
    expect(storedAfterReload).not.toBeNull();
  });

  // ── 5. Mobile-only light theme refinements ─────────────────────────────
  test("mobile light theme uses light overlay tint and disables settings blur", async ({
    page,
  }) => {
    const meetingId = `e2e_theme_mobile_light_${Date.now()}`;

    await page.setViewportSize({ width: 375, height: 812 });
    await openAppearanceTab(page, meetingId, "theme-mobile-light-user");

    await page.getByRole("button", { name: "Light" }).click();

    const theme = await page.evaluate(() => document.documentElement.getAttribute("data-theme"));
    expect(theme).toBe("light");

    const mobileProbe = await readMobileModalStyleProbe(page);
    expect(mobileProbe.overlayBackgroundColor).toBe("rgba(238, 244, 252, 0.94)");
    expect(mobileProbe.modalBackdropFilter).toBe("none");
  });

  // ── 6. System theme resolves via prefers-color-scheme ───────────────────
  test("System theme resolves to dark when prefers-color-scheme is dark", async ({ page }) => {
    const meetingId = `e2e_theme_system_${Date.now()}`;

    // Tell the browser to emulate a dark OS preference before navigating.
    await page.emulateMedia({ colorScheme: "dark" });

    await openAppearanceTab(page, meetingId, "theme-system-user");

    // Click the System icon button.
    await page.getByRole("button", { name: "System" }).click();

    // apply_theme_to_dom resolves System via prefers-color-scheme; with a dark
    // emulated preference the resolved value must be "dark".
    await expect(async () =>
      expect(await page.evaluate(() => document.documentElement.getAttribute("data-theme"))).toBe(
        "dark",
      ),
    ).toPass({ timeout: 3_000 });

    // The System button must now be active.
    const systemClass = await page.getByRole("button", { name: "System" }).getAttribute("class");
    expect(systemClass).toContain("theme-icon-button--active");

    // Dark and Light must not be active.
    const darkClass = await page.getByRole("button", { name: "Dark" }).getAttribute("class");
    const lightClass = await page.getByRole("button", { name: "Light" }).getAttribute("class");
    expect(darkClass).not.toContain("theme-icon-button--active");
    expect(lightClass).not.toContain("theme-icon-button--active");

    // The stored value is CBOR+zlib+hex-encoded "system"; assert non-null only.
    const stored = await page.evaluate(() => localStorage.getItem("ui-theme"));
    expect(stored).not.toBeNull();
  });
});
