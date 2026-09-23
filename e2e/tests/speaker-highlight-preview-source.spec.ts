import { test, expect, chromium, Browser, Locator, Page } from "@playwright/test";
import { BROWSER_ARGS, createAuthenticatedContext } from "../helpers/auth-context";
import { continuousToneWavPath } from "../helpers/audio-fixtures";
import { wakeControls } from "../helpers/controls";
import { classifyGlow } from "../helpers/speaking-glow";
import { enterMeetingAsHost, enterTwoUserMeeting } from "../helpers/two-user-meeting";
import { waitForServices } from "../helpers/wait-for-services";

/**
 * `data-preview-source` names what drives the Appearance panel's speaker
 * highlight preview: the local mic, a remote speaker, or the simulation.
 *
 * Untagged: each test runs a fake-microphone browser through a full join, so it
 * runs only in the full `--project=dioxus` suite.
 */

async function launchPage(
  uiURL: string,
  name: string,
  fakeAudioFile?: string,
): Promise<{ browser: Browser; page: Page }> {
  const args = fakeAudioFile
    ? [...BROWSER_ARGS, `--use-file-for-fake-audio-capture=${fakeAudioFile}`]
    : [...BROWSER_ARGS];
  const browser = await chromium.launch({ args });
  const context = await createAuthenticatedContext(
    browser,
    `${name.toLowerCase()}@videocall.rs`,
    name,
    uiURL,
  );
  return { browser, page: await context.newPage() };
}

function micToggle(page: Page): Locator {
  return page.locator('[data-testid="mic-toggle-button"]');
}

async function enableMic(page: Page): Promise<void> {
  await wakeControls(page);
  const toggle = micToggle(page);
  await expect(toggle).toBeVisible({ timeout: 15_000 });
  await expect(toggle).not.toHaveClass(/\bactive\b/);
  await toggle.click({ timeout: 5_000 });
  await expect(toggle).toHaveClass(/\bactive\b/, { timeout: 15_000 });
}

async function openAppearanceSettings(page: Page): Promise<Locator> {
  await wakeControls(page);
  const settingsButton = page.locator('[data-testid="open-settings"]');
  await expect(settingsButton).toBeVisible({ timeout: 10_000 });
  await settingsButton.click({ timeout: 5_000 });
  await expect(page.locator(".device-settings-modal")).toBeVisible({ timeout: 10_000 });
  await page.getByRole("tab", { name: "Appearance" }).click({ timeout: 5_000 });
  await expect(page.locator("#settings-panel-appearance")).toBeVisible({ timeout: 5_000 });
  const tile = page.locator(".speaker-highlight-preview .preview-tile");
  await expect(tile).toBeVisible({ timeout: 5_000 });
  return tile;
}

async function samplePreview(
  page: Page,
  windowMs: number,
): Promise<{ source: string | null; cls: string; style: string }[]> {
  return page.evaluate(async (ms) => {
    const out: { source: string | null; cls: string; style: string }[] = [];
    const end = performance.now() + ms;
    while (performance.now() < end) {
      const tile = document.querySelector(".speaker-highlight-preview .preview-tile");
      if (!tile) {
        throw new Error("the preview tile unmounted mid-window");
      }
      out.push({
        source: tile.getAttribute("data-preview-source"),
        cls: tile.getAttribute("class") || "",
        style: tile.getAttribute("style") || "",
      });
      await new Promise((resolve) => setTimeout(resolve, 50));
    }
    return out;
  }, windowMs);
}

