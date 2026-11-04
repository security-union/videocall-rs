import { createHash } from "node:crypto";
import * as fs from "node:fs";
import * as path from "node:path";
import {
  test,
  expect,
  chromium,
  Browser,
  BrowserContext,
  Locator,
  Page,
  type Request as PwRequest,
  type Response as PwResponse,
} from "@playwright/test";
import { generateSessionToken, injectSessionCookie } from "../helpers/auth";
import {
  BROWSER_ARGS,
  PIN_WEBSOCKET_TRANSPORT_INIT_SCRIPT,
  createAuthenticatedContext,
} from "../helpers/auth-context";
import { createMeeting } from "../helpers/meeting-api";
import {
  enterMeetingAsHost,
  guestJoinsMeeting,
  joinMeetingFromPage,
} from "../helpers/two-user-meeting";
import { waitForServices } from "../helpers/wait-for-services";

// Avatars persist per identity and writes share a 10-per-minute budget, so every
// test signs in as fresh identities and afterEach deletes their avatars.

const API_URL = process.env.API_BASE_URL || "http://localhost:8081";
const COOKIE_NAME = process.env.COOKIE_NAME || "session";

const FIXTURES = path.resolve(__dirname, "..", "fixtures", "avatars");
const PNG_PATH = path.join(FIXTURES, "ok-64.png");
const GIF_PATH = path.join(FIXTURES, "ok-64.gif");
const PNG = fs.readFileSync(PNG_PATH);
const JPEG = fs.readFileSync(path.join(FIXTURES, "ok-64.jpg"));

const CLIENT_MAX_FILE_BYTES = 2 * 1024 * 1024;

type Rgb = [number, number, number];
const BLUE: Rgb = [0, 0, 255];
const SOLID_TOLERANCE = 60;
const SAME_DOCUMENT_MARK = "__customAvatarSameDocument";
const AVATAR_EVENT_LOG = "Received MEETING packet: event_type=Ok(PARTICIPANT_AVATAR_CHANGED)";

const COPY = {
  noAvatar: "No avatar set. Others see the default icon.",
  uploaded: "Using an uploaded image.",
  gravatar: "Using your Gravatar image.",
  tooLarge: "That image is larger than 2 MB. Choose a smaller file.",
  unsupported: "That file type isn't supported. Use a PNG, JPEG or WebP image.",
  gravatarInvalid: "Enter your Gravatar email or a link like https://gravatar.com/avatar/…",
} as const;

const CAMERA_OFF_INIT_SCRIPT = `(() => {
  try {
    localStorage.setItem("vc_prejoin_camera_on", "false");
  } catch (_) {}
})();`;
const CAMERA_ON_INIT_SCRIPT = `(() => {
  try {
    localStorage.setItem("vc_prejoin_camera_on", "true");
  } catch (_) {}
})();`;
const DECODE_ONE_TILE_INIT_SCRIPT = `(() => {
  try {
    localStorage.setItem("vc_decode_budget_override", "1");
  } catch (_) {}
})();`;

const PREJOIN_PREVIEW = '[data-testid="prejoin-preview"]';
const PEER_AVATAR_IMG =
  ".placeholder-content > .peer-avatar-slot[data-avatar-state='image'] > img.peer-avatar[data-testid='peer-avatar-img']";

interface Identity {
  email: string;
  name: string;
}

interface Participant {
  browser: Browser;
  context: BrowserContext;
  page: Page;
}

interface AvatarRead {
  userId: string | null;
  status: number;
  contentType: string | undefined;
  code?: string;
}

interface PixelRange {
  width: number;
  height: number;
  min: number[];
  max: number[];
}

const identities: Identity[] = [];
const gravatarRequests: string[] = [];

function newIdentity(name: string): Identity {
  const stamp = `${Date.now()}-${Math.random().toString(36).slice(2, 8)}`;
  const identity = { email: `avatar-${name.toLowerCase()}-${stamp}@videocall.rs`, name };
  identities.push(identity);
  return identity;
}

function sessionCookie(identity: Identity): string {
  return `${COOKIE_NAME}=${generateSessionToken(identity.email, identity.name)}`;
}

async function uploadAvatarViaApi(
  identity: Identity,
  body: Buffer,
  contentType: string,
): Promise<void> {
  const res = await fetch(`${API_URL}/api/v1/user/avatar`, {
    method: "PUT",
    headers: { "Content-Type": contentType, Cookie: sessionCookie(identity) },
    body: new Uint8Array(body),
  });
  if (!res.ok) {
    throw new Error(`PUT /api/v1/user/avatar failed (${res.status}): ${await res.text()}`);
  }
}

async function deleteAvatarViaApi(identity: Identity): Promise<number> {
  const res = await fetch(`${API_URL}/api/v1/user/avatar`, {
    method: "DELETE",
    headers: { Cookie: sessionCookie(identity) },
  });
  return res.status;
}

function sha256(bytes: Uint8Array): string {
  return createHash("sha256").update(bytes).digest("hex");
}

async function storedAvatarSha(identity: Identity): Promise<string> {
  const res = await fetch(`${API_URL}/api/v1/user/avatar/image`, {
    headers: { Cookie: sessionCookie(identity) },
  });
  expect(res.status, `GET avatar image of ${identity.email}`).toBe(200);
  return sha256(new Uint8Array(await res.arrayBuffer()));
}

async function storedAvatarEtag(identity: Identity): Promise<string> {
  const res = await fetch(`${API_URL}/api/v1/user/avatar`, {
    headers: { Cookie: sessionCookie(identity) },
  });
  expect(res.status, `GET avatar of ${identity.email}`).toBe(200);
  const etag = (await res.json())?.result?.avatar?.etag;
  expect(typeof etag, `stored avatar etag of ${identity.email}`).toBe("string");
  return etag;
}

async function shownImageSha(img: Locator): Promise<string> {
  const bytes = await img.evaluate(async (el) => {
    const res = await fetch((el as HTMLImageElement).src);
    return Array.from(new Uint8Array(await res.arrayBuffer()));
  });
  return sha256(Uint8Array.from(bytes));
}

async function solidPng(page: Page, rgb: Rgb, side = 64): Promise<Buffer> {
  const bytes = await page.evaluate(
    async ({ rgb, side }) => {
      const canvas = new OffscreenCanvas(side, side);
      const context = canvas.getContext("2d");
      if (!context) throw new Error("no 2d canvas context");
      context.fillStyle = `rgb(${rgb.join(",")})`;
      context.fillRect(0, 0, side, side);
      const blob = await canvas.convertToBlob({ type: "image/png" });
      return Array.from(new Uint8Array(await blob.arrayBuffer()));
    },
    { rgb, side },
  );
  return Buffer.from(bytes);
}

