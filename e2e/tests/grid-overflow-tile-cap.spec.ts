import { test, expect, chromium, Browser, Locator, Page } from "@playwright/test";
import {
  BROWSER_ARGS,
  createAuthenticatedContext,
  pinWebSocketTransport,
} from "../helpers/auth-context";
import { wakeActionBar } from "../helpers/controls";
import { createMeeting } from "../helpers/meeting-api";
import { joinRelayPeer, RelayPeer } from "../helpers/relay-peers";
import { CANVAS_LIMIT } from "../helpers/rust-mirrored-constants";
import { enterMeetingAsHost } from "../helpers/two-user-meeting";
import { waitForServices } from "../helpers/wait-for-services";

// Real sessions, not Mock Peers: mocks are added after the cap, so they cannot reach it.

const HOST_NAME = "TileCapHost";
const HOST_EMAIL = "tile-cap-host@videocall.rs";
const VIEWPORT = { width: 1280, height: 720 };
const JOIN_WAVE_TIMEOUT_MS = 30_000;
const KEEPALIVE_HOLD_MS = 45_000;

const GRID = "#grid-container";
const PEER_TILES = `${GRID} > .ss-peer-panel > .tile-slot > [data-tile-root="true"]`;
const OVERFLOW_BADGE = `${GRID} > .ss-peer-panel > button#grid-overflow-badge.grid-overflow-badge`;
const OVERFLOW_LIST = '#grid-overflow-list[role="dialog"]';
const OVERFLOW_TITLE = `${OVERFLOW_LIST} > .grid-overflow-list-header > h2#grid-overflow-list-title`;
const OVERFLOW_CLOSE = `${OVERFLOW_LIST} > .grid-overflow-list-header > button#grid-overflow-list-close`;
const OVERFLOW_ROWS = `${OVERFLOW_LIST} > ul.grid-overflow-list-rows > li`;
const OVERFLOW_ROW_BUTTONS = `${OVERFLOW_ROWS} > button.grid-overflow-row`;
const OVERFLOW_ROW_NAMES = `${OVERFLOW_ROW_BUTTONS} > .grid-overflow-row-name`;
const SELF_IN_GRID = 'nav#host-controls-nav[data-self-placement="grid"]';

const QUIET_JOINS = {
  vc_appearance_entry_notifications: "false",
  vc_appearance_entry_sound: "false",
};
const SELF_VIEW_IN_GRID = { vc_self_view_placement: "grid" };

function seedStorage(entries: Record<string, string>): void {
  try {
    Object.entries(entries).forEach(([key, value]) => window.localStorage.setItem(key, value));
  } catch {
    /* no storage on this document */
  }
}

async function openMaximumOption(host: Page): Promise<Locator> {
  const maximum = host.locator('.density-popover[role="menu"] > #density-opt-maximum');
  await wakeActionBar(host);
  await host.locator("#density-mode-trigger").click({ timeout: 10_000 });
  await expect(maximum).toBeVisible({ timeout: 5_000 });
  return maximum;
}

async function setMaximumDensity(host: Page): Promise<void> {
  const maximum = await openMaximumOption(host);
  await maximum.click({ timeout: 10_000 });
  await expect(host.locator(".density-popover")).toHaveCount(0);
  await expect(host.locator("#density-mode-trigger")).toHaveAttribute(
    "title",
    "Layout density: Maximum",
  );
}

function namesOnScreen(host: Page): Promise<string[]> {
  return host.evaluate(
    ({ tiles, rows }) => {
      const text = (el: Element | null) => (el?.textContent ?? "").trim();
      return [
        ...Array.from(document.querySelectorAll(tiles), (tile) =>
          text(tile.querySelector(".floating-name-text")),
        ),
        ...Array.from(document.querySelectorAll(rows), text),
      ].sort();
    },
    { tiles: PEER_TILES, rows: OVERFLOW_ROW_NAMES },
  );
}

async function expectEveryPeerOnce(host: Page, peers: RelayPeer[]): Promise<void> {
  await expect
    .poll(() => namesOnScreen(host), {
      timeout: 10_000,
      message: "every peer is either a tile or a row of the list, and none is both",
    })
    .toEqual(peers.map((p) => p.name).sort());
}

async function expectOverflowList(host: Page, peers: RelayPeer[]): Promise<void> {
  const badge = host.locator(OVERFLOW_BADGE);
  const list = host.locator(OVERFLOW_LIST);
  const hidden = peers.length - CANVAS_LIMIT;
  await expect(host.locator(PEER_TILES)).toHaveCount(CANVAS_LIMIT);
  await expect(badge).toHaveAttribute("data-overflow-count", String(hidden));
  await expect(badge).toHaveText(new RegExp(`^\\+${hidden}\\s*more in meeting$`));

  await badge.click({ timeout: 10_000 });
  await expect(badge).toHaveAttribute("aria-expanded", "true");
  await expect(list).toBeVisible();
  await expect(host.locator(OVERFLOW_TITLE)).toHaveText(`${hidden} more in meeting`);
  await expect(host.locator(OVERFLOW_ROWS)).toHaveCount(hidden);
  await expectEveryPeerOnce(host, peers);

  await host.locator(OVERFLOW_CLOSE).click({ timeout: 10_000 });
  await expect(list).toHaveCount(0);
  await expect(badge).toHaveAttribute("aria-expanded", "false");
}

