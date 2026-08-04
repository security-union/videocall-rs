import { test, expect } from "@playwright/test";
import { injectSessionCookie } from "../helpers/auth";
import { waitForServices } from "../helpers/wait-for-services";

async function ensureInCall(page: import("@playwright/test").Page): Promise<void> {
  const grid = page.locator("#grid-container");
  if (await grid.isVisible().catch(() => false)) {
    return;
  }

  const joinButton = page.getByRole("button", { name: /Start Meeting|Join Meeting/ });
  if (await joinButton.isVisible().catch(() => false)) {
    await joinButton.click();
  }

  await expect(grid).toBeVisible({ timeout: 15_000 });
}

async function openDeviceSettingsModal(page: import("@playwright/test").Page): Promise<void> {
  const modal = page.locator(".device-settings-modal");
  if (await modal.isVisible().catch(() => false)) {
    return;
  }

  await ensureInCall(page);

  // Action bar can auto-hide and settings can move into the overflow menu.
  await page.locator(".video-controls-container").hover();

  const settingsButton = page.locator('[data-testid="open-settings"]');
  if (
    (await settingsButton.count()) > 0 &&
    (await settingsButton
      .first()
      .isVisible()
      .catch(() => false))
  ) {
    await settingsButton.first().click();
  } else {
    const overflowTrigger = page.locator("#overflow-menu-trigger");
    await expect(overflowTrigger).toBeVisible({ timeout: 5_000 });
    await overflowTrigger.click();
    const overflowSettings = page.locator(".action-bar-overflow-popover .overflow-item", {
      hasText: /Device settings/i,
    });
    await expect(overflowSettings).toBeVisible({ timeout: 5_000 });
    await overflowSettings.click();
  }

  await expect(modal).toBeVisible({ timeout: 10_000 });
}

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

  await expect(page).toHaveURL(new RegExp(`/meeting/${meetingId}`), { timeout: 10_000 });

  const joinButton = page.getByRole("button", { name: /Start Meeting|Join Meeting/ });
  await expect(joinButton).toBeVisible({ timeout: 20_000 });
  await joinButton.click();

  await expect(page.locator("#grid-container")).toBeVisible({ timeout: 15_000 });

  await openDeviceSettingsModal(page);
  await page.getByRole("tab", { name: "Appearance" }).click();
  await expect(page.locator("#settings-panel-appearance")).toBeVisible({ timeout: 5_000 });
}

/**
 * The Decay explanation, pinned here independently of the Rust source
 * (`DECAY_HELP_TEXT` in `dioxus-ui/src/components/appearance_settings_panel.rs`).
 * Changing the copy on one side without the other turns this spec red.
 */
const DECAY_HELP_TEXT =
  "Decay controls how long the glow lingers after speech. 0% is instant on/off; 100% is the longest lingering tail.";

/** `rgb()` / `rgba()` (the only forms `getComputedStyle` returns) → channels. */
function parseCssColor(value: string): { r: number; g: number; b: number; a: number } {
  const match = value.match(/rgba?\(([^)]+)\)/i);
  if (!match) {
    throw new Error(`Unsupported color format: ${value}`);
  }
  const parts = match[1].split(",").map((p) => Number(p.trim()));
  return { r: parts[0], g: parts[1], b: parts[2], a: parts.length > 3 ? parts[3] : 1 };
}

/**
 * Alpha-composite a color over an opaque backdrop.
 *
 * `getComputedStyle` reports the *declared* color, not the rendered pixel: a
 * translucent value has to be flattened before a contrast ratio means anything.
 */
function compositeOver(
  value: string,
  backdrop: { r: number; g: number; b: number },
): { r: number; g: number; b: number } {
  const { r, g, b, a } = parseCssColor(value);
  return {
    r: r * a + backdrop.r * (1 - a),
    g: g * a + backdrop.g * (1 - a),
    b: b * a + backdrop.b * (1 - a),
  };
}

const WHITE = { r: 255, g: 255, b: 255 };

/**
 * Flatten over opaque white — the lightest backdrop a light-theme surface can
 * present. Used for the tooltip BUBBLE, whose 95%-opaque dark fill is barely
 * moved by what sits behind it; white is the worst case for the light text that
 * then goes on top, so the resulting ratio is a floor rather than a flattery.
 */
function compositeOverWhite(value: string): { r: number; g: number; b: number } {
  return compositeOver(value, WHITE);
}

/**
 * Flatten every `background-color` from `selector` up to `<html>` into an
 * approximation of the opaque color the element is painted on, evaluated in the
 * page. The settings surfaces are stacked translucent glass, so no single
 * ancestor's `backgroundColor` is the answer.
 *
 * APPROXIMATION, and knowingly so: this walks `background-color` only. The
 * settings panel also paints a `background-image` gradient and a
 * `backdrop-filter`, neither of which is accounted for, so the value returned is
 * DARKER than the true composited surface (measured rgb(10.6, 10.6, 11.4) here
 * against a true panel nearer rgb(42, 42, 44)). For light text that error is
 * generous, so a passing ratio is not by itself proof of the real-world margin —
 * it is a regression tripwire. It is sound in that role because the pre-fix
 * `--text-quaternary` glyph fails it (3.29 < 4.5) on the same approximation, and
 * an independent hand-computation against rgb(42, 42, 44) puts the pre-fix glyph
 * at 2.38:1 and the fixed one comfortably above 4.5.
 */
async function effectiveBackdrop(
  page: import("@playwright/test").Page,
  selector: string,
): Promise<string> {
  return page.evaluate((sel) => {
    const parse = (v: string) => {
      const m = v.match(/rgba?\(([^)]+)\)/i);
      if (!m) return null;
      const p = m[1].split(",").map((x) => Number(x.trim()));
      return { r: p[0], g: p[1], b: p[2], a: p.length > 3 ? p[3] : 1 };
    };
    const el = document.querySelector(sel);
    if (!el) throw new Error(`effectiveBackdrop: no element for ${sel}`);

    const layers: { r: number; g: number; b: number; a: number }[] = [];
    let node: Element | null = el;
    while (node) {
      const c = parse(getComputedStyle(node).backgroundColor);
      if (c && c.a > 0) layers.push(c);
      node = node.parentElement;
    }
    // Composite far-to-near. The seed is opaque black; any fully opaque layer
    // in the stack short-circuits it, and the app's root surface is dark.
    let acc = { r: 0, g: 0, b: 0 };
    for (let i = layers.length - 1; i >= 0; i--) {
      const l = layers[i];
      acc = {
        r: l.r * l.a + acc.r * (1 - l.a),
        g: l.g * l.a + acc.g * (1 - l.a),
        b: l.b * l.a + acc.b * (1 - l.a),
      };
    }
    return `rgb(${acc.r}, ${acc.g}, ${acc.b})`;
  }, selector);
}

/** WCAG 2.1 relative luminance (sRGB linearisation, not a Rec.709 shortcut). */
function relativeLuminance({ r, g, b }: { r: number; g: number; b: number }): number {
  const channel = (c: number) => {
    const s = c / 255;
    return s <= 0.03928 ? s / 12.92 : Math.pow((s + 0.055) / 1.055, 2.4);
  };
  return 0.2126 * channel(r) + 0.7152 * channel(g) + 0.0722 * channel(b);
}

/** WCAG 2.1 contrast ratio between two opaque colors. */
function contrastRatio(
  fg: { r: number; g: number; b: number },
  bg: { r: number; g: number; b: number },
): number {
  const l1 = relativeLuminance(fg);
  const l2 = relativeLuminance(bg);
  return (Math.max(l1, l2) + 0.05) / (Math.min(l1, l2) + 0.05);
}