test.describe("Speaker highlight preview follows real sound", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test("the preview is driven by the local microphone and releases it on mute", async ({
    baseURL,
  }) => {
    test.setTimeout(180_000);
    const uiURL = baseURL || "http://localhost:3001";
    const { browser, page } = await launchPage(uiURL, "PreviewMicUser", continuousToneWavPath());

    try {
      await page.emulateMedia({ reducedMotion: "no-preference" });
      await enterMeetingAsHost(page, `e2e_preview_mic_${Date.now()}`);
      await enableMic(page);
      const selfTile = page.locator("#host-controls-nav");
      await expect(selfTile).toHaveAttribute("data-speaking", "true", { timeout: 30_000 });

      const tile = await openAppearanceSettings(page);
      await expect(tile).toHaveAttribute("data-preview-source", "mic", { timeout: 10_000 });
      await expect
        .poll(async () => classifyGlow((await tile.getAttribute("style")) || ""), {
          timeout: 10_000,
          message: "expected the mic-driven preview to be lit",
        })
        .toBe("lit");
      await expect(tile).toHaveAttribute("style", /border-color: rgba\(91, 207, 159,/);

      // The modal covers the dock; the click only has to reach the handler.
      await micToggle(page).dispatchEvent("click");
      await expect(micToggle(page)).toHaveClass(/\boff\b/, { timeout: 15_000 });
      await expect(selfTile).toHaveAttribute("data-speaking", "false", { timeout: 10_000 });
      await expect(tile).toHaveAttribute("data-preview-source", "simulated", { timeout: 15_000 });
    } finally {
      await browser.close();
    }
  });

  test("under reduced motion the simulation holds still, but the mic still drives the preview", async ({
    baseURL,
  }) => {
    test.setTimeout(180_000);
    const uiURL = baseURL || "http://localhost:3001";
    const { browser, page } = await launchPage(
      uiURL,
      "PreviewReducedMotionUser",
      continuousToneWavPath(),
    );

    try {
      await page.emulateMedia({ reducedMotion: "reduce" });
      await enterMeetingAsHost(page, `e2e_preview_reduced_motion_${Date.now()}`);
      const selfTile = page.locator("#host-controls-nav");
      await expect(micToggle(page)).not.toHaveClass(/\bactive\b/, { timeout: 15_000 });
      await expect(selfTile).toHaveAttribute("data-speaking", "false");

      const tile = await openAppearanceSettings(page);
      await expect(tile).toHaveAttribute("data-preview-source", "simulated", { timeout: 5_000 });

      const samples = await samplePreview(page, 5_000);
      expect(samples.length).toBeGreaterThan(50);
      expect(samples.filter((s) => s.source !== "simulated")).toEqual([]);
      expect(samples.filter((s) => !s.cls.includes("preview-tile--speaking"))).toEqual([]);
      expect(new Set(samples.map((s) => s.style)).size, "the simulated frame changed").toBe(1);
      expect(samples[0].style).toContain("transition: none;");

      await micToggle(page).dispatchEvent("click");
      await expect(micToggle(page)).toHaveClass(/\bactive\b/, { timeout: 15_000 });
      await expect(selfTile).toHaveAttribute("data-speaking", "true", { timeout: 30_000 });
      await expect(tile).toHaveAttribute("data-preview-source", "mic", { timeout: 10_000 });
      await expect(tile).toHaveAttribute("style", /border-color: rgba\(91, 207, 159,/);
    } finally {
      await browser.close();
    }
  });

  test("the preview is driven by a remote speaker while the local mic is muted", async ({
    baseURL,
  }) => {
    test.setTimeout(240_000);
    const uiURL = baseURL || "http://localhost:3001";
    const host = await launchPage(uiURL, "PreviewRemoteHost");
    const guest = await launchPage(uiURL, "PreviewRemoteGuest", continuousToneWavPath());

    try {
      await host.page.emulateMedia({ reducedMotion: "no-preference" });
      await enterTwoUserMeeting(host.page, guest.page, `e2e_preview_remote_${Date.now()}`);
      await enableMic(guest.page);
      await expect(micToggle(host.page)).not.toHaveClass(/\bactive\b/);

      const guestTile = host.page.locator("#grid-container .grid-item").first();
      await expect(guestTile).toBeVisible({ timeout: 30_000 });
      await expect
        .poll(async () => classifyGlow((await guestTile.getAttribute("style")) || ""), {
          timeout: 45_000,
          message: "expected the guest's tile to glow before the preview is checked",
        })
        .toBe("lit");

      const tile = await openAppearanceSettings(host.page);
      await expect(tile).toHaveAttribute("data-preview-source", "remote", { timeout: 15_000 });
      await expect
        .poll(async () => classifyGlow((await tile.getAttribute("style")) || ""), {
          timeout: 10_000,
          message: "expected the remote-driven preview to be lit",
        })
        .toBe("lit");

      await wakeControls(guest.page);
      await micToggle(guest.page).click({ timeout: 5_000 });
      await expect(micToggle(guest.page)).toHaveClass(/\boff\b/, { timeout: 15_000 });
      await expect(tile).toHaveAttribute("data-preview-source", "simulated", { timeout: 30_000 });
    } finally {
      await host.browser.close();
      await guest.browser.close();
    }
  });
});
