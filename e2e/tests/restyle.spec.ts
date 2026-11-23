import { test, expect } from "@playwright/test";
import { injectSessionCookie } from "../helpers/auth";
import { waitForServices } from "../helpers/wait-for-services";
import {
  cameraButton,
  controlButton,
  joinMeetingFromPage,
  micButton,
  navigateToMeeting,
  shareButton,
} from "../helpers/meeting";

/** Computed `background-color` and `color-scheme` of the element matching `selector`. */
async function paint(page: import("@playwright/test").Page, selector: string) {
  return page.locator(selector).evaluate((el) => {
    const style = getComputedStyle(el);
    return { background: style.backgroundColor, colorScheme: style.colorScheme };
  });
}

test.describe("Unknown routes", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test.beforeEach(async ({ context, baseURL }) => {
    await injectSessionCookie(context, { baseURL });
  });

  test("an unknown URL shows the not-found page with a way home", async ({ page }) => {
    await page.goto("/this/route/does-not-exist");
    await expect(page.getByRole("heading", { name: "404" })).toBeVisible({ timeout: 15_000 });
    await expect(page.getByText("Page not found")).toBeVisible();

    await page.getByRole("link", { name: "Go Home" }).click();
    await expect(page).toHaveURL(/\/$/);
    await expect(page.locator("#meeting-id")).toBeVisible({ timeout: 15_000 });
  });

  test("/404 shows the same page", async ({ page }) => {
    await page.goto("/404");
    await expect(page.getByRole("heading", { name: "404" })).toBeVisible({ timeout: 15_000 });
    await expect(page.getByText("Page not found")).toBeVisible();
  });
});

test.describe("Appearance", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test.beforeEach(async ({ context, baseURL }) => {
    await injectSessionCookie(context, { baseURL });
  });

  test("the landing page follows the system light and dark setting", async ({ page }) => {
    await page.emulateMedia({ colorScheme: "light" });
    await page.goto("/");
    await expect(page.locator("#meeting-id")).toBeVisible({ timeout: 15_000 });
    const light = await paint(page, "body");

    await page.emulateMedia({ colorScheme: "dark" });
    await expect(page.locator("#meeting-id")).toBeVisible();
    const dark = await paint(page, "body");

    expect(dark.background).not.toBe(light.background);
    // The page opts in to both schemes, so the browser draws form controls
    // and scrollbars to match whichever one the system picks.
    for (const scheme of [light.colorScheme, dark.colorScheme]) {
      expect(scheme).toContain("light");
      expect(scheme).toContain("dark");
    }
  });

  test("the in-call screen stays dark under a light system setting", async ({ page }) => {
    await page.emulateMedia({ colorScheme: "light" });
    const landing = await (async () => {
      await page.goto("/");
      await expect(page.locator("#meeting-id")).toBeVisible({ timeout: 15_000 });
      return paint(page, "body");
    })();

    await navigateToMeeting(page, `e2e_theme_${Date.now()}`, "theme-user");
    expect(await joinMeetingFromPage(page)).toBe("in-meeting");

    const call = await paint(page, ".call-root");
    expect(call.colorScheme).toBe("dark");
    const callScreen = await paint(page, "#main-container");
    expect(callScreen.background).not.toBe(landing.background);
    // Dark: every channel of the screen background is below mid-grey.
    const channels = callScreen.background
      .match(/\d+(\.\d+)?/g)!
      .slice(0, 3)
      .map(Number);
    for (const channel of channels) {
      expect(channel).toBeLessThan(128);
    }
  });
});