/**
 * Read an element's role, accessible name and accessible description straight
 * out of the browser's accessibility tree (Chromium CDP — every project in
 * playwright.config.ts runs Desktop Chrome).
 *
 * This is what a screen reader actually consumes. A DOM-level check of the
 * `aria-describedby` attribute cannot tell you whether the reference resolves,
 * and cannot tell you whether a `visibility: hidden` target still contributes
 * its text — both of which are exactly what issue 1871 turns on.
 */
async function axInfo(
  page: import("@playwright/test").Page,
  selector: string,
): Promise<{ role?: string; name?: string; description?: string; focusable?: boolean }> {
  const client = await page.context().newCDPSession(page);
  try {
    await client.send("Accessibility.enable");
    await client.send("DOM.enable");
    const { root } = await client.send("DOM.getDocument", { depth: -1 });
    const { nodeId } = await client.send("DOM.querySelector", { nodeId: root.nodeId, selector });
    const { nodes } = await client.send("Accessibility.getPartialAXTree", {
      nodeId,
      fetchRelatives: false,
    });
    const node = nodes.find((n) => !n.ignored);
    return {
      role: node?.role?.value as string | undefined,
      name: node?.name?.value as string | undefined,
      description: node?.description?.value as string | undefined,
      focusable: node?.properties?.find((p) => p.name === "focusable")?.value?.value as
        | boolean
        | undefined,
    };
  } finally {
    await client.detach();
  }
}

