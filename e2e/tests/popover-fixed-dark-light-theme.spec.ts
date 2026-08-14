import { test, expect, Page } from "@playwright/test";
import { injectSessionCookie } from "../helpers/auth";
import { waitForServices } from "../helpers/wait-for-services";

/**
 * E2E regression test for HCL issue 2091: the dock popover glass tokens
 * (`--glass-popover-bg` / `-border` / `-shadow`) had NO light-theme definition.
 *
 * THE BUG: all three were declared only inside `html:not([data-theme]),
 * html[data-theme="dark"]` in global.css, with no counterpart in the
 * `html[data-theme="light"]` block. A `var()` that resolves to nothing is
 * invalid at computed-value time, so under light theme every consumer fell back
 * to its INITIAL value — `background-image: none`, `border-style: none`,
 * `box-shadow: none`. The dock's popovers rendered as unpainted, borderless
 * rectangles floating over the video grid. A comment in global.css
 * simultaneously claimed the token was "dark-navy in both themes", which was
 * false.
 *
 * THE FIX chose the fixed-dark reading of that comment and made it true: the
 * three tokens moved to the theme-INDEPENDENT `:root` block, beside the
 * `--fixed-dark` / `--on-dark-*` set, so the surface is dark-navy glass in both
 * themes — matching the controls dock these popovers hang off, which is itself
 * hardcoded dark in both themes (`.video-controls-container` background
 * rgba(28,28,30,0.85), `.video-control-button` color white, no light override).
 * Every child that used a theme-flipping token was re-based onto the
 * `--on-dark-*` / `--glass-popover-item-*` set.
 *
 * WHAT EACH ASSERTION PINS (mutation honesty):
 *
 *   1. Token identity across themes. Read off `document.documentElement` under
 *      the REAL Dark and Light toggles. Reverting the hoist makes the light
 *      read an EMPTY STRING while the dark read is the gradient — the equality
 *      assertion fails, and so does the non-empty assertion.
 *
 *   2. The tokens actually land on a rendered popover. Reading the custom
 *      property alone would not prove the cascade reaches the consumer, so we
 *      also open a real popover in light theme and assert the resolved
 *      `background-image` / `border-top-style` / `box-shadow`. On the un-fixed
 *      code these are exactly `none` / `none` / `none`.
 *
 *   3. Child legibility on that surface — the half of the change the surface
 *      assertions cannot see. The density options and the dock menu's
 *      `.glass-select-option` resolved to `--text-secondary` (`rgb(99,99,102)`,
 *      luminance ~99) or `--text-primary` (`rgb(26,26,26)`, luminance 26) under
 *      light theme, both far below the 180 floor.
 *
 * MUTATION RECEIPTS (run against the local docker stack, which was verified to
 * be serving this worktree's CSS):
 *   - Both tests green on the fixed code.
 *   - Reverting BOTH stylesheets to the base commit fails test 1 at the
 *     token-identity assertion and test 2 at the density surface guard.
 *   - Restoring the tokens but leaving the child rules reverted fails test 2
 *     at the density colour assertion with a measured luminance of 26 — so
 *     that assertion is load-bearing, not incidental.
 *   - The dock `.glass-select-option` assertion was NOT separately mutated (a
 *     parallel agent claimed the shared stack mid-run). It fails by the same
 *     mechanism as the density one: without the override in global.css that
 *     option resolves to the shared `--text-primary`, which is rgb(26,26,26)
 *     under light theme.
 *
 * The theme is driven through the real Appearance toggle rather than a
 * `setAttribute`, mirroring the @bvt1 peer-toast-light-contrast.spec.ts idiom,
 * so the assertions run against the same code path a user exercises.
 */

// White text lands at 255, the reverted near-black tokens at ~26-99. A floor of
// 180 sits clear of both clusters.
const LIGHT_LUMINANCE_FLOOR = 180;