async function shownPixelRange(img: Locator): Promise<PixelRange> {
  return img.evaluate(async (el) => {
    const image = el as HTMLImageElement;
    await image.decode();
    const canvas = document.createElement("canvas");
    canvas.width = image.naturalWidth;
    canvas.height = image.naturalHeight;
    const context = canvas.getContext("2d");
    if (!context) throw new Error("no 2d canvas context");
    context.drawImage(image, 0, 0);
    const { data } = context.getImageData(0, 0, canvas.width, canvas.height);
    const min = [255, 255, 255];
    const max = [0, 0, 0];
    for (let i = 0; i < data.length; i += 4) {
      for (let c = 0; c < 3; c++) {
        min[c] = Math.min(min[c], data[i + c]);
        max[c] = Math.max(max[c], data[i + c]);
      }
    }
    return { width: canvas.width, height: canvas.height, min, max };
  });
}

function offSolid(range: PixelRange, rgb: Rgb): number {
  return Math.max(...[...range.min, ...range.max].map((v, i) => Math.abs(v - rgb[i % 3])));
}

async function peerTileId(tile: Locator): Promise<string> {
  await expect(tile).toHaveAttribute("id", /^peer-video-.+-div$/);
  return (await tile.getAttribute("id")) ?? "";
}

function trackGravatarRequests(context: BrowserContext): void {
  context.on("request", (request) => {
    const host = new URL(request.url()).hostname;
    if (host === "gravatar.com" || host.endsWith(".gravatar.com")) {
      gravatarRequests.push(request.url());
    }
  });
}

function recordMeetingAvatarReads(page: Page, meetingId: string): AvatarRead[] {
  const reads: AvatarRead[] = [];
  page.on("response", (response) => {
    const url = new URL(response.url());
    if (
      response.request().method() === "GET" &&
      url.pathname === `/api/v1/meetings/${meetingId}/avatar`
    ) {
      const read: AvatarRead = {
        userId: url.searchParams.get("user_id"),
        status: response.status(),
        contentType: response.headers()["content-type"],
      };
      reads.push(read);
      if (read.status !== 200) {
        response
          .json()
          .then((body) => {
            read.code = body?.result?.code;
          })
          .catch(() => {});
      }
    }
  });
  return reads;
}

function readsOf(reads: AvatarRead[], userId: string): AvatarRead[] {
  return reads.filter((r) => r.userId === userId);
}

interface OwnAvatarTraffic {
  accountReads: PwRequest[];
  slotReads: PwRequest[];
  settled: Set<PwRequest>;
  eventsAt: number[];
  writesAt: number[];
}

function recordOwnAvatarTraffic(page: Page, meetingId: string, userId: string): OwnAvatarTraffic {
  const traffic: OwnAvatarTraffic = {
    accountReads: [],
    slotReads: [],
    settled: new Set(),
    eventsAt: [],
    writesAt: [],
  };
  page.on("request", (request) => {
    const url = new URL(request.url());
    const method = request.method();
    if (url.pathname === "/api/v1/user/avatar") {
      if (method === "GET") {
        traffic.accountReads.push(request);
      } else if (method === "PUT" || method === "DELETE") {
        traffic.writesAt.push(traffic.eventsAt.length);
      }
    } else if (
      method === "GET" &&
      url.pathname === `/api/v1/meetings/${meetingId}/avatar` &&
      url.searchParams.get("user_id") === userId
    ) {
      traffic.slotReads.push(request);
    }
  });
  const settle = (request: PwRequest) => traffic.settled.add(request);
  page.on("requestfinished", settle);
  page.on("requestfailed", settle);
  page.on("console", (message) => {
    const text = message.text();
    if (text.includes(AVATAR_EVENT_LOG) && text.includes(`target=${userId},`)) {
      traffic.eventsAt.push(traffic.accountReads.length);
    }
  });
  return traffic;
}

function ownReadsInFlight(traffic: OwnAvatarTraffic): number {
  return [...traffic.accountReads, ...traffic.slotReads].filter((r) => !traffic.settled.has(r))
    .length;
}

async function ownReadsSettle(traffic: OwnAvatarTraffic, accountReadsBefore: number) {
  await expect
    .poll(
      () => traffic.accountReads.length > accountReadsBefore && ownReadsInFlight(traffic) === 0,
      { timeout: 20_000, message: "the Profile panel's account re-read was sent and answered" },
    )
    .toBe(true);
}

async function ownEchoApplied(traffic: OwnAvatarTraffic): Promise<void> {
  expect(traffic.writesAt.length, "the owner's tab sent an avatar write").toBeGreaterThan(0);
  const eventsBeforeWrite = traffic.writesAt[traffic.writesAt.length - 1];
  await expect
    .poll(() => traffic.eventsAt.length, {
      timeout: 15_000,
      message: "the owner's tab receives the avatar-change event of its own write",
    })
    .toBeGreaterThan(eventsBeforeWrite);
  const accountReadsAtEvent = traffic.eventsAt[eventsBeforeWrite];
  await expect
    .poll(
      () => traffic.accountReads.length > accountReadsAtEvent && ownReadsInFlight(traffic) === 0,
      {
        timeout: 20_000,
        message: "the owner's tab re-reads its account on that event and every own read finishes",
      },
    )
    .toBe(true);
}

async function holdsFor(page: Page, check: () => Promise<void>, ms = 1_000): Promise<void> {
  const until = Date.now() + ms;
  do {
    await check();
    await page.waitForTimeout(250);
  } while (Date.now() < until);
  await check();
}

function isAvatarApi(method: string, pathname: string) {
  return (response: PwResponse) =>
    response.request().method() === method && new URL(response.url()).pathname === pathname;
}

async function signIn(
  context: BrowserContext,
  identity: Identity,
  baseURL: string | undefined,
): Promise<void> {
  await injectSessionCookie(context, { email: identity.email, name: identity.name, baseURL });
  await context.addInitScript(PIN_WEBSOCKET_TRANSPORT_INIT_SCRIPT);
  await context.addInitScript(CAMERA_OFF_INIT_SCRIPT);
  await context.addInitScript((name) => {
    try {
      localStorage.setItem("vc_display_name", name);
    } catch {
      // storage unavailable before the app origin loads
    }
  }, identity.name);
  trackGravatarRequests(context);
}

async function launchParticipant(
  identity: Identity | null,
  uiURL: string,
  laterInitScripts: string[] = [],
): Promise<Participant> {
  const browser = await chromium.launch({ args: BROWSER_ARGS });
  const context = identity
    ? await createAuthenticatedContext(browser, identity.email, identity.name, uiURL)
    : await browser.newContext({ baseURL: uiURL, ignoreHTTPSErrors: true });
  await context.addInitScript(PIN_WEBSOCKET_TRANSPORT_INIT_SCRIPT);
  await context.addInitScript(CAMERA_OFF_INIT_SCRIPT);
  for (const script of laterInitScripts) {
    await context.addInitScript(script);
  }
  if (identity) {
    await context.addInitScript((name) => {
      try {
        localStorage.setItem("vc_display_name", name);
      } catch {
        // storage unavailable before the app origin loads
      }
    }, identity.name);
  }
  trackGravatarRequests(context);
  return { browser, context, page: await context.newPage() };
}