function speakerHighlightRow(page: import("@playwright/test").Page, label: string) {
  return page.locator(".speaker-highlight-controls .appearance-slider-row").filter({
    has: page.getByText(label, { exact: true }),
  });
}

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

  test("Decay slider updates the speaker glow persistence setting", async ({ page }) => {
    const meetingId = `e2e_settings_decay_${Date.now()}`;

    await openAppearanceTab(page, meetingId, "decay-user");

    const decaySlider = page.locator('[data-testid="speaker-highlight-decay-slider"]');
    const decayValue = decaySlider.locator(
      "xpath=ancestor::div[contains(@class, 'appearance-slider-row')]//span[contains(@class, 'appearance-slider-value')]",
    );
    const decayHelpIcon = page.locator('[data-testid="speaker-highlight-decay-help"]');
    const decayHelpText = page.locator('[data-testid="speaker-highlight-decay-help-text"]');

    await expect(decaySlider).toHaveValue("50");
    await expect(decayValue).toHaveText("50%");
    await expect(decayHelpIcon).toBeVisible();

    // ── issue 1871: the Decay help affordance is exposed to assistive tech,
    //    keyboard-reachable, revealed by focus, and Escape-dismissible. ──

    // Discoverable before it is operable: a correct AX tree behind a glyph a
    // low-vision user cannot locate is not accessible. The shared
    // `.settings-info-icon` base paints `--text-quaternary`, which measures
    // ~2.4:1 on the composited dark settings surface — authored for a purely
    // decorative hover hint. Making this instance a focusable control attaches
    // SC 1.4.11 (3:1), and SC 1.4.3 (4.5:1) applies regardless because "(?)" is
    // literal text, not an icon. Revert `.speaker-highlight-help-icon`'s colour
    // override and this fails.
    const glyphColor = await decayHelpIcon.evaluate((el) => getComputedStyle(el).color);
    const glyphBackdrop = await effectiveBackdrop(
      page,
      '[data-testid="speaker-highlight-decay-help"]',
    );
    const glyphRatio = contrastRatio(
      compositeOver(glyphColor, parseCssColor(glyphBackdrop)),
      parseCssColor(glyphBackdrop),
    );
    expect(glyphRatio, `glyph ${glyphColor} on ${glyphBackdrop}`).toBeGreaterThanOrEqual(4.5);

    // Exposed as a control rather than an anonymous decorative span.
    await expect(decayHelpIcon).toHaveAttribute("role", "button");
    await expect(decayHelpIcon).toHaveAttribute("aria-label", "About the Decay setting");

    // The explanation is real DOM text (not CSS `content`), so it exists in the
    // document and is merely hidden until revealed.
    await expect(decayHelpText).toHaveCount(1);
    await expect(decayHelpText).toHaveText(DECAY_HELP_TEXT);
    await expect(decayHelpText).toBeHidden();

    // The description association actually RESOLVES — asserting the
    // `aria-describedby` attribute alone would still pass on a dangling id.
    const describedText = await decayHelpIcon.evaluate((el) => {
      const id = el.getAttribute("aria-describedby");
      if (!id) return null;
      return document.getElementById(id)?.textContent ?? null;
    });
    expect(describedText).toBe(DECAY_HELP_TEXT);

    // ...and it reaches the accessibility tree, which is what a screen reader
    // reads. This holds while the bubble is still `visibility: hidden`, because
    // `aria-describedby` targets contribute to the description even unrendered.
    // Drop `aria-describedby` and this description goes empty; drop `tabindex`
    // and `focusable` goes false; drop `role` and it is no longer a button.
    const ax = await axInfo(page, '[data-testid="speaker-highlight-decay-help"]');
    expect(ax.role).toBe("button");
    expect(ax.focusable).toBe(true);
    expect(ax.name).toBe("About the Decay setting");
    expect(ax.description).toBe(DECAY_HELP_TEXT);

    // Keyboard-reachable: Shift+Tab off the Decay slider lands on the trigger.
    // Without `tabindex` the browser skips straight past it.
    await decaySlider.focus();
    await page.keyboard.press("Shift+Tab");
    await expect(decayHelpIcon).toBeFocused();

    // Focus alone reveals the explanation — WCAG 2.1 SC 1.4.13. `toBeVisible()`
    // ignores opacity, so the computed opacity is what proves the reveal.
    await expect(decayHelpText).toBeVisible();
    await expect
      .poll(async () => decayHelpText.evaluate((el) => getComputedStyle(el).opacity), {
        timeout: 3_000,
      })
      .toBe("1");

    // The tap/click latch is real state driven from Rust. Remove the `onclick`
    // handler and `--open` never appears.
    await decayHelpIcon.click();
    await expect(decayHelpIcon).toHaveClass(/speaker-highlight-help-icon--open/);

    // ...and tapping again actually DISMISSES it. This is the assertion that
    // matters: clearing `--open` alone is not a dismissal, because the trigger
    // still holds focus after the click and `:focus-within` keeps the bubble on
    // screen. Asserting only the absence of `--open` would pass on that broken
    // behaviour. On touch there is no hover and no Escape key, so this re-tap is
    // the ONLY dismissal that does not move focus, which is exactly what WCAG
    // 2.1 SC 1.4.13 "Dismissible" requires.
    await decayHelpIcon.click();
    await expect(decayHelpIcon).not.toHaveClass(/speaker-highlight-help-icon--open/);
    await expect(decayHelpIcon).toHaveClass(/speaker-highlight-help-icon--suppressed/);
    await expect(decayHelpIcon).toBeFocused();
    await expect(decayHelpText).toBeHidden();
    // `toBeHidden()` is satisfied by `visibility: hidden` alone; poll the pair
    // so a bubble left at full opacity cannot slip through. Polled rather than
    // snapshotted because the hide is a 0.15s fade.
    await expect
      .poll(
        async () =>
          decayHelpText.evaluate((el) => {
            const s = getComputedStyle(el);
            return `${s.opacity}/${s.visibility}`;
          }),
        { timeout: 3_000 },
      )
      .toBe("0/hidden");

    // Enter and Space drive the same latch through the keydown handler.
    // Remove the `is_keyboard_activation_key` branch and both of these stall.
    await decayHelpIcon.press("Enter");
    await expect(decayHelpIcon).toHaveClass(/speaker-highlight-help-icon--open/);
    await decayHelpIcon.press("Enter");
    await expect(decayHelpIcon).not.toHaveClass(/speaker-highlight-help-icon--open/);
    await decayHelpIcon.press("Space");
    await expect(decayHelpIcon).toHaveClass(/speaker-highlight-help-icon--open/);

    // Escape dismisses the explanation WITHOUT moving focus and WITHOUT
    // closing the settings modal. The modal's own Escape handler
    // (device_settings_modal.rs) closes it on any Escape that reaches it, so
    // this only holds because the trigger calls `stop_propagation` first.
    await page.keyboard.press("Escape");
    await expect(decayHelpText).toBeHidden();
    await expect(decayHelpIcon).toHaveClass(/speaker-highlight-help-icon--suppressed/);
    await expect(decayHelpIcon).toBeFocused();
    await expect(page.locator("#device-settings-dialog")).toBeVisible();

    // Escape-suppression is per-visit, not permanent: leaving the trigger must
    // re-arm it (`onfocusout`), so coming back reveals the explanation again.
    // Drop the `onfocusout` reset and the affordance stays dead for the rest of
    // the session.
    await decaySlider.focus();
    await expect(decayHelpIcon).not.toHaveClass(/speaker-highlight-help-icon--suppressed/);
    await page.keyboard.press("Shift+Tab");
    await expect(decayHelpIcon).toBeFocused();
    await expect(decayHelpText).toBeVisible();

    // Leave the trigger before continuing so its latch/suppression state cannot
    // bleed into the slider assertions below.
    await decaySlider.focus();

    await decaySlider.fill("20");
    await expect(decayValue).toHaveText("20%");

    await page.waitForTimeout(500);

    await expect
      .poll(async () => page.evaluate(() => localStorage.getItem("vc_appearance_glow_decay")), {
        timeout: 5_000,
      })
      .toBe("0.2");

    await page.reload();
    await page.waitForTimeout(1500);

    await openDeviceSettingsModal(page);
    await page.getByRole("tab", { name: "Appearance" }).click();

    const decaySliderAfterReload = page.locator('[data-testid="speaker-highlight-decay-slider"]');
    const decayValueAfterReload = decaySliderAfterReload.locator(
      "xpath=ancestor::div[contains(@class, 'appearance-slider-row')]//span[contains(@class, 'appearance-slider-value')]",
    );
    await expect(decaySliderAfterReload).toHaveValue("20");
    await expect(decayValueAfterReload).toHaveText("20%");
  });

  test("Decay help tooltip stays inside the settings panel on a 320px viewport", async ({
    page,
  }) => {
    const meetingId = `e2e_settings_decay_narrow_${Date.now()}`;

    await openAppearanceTab(page, meetingId, "decay-narrow-user");

    // Narrowest phone width the issue calls out (issue 1871).
    await page.setViewportSize({ width: 320, height: 720 });
    await page.waitForTimeout(300);

    const decayHelpIcon = page.locator('[data-testid="speaker-highlight-decay-help"]');
    const decayHelpText = page.locator('[data-testid="speaker-highlight-decay-help-text"]');
    const settingsPanel = page.locator("#device-settings-dialog .settings-panel");
    const dialog = page.locator("#device-settings-dialog");

    await decayHelpIcon.scrollIntoViewIfNeeded();
    // Touch devices have no hover, so a tap latches the tooltip open. Assert the
    // latch itself: the click leaves Playwright's pointer resting ON the trigger,
    // so `toBeVisible()`/`opacity === "1"` alone would also pass via `:hover`
    // even if the `--open` latch were broken.
    await decayHelpIcon.click();
    await expect(decayHelpIcon).toHaveClass(/--open/);
    await expect(decayHelpText).toBeVisible();
    await expect
      .poll(async () => decayHelpText.evaluate((el) => getComputedStyle(el).opacity), {
        timeout: 3_000,
      })
      .toBe("1");

    const iconBox = await decayHelpIcon.boundingBox();
    const tipBox = await decayHelpText.boundingBox();
    expect(iconBox).not.toBeNull();
    expect(tipBox).not.toBeNull();

    // The mobile override now falls back to the base below-the-trigger
    // placement instead of keeping the desktop right-and-up one. Pre-fix the
    // bubble sat ABOVE the trigger and to its RIGHT (`left: calc(100% + 10px);
    // bottom: calc(100% + 10px)`), which is what marched it toward the clipped
    // edge — at 300px wide it already overflowed. These two assertions are what
    // fail if the old placement comes back.
    expect(tipBox!.y).toBeGreaterThanOrEqual(iconBox!.y + iconBox!.height - 1);
    expect(tipBox!.x).toBeLessThanOrEqual(iconBox!.x + 1);

    // ...and the whole bubble is inside the panel. `.settings-panel` sets
    // `overflow-x: hidden`, so anything outside that box is clipped away —
    // `z-index` cannot escape an overflow box. Checked at 320px (the width the
    // issue names) and again at 300px, where the old placement demonstrably
    // overflowed. 1px of rounding slack.
    const assertInsidePanel = async (label: string) => {
      const tip = await decayHelpText.boundingBox();
      const panel = await settingsPanel.boundingBox();
      expect(tip, label).not.toBeNull();
      expect(panel, label).not.toBeNull();
      expect(tip!.x, `${label}: left edge`).toBeGreaterThanOrEqual(panel!.x - 1);
      expect(tip!.x + tip!.width, `${label}: right edge`).toBeLessThanOrEqual(
        panel!.x + panel!.width + 1,
      );
    };

    await assertInsidePanel("320px viewport");

    await page.setViewportSize({ width: 300, height: 720 });
    await page.waitForTimeout(300);
    await expect(decayHelpText).toBeVisible();
    await assertInsidePanel("300px viewport");

    await page.setViewportSize({ width: 320, height: 720 });
    await page.waitForTimeout(300);

    // Escape is only swallowed while there is a tooltip to dismiss: the first
    // press closes the bubble, the second reaches the modal.
    await page.keyboard.press("Escape");
    await expect(decayHelpText).toBeHidden();
    await expect(dialog).toBeVisible();
    await page.keyboard.press("Escape");
    await expect(dialog).toBeHidden();
  });

  test("Decay help tooltip text stays readable on its fixed dark bubble in light theme", async ({
    page,
  }) => {
    const meetingId = `e2e_settings_decay_light_${Date.now()}`;

    await openAppearanceTab(page, meetingId, "decay-light-user");

    // Real Appearance-panel Light toggle, the same mechanism theme-toggle.spec.ts
    // and peer-toast-light-contrast.spec.ts use. It sets `html[data-theme]`
    // synchronously; poll defensively.
    await page.getByRole("button", { name: "Light", exact: true }).click();
    await expect
      .poll(() => page.evaluate(() => document.documentElement.getAttribute("data-theme")), {
        timeout: 3_000,
      })
      .toBe("light");

    const decayHelpIcon = page.locator('[data-testid="speaker-highlight-decay-help"]');
    const decayHelpText = page.locator('[data-testid="speaker-highlight-decay-help-text"]');

    await decayHelpIcon.scrollIntoViewIfNeeded();
    // As above: assert the `--open` latch, not just visibility — the pointer is
    // left hovering the trigger, which would reveal the bubble on its own.
    await decayHelpIcon.click();
    await expect(decayHelpIcon).toHaveClass(/--open/);
    await expect(decayHelpText).toBeVisible();
    await expect
      .poll(async () => decayHelpText.evaluate((el) => getComputedStyle(el).opacity), {
        timeout: 3_000,
      })
      .toBe("1");

    // THE REGRESSION THIS PINS (issue 1871). The bubble paints on a FIXED dark
    // surface (`--fixed-dark-strong`) in both themes, so its text must stay
    // light. It used to be `.settings-info-icon::after`, and the light-theme
    // blanket `html[data-theme="light"] .settings-panel *` cannot match a
    // pseudo-element — so the old bubble was immune by accident. Promoting it to
    // a real child `<span>` (required for `aria-describedby` to reach it) walked
    // it into that blanket, where it resolved to dark navy on near-black at
    // 1.15:1: revealed but completely unreadable. Dark theme was unaffected,
    // which is why it slipped through.
    const { color, background } = await decayHelpText.evaluate((el) => {
      const s = getComputedStyle(el);
      return { color: s.color, background: s.backgroundColor };
    });

    // Diagnostic: the exact token that must win. `--on-dark-text-secondary`.
    expect(color).toBe("rgba(255, 255, 255, 0.7)");

    // ...and the requirement itself, computed rather than asserted, against the
    // SC 1.4.3 body-text threshold.
    //
    // The compositing order matters and is easy to get wrong: the text is
    // translucent and sits on the BUBBLE, not on the page. Flattening both over
    // white independently turns `rgba(255,255,255,0.7)` into pure white and
    // reports 14.5:1 — a number the user never sees. Compositing the bubble over
    // white (the lightest a light-theme panel can be, so the floor for light
    // text) and then the text over that bubble gives the ratio that is actually
    // rendered.
    const bubble = compositeOverWhite(background);
    const ratio = contrastRatio(compositeOver(color, bubble), bubble);
    expect(ratio, `tooltip text ${color} on bubble ${background}`).toBeGreaterThanOrEqual(4.5);
  });

  test("Decay slider preview: low decay glow tail turns off quickly, high decay lingers", async ({
    page,
  }) => {
    test.setTimeout(90_000);
    const meetingId = `e2e_decay_preview_${Date.now()}`;

    // Force animation-enabled mode for this test regardless of host OS setting.
    await page.emulateMedia({ reducedMotion: "no-preference" });

    await openAppearanceTab(page, meetingId, "decay-preview-user");

    const decaySlider = page.locator('[data-testid="speaker-highlight-decay-slider"]');
    const previewTile = page.locator(".speaker-highlight-preview .preview-tile");

    await expect(previewTile).toBeVisible({ timeout: 5_000 });

    const previewStyle = async () => (await previewTile.getAttribute("style")) || "";
    const previewClass = async () => (await previewTile.getAttribute("class")) || "";

    // ── Low decay (0%): glow tail turns off instantly ────────────────────────
    await decaySlider.fill("0");
    await page.waitForTimeout(300);

    // At 0% decay the preview variables must reflect an instant-off tail
    // (no hold, no fade-out).
    await expect
      .poll(
        async () => {
          const style = await previewStyle();
          const instantOff =
            style.includes("--preview-glow-hold-delay: 0.00s;") &&
            style.includes("--preview-glow-fade-out: 0.00s;");
          return instantOff ? "instant-off" : `style: ${style}`;
        },
        {
          timeout: 15_000,
          intervals: [200],
          message: "expected preview tile to show instant-off transition at 0% decay",
        },
      )
      .toBe("instant-off");

    // The preview loop should include a silent phase when motion is enabled.
    await expect
      .poll(
        async () => {
          const className = await previewClass();
          return className.includes("preview-tile--silent") ? "silent" : "still-speaking";
        },
        {
          timeout: 15_000,
          intervals: [200],
          message: "expected preview tile to enter silent phase with motion enabled",
        },
      )
      .toBe("silent");

    // ── High decay (100%): glow lingers with long hold ──────────────────────
    await decaySlider.fill("100");
    await page.waitForTimeout(300);

    // At 100% decay the preview variables must advertise the long hold used by
    // the production glow.
    await expect
      .poll(
        async () => {
          const style = await previewStyle();
          const longLinger = style.includes("--preview-glow-hold-delay: 5.00s;");
          return longLinger ? "long-linger" : `style: ${style}`;
        },
        {
          timeout: 15_000,
          intervals: [200],
          message: "expected preview tile to show long-linger transition at 100% decay",
        },
      )
      .toBe("long-linger");

    // With high decay configured, we should still observe the silent phase.
    await expect
      .poll(
        async () => {
          const className = await previewClass();
          return className.includes("preview-tile--silent") ? "silent" : "still-speaking";
        },
        {
          timeout: 15_000,
          intervals: [200],
          message: "expected preview tile to enter silent phase at 100% decay",
        },
      )
      .toBe("silent");

    // ── Verify speaking phase also occurs (glow is applied) ─────────────────
    // After confirming the silent phase, the cycle should bring back the glow.
    await expect
      .poll(
        async () => {
          const className = await previewClass();
          const style = await previewStyle();
          return className.includes("preview-tile--speaking") &&
            style.includes("--preview-glow-border-alpha:")
            ? "speaking"
            : "silent";
        },
        {
          timeout: 15_000,
          intervals: [200],
          message: "expected preview tile to enter speaking phase with active glow",
        },
      )
      .toBe("speaking");
  });

  test("Decay slider respects reduced motion and stays in a static speaking state", async ({
    page,
  }) => {
    test.setTimeout(45_000);
    await page.emulateMedia({ reducedMotion: "reduce" });

    const meetingId = `e2e_decay_preview_reduced_motion_${Date.now()}`;
    await openAppearanceTab(page, meetingId, "decay-preview-reduced-motion-user");

    const previewTile = page.locator(".speaker-highlight-preview .preview-tile");
    await expect(previewTile).toBeVisible({ timeout: 5_000 });

    const initialClassName = await previewTile.getAttribute("class");
    expect(initialClassName).toContain("preview-tile--speaking");
    expect(initialClassName).not.toContain("preview-tile--silent");

    const initialAnimationName = await previewTile.evaluate(
      (element) => getComputedStyle(element).animationName,
    );
    expect(initialAnimationName).toBe("none");

    await page.waitForTimeout(3_500);

    const settledClassName = await previewTile.getAttribute("class");
    expect(settledClassName).toContain("preview-tile--speaking");
    expect(settledClassName).not.toContain("preview-tile--silent");

    const settledAnimationName = await previewTile.evaluate(
      (element) => getComputedStyle(element).animationName,
    );
    expect(settledAnimationName).toBe("none");

    // Observe for several seconds and ensure the JS driver never flips this
    // tile into the silent phase while reduced-motion is enabled.
    const neverEnteredSilent = await page.evaluate(async () => {
      const tile = document.querySelector(".speaker-highlight-preview .preview-tile");
      if (!tile) return false;

      const start = performance.now();
      while (performance.now() - start < 3_500) {
        const className = tile.getAttribute("class") || "";
        if (className.includes("preview-tile--silent")) {
          return false;
        }
        await new Promise((resolve) => setTimeout(resolve, 100));
      }
      return true;
    });
    expect(neverEnteredSilent).toBe(true);

    // issue 1871: the Decay help bubble must not fade under reduced motion
    // either. Checked in BOTH states because a media query adds no specificity:
    // the resting rule is (0,1,0) but the reveal rules are (0,3,0), so a
    // reduced-motion block that only restates the base selector silently leaves
    // the fade-in animating. Pre-fix both read "0.15s, 0s".
    const decayHelpIcon = page.locator('[data-testid="speaker-highlight-decay-help"]');
    const decayHelpText = page.locator('[data-testid="speaker-highlight-decay-help-text"]');
    await decayHelpIcon.scrollIntoViewIfNeeded();

    expect(
      await decayHelpText.evaluate((el) => getComputedStyle(el).transitionDuration),
      "resting tooltip transition",
    ).toBe("0s");

    await decayHelpIcon.click();
    await expect(decayHelpIcon).toHaveClass(/speaker-highlight-help-icon--open/);
    expect(
      await decayHelpText.evaluate((el) => getComputedStyle(el).transitionDuration),
      "revealed tooltip transition",
    ).toBe("0s");
  });

  test("Reset restores the speaker highlight defaults", async ({ page }) => {
    const meetingId = `e2e_settings_reset_${Date.now()}`;

    await openAppearanceTab(page, meetingId, "reset-user");

    const glowToggle = page.getByLabel("Toggle speaker highlight");
    const glowSwitch = page.locator("label.glow-switch");
    const mintSwatch = page.locator('[aria-label="Select Mint Green highlight"]');
    const cyanSwatch = page.locator('[aria-label="Select Cyan highlight"]');
    const brightnessRow = speakerHighlightRow(page, "Brightness");
    const glowRow = speakerHighlightRow(page, "Glow");
    const decaySlider = page.locator('[data-testid="speaker-highlight-decay-slider"]');
    const brightnessSlider = page.locator('[data-testid="speaker-highlight-brightness-slider"]');
    const glowSlider = page.locator('[data-testid="speaker-highlight-glow-slider"]');
    const resetButton = page.locator('[data-testid="speaker-highlight-reset-btn"]');

    const brightnessValue = brightnessRow.locator(".appearance-slider-value");
    const glowValue = glowRow.locator(".appearance-slider-value");
    const decayValue = decaySlider.locator(
      "xpath=ancestor::div[contains(@class, 'appearance-slider-row')]//span[contains(@class, 'appearance-slider-value')]",
    );

    await cyanSwatch.click();
    await glowSwitch.click();
    await brightnessSlider.fill("70");
    await glowSlider.fill("30");
    await decaySlider.fill("10");

    await expect(cyanSwatch).toHaveAttribute("aria-pressed", "true");
    await expect(glowToggle).not.toBeChecked();
    await expect(brightnessValue).toHaveText("70%");
    await expect(glowValue).toHaveText("30%");
    await expect(decayValue).toHaveText("10%");
    await expect(resetButton).toHaveText("Reset highlight");

    await resetButton.click();

    await expect(glowToggle).toBeChecked();
    await expect(mintSwatch).toHaveAttribute("aria-pressed", "true");
    await expect(cyanSwatch).toHaveAttribute("aria-pressed", "false");
    await expect(brightnessValue).toHaveText("50%");
    await expect(glowValue).toHaveText("50%");
    await expect(decayValue).toHaveText("50%");
    await expect(brightnessSlider).toHaveValue("50");
    await expect(glowSlider).toHaveValue("50");
    await expect(decaySlider).toHaveValue("50");

    await page.waitForTimeout(500);

    await expect
      .poll(async () => page.evaluate(() => localStorage.getItem("vc_appearance_glow_decay")), {
        timeout: 5_000,
      })
      .toBe("0.5");
  });

  test("preset color swatches are keyboard-activatable with Enter and Space", async ({ page }) => {
    const meetingId = `e2e_swatches_keyboard_activate_${Date.now()}`;

    await openAppearanceTab(page, meetingId, "swatch-keyboard-user");

    const mintSwatch = page.locator('[aria-label="Select Mint Green highlight"]');
    const cyanSwatch = page.locator('[aria-label="Select Cyan highlight"]');

    await expect(mintSwatch).toHaveAttribute("aria-pressed", "true");
    await expect(cyanSwatch).toHaveAttribute("aria-pressed", "false");

    await cyanSwatch.focus();
    await cyanSwatch.press("Enter");
    await expect(cyanSwatch).toHaveAttribute("aria-pressed", "true");
    await expect(mintSwatch).toHaveAttribute("aria-pressed", "false");

    await mintSwatch.focus();
    await mintSwatch.press("Space");
    await expect(mintSwatch).toHaveAttribute("aria-pressed", "true");
    await expect(cyanSwatch).toHaveAttribute("aria-pressed", "false");
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
    // After selecting Cyan (rgb 12, 175, 255) the preview CSS variables must contain those values.
    const previewTile = page.locator(".appearance-preview-area .preview-tile");
    await expect(previewTile).toBeVisible();
    // Decorative avatar silhouette SVG is rendered inside the preview tile
    await expect(previewTile.locator("svg")).toBeVisible();
    await expect(previewTile).toHaveAttribute("style", /--preview-glow-r: 12;/);
    await expect(previewTile).toHaveAttribute("style", /--preview-glow-g: 175;/);
    await expect(previewTile).toHaveAttribute("style", /--preview-glow-b: 255;/);
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

    await openDeviceSettingsModal(page);
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
    // Magenta colour via CSS variables (rgb 255, 0, 191).
    const previewTile = page.locator(".appearance-preview-area .preview-tile");
    await expect(previewTile).toBeVisible();
    await expect(previewTile).toHaveAttribute("style", /--preview-glow-r: 255;/);
    await expect(previewTile).toHaveAttribute("style", /--preview-glow-g: 0;/);
    await expect(previewTile).toHaveAttribute("style", /--preview-glow-b: 191;/);
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

    await openDeviceSettingsModal(page);
    await page.getByRole("tab", { name: "Appearance" }).click();

    const cyanSwatch = page.locator('[aria-label="Select Cyan glow"]');
    const brightnessSlider = page.locator(".brightness-section .appearance-slider");
    const innerGlowSlider = page.locator(".inner-glow-section .appearance-slider");
    const glowToggle = page.getByLabel("Toggle speaker highlight");

    await cyanSwatch.click();
    await brightnessSlider.fill("55");
    await innerGlowSlider.fill("40");
    await glowToggle.click();

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

    await openDeviceSettingsModal(page);
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

    await openDeviceSettingsModal(page);
    await page.getByRole("tab", { name: "Appearance" }).click();

    // The + button is visible inside the swatches row
    const addBtn = page.locator('[aria-label="Add custom color"]');
    await expect(addBtn).toBeVisible();

    // The + button renders an inline SVG icon (not a text glyph)
    await expect(addBtn.locator("svg")).toBeVisible();

    // Inline popover is not yet visible
    await expect(page.locator(".custom-color-popover")).toHaveCount(0);

    // Click + opens the inline popover with preview, hex input, and Add button
    await addBtn.click();

    const popover = page.locator(".custom-color-popover");
    await expect(popover).toBeVisible();

    // Regression guard for #487 / issue #694: the "+" button must open the
    // full visual HSV color picker (saturation/value square + hue slider),
    // not the plain text-only popover it fell back to after the color picker
    // was deleted in commit 252cabd (via PR #882 "light-and-dark-themes").
    // These assertions FAIL on that broken state and PASS on the restored
    // modal; do not remove them without re-confirming the picker still ships.
    await expect(page.locator(".custom-color-modal-overlay")).toBeVisible();
    await expect(popover.locator(".color-picker-sv-square")).toBeVisible();
    await expect(popover.locator(".color-picker-hue-track")).toBeVisible();

    // Hex text input
    await expect(popover.locator(".custom-color-input")).toBeVisible();
    await expect(popover.locator(".custom-color-input")).toHaveAttribute("placeholder", "#RRGGBB");

    // Add button
    await expect(popover.locator(".custom-color-add-btn")).toBeVisible();
    await expect(popover.locator(".custom-color-add-btn")).toHaveText("Add");
  });

  // Regression for the hex validator mismatch: the picker reports errors via
  // the lenient `parse_hex` (trims whitespace, `#` optional) but the Add
  // button previously gated on the strict `GlowColor::from_hex` (exactly 7
  // chars starting with `#`). Inputs like `ABCDEF` or `#FF0000 ` produced
  // NO error message AND a disabled Add button — a silent dead state. Both
  // gates now share `parse_hex`, so these inputs must yield: no error shown,
  // Add enabled. FAILS on the pre-fix code where Add stays disabled.
  test("Add button gate matches the picker's own hex validator (no silent dead state)", async ({
    page,
  }) => {
    const meetingId = `e2e_custom_color_validator_parity_${Date.now()}`;

    await page.goto("/");
    await page.waitForTimeout(1500);

    await page.locator("#meeting-id").click();
    await page.locator("#meeting-id").pressSequentially(meetingId, { delay: 80 });

    await page.locator("#username").click();
    await page.locator("#username").fill("");
    await page.locator("#username").pressSequentially("validator-parity-user", { delay: 80 });
    await page.waitForTimeout(500);
    await page.locator("#username").press("Enter");

    await expect(page).toHaveURL(new RegExp(`/meeting/${meetingId}`), { timeout: 10_000 });

    const joinButton = page.getByRole("button", { name: /Start Meeting|Join Meeting/ });
    await expect(joinButton).toBeVisible({ timeout: 20_000 });
    await joinButton.click();

    await expect(page.locator("#grid-container")).toBeVisible({ timeout: 15_000 });

    await openDeviceSettingsModal(page);
    await page.getByRole("tab", { name: "Appearance" }).click();

    await page.locator('[aria-label="Add custom color"]').click();

    const popover = page.locator(".custom-color-popover");
    await expect(popover).toBeVisible();

    const colorInput = popover.locator(".custom-color-input");
    const addColorBtn = popover.locator(".custom-color-add-btn");

    // Input 1: no leading `#`. Accepted by parse_hex, rejected by the old
    // strict validator. Pre-fix: Add disabled + no error → dead state.
    await colorInput.fill("ABCDEF");
    await expect(popover.locator(".input-error-message")).toHaveCount(0);
    await expect(addColorBtn).toBeEnabled();

    // Input 2: valid hex with trailing whitespace. parse_hex trims it; the
    // old strict validator saw length 8 and rejected. Same silent dead state.
    await colorInput.fill("#FF0000 ");
    await expect(popover.locator(".input-error-message")).toHaveCount(0);
    await expect(addColorBtn).toBeEnabled();

    // Truly invalid input still surfaces an error and disables Add.
    await colorInput.fill("nope");
    await expect(popover.locator(".input-error-message")).toBeVisible();
    await expect(addColorBtn).toBeDisabled();

    // Clicking Add on a leniently-valid value creates the swatch and closes
    // the modal — proving the onclick path also uses the lenient validator.
    await colorInput.fill("ABCDEF");
    await addColorBtn.click();
    await expect(popover).toHaveCount(0);
    const customSwatch = page.locator(
      '.color-swatches .color-swatch:not(.add-color-btn)[style*="--glow-color: #ABCDEF"]',
    );
    await expect(customSwatch).toHaveCount(1);
    await expect(customSwatch).toHaveAttribute("style", /--glow-color:\s*#ABCDEF/i);
    await expect(customSwatch).toBeVisible();
  });

  // Regression for the focus-trap a11y bug: without a focus trap, Tab from
  // the last focusable element in the modal moves focus to the Brightness
  // slider that lives immediately after the modal in DOM order (the scrim
  // blocks mouse clicks but does NOT block keyboard focus). This test
  // FAILS on the un-fixed code because focus escapes to
  // `.appearance-slider`.
  test("Tab and Shift+Tab wrap focus inside the color picker modal (no escape to background)", async ({
    page,
  }) => {
    const meetingId = `e2e_custom_color_focus_trap_${Date.now()}`;

    await page.goto("/");
    await page.waitForTimeout(1500);

    await page.locator("#meeting-id").click();
    await page.locator("#meeting-id").pressSequentially(meetingId, { delay: 80 });

    await page.locator("#username").click();
    await page.locator("#username").fill("");
    await page.locator("#username").pressSequentially("focus-trap-user", { delay: 80 });
    await page.waitForTimeout(500);
    await page.locator("#username").press("Enter");

    await expect(page).toHaveURL(new RegExp(`/meeting/${meetingId}`), {
      timeout: 10_000,
    });

    const joinButton = page.getByRole("button", {
      name: /Start Meeting|Join Meeting/,
    });
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

    await page.locator('[aria-label="Add custom color"]').click();
    const popover = page.locator(".custom-color-popover");
    await expect(popover).toBeVisible();

    // Type a valid hex so the Add button is enabled and therefore in the
    // focusable set — this locks the "last focusable" position.
    await popover.locator(".custom-color-input").fill("#123456");
    await expect(popover.locator(".custom-color-add-btn")).toBeEnabled();

    // Focus the last focusable in the modal (Add button) and press Tab.
    // Without the trap, focus jumps to a Brightness slider outside the
    // dialog; with the trap, it wraps to the first focusable (close btn).
    await popover.locator(".custom-color-add-btn").focus();
    await page.keyboard.press("Tab");
    const afterTabInsideModal = await page.evaluate(
      () => !!document.activeElement?.closest(".custom-color-modal"),
    );
    expect(afterTabInsideModal).toBe(true);
    // The specific wrap target: first focusable is the close button.
    const afterTabIsClose = await page.evaluate(() =>
      document.activeElement?.classList.contains("custom-color-modal-close"),
    );
    expect(afterTabIsClose).toBe(true);

    // Now Shift+Tab from the close button must wrap to the last focusable
    // (Add button), not escape backwards into the panel behind the scrim.
    await page.keyboard.press("Shift+Tab");
    const afterShiftTabInsideModal = await page.evaluate(
      () => !!document.activeElement?.closest(".custom-color-modal"),
    );
    expect(afterShiftTabInsideModal).toBe(true);
    const afterShiftTabIsAdd = await page.evaluate(() =>
      document.activeElement?.classList.contains("custom-color-add-btn"),
    );
    expect(afterShiftTabIsAdd).toBe(true);

    // Regression symptom: assert focus is NOT on any background control
    // (sliders behind the scrim).
    const focusedIsAppearanceSlider = await page.evaluate(
      () => document.activeElement?.classList.contains("appearance-slider") ?? false,
    );
    expect(focusedIsAppearanceSlider).toBe(false);
  });

  // Regression for the focus-on-open a11y bug: after clicking "+", focus
  // used to stay on the "+" button (a DOM sibling of the dialog), so an
  // immediate Escape did NOT close the modal because the keydown handler
  // lives on the dialog. The modal now has tabindex="-1" and calls
  // set_focus(true) onmounted. FAILS on the pre-fix code because Escape
  // is swallowed by the "+" button and the modal stays open.
  test("pressing Escape immediately after opening the picker closes the modal", async ({
    page,
  }) => {
    const meetingId = `e2e_custom_color_escape_focus_${Date.now()}`;

    await page.goto("/");
    await page.waitForTimeout(1500);

    await page.locator("#meeting-id").click();
    await page.locator("#meeting-id").pressSequentially(meetingId, { delay: 80 });

    await page.locator("#username").click();
    await page.locator("#username").fill("");
    await page.locator("#username").pressSequentially("escape-focus-user", { delay: 80 });
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

    await page.locator('[aria-label="Add custom color"]').click();

    const popover = page.locator(".custom-color-popover");
    await expect(popover).toBeVisible();

    // Focus must have moved off the "+" button and into the dialog. Assert
    // the active element is inside the modal — this is the direct DOM
    // symptom of the fix; without it, activeElement stays on "+".
    const activeInsideDialog = await page.evaluate(
      () => !!document.activeElement?.closest(".custom-color-modal"),
    );
    expect(activeInsideDialog).toBe(true);

    // Press Escape without clicking inside the modal first. The dialog's
    // onkeydown handler must catch this and close the modal.
    await page.keyboard.press("Escape");
    await expect(popover).toHaveCount(0);
    await expect(page.locator(".custom-color-modal-overlay")).toHaveCount(0);

    // Focus returns to a meaningful control inside the Appearance panel.
    await expect
      .poll(async () => page.evaluate(() => document.activeElement === document.body), {
        timeout: 3_000,
      })
      .toBe(false);
    await expect
      .poll(
        async () =>
          page.evaluate(() => !!document.activeElement?.closest("#settings-panel-appearance")),
        {
          timeout: 3_000,
        },
      )
      .toBe(true);
  });

  // Regression coverage for #487 / #694: the modal HSV color picker was
  // deleted by commit 252cabd (via PR #882) and restored on this branch.
  // These tests exercise the picker's DOM contract; if the picker regresses
  // to a text-only popover they fail because .color-picker-* selectors go
  // away. `settings-modal.spec.ts` is untagged — validate via `/run-e2e
  // dioxus` dispatch or the local docker e2e stack; per-PR CI does not run
  // untagged specs.
  test("copy button aria-label changes to Copied after click", async ({ page }) => {
    const meetingId = `e2e_copy_btn_aria_${Date.now()}`;

    await openAppearanceTab(page, meetingId, "copy-aria-user");

    // Make clipboard writes deterministic in CI/headless runs.
    await page.evaluate(() => {
      const nav = navigator as Navigator & {
        clipboard?: Clipboard & { writeText?: (text: string) => Promise<void> };
      };
      if (!nav.clipboard) {
        Object.defineProperty(nav, "clipboard", {
          value: { writeText: async (_text: string) => {} },
          configurable: true,
        });
        return;
      }
      nav.clipboard.writeText = async (_text: string) => {};
    });

    await page.locator('[aria-label="Add custom color"]').click();
    const popover = page.locator(".custom-color-popover");
    await expect(popover).toBeVisible();

    const copyBtn = popover.locator(".color-picker-copy-btn");
    const copyStatus = popover.locator('[aria-live="polite"]').nth(1);
    await expect(copyStatus).toHaveText("");

    await copyBtn.click();

    // The live region should announce the copied hex value for assistive tech.
    await expect(copyStatus).toContainText(/Copied #?[0-9A-Fa-f]{6}/, { timeout: 3_000 });

    // After ~1.2s it reverts back to the default label
    await expect(copyStatus).toHaveText("", { timeout: 3_000 });
  });

  test("custom swatch delete button has descriptive accessible name", async ({ page }) => {
    const meetingId = `e2e_delete_aria_${Date.now()}`;

    await openAppearanceTab(page, meetingId, "delete-aria-user");

    await expect(page.locator("#color-swatches-container")).toHaveAttribute("role", "group");
    await expect(page.locator("#color-swatches-container")).toHaveAttribute(
      "aria-label",
      "Speaker highlight colors",
    );

    await page.locator('[aria-label="Add custom color"]').click();
    const popover = page.locator(".custom-color-popover");
    await expect(popover).toBeVisible();

    const colorInput = popover.locator(".custom-color-input");
    const addColorBtn = popover.locator(".custom-color-add-btn");

    await colorInput.fill("#FF5733");
    await addColorBtn.click();
    await expect(popover).toHaveCount(0);

    const customSwatch = page.locator('[aria-label*="Select custom highlight #FF5733"]').first();
    await expect(customSwatch).toBeVisible();

    const deleteBtn = customSwatch.locator(".color-swatch-delete-btn");
    await expect(deleteBtn).toHaveAttribute(
      "aria-label",
      /Delete custom highlight #[0-9A-Fa-f]{6}/,
    );
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

    await openDeviceSettingsModal(page);
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

    await openDeviceSettingsModal(page);
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
    await expect(
      page.locator(
        '.color-swatches .color-swatch:not(.add-color-btn)[style*="--glow-color: #ABCDEF"]',
      ),
    ).toHaveCount(0);

    // Focus returns to a meaningful control inside the Appearance panel.
    await expect
      .poll(async () => page.evaluate(() => document.activeElement === document.body), {
        timeout: 3_000,
      })
      .toBe(false);
    await expect
      .poll(
        async () =>
          page.evaluate(() => !!document.activeElement?.closest("#settings-panel-appearance")),
        {
          timeout: 3_000,
        },
      )
      .toBe(true);
  });

  test("custom color popover closes when clicking outside and focus returns to add button", async ({
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

    await openDeviceSettingsModal(page);
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

    // Focus should return to a meaningful control inside the Appearance panel.
    await expect
      .poll(async () => page.evaluate(() => document.activeElement === document.body), {
        timeout: 3_000,
      })
      .toBe(false);
    await expect
      .poll(
        async () =>
          page.evaluate(() => !!document.activeElement?.closest("#settings-panel-appearance")),
        {
          timeout: 3_000,
        },
      )
      .toBe(true);
  });

  test("invalid custom color input shows error and does not add swatch", async ({ page }) => {
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

    await openDeviceSettingsModal(page);
    await page.getByRole("tab", { name: "Appearance" }).click();

    const addBtn = page.locator('[aria-label="Add custom color"]');
    await addBtn.click();

    const popover = page.locator(".custom-color-popover");
    await expect(popover).toBeVisible();

    const colorInput = popover.locator(".custom-color-input");
    const addColorBtn = popover.locator(".custom-color-add-btn");

    // Count existing swatches before attempting invalid adds
    const swatchCountBefore = await page.locator(".color-swatches .color-swatch").count();

    // Missing '#' is accepted by the current lenient parser.
    await colorInput.fill("123456");
    await expect(addColorBtn).toBeEnabled();
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

  test("valid custom color adds swatch, selects it, and it can be reselected", async ({ page }) => {
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
    const customSwatchStyle = await page.evaluate(() => {
      const swatch = Array.from(
        document.querySelectorAll(".color-swatches .color-swatch:not(.add-color-btn)"),
      ).find((element) => (element.getAttribute("style") || "").includes("--glow-color: #12ABEF"));
      return swatch?.getAttribute("style") || null;
    });
    expect(customSwatchStyle).toMatch(/--glow-color:\s*#12ABEF/i);
    const customSwatchPressed = await page.evaluate(() => {
      const swatch = Array.from(
        document.querySelectorAll(".color-swatches .color-swatch:not(.add-color-btn)"),
      ).find((element) =>
        (element.getAttribute("style") || "").includes("--glow-color: #12ABEF"),
      ) as HTMLElement | undefined;
      return swatch?.getAttribute("aria-pressed") || null;
    });
    expect(customSwatchPressed).toBe("true");

    // Previously selected preset (default Mint Green) is deselected
    await expect(page.locator('[aria-label="Select Mint Green highlight"]')).toHaveAttribute(
      "aria-pressed",
      "false",
    );

    // Switch to a preset, then reselect the custom color
    const cyanSwatch = page.locator('[aria-label="Select Cyan highlight"]');
    await cyanSwatch.click();
    await expect(cyanSwatch).toHaveAttribute("aria-pressed", "true");
    const customSwatchPressedAfterCyan = await page.evaluate(() => {
      const swatch = Array.from(
        document.querySelectorAll(".color-swatches .color-swatch:not(.add-color-btn)"),
      ).find((element) =>
        (element.getAttribute("style") || "").includes("--glow-color: #12ABEF"),
      ) as HTMLElement | undefined;
      return swatch?.getAttribute("aria-pressed") || null;
    });
    expect(customSwatchPressedAfterCyan).toBe("false");

    await page.evaluate(() => {
      const swatch = Array.from(
        document.querySelectorAll(".color-swatches .color-swatch:not(.add-color-btn)"),
      ).find((element) =>
        (element.getAttribute("style") || "").includes("--glow-color: #12ABEF"),
      ) as HTMLElement | undefined;
      swatch?.click();
    });
    const customSwatchPressedAfterClick = await page.evaluate(() => {
      const swatch = Array.from(
        document.querySelectorAll(".color-swatches .color-swatch:not(.add-color-btn)"),
      ).find((element) =>
        (element.getAttribute("style") || "").includes("--glow-color: #12ABEF"),
      ) as HTMLElement | undefined;
      return swatch?.getAttribute("aria-pressed") || null;
    });
    expect(customSwatchPressedAfterClick).toBe("true");
    await expect(cyanSwatch).toHaveAttribute("aria-pressed", "false");

    // Preview tile uses fixed-intensity glow — selecting custom #12ABEF
    // (rgb 18, 171, 239) now appears in the preview CSS custom properties.
    const previewTile = page.locator(".speaker-highlight-preview .preview-tile");
    await expect(previewTile).toBeVisible();
    await expect(previewTile).toHaveAttribute("style", /--preview-glow-r: 18;/);
    await expect(previewTile).toHaveAttribute("style", /--preview-glow-g: 171;/);
    await expect(previewTile).toHaveAttribute("style", /--preview-glow-b: 239;/);
    await expect(previewTile).toHaveClass(/preview-tile-pulsing/);
  });

  test("custom color can be deleted via delete button", async ({ page }) => {
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

    const swatchCountBefore = await page.locator(".color-swatches .color-swatch").count();

    // Add a custom color
    const addBtn = page.locator('[aria-label="Add custom color"]');
    await addBtn.click();

    const popover = page.locator(".custom-color-popover");
    const colorInput = popover.locator(".custom-color-input");
    const addColorBtn = popover.locator(".custom-color-add-btn");

    await colorInput.fill("#FF5733");
    await addColorBtn.click();

    // Verify custom swatch was created and is selected
    const customSwatch = page.locator(
      '.color-swatches .color-swatch:not(.add-color-btn)[style*="--glow-color: #FF5733"]',
    );
    await expect(customSwatch).toHaveCount(1);
    await expect(customSwatch).toHaveAttribute("style", /--glow-color:\s*#FF5733/i);
    await expect(customSwatch).toBeVisible();
    await expect(customSwatch).toHaveAttribute("aria-pressed", "true");

    // Hover over the custom swatch to reveal delete button and click it
    await customSwatch.hover();
    const deleteBtn = customSwatch.locator(".color-swatch-delete-btn");
    await expect(deleteBtn).toBeVisible();

    // Delete button renders an inline SVG × icon (not a text glyph)
    await expect(deleteBtn.locator("svg")).toBeVisible();

    // a11y: delete button must have a descriptive accessible name including hex
    await expect(deleteBtn).toHaveAttribute(
      "aria-label",
      /Delete custom highlight #[0-9A-Fa-f]{6}/,
    );

    await deleteBtn.click();

    // Verify swatch was deleted
    await expect(
      page.locator('.color-swatches .color-swatch[style*="--glow-color: #FF5733"]'),
    ).toHaveCount(0);
    const swatchCountAfter = await page.locator(".color-swatches .color-swatch").count();
    expect(swatchCountAfter).toBe(swatchCountBefore);

    // Verify user is switched back to default color (Mint Green) after deletion
    const mintSwatch = page.locator('[aria-label="Select Mint Green highlight"]');
    await expect(mintSwatch).toHaveAttribute("aria-pressed", "true");
  });

  test("deleting a custom color focuses the next swatch, or previous if last", async ({ page }) => {
    const meetingId = `e2e_custom_color_delete_focus_${Date.now()}`;

    await openAppearanceTab(page, meetingId, "delete-focus-user");

    const colors = ["#FF0000", "#00FF00", "#0000FF"];

    for (const hex of colors) {
      await page.locator('[aria-label="Add custom color"]').click();
      const popover = page.locator(".custom-color-popover");
      await expect(popover).toBeVisible();

      const colorInput = popover.locator(".custom-color-input");
      const addColorBtn = popover.locator(".custom-color-add-btn");
      await colorInput.fill(hex);
      await addColorBtn.click();
      await expect(popover).toHaveCount(0);
    }

    const middleSwatch = page.locator('[aria-label*="Select custom highlight #00FF00"]').first();
    const middleDeleteBtn = middleSwatch.locator(".color-swatch-delete-btn");
    await middleSwatch.hover();
    await expect(middleDeleteBtn).toBeVisible();
    await middleDeleteBtn.click();

    await expect
      .poll(
        async () => page.evaluate(() => document.activeElement?.getAttribute("aria-label") ?? ""),
        { timeout: 3_000 },
      )
      .toContain("Select custom highlight #0000FF");

    const lastSwatch = page.locator('[aria-label*="Select custom highlight #0000FF"]').first();
    const lastDeleteBtn = lastSwatch.locator(".color-swatch-delete-btn");
    await lastDeleteBtn.click();

    await expect
      .poll(
        async () => page.evaluate(() => document.activeElement?.getAttribute("aria-label") ?? ""),
        { timeout: 3_000 },
      )
      .toContain("Select custom highlight #FF0000");
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
    await expect(previewTile).toHaveAttribute("style", /--preview-glow-r: 91;/);
    await expect(previewTile).toHaveAttribute("style", /--preview-glow-g: 207;/);
    await expect(previewTile).toHaveAttribute("style", /--preview-glow-b: 159;/);
    await expect(panel).not.toHaveClass(/glow-disabled/);

    // Toggle OFF — preview shows no glow; panel gets glow-disabled class
    // which fades luminous effects on swatches, sliders, and preview animation
    await glowToggle.uncheck();
    await expect(previewTile).toHaveClass(/preview-tile--silent/);
    await expect(panel).toHaveClass(/glow-disabled/);

    // Toggle ON — glow returns with the same color; glow-disabled removed
    await glowToggle.check();
    await expect(previewTile).toHaveAttribute("style", /--preview-glow-r: 91;/);
    await expect(previewTile).toHaveAttribute("style", /--preview-glow-g: 207;/);
    await expect(previewTile).toHaveAttribute("style", /--preview-glow-b: 159;/);
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
    await expect(previewTile).toHaveAttribute("style", /--preview-glow-r: 12;/);
    await expect(previewTile).toHaveAttribute("style", /--preview-glow-g: 175;/);
    await expect(previewTile).toHaveAttribute("style", /--preview-glow-b: 255;/);
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
    await expect
      .poll(async () => page.evaluate(() => document.activeElement?.id ?? ""), {
        timeout: 3_000,
      })
      .toBe("add-custom-color-btn");
  });

  // Regression for a11y focus fallback: after adding the 10th custom color
  // the + button unmounts. Focus must land on a meaningful in-panel control
  // (selected swatch with aria-pressed=true), NOT document.body.
  test("custom color storage is capped at MAX_CUSTOM_COLORS and focus falls back to selected swatch", async ({
    page,
  }) => {
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

    // a11y focus fallback: after adding the last color (which unmounts the +
    // button), focus must move to a meaningful control inside the appearance
    // panel and not remain on document.body.
    await expect
      .poll(async () => page.evaluate(() => document.activeElement === document.body), {
        timeout: 3_000,
      })
      .toBe(false);

    await expect
      .poll(
        async () =>
          page.evaluate(() => !!document.activeElement?.closest("#settings-panel-appearance")),
        { timeout: 3_000 },
      )
      .toBe(true);

    // Accept either fallback target: selected swatch (preferred) or the
    // swatch container when selection is temporarily unavailable.
    const focusedIsMeaningfulFallback = await page.evaluate(() => {
      const el = document.activeElement;
      if (!el) {
        return false;
      }
      const isSelectedSwatch = el.getAttribute("aria-pressed") === "true";
      const isSwatchContainer = el.id === "color-swatches-container";
      return isSelectedSwatch || isSwatchContainer;
    });
    expect(focusedIsMeaningfulFallback).toBe(true);

    // Verify UI reflects the cap: add button is hidden once limit is reached.
    await expect(page.locator('[aria-label="Add custom color"]')).toHaveCount(0);
  });
});