/** Rec.709 relative luminance of a computed `color`. Alpha is ignored on
 *  purpose: `getComputedStyle().color` is not composited against the
 *  background, so the raw channels are exactly the white-vs-near-black values
 *  we want to discriminate (`rgba(255,255,255,0.7)` parses to 255). */
function luminance(value: string): number {
  const match = value.match(/rgba?\((\d+),\s*(\d+),\s*(\d+)/i);
  if (!match) {
    throw new Error(`Unsupported color format: ${value}`);
  }
  return 0.2126 * Number(match[1]) + 0.7152 * Number(match[2]) + 0.0722 * Number(match[3]);
}

const POPOVER_TOKENS = [
  "--glass-popover-bg",
  "--glass-popover-border",
  "--glass-popover-shadow",
] as const;

async function readPopoverTokens(page: Page): Promise<Record<string, string>> {
  return page.evaluate((names: readonly string[]) => {
    const style = window.getComputedStyle(document.documentElement);
    const out: Record<string, string> = {};
    for (const name of names) {
      out[name] = style.getPropertyValue(name).trim();
    }
    return out;
  }, POPOVER_TOKENS);
}

test.describe("Dock popover surface is fixed-dark in both themes (issue 2091)", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test.beforeEach(async ({ context, baseURL }) => {
    await injectSessionCookie(context, { baseURL });
  });

  /** Navigate home, create+join a meeting, and wait for the call grid. */
  async function joinMeeting(page: Page, testLabel: string): Promise<void> {
    const safeLabel = testLabel.replace(/-/g, "_");
    const meetingId = `popover_2091_${safeLabel}_${Date.now()}`;

    await page.goto("/");
    await page.waitForTimeout(1500);

    await page.locator("#meeting-id").click();
    await page.locator("#meeting-id").pressSequentially(meetingId, { delay: 80 });

    await page.locator("#username").click();
    await page.locator("#username").fill("");
    await page.locator("#username").pressSequentially("popover-user", { delay: 80 });
    await page.waitForTimeout(500);
    await page.locator("#username").press("Enter");

    await expect(page).toHaveURL(new RegExp(`/meeting/${meetingId}`), { timeout: 10_000 });

    const joinButton = page.getByText(/Start Meeting|Join Meeting/);
    const grid = page.locator("#grid-container");
    const which = await Promise.race([
      joinButton.waitFor({ timeout: 20_000 }).then(() => "join" as const),
      grid.waitFor({ timeout: 20_000 }).then(() => "grid" as const),
    ]);
    if (which === "join") {
      if ((await joinButton.count()) > 0 && (await joinButton.first().isVisible())) {
        await joinButton.click().catch(() => {
          // Swallow click-after-detach: the auto-join effect has already
          // transitioned past NotJoined and unmounted the button.
        });
      }
    }
    await expect(grid).toBeVisible({ timeout: 15_000 });
  }

  /**
   * Reveal the action bar. `.video-controls-container` carries a 0.6s opacity
   * transition (global.css), and autohide can have it mid-fade when we arrive.
   * Playwright's actionability check treats an animating element as unstable,
   * so a click issued the instant after `hover()` can burn its whole timeout
   * waiting for the fade — observed twice as a `toBeVisible` timeout on the
   * settings modal. Settling past the transition first makes the subsequent
   * click deterministic.
   */
  async function hoverActionBar(page: Page): Promise<void> {
    const bar = page.locator(".video-controls-container");
    await bar.hover();
    await page.waitForTimeout(700);
  }

  /** Open device settings on the Appearance tab. Leaves the modal OPEN. */
  async function openAppearancePanel(page: Page): Promise<void> {
    await hoverActionBar(page);
    const settingsBtn = page.locator('[data-testid="open-settings"]');
    await expect(settingsBtn).toBeVisible({ timeout: 10_000 });
    await settingsBtn.click();
    await expect(page.locator(".device-settings-modal")).toBeVisible({ timeout: 10_000 });
    await page.getByRole("tab", { name: "Appearance" }).click();
    await expect(page.locator("#settings-panel-appearance")).toBeVisible({ timeout: 5_000 });
  }

  /** Click a theme button in the already-open Appearance panel and wait for
   *  `data-theme` to settle. The toggle sets the attribute synchronously; the
   *  poll is defensive. */
  async function selectTheme(page: Page, label: "Dark" | "Light"): Promise<void> {
    await page
      .locator("#settings-panel-appearance")
      .getByRole("button", { name: label, exact: true })
      .click();
    await expect
      .poll(() => page.evaluate(() => document.documentElement.getAttribute("data-theme")), {
        timeout: 3_000,
      })
      .toBe(label.toLowerCase());
  }

  async function closeSettings(page: Page): Promise<void> {
    await page.locator('button[aria-label="Close settings"]').click();
    await expect(page.locator(".device-settings-modal")).not.toBeVisible({ timeout: 5_000 });
  }

  // -- The token contract, plus proof it reaches a rendered popover ---------

  test("popover glass tokens are identical in dark and light theme, and paint the overflow popover @bvt1", async ({
    page,
  }) => {
    test.setTimeout(120_000);
    await joinMeeting(page, "token_identity");

    // Read the three tokens under BOTH real themes without closing the modal
    // in between — the values live on <html>, which the modal never covers.
    await openAppearancePanel(page);
    await selectTheme(page, "Dark");
    const darkTokens = await readPopoverTokens(page);
    await selectTheme(page, "Light");
    const lightTokens = await readPopoverTokens(page);
    await closeSettings(page);

    // Sanity: the tokens exist at all in dark theme (they always did).
    for (const name of POPOVER_TOKENS) {
      expect(darkTokens[name], `${name} should be defined in dark theme`).not.toBe("");
    }

    // THE CONTRACT. Before the fix the light read was "" for all three.
    for (const name of POPOVER_TOKENS) {
      expect(lightTokens[name], `${name} must be defined in light theme`).not.toBe("");
      expect(lightTokens[name], `${name} must not flip between themes`).toBe(darkTokens[name]);
    }

    // Now prove the cascade actually delivers them to a consumer. The overflow
    // popover sets all three, so it fails on every axis when they are unset.
    expect(await page.evaluate(() => document.documentElement.getAttribute("data-theme"))).toBe(
      "light",
    );
    await page.setViewportSize({ width: 400, height: 720 });
    // Let the rAF-throttled overflow recompute settle before reaching for the
    // trigger it creates.
    await page.waitForTimeout(300);
    await hoverActionBar(page);
    await page.locator("#overflow-menu-trigger").click();

    const popover = page.locator(".action-bar-overflow-popover");
    await expect(popover).toBeVisible({ timeout: 5_000 });

    const surface = await popover.evaluate((el) => {
      const s = window.getComputedStyle(el);
      return {
        backgroundImage: s.backgroundImage,
        borderTopStyle: s.borderTopStyle,
        boxShadow: s.boxShadow,
      };
    });

    // Un-fixed light theme yields exactly these initial values.
    expect(surface.backgroundImage).not.toBe("none");
    expect(surface.borderTopStyle).toBe("solid");
    expect(surface.boxShadow).not.toBe("none");

    // The overflow items were already on --on-dark-text; assert they stayed
    // light so the fixed-dark reading of the surface remains self-consistent.
    const itemColor = await popover
      .locator(".overflow-item")
      .first()
      .evaluate((el) => window.getComputedStyle(el).color);
    expect(luminance(itemColor)).toBeGreaterThan(LIGHT_LUMINANCE_FLOOR);
  });

  // -- Children re-based onto the --on-dark-* set --------------------------

  test("density and dock-menu options stay light-on-dark in light theme @bvt1", async ({
    page,
  }) => {
    test.setTimeout(120_000);
    await joinMeeting(page, "child_contrast");

    await openAppearancePanel(page);
    await selectTheme(page, "Light");
    await closeSettings(page);

    // Density popover. The FIRST option is the committed mode, so it is styled
    // by `.density-option[aria-checked="true"]`, which used --text-primary =
    // rgb(26,26,26) under light theme (measured luminance 26). Its unchecked
    // siblings used the base --text-secondary = rgb(99,99,102) (~99). Both are
    // invisible on the dark-navy glass and both fail the 180 floor.
    await hoverActionBar(page);
    await page.locator("#density-mode-trigger").click();
    const density = page.locator(".density-popover");
    await expect(density).toBeVisible({ timeout: 5_000 });

    // Guard the assertion below: it is only meaningful if the surface really
    // is painted dark here.
    expect(await density.evaluate((el) => window.getComputedStyle(el).backgroundImage)).not.toBe(
      "none",
    );

    const densityColor = await density
      .locator(".density-option")
      .first()
      .evaluate((el) => window.getComputedStyle(el).color);
    expect(luminance(densityColor)).toBeGreaterThan(LIGHT_LUMINANCE_FLOOR);

    // Opening the dock menu closes the density popover (mutual exclusivity).
    await hoverActionBar(page);
    // Stable id, not `aria-haspopup` — issue 1762 re-roled the popup from
    // listbox to menu, so the trigger now advertises `aria-haspopup="menu"`
    // and the old `[aria-haspopup="listbox"]` selector matches nothing. The
    // haspopup VALUE is now the subject of a dedicated assertion in
    // `action-bar-customize.spec.ts` rather than a navigation handle, which is
    // the convention the other dock specs already follow.
    await page.locator("#dock-menu-trigger").click();
    const dockMenu = page.locator(".dock-position-wrapper .glass-select-menu");
    await expect(dockMenu).toBeVisible({ timeout: 5_000 });

    expect(await dockMenu.evaluate((el) => window.getComputedStyle(el).backgroundImage)).not.toBe(
      "none",
    );

    // `.glass-select-option` is shared with the settings modal's light select,
    // where --text-primary is correct. This instance overrides it to
    // --on-dark-text because the dock swaps in the fixed-dark surface; without
    // that override it is rgb(26,26,26) here.
    const optionColor = await dockMenu
      .locator(".glass-select-option")
      .first()
      .evaluate((el) => window.getComputedStyle(el).color);
    expect(luminance(optionColor)).toBeGreaterThan(LIGHT_LUMINANCE_FLOOR);

    // ── Keyboard focus indicator (issue 2091 stage-B, UX finding F1). ──
    // The shared `.glass-select-option:focus-visible` sets `outline: none` and
    // draws a 1px --glass-border hairline instead — 1.57:1 dark / 1.27:1 light
    // against its own fill, failing SC 1.4.11 in both. The dock instance now
    // restores the app's standard --accent ring on the bare fixed-dark surface
    // (3.47:1). ArrowDown is the real keyboard entry point: the trigger's
    // handler calls `focus_glass_option_at(".dock-position-wrapper", false)`
    // (attendants.rs), and driving it from the keyboard is what makes Chrome
    // match `:focus-visible` at all.
    // MUTATION HONESTY: on the un-fixed rule the computed outline is
    // `none` / `0px`, so both assertions below fail.
    await page.keyboard.press("ArrowDown");
    const focusRing = await page.evaluate(() => {
      const el = document.activeElement as HTMLElement | null;
      if (!el) return null;
      const s = window.getComputedStyle(el);
      return {
        className: el.className,
        outlineStyle: s.outlineStyle,
        outlineWidth: s.outlineWidth,
        outlineColor: s.outlineColor,
      };
    });
    expect(focusRing).not.toBeNull();
    expect(focusRing?.className).toContain("glass-select-option");
    expect(focusRing?.outlineStyle).toBe("solid");
    expect(parseFloat(focusRing?.outlineWidth ?? "0")).toBeGreaterThanOrEqual(2);
    // --accent is #007aff in BOTH themes, so this value is theme-independent.
    expect(focusRing?.outlineColor).toBe("rgb(0, 122, 255)");
  });
});