async function closeAll(participants: Participant[]): Promise<void> {
  await Promise.all(participants.map((p) => p.browser.close()));
}

function tileByName(page: Page, name: string): Locator {
  return page.locator("#grid-container .grid-item", {
    has: page.locator("h4.floating-name", { hasText: name }),
  });
}

function peerAvatarSlot(tile: Locator): Locator {
  return tile.locator(".placeholder-content > .peer-avatar-slot");
}

async function peerAvatarStates(page: Page, name: string): Promise<(string | null)[]> {
  return peerAvatarSlot(tileByName(page, name)).evaluateAll((slots) =>
    slots.map((slot) => slot.getAttribute("data-avatar-state")),
  );
}

function selfAvatarImg(page: Page): Locator {
  return page.locator(
    "#host-controls-nav .placeholder-content .peer-avatar-slot > img[data-testid='self-avatar-img']",
  );
}

function selfAvatarEditButton(page: Page): Locator {
  return page.locator(
    "#host-controls-nav .placeholder-content > button.self-avatar-edit-button[data-testid='self-avatar-edit-button']",
  );
}

function profilePanel(page: Page): Locator {
  return page.locator("#settings-panel-profile");
}

function settingsPreview(scope: Locator): Locator {
  return scope.locator(".avatar-editor > .peer-avatar-slot.avatar-preview");
}

function prejoinAvatarButton(page: Page): Locator {
  return page.locator(
    `${PREJOIN_PREVIEW} .prejoin-video-placeholder > button[data-testid="prejoin-avatar-button"]`,
  );
}

async function gotoPreJoin(page: Page, meetingId: string): Promise<void> {
  await page.goto(`/meeting/${meetingId}`);
  await expect(page.getByRole("button", { name: /Start Meeting|Join Meeting/ })).toBeVisible({
    timeout: 30_000,
  });
  await expect(
    page.locator(PREJOIN_PREVIEW).getByText("Camera is off", { exact: true }),
  ).toBeVisible({ timeout: 30_000 });
}

async function openAvatarDialog(page: Page): Promise<Locator> {
  await prejoinAvatarButton(page).click({ timeout: 10_000 });
  const dialog = page.locator('dialog.avatar-dialog[data-testid="avatar-dialog"]');
  await expect(dialog).toBeVisible({ timeout: 10_000 });
  await expect(dialog.getByTestId("avatar-source-label")).toHaveText(COPY.noAvatar, {
    timeout: 15_000,
  });
  return dialog;
}

async function openProfileSettings(page: Page): Promise<void> {
  const modal = page.locator(".device-settings-modal");
  if (!(await modal.isVisible().catch(() => false))) {
    await page.locator(".video-controls-container").hover();
    const vp = page.viewportSize() ?? { width: 800, height: 600 };
    await page.mouse.move(Math.floor(vp.width / 2), Math.floor(vp.height / 2));
    const gear = page.locator('[data-testid="open-settings"]').first();
    if (await gear.isVisible().catch(() => false)) {
      await gear.click({ force: true, timeout: 10_000 });
    } else {
      const overflowTrigger = page.locator("#overflow-menu-trigger");
      await expect(overflowTrigger).toBeVisible({ timeout: 5_000 });
      await overflowTrigger.click();
      await page
        .locator(".action-bar-overflow-popover .overflow-item", { hasText: /Device settings/i })
        .click({ timeout: 5_000 });
    }
    await expect(modal).toBeVisible({ timeout: 10_000 });
  }
  await modal.getByTestId("settings-nav-profile").click({ timeout: 5_000 });
  await expect(profilePanel(page)).toBeVisible({ timeout: 5_000 });
}

async function expectSettingsOpenOnProfile(
  page: Page,
  sourceLabel: string = COPY.noAvatar,
): Promise<void> {
  const dialog = page.locator("#device-settings-dialog");
  await expect(dialog).toBeVisible({ timeout: 10_000 });
  const tab = dialog.locator("#settings-tab-profile[data-testid='settings-nav-profile']");
  await expect(tab).toHaveCount(1);
  await expect(tab).toHaveAttribute("aria-selected", "true");
  await expect(tab).toBeFocused({ timeout: 5_000 });
  const panel = dialog.locator("#settings-panel-profile");
  await expect(panel).toBeVisible();
  await expect(panel.getByTestId("avatar-source-label")).toHaveText(sourceLabel, {
    timeout: 15_000,
  });
  await expect(panel.locator("#avatar-upload-input")).toHaveCount(1);
}

async function closeSettings(page: Page): Promise<void> {
  const modal = page.locator(".device-settings-modal");
  await modal.getByRole("button", { name: "Close settings" }).click({ timeout: 5_000 });
  await expect(modal).toHaveCount(0, { timeout: 5_000 });
}

