import { createSocket, Socket } from "node:dgram";
import { readFileSync } from "node:fs";
import { AddressInfo } from "node:net";
import { resolve } from "node:path";
import { test, expect, Browser, BrowserContext, Page } from "@playwright/test";
import { createAuthenticatedContext, pinWebSocketTransport } from "../helpers/auth-context";
import { wakeControls } from "../helpers/controls";
import { enableDiagnosticsTileIndicators } from "../helpers/diagnostics-tile-indicators";
import { fillAndSubmitJoinForm } from "../helpers/join-meeting";
import { installMediaSocketRecorder, severMediaWebSocket } from "../helpers/media-socket-sever";
import { createMeeting, fetchMeetingState } from "../helpers/meeting-api";
import { joinMeetingFromPage } from "../helpers/two-user-meeting";
import { setRuntimeConfig } from "../helpers/runtime-config";
import { clickJoinAndEnterGrid } from "../helpers/screen-share-meeting";
import { waitForServices } from "../helpers/wait-for-services";
import {
  announcements,
  CARD_SELECTOR,
  CARD_TITLE,
  installConnectionCardRecorder,
  phases,
  settledCardLog,
} from "../helpers/connection-status-card";

const TITLE = CARD_TITLE;
// Copied from dioxus-ui/src/components/transport_fallback.rs.
const FALLBACK_NOTE =
  "This call is using WebSocket because WebTransport isn't working well on this device or network.";
const REVERSE_FALLBACK_NOTE =
  "This call is using WebTransport because WebSocket isn't working well on this device or network.";
const NOTE_TITLE = "Not using your preferred protocol";
const CURRENT_LEAD = "Currently using:";
const PREFERRED_LABEL = "Preferred protocol";
const WT_OPTION_DESC = "Switches to WebSocket automatically when a network blocks it.";
const WS_OPTION_DESC = "Tries WebSocket first; uses WebTransport only if WebSocket can't connect.";
const COUNTDOWN_DELAY_MS = 1_500;

// Short enough to keep the run fast, long enough for the countdown to show.
const WT_TIMEOUT_MS = 3_000;
const WS_TIMEOUT_MS = 2_000;
// Keeps the handover's WebSocket candidate unelected well past the relay's
// departure grace.
const WS_HOLD_MS = 10_000;
// A closed loopback UDP port: the WebTransport handshake is refused at once.
const REFUSING_WT_HOST = "https://127.0.0.1:4499";

