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
 * IMPORTANT: `localStorage["ui-theme"]` is written by `apply_and_save_theme()`
 * via `dioxus_sdk_storage::LocalStorage::set`, which CBOR+zlib+hex-encodes the
 * value.  Plain strings written directly via `localStorage.setItem()` are NOT
 * recognised by the CBOR-aware `load_theme_from_storage()` decoder and will
 * always fall back to `Theme::Dark`.  Tests that seed plain strings here are
 * therefore intentionally exercising the fallback / unknown-value path.
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

    const theme = await page.evaluate(() => document.documentElement.getAttribute("data-theme"));
    expect(theme).toBe("dark");

    const darkBgImage = await page.evaluate(
      () => window.getComputedStyle(document.documentElement).backgroundImage,
    );
    expect(darkBgImage).not.toContain("theme_v2.png");
  });

  // 2. Plain non-CBOR string in storage → not decoded by SDK → falls back to "dark".
  //    Verifies the FOUC guard is not accidentally trusting raw strings.
  test("falls back to dark theme when ui-theme is a plain non-CBOR string", async ({ page }) => {
    await page.goto("/");
    await page.evaluate(() => localStorage.setItem("ui-theme", "dark"));
    await page.reload();

    const theme = await page.evaluate(() => document.documentElement.getAttribute("data-theme"));
    expect(theme).toBe("dark");
  });

  // (Test 3 removed: "light" cannot be seeded via a plain localStorage.setItem
  //  because load_theme_from_storage() uses the CBOR-aware SDK decoder and
  //  will not recognise the raw string.  Light persistence is covered end-to-end
  //  in theme-toggle.spec.ts via a real UI toggle interaction.)

  // 4. Empty string stored → falls back to "dark"
  test("falls back to dark theme when ui-theme is an empty string", async ({ page }) => {
    await page.goto("/");
    await page.evaluate(() => localStorage.setItem("ui-theme", ""));
    await page.reload();

    const theme = await page.evaluate(() => document.documentElement.getAttribute("data-theme"));
    expect(theme).toBe("dark");
  });

  // 5. Unknown / non-allowlisted value → falls back to "dark"
  //    Plain strings are not decoded by the CBOR-aware storage reader.
  //    This raw "light" value must therefore still resolve to "dark".
  test("falls back to dark theme when ui-theme is an unknown value", async ({ page }) => {
    await page.goto("/");
    await page.evaluate(() => localStorage.setItem("ui-theme", "light"));
    await page.reload();

    const theme = await page.evaluate(() => document.documentElement.getAttribute("data-theme"));
    expect(theme).toBe("dark");
  });

  test("falls back to dark theme when ui-theme is a whitespace-padded unknown value", async ({
    page,
  }) => {
    await page.goto("/");
    await page.evaluate(() => localStorage.setItem("ui-theme", "  system  "));
    await page.reload();

    const theme = await page.evaluate(() => document.documentElement.getAttribute("data-theme"));
    expect(theme).toBe("dark");
  });
});
