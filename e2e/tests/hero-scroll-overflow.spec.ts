import { test, expect, type BrowserContext, type Locator, type Page } from "@playwright/test";
import { injectSessionCookie } from "../helpers/auth";
import { createMeeting } from "../helpers/meeting-api";
import { waitForServices } from "../helpers/wait-for-services";

const WIDTH = 1400;
const OWNER = { email: "e2e-585-scrollbar@videocall.rs", name: "Scrollbar585" };

function heroOverflow(hero: Locator): Promise<number> {
  return hero.evaluate((el) => el.scrollHeight - el.clientHeight);
}

function documentOverflow(page: Page): Promise<number> {
  return page.evaluate(() => {
    const root = document.documentElement;
    return root.scrollHeight - root.clientHeight;
  });
}

async function bottomEdge(locator: Locator): Promise<number> {
  const box = await locator.boundingBox({ timeout: 5_000 });
  if (!box) throw new Error(`${locator} has no layout box`);
  return box.y + box.height;
}

async function expectHeroShell(hero: Locator): Promise<void> {
  await expect(hero).toBeVisible({ timeout: 30_000 });
  await expect(hero.locator(".hero-content")).toBeVisible({ timeout: 10_000 });
  await expect(hero.locator(".floating-element")).toHaveCount(3, { timeout: 10_000 });
}

async function expectHeroHasNoPhantomOverflow(hero: Locator, label: string): Promise<void> {
  expect(
    await bottomEdge(hero.locator(".floating-element-2")),
    `${label}: the bottom orb must overhang the hero, or this check proves nothing`,
  ).toBeGreaterThan(await bottomEdge(hero));
  await expect
    .poll(() => heroOverflow(hero), { message: `${label}: hero scroll overflow`, timeout: 5_000 })
    .toBeLessThanOrEqual(1);
}

async function expectNoScrollbarAt(page: Page, hero: Locator, height: number): Promise<void> {
  const label = `${WIDTH}x${height}`;
  await page.setViewportSize({ width: WIDTH, height });
  await expectHeroHasNoPhantomOverflow(hero, label);
  await expect
    .poll(() => documentOverflow(page), {
      message: `${label}: document scroll overflow`,
      timeout: 5_000,
    })
    .toBeLessThanOrEqual(1);
}

async function signInAsOwner(context: BrowserContext, baseURL: string | undefined): Promise<void> {
  await injectSessionCookie(context, { baseURL, ...OWNER });
}

function createOwnedMeeting(tag: string): Promise<string> {
  return createMeeting(OWNER.email, OWNER.name, { meetingId: `e2e_585_${tag}_${Date.now()}` });
}

async function openPreJoin(page: Page, meetingId: string): Promise<Locator> {
  await page.addInitScript((name) => localStorage.setItem("vc_display_name", name), OWNER.name);
  await page.goto(`/meeting/${meetingId}`, { timeout: 30_000 });
  const startButton = page.getByRole("button", { name: "Start Meeting", exact: true });
  await expect(startButton).toBeVisible({ timeout: 30_000 });
  // The lobby grows once the on-mount media auto-request grants and renders the device selects.
  await expect(page.getByTestId("prejoin-mic-select")).toBeVisible({ timeout: 15_000 });
  return startButton;
}

test.describe("Hero page shell shows a scrollbar only when content overflows (#585)", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test("home page has no scrollbar at tall viewports @bvt1", async ({ page, context, baseURL }) => {
    await signInAsOwner(context, baseURL);
    await page.setViewportSize({ width: WIDTH, height: 1200 });
    await page.goto("/", { timeout: 30_000 });
    const hero = page.locator(".hero-container");
    await expectHeroShell(hero);

    for (const height of [1200, 1600]) {
      await expectNoScrollbarAt(page, hero, height);
    }
  });

  test("home page at a short viewport scrolls the document, not a nested hero scroller @bvt1", async ({
    page,
    context,
    baseURL,
  }) => {
    await signInAsOwner(context, baseURL);
    await page.setViewportSize({ width: WIDTH, height: 500 });
    await page.goto("/", { timeout: 30_000 });
    const hero = page.locator(".hero-container");
    await expectHeroShell(hero);

    await expect
      .poll(() => documentOverflow(page), { message: "document scroll overflow", timeout: 5_000 })
      .toBeGreaterThan(0);
    await expectHeroHasNoPhantomOverflow(hero, `${WIDTH}x500`);
  });

  test("pre-join lobby has no scrollbar at a tall viewport @bvt1", async ({
    page,
    context,
    baseURL,
  }) => {
    await signInAsOwner(context, baseURL);
    const meetingId = await createOwnedMeeting("tall");
    await page.setViewportSize({ width: WIDTH, height: 1600 });
    await openPreJoin(page, meetingId);
    const hero = page.locator("#join-meeting-container");
    await expectHeroShell(hero);

    await expectNoScrollbarAt(page, hero, 1600);
  });

  test("pre-join lobby at a short viewport still scrolls its content inside the hero @bvt1", async ({
    page,
    context,
    baseURL,
  }) => {
    await signInAsOwner(context, baseURL);
    const meetingId = await createOwnedMeeting("short");
    await page.setViewportSize({ width: WIDTH, height: 500 });
    const startButton = await openPreJoin(page, meetingId);
    const hero = page.locator("#join-meeting-container");
    await expectHeroShell(hero);
    await expect(startButton).not.toBeInViewport({ timeout: 5_000 });
    expect(
      await heroOverflow(hero),
      "pre-join content must overflow a 500px viewport",
    ).toBeGreaterThan(0);

    await hero.hover({ position: { x: 20, y: 250 }, timeout: 5_000 });
    await expect(async () => {
      await page.mouse.wheel(0, 5_000);
      const distanceFromBottom = await hero.evaluate(
        (el) => el.scrollHeight - el.clientHeight - el.scrollTop,
      );
      expect(
        distanceFromBottom,
        "hero distance from its scroll bottom after a wheel scroll",
      ).toBeLessThanOrEqual(1);
    }).toPass({ timeout: 10_000 });
    await expect(startButton).toBeInViewport({ timeout: 5_000 });
  });

  test("guest join page has no scrollbar at a tall viewport @bvt1", async ({ page }) => {
    await page.setViewportSize({ width: WIDTH, height: 1600 });
    await page.goto(`/meeting/e2e_585_guest_${Date.now()}/guest`, { timeout: 30_000 });
    await expect(page.getByRole("heading", { name: "Join as Guest", exact: true })).toBeVisible({
      timeout: 30_000,
    });
    const hero = page.locator(".hero-container");
    await expectHeroShell(hero);

    await expectNoScrollbarAt(page, hero, 1600);
  });

  test("meeting settings page has no scrollbar at a tall viewport @bvt1", async ({
    page,
    context,
    baseURL,
  }) => {
    await signInAsOwner(context, baseURL);
    const meetingId = await createOwnedMeeting("settings");
    await page.setViewportSize({ width: WIDTH, height: 1600 });
    await page.goto(`/meeting/${meetingId}/settings`, { timeout: 30_000 });
    await expect(page.getByRole("heading", { name: "Meeting Settings", level: 1 })).toBeVisible({
      timeout: 30_000,
    });
    const hero = page.locator(".hero-container");
    await expectHeroShell(hero);

    // Size the viewport to the page's real content, not a fixed constant.
    const contentHeight = await hero
      .locator(".hero-content")
      .evaluate((el) => el.getBoundingClientRect().height);
    const height = Math.max(1600, Math.ceil(contentHeight) + 200);
    await expectNoScrollbarAt(page, hero, height);
  });
});