test.describe("Custom avatar (issue 3125)", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test.beforeEach(() => {
    identities.length = 0;
    gravatarRequests.length = 0;
  });

  test.afterEach(async () => {
    for (const identity of identities) {
      expect(await deleteAvatarViaApi(identity), `DELETE avatar of ${identity.email}`).toBe(204);
    }
    expect(gravatarRequests, "browser requests to a gravatar.com host").toEqual([]);
  });

  test("a peer sees the avatar uploaded in Settings > Profile on the camera-off tile, with the state label still shown, and the image that replaces it mid-meeting @bvt1", async ({
    baseURL,
  }) => {
    test.setTimeout(150_000);
    const uiURL = baseURL || "http://localhost:3001";
    const owner = newIdentity("AvatarAnn");
    const viewer = newIdentity("AvatarBob");
    const meetingId = `e2e_avatar_peer_${Date.now()}`;
    await createMeeting(owner.email, owner.name, { meetingId, waitingRoomEnabled: false });

    const a = await launchParticipant(owner, uiURL);
    const b = await launchParticipant(viewer, uiURL);
    const own = recordOwnAvatarTraffic(a.page, meetingId, owner.email);
    try {
      await enterMeetingAsHost(a.page, meetingId, owner.name);
      await openProfileSettings(a.page);
      const panel = profilePanel(a.page);
      await expect(panel.getByTestId("avatar-source-label")).toHaveText(COPY.noAvatar, {
        timeout: 15_000,
      });
      await expect(settingsPreview(panel)).toHaveAttribute("data-avatar-state", "default");
      await expect(panel.locator(".avatar-editor-body > p#avatar-upload-help")).toBeVisible();
      await expect(panel.getByText(/\brejoin/i)).toHaveCount(0);

      const upload = a.page.waitForResponse(isAvatarApi("PUT", "/api/v1/user/avatar"), {
        timeout: 20_000,
      });
      await panel.locator("#avatar-upload-input").setInputFiles(PNG_PATH);
      const uploaded = await upload;
      expect(uploaded.status()).toBe(200);
      expect((await uploaded.request().allHeaders())["content-type"]).toMatch(
        /^image\/(webp|jpeg)$/,
      );
      await expect(panel.getByTestId("avatar-source-label")).toHaveText(COPY.uploaded, {
        timeout: 15_000,
      });
      await expect(panel.getByTestId("avatar-status")).toContainText("Avatar updated.");
      await expect(settingsPreview(panel)).toHaveAttribute("data-avatar-state", "image");
      await expect(settingsPreview(panel).getByTestId("settings-avatar-preview")).toHaveAttribute(
        "src",
        /^blob:/,
      );
      await closeSettings(a.page);

      const reads = recordMeetingAvatarReads(b.page, meetingId);
      await guestJoinsMeeting(a.page, b.page, meetingId, viewer.name);

      const tile = tileByName(b.page, owner.name);
      const img = tile.locator(PEER_AVATAR_IMG);
      await expect(img).toBeVisible({ timeout: 45_000 });
      await expect(img).toHaveAttribute("src", /^blob:/);
      await expect(img).toHaveAttribute("alt", "");
      await expect
        .poll(() => img.evaluate((el) => (el as HTMLImageElement).naturalWidth), {
          timeout: 10_000,
        })
        .toBeGreaterThan(0);
      expect(
        offSolid(await shownPixelRange(img), BLUE),
        "image A is not solid blue",
      ).toBeGreaterThan(SOLID_TOLERANCE);
      const ownerTileId = await peerTileId(tile);
      await expect(tile.locator(".placeholder-content > .placeholder-text")).toHaveText(
        /^(Video Disabled|Camera Off)$/,
      );
      await expect(
        tile.locator(".floating-name .peer-avatar, .tile-top-icons .peer-avatar"),
      ).toHaveCount(0);
      await expect
        .poll(() => readsOf(reads, owner.email), { timeout: 10_000 })
        .toContainEqual({ userId: owner.email, status: 200, contentType: "image/jpeg" });

      const selfImg = selfAvatarImg(a.page);
      await expect(selfImg).toBeVisible({ timeout: 15_000 });
      await expect(selfImg).toHaveAttribute("src", /^blob:/);
      const ownerButton = selfAvatarEditButton(a.page);
      await expect(ownerButton.getByTestId("self-avatar-img")).toBeVisible();
      await expect(ownerButton).toHaveAttribute("aria-label", "Change avatar");
      await ownerButton.click();
      await expectSettingsOpenOnProfile(a.page, COPY.uploaded);
      await closeSettings(a.page);
      await expect(ownerButton).toBeFocused({ timeout: 5_000 });

      const readsBeforeReplace = readsOf(reads, owner.email).length;
      await expect
        .poll(() => own.eventsAt.length, {
          timeout: 15_000,
          message: "the owner's tab receives the avatar-change event of its first upload",
        })
        .toBeGreaterThan(0);
      const accountReadsBeforeReopen = own.accountReads.length;
      await ownerButton.click();
      await expectSettingsOpenOnProfile(a.page, COPY.uploaded);
      await ownReadsSettle(own, accountReadsBeforeReopen);
      const replace = a.page.waitForResponse(isAvatarApi("PUT", "/api/v1/user/avatar"), {
        timeout: 20_000,
      });
      await panel.locator("#avatar-upload-input").setInputFiles({
        name: "blue.png",
        mimeType: "image/png",
        buffer: await solidPng(a.page, BLUE),
      });
      expect((await replace).status()).toBe(200);
      await expect(panel.getByTestId("avatar-status")).toContainText("Avatar updated.");
      await closeSettings(a.page);

      await expect(selfImg).toBeVisible();
      await expect(async () => {
        const range = await shownPixelRange(selfImg);
        expect(
          offSolid(range, BLUE),
          `self avatar pixels ${JSON.stringify(range)}`,
        ).toBeLessThanOrEqual(SOLID_TOLERANCE);
      }).toPass({ timeout: 15_000 });
      await expect(img).toBeVisible();
      await expect(async () => {
        const range = await shownPixelRange(img);
        expect(
          offSolid(range, BLUE),
          `peer tile pixels ${JSON.stringify(range)}`,
        ).toBeLessThanOrEqual(SOLID_TOLERANCE);
      }).toPass({ timeout: 15_000 });
      await expect
        .poll(() => readsOf(reads, owner.email).slice(readsBeforeReplace), { timeout: 5_000 })
        .toContainEqual({ userId: owner.email, status: 200, contentType: "image/jpeg" });
      await expect(tile).toHaveAttribute("id", ownerTileId);
      await ownEchoApplied(own);
      await expect(selfImg).toBeVisible();
      await holdsFor(a.page, async () => {
        expect(await selfImg.count(), "the owner's self avatar image").toBe(1);
        const range = await shownPixelRange(selfImg);
        expect(
          offSolid(range, BLUE),
          `self avatar pixels after the owner's own change event ${JSON.stringify(range)}`,
        ).toBeLessThanOrEqual(SOLID_TOLERANCE);
      });
    } finally {
      await closeAll([a, b]);
    }
  });

  test("removing the avatar reverts the owner's views at once, and the peer's tile without a rejoin", async ({
    baseURL,
  }) => {
    test.setTimeout(150_000);
    const uiURL = baseURL || "http://localhost:3001";
    const host = newIdentity("AvatarHal");
    const owner = newIdentity("AvatarAmy");
    const meetingId = `e2e_avatar_remove_${Date.now()}`;
    await createMeeting(host.email, host.name, { meetingId, waitingRoomEnabled: false });
    await uploadAvatarViaApi(owner, PNG, "image/png");

    const h = await launchParticipant(host, uiURL);
    const a = await launchParticipant(owner, uiURL);
    const own = recordOwnAvatarTraffic(a.page, meetingId, owner.email);
    try {
      const reads = recordMeetingAvatarReads(h.page, meetingId);
      await enterMeetingAsHost(h.page, meetingId, host.name);
      await guestJoinsMeeting(h.page, a.page, meetingId, owner.name);

      const ownerTile = tileByName(h.page, owner.name);
      await expect(ownerTile.locator(PEER_AVATAR_IMG)).toBeVisible({ timeout: 45_000 });
      const ownerTileId = await peerTileId(ownerTile);
      await expect(selfAvatarImg(a.page)).toBeVisible({ timeout: 15_000 });
      await expect(selfAvatarEditButton(a.page)).toHaveAttribute("aria-label", "Change avatar");
      await a.page.evaluate((mark) => Reflect.set(window, mark, true), SAME_DOCUMENT_MARK);

      const accountReadsBeforeOpen = own.accountReads.length;
      await openProfileSettings(a.page);
      const panel = profilePanel(a.page);
      await expect(panel.getByTestId("avatar-source-label")).toHaveText(COPY.uploaded, {
        timeout: 15_000,
      });
      await expect(panel.locator(".avatar-editor-body > p#avatar-upload-help")).toBeVisible();
      await expect(panel.getByText(/\brejoin/i)).toHaveCount(0);
      await ownReadsSettle(own, accountReadsBeforeOpen);
      const readsBeforeRemove = readsOf(reads, owner.email).length;
      const removed = a.page.waitForResponse(isAvatarApi("DELETE", "/api/v1/user/avatar"), {
        timeout: 20_000,
      });
      await panel.getByTestId("avatar-remove-button").click({ timeout: 10_000 });
      expect((await removed).status()).toBe(204);
      await expect(settingsPreview(panel)).toHaveAttribute("data-avatar-state", "default", {
        timeout: 10_000,
      });
      await expect(a.page.locator("#avatar-upload-input")).toBeFocused({ timeout: 5_000 });
      await expect(panel.getByTestId("avatar-source-label")).toHaveText(COPY.noAvatar);
      await expect(panel.getByTestId("avatar-status")).toContainText(
        "Avatar removed. Others will see the default icon.",
      );
      await expect(panel.getByTestId("avatar-remove-button")).toHaveCount(0);
      await closeSettings(a.page);
      await expect(a.page.locator("#host-controls-nav .placeholder-content")).toBeVisible();
      await expect(selfAvatarEditButton(a.page)).toHaveAttribute("aria-label", "Add avatar", {
        timeout: 10_000,
      });
      await expect(selfAvatarImg(a.page)).toHaveCount(0, { timeout: 10_000 });

      await expect
        .poll(
          () =>
            readsOf(reads, owner.email)
              .slice(readsBeforeRemove)
              .some((r) => r.status === 404 && r.code === "AVATAR_NOT_FOUND"),
          { timeout: 15_000 },
        )
        .toBe(true);
      await expect
        .poll(() => peerAvatarStates(h.page, owner.name), { timeout: 15_000 })
        .toEqual(["default"]);
      await expect(ownerTile.locator("[data-testid='peer-avatar-img']")).toHaveCount(0);
      await expect(ownerTile).toHaveAttribute("id", ownerTileId);
      await ownEchoApplied(own);
      await expect(selfAvatarEditButton(a.page)).toHaveCount(1);
      await holdsFor(a.page, async () => {
        expect(
          await selfAvatarEditButton(a.page).getAttribute("aria-label", { timeout: 1_000 }),
          "the owner's self avatar label after its own change event",
        ).toBe("Add avatar");
        expect(
          await selfAvatarImg(a.page).count(),
          "the owner's self avatar after its own change event",
        ).toBe(0);
      });
      expect(
        await a.page.evaluate((mark) => Reflect.get(window, mark), SAME_DOCUMENT_MARK),
        "the owner's meeting page was neither reloaded nor navigated",
      ).toBe(true);
    } finally {
      await closeAll([h, a]);
    }
  });

  test("a budget-paused camera-on tile shows the avatar under a PLAY button that still resumes the video", async ({
    baseURL,
  }) => {
    test.setTimeout(240_000);
    const uiURL = baseURL || "http://localhost:3001";
    const host = newIdentity("AvatarHank");
    const first = newIdentity("AvatarCleo");
    const second = newIdentity("AvatarDion");
    const meetingId = `e2e_avatar_paused_${Date.now()}`;
    await createMeeting(host.email, host.name, { meetingId, waitingRoomEnabled: false });
    await uploadAvatarViaApi(first, PNG, "image/png");
    await uploadAvatarViaApi(second, PNG, "image/png");

    const h = await launchParticipant(host, uiURL, [DECODE_ONE_TILE_INIT_SCRIPT]);
    const p1 = await launchParticipant(first, uiURL, [CAMERA_ON_INIT_SCRIPT]);
    const p2 = await launchParticipant(second, uiURL, [CAMERA_ON_INIT_SCRIPT]);
    try {
      await enterMeetingAsHost(h.page, meetingId, host.name);
      await guestJoinsMeeting(h.page, p1.page, meetingId, first.name);
      await guestJoinsMeeting(h.page, p2.page, meetingId, second.name);

      const paused = h.page.locator("#grid-container .grid-item.off-budget-tile");
      await expect(paused).toHaveCount(1, { timeout: 45_000 });
      const slot = paused.locator(".placeholder-content--paused > .peer-avatar-slot");
      await expect(
        slot.locator(":scope > img.peer-avatar[data-testid='peer-avatar-img']"),
      ).toBeVisible({ timeout: 45_000 });
      await expect(paused.locator(".placeholder-content--paused > .placeholder-text")).toHaveText(
        "Video paused",
      );
      const play = slot.locator(":scope > button[data-testid='decode-play-btn']");
      await expect(play).toBeVisible({ timeout: 10_000 });
      const shedName = (await paused.locator("h4.floating-name").first().innerText())
        .trim()
        .split("\n")[0];
      await play.click({ timeout: 10_000 });

      const resumed = tileByName(h.page, shedName);
      await expect(resumed.locator("canvas")).toHaveCount(1, { timeout: 45_000 });
      await expect(resumed.locator("img.peer-avatar")).toHaveCount(0, { timeout: 15_000 });
    } finally {
      await closeAll([h, p1, p2]);
    }
  });

  test("the pre-join avatar button opens the avatar dialog, and Escape or Done closes it with focus back on the button", async ({
    page,
    context,
    baseURL,
  }) => {
    const user = newIdentity("AvatarPia");
    await signIn(context, user, baseURL);
    await gotoPreJoin(page, `e2e_avatar_prejoin_${Date.now()}`);

    const button = prejoinAvatarButton(page);
    await expect(button).toBeVisible({ timeout: 15_000 });
    await expect(button).toHaveAttribute("aria-haspopup", "dialog");
    await expect(button.locator(":scope > .peer-avatar-slot")).toHaveAttribute(
      "data-avatar-state",
      "default",
      { timeout: 15_000 },
    );
    await expect(button).toHaveAttribute("aria-label", "Add avatar");

    const dialog = await openAvatarDialog(page);
    await expect(dialog.locator("#avatar-upload-input")).toBeFocused({ timeout: 5_000 });
    await page.keyboard.press("Escape");
    await expect(dialog).toHaveCount(0, { timeout: 5_000 });
    await expect(button).toBeFocused({ timeout: 5_000 });

    await openAvatarDialog(page);
    await page.getByTestId("avatar-dialog-done").click({ timeout: 5_000 });
    await expect(page.getByTestId("avatar-dialog")).toHaveCount(0, { timeout: 5_000 });
    await expect(button).toBeFocused({ timeout: 5_000 });
  });

  test("an oversize or non-image file is refused with an error and never uploaded", async ({
    page,
    context,
    baseURL,
  }) => {
    test.setTimeout(90_000);
    const user = newIdentity("AvatarRex");
    await signIn(context, user, baseURL);
    const avatarPuts: string[] = [];
    page.on("request", (request) => {
      if (
        request.method() === "PUT" &&
        new URL(request.url()).pathname.startsWith("/api/v1/user/avatar")
      ) {
        avatarPuts.push(request.url());
      }
    });
    await gotoPreJoin(page, `e2e_avatar_refuse_${Date.now()}`);
    const dialog = await openAvatarDialog(page);
    const input = dialog.locator("#avatar-upload-input");
    const error = dialog.getByTestId("avatar-upload-error");

    await input.setInputFiles({
      name: "notes.txt",
      mimeType: "text/plain",
      buffer: Buffer.from("not an image"),
    });
    await expect(error).toHaveText(COPY.unsupported, { timeout: 5_000 });
    await expect(error).toHaveAttribute("role", "alert");
    await expect(input).toHaveAttribute(
      "aria-describedby",
      "avatar-upload-help avatar-upload-error",
    );

    await input.setInputFiles({
      name: "large.png",
      mimeType: "image/png",
      buffer: Buffer.concat([PNG, Buffer.alloc(CLIENT_MAX_FILE_BYTES + 1 - PNG.length)]),
    });
    await expect(error).toHaveText(COPY.tooLarge, { timeout: 5_000 });

    await input.setInputFiles(GIF_PATH);
    await expect(error).toHaveText(COPY.unsupported, { timeout: 5_000 });
    await expect(dialog.getByTestId("avatar-source-label")).toHaveText(COPY.noAvatar);
    expect(avatarPuts).toEqual([]);

    const upload = page.waitForResponse(isAvatarApi("PUT", "/api/v1/user/avatar"), {
      timeout: 20_000,
    });
    await input.setInputFiles(PNG_PATH);
    expect((await upload).status()).toBe(200);
    await expect(dialog.getByTestId("avatar-source-label")).toHaveText(COPY.uploaded, {
      timeout: 15_000,
    });
    await expect(error).toHaveCount(0);
    expect(avatarPuts).toHaveLength(1);
  });

  test("the server refuses a non-Gravatar link, and a Gravatar save shows the copied image without the browser contacting gravatar.com", async ({
    page,
    context,
    baseURL,
  }) => {
    test.setTimeout(90_000);
    const user = newIdentity("AvatarGus");
    const meetingId = `e2e_avatar_gravatar_${Date.now()}`;
    await signIn(context, user, baseURL);
    const browserImageReads: string[] = [];
    page.on("request", (request) => {
      if (new URL(request.url()).pathname === "/api/v1/user/avatar/image") {
        browserImageReads.push(request.url());
      }
    });
    await gotoPreJoin(page, meetingId);
    const dialog = await openAvatarDialog(page);
    const input = dialog.locator("#avatar-gravatar-input");
    const error = dialog.getByTestId("avatar-gravatar-error");
    await expect(input).toBeVisible({ timeout: 15_000 });

    const refused = page.waitForResponse(isAvatarApi("PUT", "/api/v1/user/avatar/gravatar"), {
      timeout: 20_000,
    });
    await input.fill(`https://evilgravatar.com/avatar/${"0".repeat(32)}`);
    await dialog.getByTestId("avatar-gravatar-save").click({ timeout: 5_000 });
    const refusal = await refused;
    expect(new URL(refusal.url()).searchParams.get("expected_user_id")).toBe(user.email);
    expect(refusal.status()).toBe(400);
    expect((await refusal.json()).result.code).toBe("INVALID_GRAVATAR");
    await expect(error).toHaveText(COPY.gravatarInvalid, { timeout: 5_000 });
    await expect(error).toHaveAttribute("role", "alert");
    await expect(input).toHaveAttribute("aria-invalid", "true");

    const email = "avatar-gravatar-e2e@example.com";
    const hash = createHash("sha256").update(email).digest("hex");
    const etag = createHash("sha256").update(JPEG).digest("hex").slice(0, 32);
    const gravatarSaves: { expectedUserId: string | null; body: unknown }[] = [];
    let ownImageReads = 0;
    await page.route(
      (url) => url.pathname === "/api/v1/user/avatar/gravatar",
      async (route) => {
        if (route.request().method() !== "PUT") {
          await route.fallback();
          return;
        }
        gravatarSaves.push({
          expectedUserId: new URL(route.request().url()).searchParams.get("expected_user_id"),
          body: route.request().postDataJSON(),
        });
        await route.fulfill({
          status: 200,
          contentType: "application/json",
          body: JSON.stringify({
            success: true,
            result: {
              source: "gravatar",
              gravatar_url: `https://gravatar.com/avatar/${hash}`,
              content_type: "image/jpeg",
              etag,
              updated_at: Date.now(),
            },
          }),
        });
      },
    );
    await page.route(
      (url) =>
        url.pathname === `/api/v1/meetings/${meetingId}/avatar` &&
        url.searchParams.get("user_id") === user.email,
      async (route) => {
        if (route.request().method() !== "GET" || gravatarSaves.length === 0) {
          await route.fallback();
          return;
        }
        ownImageReads += 1;
        await route.fulfill({
          status: 200,
          contentType: "image/jpeg",
          headers: { etag: `"${etag}"`, "cache-control": "private, no-cache" },
          body: JPEG,
        });
      },
    );

    await input.fill(email);
    await expect(error).toHaveCount(0);
    await input.press("Enter");
    await expect(dialog.getByTestId("avatar-source-label")).toHaveText(COPY.gravatar, {
      timeout: 15_000,
    });
    await expect(dialog.getByTestId("avatar-status")).toContainText("Gravatar image saved.");
    const preview = settingsPreview(dialog).getByTestId("settings-avatar-preview");
    await expect(preview).toHaveAttribute("src", /^blob:/, { timeout: 15_000 });
    expect(await shownImageSha(preview)).toBe(sha256(JPEG));
    expect(gravatarSaves).toEqual([{ expectedUserId: user.email, body: { gravatar: email } }]);
    expect(ownImageReads).toBeGreaterThanOrEqual(1);

    await page.keyboard.press("Escape");
    await expect(dialog).toHaveCount(0, { timeout: 5_000 });
    await expect(prejoinAvatarButton(page)).toHaveAttribute("aria-label", "Change avatar");
    const prejoinImg = prejoinAvatarButton(page).getByTestId("prejoin-avatar-img");
    await expect(prejoinImg).toHaveAttribute("src", /^blob:/);
    expect(await shownImageSha(prejoinImg)).toBe(sha256(JPEG));
    expect(browserImageReads, "own-avatar image reads outside the meeting route").toEqual([]);
    expect(gravatarRequests).toEqual([]);
  });

  test("a guest is told to sign in under Settings > Profile, gets no pre-join or self-tile avatar button, and is never looked up", async ({
    baseURL,
  }) => {
    test.setTimeout(240_000);
    const uiURL = baseURL || "http://localhost:3001";
    const host = newIdentity("AvatarHost");
    const guestName = "AvatarGuest";
    const meetingId = `e2e_avatar_guest_${Date.now()}`;
    await createMeeting(host.email, host.name, {
      meetingId,
      waitingRoomEnabled: false,
      allowGuests: true,
    });

    const h = await launchParticipant(host, uiURL);
    const g = await launchParticipant(null, uiURL);
    try {
      const hostReads = recordMeetingAvatarReads(h.page, meetingId);
      const guestOwnAvatarCalls: string[] = [];
      g.page.on("request", (request) => {
        if (new URL(request.url()).pathname.startsWith("/api/v1/user/avatar")) {
          guestOwnAvatarCalls.push(request.url());
        }
      });
      await enterMeetingAsHost(h.page, meetingId, host.name);

      await g.page.goto(`/meeting/${meetingId}/guest`);
      const nameField = g.page.locator("#guest-name");
      await expect(nameField).toBeVisible({ timeout: 20_000 });
      await nameField.fill(guestName);
      await nameField.press("Enter");
      const preview = g.page.locator(PREJOIN_PREVIEW);
      await expect(preview.getByText("Camera is off", { exact: true })).toBeVisible({
        timeout: 30_000,
      });
      await expect(preview.getByTestId("prejoin-avatar-button")).toHaveCount(0);
      expect(await joinMeetingFromPage(g.page)).toBe("in-meeting");

      const guestPlaceholder = g.page.locator("#host-controls-nav .placeholder-content");
      await expect(guestPlaceholder).toBeVisible({ timeout: 20_000 });
      await expect(
        guestPlaceholder.locator(":scope > svg, :scope > .peer-avatar-slot"),
      ).toHaveCount(1);
      await expect(g.page.getByTestId("self-avatar-edit-button")).toHaveCount(0);
      await expect(g.page.locator(".self-avatar-edit-badge")).toHaveCount(0);

      await expect(peerAvatarSlot(tileByName(h.page, guestName))).toHaveAttribute(
        "data-avatar-state",
        "default",
        { timeout: 45_000 },
      );

      await openProfileSettings(g.page);
      const panel = profilePanel(g.page);
      const note = panel.getByTestId("avatar-guest-note");
      await expect(note).toBeVisible({ timeout: 10_000 });
      await expect(note).toContainText("Sign in to set an avatar.");
      await expect(note).toContainText("Guests appear with the default icon.");
      await expect(panel.locator("#avatar-upload-input")).toHaveCount(0);
      await expect(panel.locator("#avatar-gravatar-input")).toHaveCount(0);
      expect(guestOwnAvatarCalls).toEqual([]);
      await expect
        .poll(() => readsOf(hostReads, host.email).length, { timeout: 10_000 })
        .toBeGreaterThan(0);
      expect(hostReads.filter((read) => read.userId !== host.email)).toEqual([]);
    } finally {
      await closeAll([h, g]);
    }
  });

  test("arrow keys typed in the Gravatar field do not switch the active settings tab", async ({
    page,
    context,
    baseURL,
  }) => {
    test.setTimeout(120_000);
    const user = newIdentity("AvatarKai");
    await signIn(context, user, baseURL);
    await gotoPreJoin(page, `e2e_avatar_keys_${Date.now()}`);
    expect(await joinMeetingFromPage(page)).toBe("in-meeting");
    await openProfileSettings(page);

    const modal = page.locator(".device-settings-modal");
    const profileTab = modal.getByTestId("settings-nav-profile");
    const panel = profilePanel(page);
    const input = panel.locator("#avatar-gravatar-input");
    const save = panel.getByTestId("avatar-gravatar-save");
    await expect(input).toBeVisible({ timeout: 15_000 });
    await expect(save).toHaveAttribute("aria-disabled", "true");

    await input.focus();
    await page.keyboard.press("ArrowRight");
    await page.keyboard.press("ArrowDown");
    await page.keyboard.type("a");
    await expect(save).toHaveAttribute("aria-disabled", "false", { timeout: 5_000 });
    await expect(profileTab).toHaveAttribute("aria-selected", "true");
    await expect(input).toBeFocused();

    await page.keyboard.press("ArrowLeft");
    await page.keyboard.press("ArrowUp");
    await page.keyboard.press("Delete");
    await expect(save).toHaveAttribute("aria-disabled", "true", { timeout: 5_000 });
    await expect(input).toHaveValue("");
    await expect(profileTab).toHaveAttribute("aria-selected", "true");
    await expect(input).toBeFocused();

    await profileTab.focus();
    await page.keyboard.press("ArrowRight");
    await expect(modal.getByTestId("settings-nav-audio")).toHaveAttribute("aria-selected", "true", {
      timeout: 5_000,
    });
    await expect(page.locator("#settings-panel-audio")).toBeVisible();
  });

  test("after another account signs in to the same browser, the meeting keeps its user's avatar and its tab refuses to change either account's", async ({
    baseURL,
  }) => {
    test.setTimeout(150_000);
    const uiURL = baseURL || "http://localhost:3001";
    const owner = newIdentity("AvatarOla");
    const other = newIdentity("AvatarOtto");
    const meetingId = `e2e_avatar_identity_${Date.now()}`;
    await createMeeting(owner.email, owner.name, { meetingId, waitingRoomEnabled: false });
    await uploadAvatarViaApi(owner, PNG, "image/png");
    await uploadAvatarViaApi(other, JPEG, "image/jpeg");
    const ownerImage = await storedAvatarSha(owner);
    const otherEtag = await storedAvatarEtag(other);
    expect(await storedAvatarSha(other)).not.toBe(ownerImage);

    const a = await launchParticipant(owner, uiURL);
    try {
      const accountsRead: string[] = [];
      const writes: string[] = [];
      a.page.on("request", (request) => {
        const url = new URL(request.url());
        if (request.method() !== "GET" && url.pathname.startsWith("/api/v1/user/avatar")) {
          writes.push(`${request.method()} ${url.pathname}`);
        }
      });
      a.page.on("response", (response) => {
        if (isAvatarApi("GET", "/api/v1/user/avatar")(response)) {
          response
            .json()
            .then((body) => accountsRead.push(body?.result?.user_id))
            .catch(() => {});
        }
      });
      await enterMeetingAsHost(a.page, meetingId, owner.name);
      const selfImg = selfAvatarImg(a.page);
      await expect(selfImg).toBeVisible({ timeout: 15_000 });
      expect(await shownImageSha(selfImg)).toBe(ownerImage);

      const panel = profilePanel(a.page);
      const upload = panel.locator("#avatar-upload-input");
      const remove = panel.getByTestId("avatar-remove-button");
      const gravatarSave = panel.getByTestId("avatar-gravatar-save");
      const notice = panel.getByTestId("avatar-signed-in-elsewhere");
      const checkAgain = panel.getByTestId("avatar-check-again");
      const signInAs = (who: Identity) =>
        injectSessionCookie(a.context, { email: who.email, name: who.name, baseURL: uiURL });
      const expectLocked = async (locked: boolean) => {
        const value = locked ? "true" : "false";
        await expect(upload).toHaveAttribute("aria-disabled", value, { timeout: 10_000 });
        await expect(remove).toHaveAttribute("aria-disabled", value);
        await expect(gravatarSave).toHaveAttribute("aria-disabled", value);
      };

      const firstRead = a.page.waitForResponse(isAvatarApi("GET", "/api/v1/user/avatar"), {
        timeout: 20_000,
      });
      await openProfileSettings(a.page);
      expect((await (await firstRead).json()).result.user_id).toBe(owner.email);
      await expect(panel.getByTestId("avatar-source-label")).toHaveText(COPY.uploaded, {
        timeout: 15_000,
      });
      await expect(upload).toHaveAttribute("aria-disabled", "false");
      await expect(notice).toHaveCount(0);
      await closeSettings(a.page);

      await signInAs(other);
      await selfAvatarEditButton(a.page).click({ timeout: 10_000 });
      await expect(panel).toBeVisible({ timeout: 10_000 });
      await expect(notice).toBeVisible({ timeout: 20_000 });
      await expect.poll(() => accountsRead, { timeout: 10_000 }).toContain(other.email);
      await expect(notice).toHaveAttribute("role", "alert");
      await expect(checkAgain).toBeVisible();
      await panel.locator("#avatar-gravatar-input").fill("someone@example.com");
      await expectLocked(true);
      await expect(remove).toHaveAttribute("aria-describedby", "avatar-signed-in-elsewhere");
      expect(await shownImageSha(selfImg)).toBe(ownerImage);
      await upload.setInputFiles(PNG_PATH);
      await expect(upload).toHaveValue("");
      await remove.click({ force: true, timeout: 10_000 });
      await expect(remove).toBeFocused();
      await gravatarSave.click({ force: true, timeout: 10_000 });
      await expect(gravatarSave).toBeFocused();
      await expect(notice).toBeVisible();

      await signInAs(owner);
      const recheck = a.page.waitForResponse(isAvatarApi("GET", "/api/v1/user/avatar"), {
        timeout: 20_000,
      });
      await checkAgain.click({ timeout: 10_000 });
      expect((await (await recheck).json()).result.user_id).toBe(owner.email);
      expect(writes, "avatar writes sent while another account was signed in").toEqual([]);
      await expect(notice).toHaveCount(0, { timeout: 10_000 });
      await expect(checkAgain).toHaveCount(0);
      await expect(upload).toBeFocused({ timeout: 5_000 });
      const status = panel.getByTestId("avatar-status");
      await expect(status).toHaveAttribute("role", "status");
      await expect(status).toContainText("You can change your avatar again.");
      await expectLocked(false);

      await signInAs(other);
      const refused = a.page.waitForResponse(isAvatarApi("DELETE", "/api/v1/user/avatar"), {
        timeout: 20_000,
      });
      await remove.click({ timeout: 10_000 });
      const response = await refused;
      expect(new URL(response.url()).searchParams.get("expected_user_id")).toBe(owner.email);
      expect(response.status()).toBe(409);
      await expect(notice).toBeVisible({ timeout: 10_000 });
      await expectLocked(true);
      await expect(selfImg).toBeVisible();
      expect(await shownImageSha(selfImg)).toBe(ownerImage);
      expect(writes).toEqual(["DELETE /api/v1/user/avatar"]);
    } finally {
      await closeAll([a]);
    }

    const direct = await fetch(
      `${API_URL}/api/v1/user/avatar?expected_user_id=${encodeURIComponent(owner.email)}`,
      {
        method: "PUT",
        headers: { "Content-Type": "image/png", Cookie: sessionCookie(other) },
        body: new Uint8Array(PNG),
      },
    );
    expect(direct.status).toBe(409);
    expect((await direct.json()).result.code).toBe("AVATAR_IDENTITY_MISMATCH");
    expect(await storedAvatarEtag(other)).toBe(otherEtag);
    expect(await storedAvatarSha(owner)).toBe(ownerImage);
  });

  const PLACEMENT_ANCHOR = {
    grid: "#host-controls-nav .self-tile-camera-off > .placeholder-content > button#self-avatar-edit-button",
    corner:
      "#host-controls-nav .placeholder-content > button#self-avatar-edit-button + span.placeholder-text",
  } as const;

  for (const placement of ["corner", "grid"] as const) {
    test(`the camera-off self avatar (${placement}) shows a pencil on hover or keyboard focus, opens Settings on the Profile tab and gets focus back on close`, async ({
      page,
      context,
      baseURL,
    }) => {
      test.setTimeout(120_000);
      const user = newIdentity("AvatarEve");
      await signIn(context, user, baseURL);
      await context.addInitScript((value) => {
        try {
          localStorage.setItem("vc_self_view_placement", value);
        } catch {
          // storage unavailable before the app origin loads
        }
      }, placement);
      await gotoPreJoin(page, `e2e_avatar_selftile_${placement}_${Date.now()}`);
      expect(await joinMeetingFromPage(page)).toBe("in-meeting");
      await expect(page.locator("#host-controls-nav")).toHaveAttribute(
        "data-self-placement",
        placement,
        { timeout: 20_000 },
      );

      const button = selfAvatarEditButton(page);
      await expect(button).toHaveCount(1, { timeout: 20_000 });
      await expect(page.locator(PLACEMENT_ANCHOR[placement])).toHaveCount(1);
      await expect(button).toHaveAttribute("type", "button");
      await expect(button).toHaveAttribute("aria-haspopup", "dialog");
      const slot = button.locator(":scope > .peer-avatar-slot");
      await expect(slot).toHaveCount(1);
      await expect(slot).toHaveAttribute("data-avatar-state", "default", { timeout: 15_000 });
      await expect(button).toHaveAttribute("aria-label", "Add avatar");
      await expect(button.locator(":scope > svg")).toHaveCount(0);
      const badge = button.locator(":scope > span.self-avatar-edit-badge[aria-hidden='true']");
      await expect(badge).toHaveCount(1);
      const modal = page.locator(".device-settings-modal");

      await page.mouse.move(0, 0);
      await expect(badge).toBeHidden();
      await button.hover();
      await expect(badge).toBeVisible({ timeout: 5_000 });
      await button.click();
      await expectSettingsOpenOnProfile(page);
      await closeSettings(page);
      await expect(button).toBeFocused({ timeout: 5_000 });

      await page.mouse.move(0, 0);
      await page.evaluate(() => (document.activeElement as HTMLElement | null)?.blur());
      await expect(badge).toBeHidden();
      await button.focus();
      await page.keyboard.press("Shift+Tab");
      await page.keyboard.press("Tab");
      await expect(button).toBeFocused();
      await expect(badge).toBeVisible({ timeout: 5_000 });
      await page.keyboard.press("Enter");
      await expectSettingsOpenOnProfile(page);
      await page.keyboard.press("Escape");
      await expect(modal).toHaveCount(0, { timeout: 5_000 });
      await expect(button).toBeFocused({ timeout: 5_000 });
      await expect(badge).toBeVisible();

      await button.click();
      await expectSettingsOpenOnProfile(page);
      await page.locator(".device-settings-modal-overlay").click({ position: { x: 5, y: 5 } });
      await expect(modal).toHaveCount(0, { timeout: 5_000 });
      await expect(button).toBeFocused({ timeout: 5_000 });

      await openProfileSettings(page);
      await closeSettings(page);
      await page.waitForTimeout(500);
      await expect(button).not.toBeFocused();
    });
  }
});