test.describe("Display name prompt", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test.beforeEach(async ({ context, baseURL }) => {
    await injectSessionCookie(context, { baseURL });
  });

  // On a touch screen focusing the field raises the keyboard over the join
  // button, so the field only takes focus by itself with a mouse.
  test("the name field autofocuses with a mouse and not on a touch screen", async ({
    page,
  }, testInfo) => {
    await page.goto(`/meeting/e2e_focus_${Date.now()}`);
    const input = page.getByTestId("display-name-input");
    await expect(input).toBeVisible({ timeout: 20_000 });
    const coarse = await page.evaluate(() => matchMedia("(pointer: coarse)").matches);
    if (testInfo.project.name === "webkit-iphone") {
      expect(coarse).toBe(true);
    }
    if (coarse) {
      await expect(input).not.toBeFocused();
    } else {
      await expect(input).toBeFocused();
    }
  });
});

test.describe("In-call control bar", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test.beforeEach(async ({ context, page, baseURL }, testInfo) => {
    await injectSessionCookie(context, { baseURL });
    await navigateToMeeting(page, `e2e_ctrl_${testInfo.workerIndex}_${Date.now()}`, "ctrl-user");
    expect(await joinMeetingFromPage(page)).toBe("in-meeting");
    await expect(controlButton(page, "Hang Up")).toBeVisible();
  });

  test("mic, camera and share expose a fixed name and aria-pressed", async ({ page }) => {
    for (const button of [micButton(page), cameraButton(page), shareButton(page)]) {
      await expect(button).toBeVisible();
      await expect(button).toHaveAttribute("aria-pressed", "false");
    }
  });

  test("every control bar button has an accessible name", async ({ page }) => {
    // The tooltips are visibility: hidden until hover or focus, so they do
    // not name the button. Each button needs its own label for VoiceOver.
    const buttons = page.locator(".call-controls button.video-control-button");
    const count = await buttons.count();
    expect(count).toBeGreaterThanOrEqual(4);
    for (let index = 0; index < count; index++) {
      await expect(buttons.nth(index), `button ${index}`).toHaveAccessibleName(/\S/);
    }
    await expect(controlButton(page, "Hang Up")).toHaveAccessibleName("Hang Up");
    await expect(controlButton(page, "Open Peers")).toHaveAccessibleName("Open Peers");
  });

  test("aria-pressed flips when the mic and camera are toggled", async ({ page }) => {
    const mic = micButton(page);
    await mic.click();
    await expect(mic).toHaveAttribute("aria-pressed", "true", { timeout: 10_000 });
    // The name does not change with the state; the tooltip says what a click does.
    await expect(mic).toHaveAccessibleName("Microphone");
    await expect(mic.locator(".tooltip")).toHaveText("Mute");
    await mic.click();
    await expect(mic).toHaveAttribute("aria-pressed", "false", { timeout: 10_000 });
    await expect(mic.locator(".tooltip")).toHaveText("Unmute");

    const camera = cameraButton(page);
    await camera.click();
    await expect(camera).toHaveAttribute("aria-pressed", "true", { timeout: 10_000 });
    await expect(camera).toHaveAccessibleName("Camera");
    await camera.click();
    await expect(camera).toHaveAttribute("aria-pressed", "false", { timeout: 10_000 });
  });

  test.describe("at a phone viewport", () => {
    test.use({ viewport: { width: 390, height: 844 }, hasTouch: true, isMobile: true });

    test("every control bar button has a hit box of at least 44x44", async ({ page }) => {
      await expect(controlButton(page, "Hang Up")).toBeVisible();
      const buttons = page.locator(".call-controls button.video-control-button");
      const count = await buttons.count();
      expect(count).toBeGreaterThanOrEqual(4);

      let checked = 0;
      for (let index = 0; index < count; index++) {
        const button = buttons.nth(index);
        if (!(await button.isVisible())) {
          continue;
        }
        const box = await button.boundingBox();
        expect(box, `button ${index} has a box`).not.toBeNull();
        expect(box!.width, `button ${index} width`).toBeGreaterThanOrEqual(44);
        expect(box!.height, `button ${index} height`).toBeGreaterThanOrEqual(44);
        checked++;
      }
      expect(checked).toBeGreaterThanOrEqual(4);
    });
  });
});