test.describe("Maximum density past the tile cap (#3309)", () => {
  let browser: Browser | undefined;
  let peers: RelayPeer[] = [];

  const joinUpTo = async (meetingId: string, total: number) => {
    while (peers.length < total) {
      const name = `CapPeer${String(peers.length + 1).padStart(2, "0")}`;
      peers.push(await joinRelayPeer(meetingId, name));
    }
  };

  const startHost = async (baseURL: string | undefined, seed: Record<string, string>) => {
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_tile_cap_${Date.now()}`;
    await createMeeting(HOST_EMAIL, HOST_NAME, {
      meetingId,
      waitingRoomEnabled: false,
      allowGuests: false,
      endOnHostLeave: false,
    });
    browser = await chromium.launch({ args: BROWSER_ARGS });
    const context = await createAuthenticatedContext(browser, HOST_EMAIL, HOST_NAME, uiURL);
    await pinWebSocketTransport(context);
    await context.addInitScript(seedStorage, seed);
    const host = await context.newPage();
    await host.setViewportSize(VIEWPORT);
    await enterMeetingAsHost(host, meetingId, HOST_NAME);
    await setMaximumDensity(host);
    return { host, meetingId };
  };

  test.beforeAll(async () => {
    await waitForServices();
  });

  test.afterEach(async () => {
    peers.forEach((peer) => peer.leave());
    peers = [];
    await browser?.close();
    browser = undefined;
  });

  test("the first real peer past the cap reaches the +N badge, and so does the tenth @bvt1", async ({
    baseURL,
  }) => {
    test.setTimeout(180_000);
    const { host, meetingId } = await startHost(baseURL, QUIET_JOINS);
    const tiles = host.locator(PEER_TILES);
    const badge = host.locator(OVERFLOW_BADGE);

    await test.step("at the cap every peer holds a tile and nothing is hidden", async () => {
      await joinUpTo(meetingId, CANVAS_LIMIT);
      await expect(tiles).toHaveCount(CANVAS_LIMIT, { timeout: JOIN_WAVE_TIMEOUT_MS });
      await expect(badge).toHaveCount(0);
    });

    await test.step("one peer past the cap", async () => {
      await joinUpTo(meetingId, CANVAS_LIMIT + 1);
      await expect(badge).toBeVisible({ timeout: JOIN_WAVE_TIMEOUT_MS });
      await expectOverflowList(host, peers);
    });

    await test.step("ten peers past the cap", async () => {
      await joinUpTo(meetingId, CANVAS_LIMIT + 10);
      await expect(badge).toHaveAttribute("data-overflow-count", "10", {
        timeout: JOIN_WAVE_TIMEOUT_MS,
      });
      await expectOverflowList(host, peers);
    });

    await test.step("pinning a hidden peer from the list gives it a tile", async () => {
      await badge.click({ timeout: 10_000 });
      const row = host.locator(OVERFLOW_ROW_BUTTONS).last();
      const name = await row.locator(":scope > .grid-overflow-row-name").innerText();
      await row.click({ timeout: 10_000 });
      await host.mouse.move(0, 0);
      await expect(
        tiles.filter({
          has: host.locator(".floating-name-text", { hasText: new RegExp(`^${name}$`) }),
        }),
      ).toHaveCount(1, { timeout: 10_000 });
      await expect(badge).toHaveAttribute("data-overflow-count", "10");
      await expect(tiles).toHaveCount(CANVAS_LIMIT);
      await expectEveryPeerOnce(host, peers);
      await host.locator(OVERFLOW_CLOSE).click({ timeout: 10_000 });
      await expect(host.locator(OVERFLOW_LIST)).toHaveCount(0);
    });

    await test.step("the Maximum option states the cap", async () => {
      const maximum = await openMaximumOption(host);
      await expect(maximum).toHaveAttribute("aria-checked", "true");
      await expect(maximum.locator(":scope > .density-option-range")).toHaveText(
        `Up to ${CANVAS_LIMIT} people`,
      );
    });
  });

  test("with the self view in the grid, peers past the cap stay listed across the keepalive window", async ({
    baseURL,
  }) => {
    test.setTimeout(180_000);
    const { host, meetingId } = await startHost(baseURL, {
      ...QUIET_JOINS,
      ...SELF_VIEW_IN_GRID,
    });
    await expect(host.locator(SELF_IN_GRID)).toHaveCount(1);

    await joinUpTo(meetingId, CANVAS_LIMIT + 10);
    await expect(host.locator(OVERFLOW_BADGE)).toHaveAttribute("data-overflow-count", "10", {
      timeout: JOIN_WAVE_TIMEOUT_MS,
    });
    await expectOverflowList(host, peers);

    await host.waitForTimeout(KEEPALIVE_HOLD_MS);
    await expectOverflowList(host, peers);
  });
});
