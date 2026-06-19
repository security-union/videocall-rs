import { test, expect } from "@playwright/test";
import { injectSessionCookie } from "../helpers/auth";
import { waitForServices } from "../helpers/wait-for-services";

/**
 * E2E tests for the startup `html[data-theme]` initialization added in
 * dioxus-ui/src/main.rs (`initialize_document_theme`).
 *
 * The function reads `localStorage["ui-theme"]` on app mount and sets the
 * `data-theme` attribute on `<html>`.  CSS design tokens in global.css are
 * scoped to `html:not([data-theme])`, `html[data-theme="dark"]` and
 * `html[data-theme="light"]`, so the attribute must be applied correctly on
 * every page load for theming to work.
 *
 * IMPORTANT: `apply_and_save_theme()` writes `localStorage["ui-theme"]` as a
 * plain-text string (e.g. "dark", "light", "system").  Tests can seed values
 * directly via `localStorage.setItem()` and assert `html[data-theme]`.
 */
test.describe("Theme initialization from localStorage", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test.beforeEach(async ({ context, baseURL }) => {
    await injectSessionCookie(context, { baseURL });
  });

  test.afterEach(async ({ page }) => {
    await page.evaluate(() => localStorage.removeItem("ui-theme"));
  });

  // 1. No stored preference → default to "dark"
  test("defaults to dark theme when ui-theme is absent from localStorage", async ({ page }) => {
    await page.goto("/");
    await page.reload();

    await expect
      .poll(() => page.evaluate(() => document.documentElement.getAttribute("data-theme")), {
        timeout: 10_000,
      })
      .toBe("dark");

    const darkBgImage = await page.evaluate(
      () => window.getComputedStyle(document.documentElement).backgroundImage,
    );
    expect(darkBgImage).not.toContain("theme_light_v1.png");
  });

  // 2. Plain "dark" string in storage → parsed by FromStr → Theme::Dark.
  test("reads dark theme correctly from plain-text localStorage", async ({ page }) => {
    await page.goto("/");
    await page.evaluate(() => localStorage.setItem("ui-theme", "dark"));
    await page.reload();

    await expect
      .poll(() => page.evaluate(() => document.documentElement.getAttribute("data-theme")), {
        timeout: 10_000,
      })
      .toBe("dark");
  });

  // 3. Plain "light" string in storage → parsed by FromStr → Theme::Light.
  test("reads light theme correctly from plain-text localStorage", async ({ page }) => {
    await page.goto("/");
    await page.evaluate(() => localStorage.setItem("ui-theme", "light"));
    await page.reload();

    await expect
      .poll(() => page.evaluate(() => document.documentElement.getAttribute("data-theme")), {
        timeout: 10_000,
      })
      .toBe("light");
  });

  // 4. Empty string stored → falls back to "dark"
  test("falls back to dark theme when ui-theme is an empty string", async ({ page }) => {
    await page.goto("/");
    await page.evaluate(() => localStorage.setItem("ui-theme", ""));
    await page.reload();

    await expect
      .poll(() => page.evaluate(() => document.documentElement.getAttribute("data-theme")), {
        timeout: 10_000,
      })
      .toBe("dark");
  });

  // 5. Unknown / non-allowlisted value → falls back to "dark" via FromStr
  //    catch-all arm.
  test("falls back to dark theme when ui-theme is an unknown value", async ({ page }) => {
    await page.goto("/");
    await page.evaluate(() => localStorage.setItem("ui-theme", "retro-wave"));
    await page.reload();

    await expect
      .poll(() => page.evaluate(() => document.documentElement.getAttribute("data-theme")), {
        timeout: 10_000,
      })
      .toBe("dark");
  });

  // 6. Whitespace-padded value → FromStr trims → correct theme.
  test("trims whitespace in plain-text ui-theme value", async ({ page }) => {
    await page.goto("/");
    await page.evaluate(() => localStorage.setItem("ui-theme", "  light  "));
    await page.reload();

    await expect
      .poll(() => page.evaluate(() => document.documentElement.getAttribute("data-theme")), {
        timeout: 10_000,
      })
      .toBe("light");
  });

  // 7. System theme resolves based on prefers-color-scheme media query.
  //    In headless Chromium without explicit emulation, the resolved value
  //    is implementation-dependent, so we only assert that data-theme is set.
  test("reads system theme from plain-text localStorage", async ({ page }) => {
    await page.goto("/");
    await page.evaluate(() => localStorage.setItem("ui-theme", "system"));
    await page.reload();

    const theme = await page.evaluate(() => document.documentElement.getAttribute("data-theme"));
    expect(theme).toBeTruthy();
    expect(["light", "dark"]).toContain(theme);
  });
});