const LOBBY_SOCKET = /\/lobby/;
const RECONNECT_HOLD_CAP_MS = 1_500;
const RECONNECT_TIMEOUT_MS = 45_000;
const RECONNECTING_LOG = /Connection state changed: Reconnecting/;
const UI_CONNECTION_LOST_LOG = /DIOXUS-UI: Connection lost \(/;

const SEL = {
  card: CARD_SELECTOR,
  networkPanel: "#settings-panel-network",
  note: '#settings-panel-network [data-testid="transport-in-use-mismatch"]',
  current: '#settings-panel-network [data-testid="transport-current"]',
  currentReading: ":scope > .transport-current-reading",
  currentValue: ':scope > [data-testid="transport-current-value"]',
  currentBadge: ':scope > [data-testid="transport-current-badge"]',
  preferredLabel: "#settings-panel-network #transport-segmented-label",
  preferredGroup:
    '#settings-panel-network [role="radiogroup"][aria-labelledby="transport-segmented-label"]',
  wtRadio: '[data-testid="transport-radio-webtransport"]',
  wsRadio: '[data-testid="transport-radio-websocket"]',
  selfBadgeWs: '.transport-badge[aria-label="Your connection transport: WebSocket"]',
  selfBadgeWt: '.transport-badge[aria-label="Your connection transport: WebTransport"]',
};

/** `RECONNECT_GRACE_PERIOD` in actix-api/src/constants.rs. */
function relayReconnectGraceMs(): number {
  const src = readFileSync(resolve(__dirname, "../../actix-api/src/constants.rs"), "utf8");
  const secs = src.match(
    /^pub const RECONNECT_GRACE_PERIOD: Duration = Duration::from_secs\((\d+)\);/m,
  );
  expect(secs, "RECONNECT_GRACE_PERIOD moved or changed shape").not.toBeNull();
  return Number(secs![1]) * 1000;
}

/** A UDP port that swallows every datagram, so a WebTransport dial hangs. */
async function silentUdpHost(): Promise<{ host: string; socket: Socket }> {
  const socket = createSocket("udp4");
  await new Promise<void>((resolve) => socket.bind(0, "127.0.0.1", resolve));
  const { port } = socket.address() as AddressInfo;
  return { host: `https://127.0.0.1:${port}`, socket };
}

interface Joined {
  context: BrowserContext;
  page: Page;
  prefLines: string[];
  lines: string[];
  stamped: Array<{ at: number; text: string }>;
}

async function joinAlone(
  browser: Browser,
  baseURL: string,
  tag: string,
  config: Record<string, string | number>,
  opts: {
    pinWebSocket?: boolean;
    beforeJoin?: (page: Page) => Promise<void>;
    keepMeetingOnHostLeave?: boolean;
    prepare?: (context: BrowserContext) => Promise<void>;
  } = {},
): Promise<Joined> {
  const name = `Fallback${tag}`;
  const unique = `${Date.now()}_${Math.random().toString(36).slice(2, 8)}`;
  const meetingId = `e2e_3044_${tag}_${unique}`;
  const email = `fallback-${tag}-${unique}@videocall.rs`;
  if (opts.keepMeetingOnHostLeave) {
    await createMeeting(email, name, {
      meetingId,
      waitingRoomEnabled: false,
      allowGuests: false,
      endOnHostLeave: false,
    });
  }
  const context = await createAuthenticatedContext(browser, email, name, baseURL);
  const keys = { transportBadgeEnabled: "true", ...config };
  await setRuntimeConfig(context, keys);
  await enableDiagnosticsTileIndicators(context);
  if (opts.pinWebSocket) {
    await pinWebSocketTransport(context);
  }
  await installConnectionCardRecorder(context);
  await opts.prepare?.(context);

  const page = await context.newPage();
  const prefLines: string[] = [];
  const lines: string[] = [];
  const stamped: Array<{ at: number; text: string }> = [];
  page.on("console", (msg) => {
    const text = msg.text();
    if (text.includes("Transport preference applied:")) prefLines.push(text);
    lines.push(text);
    stamped.push({ at: Date.now(), text });
  });
  await opts.beforeJoin?.(page);

  await fillAndSubmitJoinForm(page, meetingId, name);
  await clickJoinAndEnterGrid(page);

  const applied = await page.evaluate((wanted) => {
    const cfg = (window as unknown as { __APP_CONFIG?: Record<string, unknown> }).__APP_CONFIG;
    return Object.fromEntries(wanted.map((key) => [key, cfg?.[key]]));
  }, Object.keys(keys));
  expect(applied, "the runtime config patch must reach the app").toEqual(keys);

  await expect.poll(() => prefLines.length, { timeout: 20_000 }).toBeGreaterThan(0);
  return { context, page, prefLines, lines, stamped };
}

async function electedDecision(lines: string[]): Promise<string> {
  const find = () =>
    lines.find((l) => l.includes("Election decision:") && l.includes("outcome=elected"));
  await expect.poll(find, { timeout: 30_000 }).toBeTruthy();
  return find()!;
}

async function openNetworkSettings(page: Page): Promise<void> {
  await wakeControls(page);
  await page.locator('[data-testid="open-settings"]').click({ timeout: 10_000 });
  await expect(page.locator(".device-settings-modal")).toBeVisible({ timeout: 10_000 });
  await page.locator('[data-testid="settings-nav-network"]').click({ timeout: 10_000 });
  await expect(page.locator(SEL.networkPanel)).toBeVisible({ timeout: 10_000 });
}

async function expectNetworkSettingsOpen(page: Page): Promise<void> {
  await expect(page.locator(".device-settings-modal")).toBeVisible();
  await expect(page.locator(SEL.networkPanel)).toBeVisible();
}

type ProtocolLabel = "WebTransport" | "WebSocket" | "Not connected";

const CURRENT_BADGE = {
  webtransport: { text: "WT", modifier: "transport-badge--wt", other: "transport-badge--ws" },
  websocket: { text: "WS", modifier: "transport-badge--ws", other: "transport-badge--wt" },
} as const;

function currentRowText(label: ProtocolLabel, badgeText?: string): RegExp {
  const badge = badgeText ? `\\s*${badgeText}` : "";
  return new RegExp(`^${escapeRegExp(CURRENT_LEAD)}\\s+${label}${badge}$`);
}

async function expectCurrentProtocol(
  page: Page,
  state: "webtransport" | "websocket" | "none",
  label: ProtocolLabel,
  timeout?: number,
): Promise<void> {
  const current = page.locator(SEL.current);
  await expect(current, "the Network panel must name the protocol in use").toBeVisible();
  await expect(current).toHaveAttribute("data-state", state, { timeout });
  await expect(current.locator(":scope > span").first()).toHaveText(CURRENT_LEAD);
  const reading = current.locator(SEL.currentReading);
  await expect(reading).toHaveCount(1);
  const value = reading.locator(SEL.currentValue);
  await expect(value).toHaveCount(1);
  await expect(value).toHaveText(label);
  const badge = reading.locator(SEL.currentBadge);
  if (state === "none") {
    await expect(badge).toHaveCount(0);
    await expect(current).toHaveText(currentRowText(label));
    return;
  }
  const { text, modifier, other } = CURRENT_BADGE[state];
  await expect(badge).toHaveCount(1);
  await expect(badge).toBeVisible();
  await expect(badge).toContainClass(`transport-badge ${modifier}`);
  await expect(badge).not.toContainClass(other);
  await expect(badge).toHaveText(text);
  await expect(badge).toHaveAttribute("aria-hidden", "true");
  await expect(current).toHaveText(currentRowText(label, text));
}

function escapeRegExp(text: string): string {
  return text.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

async function storedTransport(
  page: Page,
): Promise<{ pref: string | null; fallback: string | null }> {
  return page.evaluate(() => ({
    pref: localStorage.getItem("vc_transport_preference"),
    fallback: sessionStorage.getItem("vc_transport_fallback"),
  }));
}

test.describe("Connection-status card and protocol fallback (#2895, #3044)", () => {
  let silent: { host: string; socket: Socket };

  test.beforeAll(async () => {
    await waitForServices();
    silent = await silentUdpHost();
  });

  test.afterAll(() => {
    silent?.socket.close();
  });

  test("a WebTransport join that hangs walks the card from preferred to fallback to WebSocket @bvt1", async ({
    browser,
    baseURL,
  }) => {
    test.setTimeout(120_000);
    const { context, page, prefLines, lines } = await joinAlone(browser, baseURL!, "wt2ws", {
      webTransportEnabled: "true",
      webTransportHost: silent.host,
      webTransportConnectTimeoutMs: WT_TIMEOUT_MS,
    });
    try {
      expect(prefLines[0]).toContain("pref=webtransport source=default");
      const decision = await electedDecision(lines);
      expect(decision).toContain("preferred=wt phase=fallback");
      expect(decision).toMatch(/ active=ws_\d/);
      expect(decision).toContain("transport_pick=fallback_timeout ");

      const log = await settledCardLog(page, TITLE.connectedWs);
      expect(phases(log)).toEqual(["preferred", "fallback", "connected"]);

      const preferred = log.filter((s) => s.phase === "preferred");
      expect(preferred[0]).toMatchObject({
        title: TITLE.preferred,
        detail: "Using WebTransport",
        countdown: false,
      });
      const countdownAt = preferred.find((s) => s.countdown)?.at;
      expect(countdownAt, "the countdown must show during a 3 s phase").toBeDefined();
      // The first frame may land up to one 250 ms tick either side of 1.5 s.
      expect(countdownAt! - preferred[0].at).toBeGreaterThanOrEqual(COUNTDOWN_DELAY_MS - 250);

      expect(log.find((s) => s.phase === "fallback")).toMatchObject({
        title: TITLE.fallback,
        detail: "Switching to WebSocket",
      });
      expect(log.find((s) => s.phase === "connected")).toMatchObject({
        title: TITLE.connectedWs,
        detail: null,
        countdown: false,
      });

      expect(announcements(log)).toEqual([
        `${TITLE.preferred} Using WebTransport`,
        `${TITLE.fallback} Switching to WebSocket`,
        TITLE.connectedWs,
      ]);

      await expect(page.locator(SEL.selfBadgeWs)).toHaveCount(1, { timeout: 15_000 });
      expect(await storedTransport(page)).toEqual({ pref: null, fallback: "webtransport" });

      await openNetworkSettings(page);
      await expectCurrentProtocol(page, "websocket", "WebSocket");
      await expect(page.locator(SEL.wtRadio)).toHaveAttribute("aria-checked", "true");
      const note = page.locator(SEL.note);
      await expect(note).toBeVisible();
      await expect(note.locator(".settings-info-panel-title")).toHaveText(NOTE_TITLE);
      await expect(note.locator(".settings-info-panel-text")).toHaveText(FALLBACK_NOTE);
    } finally {
      await context.close();
    }
  });

  test("a second join in the same tab after a fallback starts in the fallback phase, and a WebTransport win clears it", async ({
    browser,
    baseURL,
  }) => {
    test.setTimeout(150_000);
    const { context, page, lines } = await joinAlone(browser, baseURL!, "remember", {
      webTransportEnabled: "true",
      webTransportHost: silent.host,
      webTransportConnectTimeoutMs: WT_TIMEOUT_MS,
    });
    try {
      expect(phases(await settledCardLog(page, TITLE.connectedWs))).toEqual([
        "preferred",
        "fallback",
        "connected",
      ]);
      expect(await storedTransport(page)).toEqual({ pref: null, fallback: "webtransport" });

      // A reload keeps the tab's sessionStorage and re-runs the recorder.
      lines.length = 0;
      await page.reload();
      await clickJoinAndEnterGrid(page);

      const log = await settledCardLog(page, TITLE.connectedWs);
      expect(phases(log)).toEqual(["fallback", "connected"]);
      expect(log.find((s) => s.phase === "fallback")).toMatchObject({
        title: TITLE.fallback,
        detail: "Switching to WebSocket",
      });
      const decision = await electedDecision(lines);
      expect(decision).toContain("preferred=wt phase=fallback");
      expect(decision).toMatch(/ active=ws_\d/);

      expect(await storedTransport(page)).toEqual({ pref: null, fallback: "webtransport" });

      // WebTransport reachable again: it wins from the fallback phase and the
      // tab forgets the fallback.
      await context.unroute("**/config.js");
      await context.unroute("**/config.local.js");
      await setRuntimeConfig(context, { transportBadgeEnabled: "true" });
      lines.length = 0;
      await page.reload();
      await clickJoinAndEnterGrid(page);
      const healed = await settledCardLog(page, TITLE.connectedWt);
      expect(phases(healed)).toEqual(["fallback", "connected"]);
      expect((await electedDecision(lines)).match(/ active=(\S+)/)?.[1]).toMatch(/^wt_\d/);
      await expect(page.locator(SEL.selfBadgeWt)).toHaveCount(1, { timeout: 15_000 });
      expect(await storedTransport(page)).toEqual({ pref: null, fallback: null });
    } finally {
      await context.close();
    }
  });

  test("a WebTransport join that connects shows Connected and no fallback @bvt1", async ({
    browser,
    baseURL,
  }) => {
    test.setTimeout(120_000);
    const { context, page, prefLines, lines } = await joinAlone(browser, baseURL!, "wt", {});
    try {
      expect(prefLines[0]).toContain("pref=webtransport source=default");
      expect(prefLines[0]).toMatch(/wt_urls=1 /);
      const decision = await electedDecision(lines);
      expect(decision).toContain("preferred=wt phase=preferred");
      expect(decision).toMatch(/ active=wt_\d/);
      expect(decision).toContain("transport_pick=preferred ");

      const log = await settledCardLog(page, TITLE.connectedWt);
      expect(phases(log)).toEqual(["preferred", "connected"]);
      expect(log.find((s) => s.phase === "preferred")).toMatchObject({
        title: TITLE.preferred,
        detail: "Using WebTransport",
      });
      expect(log.find((s) => s.phase === "connected")).toMatchObject({
        title: TITLE.connectedWt,
        detail: null,
      });
      await expect(page.locator(SEL.selfBadgeWt)).toHaveCount(1, { timeout: 30_000 });
      expect(await storedTransport(page)).toEqual({ pref: null, fallback: null });

      await openNetworkSettings(page);
      await expectCurrentProtocol(page, "webtransport", "WebTransport");
      const current = page.locator(SEL.current);
      await expect(current).toHaveAttribute("aria-live", "polite");
      await expect(current).toHaveAttribute("aria-atomic", "true");
      const label = page.locator(SEL.preferredLabel);
      await expect(label).toHaveCount(1);
      await expect(label).toHaveText(PREFERRED_LABEL);
      const group = page.locator(SEL.preferredGroup);
      await expect(group).toHaveCount(1);
      await expect(group).toHaveAttribute("aria-describedby", "transport-current");
      await expect(group).toHaveAccessibleDescription(currentRowText("WebTransport"));
      await expect(page.locator(SEL.wtRadio)).toHaveAttribute("aria-checked", "true");
      await expect(page.locator(SEL.wtRadio)).toHaveAccessibleDescription(WT_OPTION_DESC);
      await expect(page.locator(SEL.wsRadio)).toHaveAccessibleDescription(WS_OPTION_DESC);
      await expect(page.locator(SEL.note)).toHaveCount(0);
    } finally {
      await context.close();
    }
  });

  test("a WebSocket preference elects WebSocket without dialling WebTransport", async ({
    browser,
    baseURL,
  }) => {
    test.setTimeout(120_000);
    const { context, page, prefLines, lines } = await joinAlone(
      browser,
      baseURL!,
      "pinws",
      {},
      { pinWebSocket: true },
    );
    try {
      expect(prefLines[0]).toContain("pref=websocket source=sticky");
      expect(prefLines[0]).toMatch(/wt_urls=1 /);
      const decision = await electedDecision(lines);
      expect(decision).toContain("preferred=ws phase=preferred");
      expect(decision).toMatch(/ active=ws_\d/);
      expect(decision).toContain("transport_pick=preferred ");

      const log = await settledCardLog(page, TITLE.connectedWs);
      expect(phases(log)).toEqual(["preferred", "connected"]);
      expect(log.find((s) => s.phase === "preferred")).toMatchObject({
        title: TITLE.preferred,
        detail: "Using WebSocket",
      });
      await expect(page.locator(SEL.selfBadgeWs)).toHaveCount(1, { timeout: 30_000 });

      const dials = lines.filter((l) => l.includes("Election candidates:"));
      expect(dials.length).toBeGreaterThan(0);
      for (const dial of dials) expect(dial).toContain(" 0 WebTransport");
      expect(lines.some((l) => /WebTransport candidate\(s\) deferred/.test(l))).toBe(true);

      await openNetworkSettings(page);
      await expectCurrentProtocol(page, "websocket", "WebSocket");
      await expect(page.locator(SEL.wsRadio)).toHaveAttribute("aria-checked", "true");
      await expect(page.locator(SEL.note)).toHaveCount(0);
    } finally {
      await context.close();
    }
  });

  test("a refused WebTransport handshake falls back without waiting out its timeout", async ({
    browser,
    baseURL,
  }) => {
    test.setTimeout(120_000);
    const { context, page, lines } = await joinAlone(browser, baseURL!, "wtrefused", {
      webTransportEnabled: "true",
      webTransportHost: REFUSING_WT_HOST,
      webTransportConnectTimeoutMs: 10_000,
    });
    try {
      const decision = await electedDecision(lines);
      expect(decision).toContain("preferred=wt phase=fallback");
      expect(decision).toMatch(/ active=ws_\d/);
      expect(decision).toContain("transport_pick=fallback_failed ");
      expect(Number(decision.match(/election_duration_ms=(\d+)/)?.[1])).toBeLessThan(5_000);

      const log = await settledCardLog(page, TITLE.connectedWs);
      expect(phases(log).slice(-2)).toEqual(["fallback", "connected"]);
      const preferredAt = log.find((s) => s.phase === "preferred")?.at;
      const fallbackAt = log.find((s) => s.phase === "fallback")!.at;
      if (preferredAt !== undefined) {
        expect(fallbackAt - preferredAt).toBeLessThan(5_000);
      }
      await expect(page.locator(SEL.selfBadgeWs)).toHaveCount(1, { timeout: 15_000 });
    } finally {
      await context.close();
    }
  });

  test("a WebSocket preference whose handshake stalls falls back to WebTransport @bvt1", async ({
    browser,
    baseURL,
  }) => {
    test.setTimeout(120_000);
    const { context, page, stamped, lines } = await joinAlone(
      browser,
      baseURL!,
      "ws2wt",
      { webSocketConnectTimeoutMs: WS_TIMEOUT_MS },
      {
        pinWebSocket: true,
        // Never connecting to the server keeps every media socket CONNECTING.
        beforeJoin: (page) =>
          page.routeWebSocket(/localhost:8080/, () => new Promise<void>(() => undefined)),
      },
    );
    try {
      const decision = await electedDecision(lines);
      expect(decision).toContain("preferred=ws phase=fallback");
      expect(decision).toMatch(/ active=wt_\d/);
      expect(decision).toContain("transport_pick=fallback_timeout ");

      const startedAt = stamped.find((l) =>
        l.text.startsWith("Starting connection election: preferred=ws"),
      )!.at;
      const dials = stamped.filter((l) => l.text.includes("Election candidates:"));
      expect(dials[0].text).toContain("1 WebSocket, 0 WebTransport");
      const wtDialAt = dials.find((l) => !l.text.includes(" 0 WebTransport"))?.at;
      expect(wtDialAt, "WebTransport must be dialled once WebSocket times out").toBeDefined();
      expect(wtDialAt! - startedAt).toBeGreaterThanOrEqual(WS_TIMEOUT_MS - 200);

      const log = await settledCardLog(page, TITLE.connectedWt);
      expect(phases(log)).toEqual(["preferred", "fallback", "connected"]);
      expect(log.find((s) => s.phase === "preferred")).toMatchObject({
        title: TITLE.preferred,
        detail: "Using WebSocket",
      });
      expect(log.find((s) => s.phase === "fallback")).toMatchObject({
        title: TITLE.fallback,
        detail: "Switching to WebTransport",
      });
      expect(announcements(log).at(-1)).toBe(TITLE.connectedWt);

      await expect(page.locator(SEL.selfBadgeWt)).toHaveCount(1, { timeout: 15_000 });
      expect(await storedTransport(page)).toEqual({ pref: "websocket", fallback: "websocket" });

      await openNetworkSettings(page);
      await expectCurrentProtocol(page, "webtransport", "WebTransport");
      await expect(page.locator(SEL.wsRadio)).toHaveAttribute("aria-checked", "true");
      const note = page.locator(SEL.note);
      await expect(note).toBeVisible();
      await expect(note.locator(".settings-info-panel-title")).toHaveText(NOTE_TITLE);
      await expect(note.locator(".settings-info-panel-text")).toHaveText(REVERSE_FALLBACK_NOTE);
    } finally {
      await context.close();
    }
  });

  test("no fallback when the deployment disables WebTransport", async ({ browser, baseURL }) => {
    test.setTimeout(120_000);
    const { context, page, prefLines } = await joinAlone(browser, baseURL!, "wtoff", {
      webTransportEnabled: "false",
    });
    try {
      expect(prefLines[0]).toContain("wt_urls=0");
      await expect(page.locator(SEL.selfBadgeWs)).toHaveCount(1, { timeout: 30_000 });
      const log = await settledCardLog(page, TITLE.connectedWs);
      expect(phases(log)).not.toContain("fallback");

      expect(
        (await storedTransport(page)).fallback,
        "nothing may write the tab's memory",
      ).toBeNull();

      await openNetworkSettings(page);
      await expectCurrentProtocol(page, "websocket", "WebSocket");
      await expect(page.locator(SEL.wtRadio)).toHaveText("WebTransport (unavailable)");
      await expect(page.locator(SEL.wtRadio)).toHaveAccessibleDescription(
        "Unavailable on this deployment.",
      );
      await expect(page.locator(SEL.note)).toHaveCount(0);
    } finally {
      await context.close();
    }
  });

  test("a host's same-tab rejoins over WebSocket, the remembered fallback, then WebTransport keep a peer's meeting running", async ({
    browser,
    baseURL,
  }) => {
    test.setTimeout(240_000);
    const unique = `${Date.now()}_${Math.random().toString(36).slice(2, 8)}`;
    const meetingId = `e2e_3044_rejoin_${unique}`;
    const HOST = { email: `rejoin-host-${unique}@videocall.rs`, name: "RejoinHost" };
    const PEER = { email: `rejoin-peer-${unique}@videocall.rs`, name: "RejoinPeer" };
    await createMeeting(HOST.email, HOST.name, { meetingId, waitingRoomEnabled: false });

    const hostCtx = await createAuthenticatedContext(browser, HOST.email, HOST.name, baseURL!);
    await setRuntimeConfig(hostCtx, {
      transportBadgeEnabled: "true",
      webTransportEnabled: "true",
      webTransportHost: silent.host,
      webTransportConnectTimeoutMs: WT_TIMEOUT_MS,
    });
    await enableDiagnosticsTileIndicators(hostCtx);
    await installConnectionCardRecorder(hostCtx);
    const peerCtx = await createAuthenticatedContext(browser, PEER.email, PEER.name, baseURL!);
    const host = await hostCtx.newPage();
    const peer = await peerCtx.newPage();
    try {
      await fillAndSubmitJoinForm(host, meetingId, HOST.name);
      expect(await joinMeetingFromPage(host)).toBe("in-meeting");
      await settledCardLog(host, TITLE.connectedWs);
      await fillAndSubmitJoinForm(peer, meetingId, PEER.name);
      expect(await joinMeetingFromPage(peer)).toBe("in-meeting");

      await host.reload();
      expect(await joinMeetingFromPage(host)).toBe("in-meeting");
      expect(phases(await settledCardLog(host, TITLE.connectedWs))).toEqual([
        "fallback",
        "connected",
      ]);

      await hostCtx.unroute("**/config.js");
      await hostCtx.unroute("**/config.local.js");
      await setRuntimeConfig(hostCtx, { transportBadgeEnabled: "true" });
      await host.reload();
      expect(await joinMeetingFromPage(host)).toBe("in-meeting");
      await settledCardLog(host, TITLE.connectedWt);
      await expect(host.locator(SEL.selfBadgeWt)).toHaveCount(1, { timeout: 15_000 });

      await host.waitForTimeout(10_000);
      for (const page of [host, peer]) {
        await expect(page.locator(".meeting-ended-message")).toHaveCount(0);
        await expect(page.locator("#grid-container")).toBeVisible();
      }
      await expect(
        peer.locator(".floating-name-text", { hasText: HOST.name }),
        "the peer must still see the host",
      ).not.toHaveCount(0, { timeout: 15_000 });
      expect(await fetchMeetingState(HOST.email, HOST.name, meetingId)).toBe("active");
    } finally {
      await hostCtx.close();
      await peerCtx.close();
    }
  });

  test("a host handing over to a tab whose WebSocket candidate loses to WebTransport keeps the meeting running @bvt1", async ({
    browser,
    baseURL,
  }) => {
    test.setTimeout(180_000);
    const unique = `${Date.now()}_${Math.random().toString(36).slice(2, 8)}`;
    const meetingId = `e2e_3044_handover_${unique}`;
    const HOST = { email: `handover-host-${unique}@videocall.rs`, name: "HandoverHost" };
    const PEER = { email: `handover-peer-${unique}@videocall.rs`, name: "HandoverPeer" };
    await createMeeting(HOST.email, HOST.name, { meetingId, waitingRoomEnabled: false });

    const oldCtx = await createAuthenticatedContext(browser, HOST.email, HOST.name, baseURL!);
    await pinWebSocketTransport(oldCtx);
    const newCtx = await createAuthenticatedContext(browser, HOST.email, HOST.name, baseURL!);
    await setRuntimeConfig(newCtx, { webSocketConnectTimeoutMs: WS_HOLD_MS });
    await pinWebSocketTransport(newCtx);
    await installConnectionCardRecorder(newCtx);
    const peerCtx = await createAuthenticatedContext(browser, PEER.email, PEER.name, baseURL!);
    const oldTab = await oldCtx.newPage();
    const newTab = await newCtx.newPage();
    const peer = await peerCtx.newPage();
    try {
      await fillAndSubmitJoinForm(oldTab, meetingId, HOST.name);
      expect(await joinMeetingFromPage(oldTab)).toBe("in-meeting");
      await fillAndSubmitJoinForm(peer, meetingId, PEER.name);
      expect(await joinMeetingFromPage(peer)).toBe("in-meeting");

      // The new tab's WebSocket candidate reaches the relay but never gets an
      // RTT echo, so it stays unactivated until WebTransport wins.
      const sent = new Set<string>();
      let relayAnswered = false;
      const key = (f: string | Buffer) =>
        typeof f === "string" ? `s:${f}` : `b:${f.toString("hex")}`;
      await newTab.routeWebSocket(/localhost:8080/, (ws) => {
        const server = ws.connectToServer();
        ws.onMessage((f) => {
          sent.add(key(f));
          server.send(f);
        });
        server.onMessage((f) => {
          relayAnswered = true;
          if (!sent.has(key(f))) ws.send(f);
        });
      });
      const lines: string[] = [];
      const stamped: Array<{ at: number; text: string }> = [];
      newTab.on("console", (msg) => {
        lines.push(msg.text());
        stamped.push({ at: Date.now(), text: msg.text() });
      });
      await fillAndSubmitJoinForm(newTab, meetingId, HOST.name);
      const joined = clickJoinAndEnterGrid(newTab);
      joined.catch(() => {});
      await expect.poll(() => relayAnswered, { timeout: 30_000 }).toBe(true);
      const electionStart = stamped.find((l) =>
        l.text.startsWith("Starting connection election: preferred=ws"),
      );
      expect(electionStart, "the new tab's election must be running").toBeDefined();
      expect(
        lines.filter((l) => l.includes("Election decision:")),
        "the old tab must leave while the new tab's WebSocket candidate is still unelected",
      ).toEqual([]);
      const closedAt = Date.now();
      await oldTab.close();
      await joined;
      await settledCardLog(newTab, TITLE.connectedWt);
      const decision = stamped.find(
        (l) => l.text.includes("Election decision:") && l.text.includes("outcome=elected"),
      )!;
      expect(decision.text.match(/ active=(\S+)/)?.[1]).toMatch(/^wt_\d/);
      expect(decision.at - electionStart!.at).toBeGreaterThanOrEqual(WS_HOLD_MS - 1_000);
      expect(
        decision.at - closedAt,
        "the relay's departure grace must run out before the candidate closes",
      ).toBeGreaterThan(relayReconnectGraceMs() + 2_000);

      await newTab.waitForTimeout(10_000);
      for (const page of [newTab, peer]) {
        await expect(page.locator(".meeting-ended-message")).toHaveCount(0);
        await expect(page.locator("#grid-container")).toBeVisible();
      }
      await expect(peer.locator(".floating-name-text", { hasText: HOST.name })).not.toHaveCount(0, {
        timeout: 15_000,
      });
      expect(await fetchMeetingState(HOST.email, HOST.name, meetingId)).toBe("active");
    } finally {
      await oldCtx.close();
      await newCtx.close();
      await peerCtx.close();
    }
  });

  test("a waiting-room observer starts in the fallback phase only in a tab that fell back", async ({
    browser,
    baseURL,
  }) => {
    test.setTimeout(150_000);
    const unique = `${Date.now()}_${Math.random().toString(36).slice(2, 8)}`;
    const meetingId = `e2e_3044_observer_${unique}`;
    const OWNER = { email: `observer-owner-${unique}@videocall.rs`, name: "ObserverOwner" };
    await createMeeting(OWNER.email, OWNER.name, { meetingId, waitingRoomEnabled: true });
    const ownerCtx = await createAuthenticatedContext(browser, OWNER.email, OWNER.name, baseURL!);
    const owner = await ownerCtx.newPage();

    const decisionFor = async (tag: string, rememberedFallback: boolean): Promise<string> => {
      const context = await createAuthenticatedContext(
        browser,
        `observer-${tag}-${unique}@videocall.rs`,
        `Observer${tag}`,
        baseURL!,
      );
      if (rememberedFallback) {
        await context.addInitScript(() =>
          sessionStorage.setItem("vc_transport_fallback", "webtransport"),
        );
      }
      const page = await context.newPage();
      const lines: string[] = [];
      page.on("console", (msg) => lines.push(msg.text()));
      try {
        await fillAndSubmitJoinForm(page, meetingId, `Observer${tag}`);
        expect(await joinMeetingFromPage(page)).toBe("waiting");
        return await electedDecision(lines);
      } finally {
        await context.close();
      }
    };

    try {
      await fillAndSubmitJoinForm(owner, meetingId, OWNER.name);
      expect(await joinMeetingFromPage(owner)).toBe("in-meeting");
      const remembered = await decisionFor("fell", true);
      expect(remembered).toContain("preferred=wt phase=fallback");
      expect(remembered).toMatch(/ active=wt_\d/);
      const fresh = await decisionFor("fresh", false);
      expect(fresh).toContain("preferred=wt phase=preferred");
    } finally {
      await ownerCtx.close();
    }
  });

  test("the protocol in use reads Not connected while the call reconnects, live in Network settings", async ({
    browser,
    baseURL,
  }) => {
    test.setTimeout(150_000);
    let armed = false;
    let reconnectSockets = 0;
    let release!: () => void;
    const released = new Promise<void>((resolve) => {
      release = resolve;
    });
    const { context, page, prefLines } = await joinAlone(
      browser,
      baseURL!,
      "reconnect",
      { webTransportEnabled: "true", webTransportHost: REFUSING_WT_HOST },
      {
        pinWebSocket: true,
        keepMeetingOnHostLeave: true,
        prepare: async (ctx) => {
          await installMediaSocketRecorder(ctx);
          await ctx.routeWebSocket(LOBBY_SOCKET, async (ws) => {
            if (armed) {
              reconnectSockets += 1;
              await Promise.race([
                released,
                new Promise<void>((resolve) => setTimeout(resolve, RECONNECT_HOLD_CAP_MS)),
              ]);
            }
            ws.connectToServer();
          });
        },
      },
    );
    const logs: string[] = [];
    page.on("console", (msg) => logs.push(msg.text()));
    try {
      expect(prefLines[0]).toContain("pref=websocket source=sticky");
      await expect(page.locator(SEL.selfBadgeWs)).toHaveCount(1, { timeout: 30_000 });
      await openNetworkSettings(page);
      await expectCurrentProtocol(page, "websocket", "WebSocket");

      armed = true;
      const sever = await severMediaWebSocket(page);
      expect(
        sever.severed,
        `no live media socket closed (the recorder saw ${sever.recorded} socket(s))`,
      ).toBeGreaterThanOrEqual(1);

      await expectCurrentProtocol(page, "none", "Not connected");
      await expectNetworkSettingsOpen(page);
      release();

      await expectCurrentProtocol(page, "websocket", "WebSocket", RECONNECT_TIMEOUT_MS);
      await expectNetworkSettingsOpen(page);
      expect(reconnectSockets, "the client must have opened a new media socket").toBeGreaterThan(0);
      expect(
        logs.some((line) => RECONNECTING_LOG.test(line)),
        "the client must have reconnected on its own",
      ).toBe(true);
      expect(
        logs.filter((line) => UI_CONNECTION_LOST_LOG.test(line)),
        "the UI's connection-lost handler must not run, or this test cannot tell the live " +
          "read from the connection_error refresh",
      ).toEqual([]);
    } finally {
      release();
      await context.close();
    }
  });
});
