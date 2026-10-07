// Resolution rules: docs/DEPLOYMENT_CONFIG_MAP.md (`diagnosticsPacketsEnabled`).

import { test, expect, chromium, Browser, BrowserContext, Page } from "@playwright/test";
import {
  createAuthenticatedContext,
  pinWebSocketTransport,
  BROWSER_ARGS,
} from "../helpers/auth-context";
import { enableCamera, wakeControls } from "../helpers/controls";
import { fillAndSubmitJoinForm } from "../helpers/join-meeting";
import { setRuntimeConfig } from "../helpers/runtime-config";
import { waitForServices } from "../helpers/wait-for-services";

// protobuf/types/packet_wrapper.proto: PacketType and MediaKind.
const PACKET_TYPE_MEDIA = 3;
const PACKET_TYPE_DIAGNOSTICS = 5;
const PACKET_TYPE_HEALTH = 6;
const MEDIA_KIND_VIDEO = 1;
const MEDIA_KIND_AUDIO = 2;

const DIAG_LOG_RE = /diagnostics packets: (ENABLED|DISABLED) \(source=(url|config|default)\)/;

type Peer = {
  label: string;
  page: Page;
  sent: Map<string, number>;
  received: Map<string, number>;
  logs: string[];
};

// PacketWrapper top-level fields: varint 1 = packet_type, varint 6 = media_kind.
function envelopeKey(payload: string | Buffer): string | null {
  if (typeof payload === "string") return null;
  let i = 0;
  const varint = (): number | null => {
    let value = 0;
    for (let mul = 1; i < payload.length; mul *= 128) {
      const b = payload[i++];
      value += (b & 0x7f) * mul;
      if ((b & 0x80) === 0) return value;
    }
    return null;
  };
  let type = 0;
  let kind = 0;
  while (i < payload.length) {
    const tag = varint();
    if (tag === null) return null;
    const field = Math.floor(tag / 8);
    const wire = tag % 8;
    if (wire === 0) {
      const v = varint();
      if (v === null) return null;
      if (field === 1) type = v;
      if (field === 6) kind = v;
    } else if (wire === 2) {
      const len = varint();
      if (len === null) return null;
      i += len;
    } else {
      return null;
    }
  }
  if (type === 0) return null;
  return type === PACKET_TYPE_MEDIA ? `${type}:${kind}` : `${type}`;
}

function bump(counts: Map<string, number>, key: string | null): void {
  if (key !== null) counts.set(key, (counts.get(key) ?? 0) + 1);
}

const count = (m: Map<string, number>, key: string) => m.get(key) ?? 0;
const diagSent = (p: Peer) => count(p.sent, `${PACKET_TYPE_DIAGNOSTICS}`);
const healthSent = (p: Peer) => count(p.sent, `${PACKET_TYPE_HEALTH}`);
const avCount = (m: Map<string, number>) =>
  count(m, `${PACKET_TYPE_MEDIA}:${MEDIA_KIND_AUDIO}`) +
  count(m, `${PACKET_TYPE_MEDIA}:${MEDIA_KIND_VIDEO}`);
const avReceived = (p: Peer) => avCount(p.received);
const avSent = (p: Peer) => avCount(p.sent);
const summary = (p: Peer) =>
  `${p.label}: sent=${JSON.stringify([...p.sent])} received=${JSON.stringify([...p.received])}`;

async function newPeer(
  browser: Browser,
  email: string,
  uiURL: string,
  config?: Record<string, string>,
): Promise<Peer> {
  const ctx: BrowserContext = await createAuthenticatedContext(
    browser,
    email,
    email.split("@")[0],
    uiURL,
  );
  await pinWebSocketTransport(ctx);
  if (config) await setRuntimeConfig(ctx, config);
  await ctx.addInitScript(() => {
    try {
      window.localStorage.setItem("vc_prejoin_camera_on", "true");
      window.localStorage.setItem("vc_prejoin_mic_on", "true");
    } catch {
      /* storage unavailable pre-navigation */
    }
  });
  const page = await ctx.newPage();
  const peer: Peer = { label: email, page, sent: new Map(), received: new Map(), logs: [] };
  page.on("console", (msg) => peer.logs.push(msg.text()));
  page.on("websocket", (ws) => {
    ws.on("framesent", (f) => bump(peer.sent, envelopeKey(f.payload)));
    ws.on("framereceived", (f) => bump(peer.received, envelopeKey(f.payload)));
  });
  return peer;
}

async function submitHomeForm(
  page: Page,
  entryPath: string,
  meetingId: string,
  name: string,
): Promise<void> {
  await fillAndSubmitJoinForm(page, meetingId, name, { entryPath });
  await expect(page).toHaveURL(new RegExp(`/meeting/${meetingId}`), { timeout: 15_000 });
}

async function enterGrid(
  page: Page,
  opts: { disableWaitingRoom?: boolean; admitBy?: Page } = {},
): Promise<void> {
  const joinButton = page.getByRole("button", { name: /Start Meeting|Join Meeting/ });
  const grid = page.locator("#grid-container");
  const waiting = page.getByText("Waiting to be admitted");
  let result = await Promise.race([
    joinButton.waitFor({ timeout: 30_000 }).then(() => "join" as const),
    grid.waitFor({ timeout: 30_000 }).then(() => "grid" as const),
    waiting.waitFor({ timeout: 30_000 }).then(() => "waiting" as const),
  ]);

  if (result === "waiting") {
    expect(opts.admitBy, "joiner parked in the Waiting Room but no host was given").toBeDefined();
    const admit = opts.admitBy!.getByTitle("Admit").first();
    await expect(admit).toBeVisible({ timeout: 20_000 });
    await admit.dispatchEvent("click");
    result = await Promise.race([
      joinButton.waitFor({ timeout: 20_000 }).then(() => "join" as const),
      grid.waitFor({ timeout: 20_000 }).then(() => "grid" as const),
    ]);
  }

  if (result === "join") {
    const allow = page.locator('[data-testid="prejoin-permission-allow"]');
    if (await allow.isVisible().catch(() => false)) {
      await allow.click({ timeout: 5_000 }).catch(() => {});
    }

    if (opts.disableWaitingRoom) {
      const toggle = page
        .locator(".settings-option-row", { has: page.getByText("Waiting Room", { exact: true }) })
        .getByRole("switch");
      if (await toggle.isVisible().catch(() => false)) {
        if ((await toggle.getAttribute("aria-checked")) === "true") {
          await toggle.click({ timeout: 5_000 });
        }
        await expect(toggle).toHaveAttribute("aria-checked", "false", { timeout: 10_000 });
      }
    }

    const cameraToggle = page.locator('[data-testid="prejoin-camera-toggle"]');
    if (await cameraToggle.isVisible().catch(() => false)) {
      if ((await cameraToggle.getAttribute("aria-pressed")) !== "true") {
        await cameraToggle.click({ timeout: 5_000 }).catch(() => {});
      }
      await expect
        .poll(
          () =>
            page
              .locator('[data-testid="prejoin-camera-preview"]')
              .evaluate(
                (el) => {
                  const s = (el as HTMLVideoElement).srcObject as MediaStream | null;
                  return s ? s.getVideoTracks().filter((t) => t.readyState === "live").length : 0;
                },
                undefined,
                { timeout: 1_000 },
              )
              .catch(() => 0),
          { timeout: 15_000 },
        )
        .toBeGreaterThan(0);
    }
    await joinButton.click({ timeout: 10_000 }).catch(() => {});
  }
  await expect(grid).toBeVisible({ timeout: 20_000 });

  await wakeControls(page);
  const startVideo = page.locator("button.video-control-button", {
    has: page.locator("span.tooltip", { hasText: "Start Video" }),
  });
  if (await startVideo.isVisible().catch(() => false)) await enableCamera(page);
}

async function expectSwitchLog(peer: Peer, state: string, source: string): Promise<void> {
  await expect
    .poll(() => peer.logs.filter((l) => DIAG_LOG_RE.test(l)).length, {
      timeout: 20_000,
      message: `${peer.label}: AttendantsComponent never logged the diagnostics-packets resolution`,
    })
    .toBeGreaterThan(0);
  const outcomes = peer.logs
    .map((l) => DIAG_LOG_RE.exec(l))
    .filter((m): m is RegExpExecArray => m !== null)
    .map((m) => `${m[1]} (source=${m[2]})`);
  expect(new Set(outcomes), peer.label).toEqual(new Set([`${state} (source=${source})`]));
}

async function waitForDiagnosticsWindow(clock: Peer, peers: Peer[], beats = 5): Promise<void> {
  for (const p of [clock, ...peers]) {
    const deadline = Date.now() + 30_000;
    while (avReceived(p) === 0 && Date.now() < deadline) await p.page.waitForTimeout(500);
    expect(
      avReceived(p),
      `no AUDIO/VIDEO MEDIA received over WebSocket (${summary(p)})`,
    ).toBeGreaterThan(0);
  }
  const start = diagSent(clock);
  const deadline = Date.now() + 30_000;
  while (diagSent(clock) - start < beats && Date.now() < deadline) {
    await clock.page.waitForTimeout(500);
  }
  expect(
    diagSent(clock) - start,
    `the switch-ON peer did not send ${beats} DIAGNOSTICS frames in 30s (${summary(clock)})`,
  ).toBeGreaterThanOrEqual(beats);
}

async function expectStillSendsAndReceives(off: Peer): Promise<void> {
  expect(
    avSent(off),
    `switch-OFF peer uploaded no AUDIO/VIDEO MEDIA (${summary(off)})`,
  ).toBeGreaterThan(0);
  const deadline = Date.now() + 15_000;
  while (healthSent(off) < 2 && Date.now() < deadline) await off.page.waitForTimeout(500);
  expect(
    healthSent(off),
    `switch-OFF peer stopped sending HEALTH (${summary(off)})`,
  ).toBeGreaterThanOrEqual(2);

  await wakeControls(off.page);
  await off.page
    .locator("button", { has: off.page.locator("span.tooltip", { hasText: "Open Diagnostics" }) })
    .click();
  const drawer = off.page.locator("#diagnostics-sidebar");
  await expect(drawer).toBeVisible({ timeout: 10_000 });
  const rawSummary = drawer.locator("summary#diag-h-raw-stats");
  await rawSummary.scrollIntoViewIfNeeded();
  await rawSummary.click();
  const reception = drawer
    .locator("details.diag-disclosure:has(> summary#diag-h-raw-stats) .diag-raw-block")
    .filter({ has: off.page.locator("h4", { hasText: "Reception Stats" }) })
    .locator("pre");
  await expect
    .poll(
      async () => {
        const text = (await reception.textContent().catch(() => null)) ?? "";
        return Math.max(
          0,
          ...[...text.matchAll(/FPS\(decoded\): ([0-9.]+)/g)].map((m) => Number(m[1])),
        );
      },
      { timeout: 20_000, message: "switch-OFF peer's drawer shows no decoded FPS for the host" },
    )
    .toBeGreaterThan(0);
}

function expectNoDiagnostics(peer: Peer): void {
  expect(diagSent(peer), `switch-OFF peer sent DIAGNOSTICS (${summary(peer)})`).toBe(0);
}

test.describe("DiagnosticsPacket send switch (?diag_packets / diagnosticsPacketsEnabled)", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  let browsers: Browser[] = [];
  const launch = async () => {
    const b = await chromium.launch({ args: BROWSER_ARGS });
    browsers.push(b);
    return b;
  };
  test.afterEach(async () => {
    await Promise.all(browsers.map((b) => b.close().catch(() => {})));
    browsers = [];
  });

  test("?diag_packets=0 on the meeting URL stops DIAGNOSTICS; the default peer still sends them", async ({
    baseURL,
  }) => {
    test.setTimeout(180_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `diag_url_${Date.now()}`;
    const control = await newPeer(await launch(), "diag-ctl@videocall.rs", uiURL);
    const off = await newPeer(await launch(), "diag-off@videocall.rs", uiURL);

    await submitHomeForm(control.page, "/", meetingId, "diag-control");
    await enterGrid(control.page, { disableWaitingRoom: true });

    await off.page.context().addInitScript(() => {
      window.localStorage.setItem("vc_display_name", "diag-off");
    });
    await off.page.goto(`/meeting/${meetingId}?diag_packets=0`);
    expect(new URL(off.page.url()).searchParams.get("diag_packets")).toBe("0");
    await enterGrid(off.page, { admitBy: control.page });

    await expectSwitchLog(control, "ENABLED", "default");
    await expectSwitchLog(off, "DISABLED", "url");

    await waitForDiagnosticsWindow(control, [off]);
    expectNoDiagnostics(off);
  });

  test('config "0" is a ceiling: OFF alone and with ?diag_packets=1 (source=config)', async ({
    baseURL,
  }) => {
    test.setTimeout(240_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `diag_cfg_${Date.now()}`;
    const cfgOn = await newPeer(await launch(), "diag-cfg1@videocall.rs", uiURL, {
      diagnosticsPacketsEnabled: "1",
    });
    const cfgOff = await newPeer(await launch(), "diag-cfg0@videocall.rs", uiURL, {
      diagnosticsPacketsEnabled: "0",
    });
    const cfgOffUrlOn = await newPeer(await launch(), "diag-cfg0url1@videocall.rs", uiURL, {
      diagnosticsPacketsEnabled: "0",
    });

    await submitHomeForm(cfgOn.page, "/", meetingId, "diag-cfg-on");
    await enterGrid(cfgOn.page, { disableWaitingRoom: true });
    await submitHomeForm(cfgOff.page, "/", meetingId, "diag-cfg-off");
    await enterGrid(cfgOff.page, { admitBy: cfgOn.page });
    await submitHomeForm(cfgOffUrlOn.page, "/?diag_packets=1", meetingId, "diag-cfg-off-url-on");
    await enterGrid(cfgOffUrlOn.page, { admitBy: cfgOn.page });

    await expectSwitchLog(cfgOn, "ENABLED", "config");
    await expectSwitchLog(cfgOff, "DISABLED", "config");
    await expectSwitchLog(cfgOffUrlOn, "DISABLED", "config");

    await waitForDiagnosticsWindow(cfgOn, [cfgOff, cfgOffUrlOn]);
    expectNoDiagnostics(cfgOff);
    expectNoDiagnostics(cfgOffUrlOn);
  });

  test('?diag_packets=0 under config "1" survives the home -> /meeting push and Waiting Room admission @bvt1', async ({
    baseURL,
  }) => {
    test.setTimeout(180_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `diag_nav_${Date.now()}`;
    const host = await newPeer(await launch(), "diag-host@videocall.rs", uiURL);
    const off = await newPeer(await launch(), "diag-nav@videocall.rs", uiURL, {
      diagnosticsPacketsEnabled: "1",
    });

    await submitHomeForm(host.page, "/", meetingId, "diag-host");
    await enterGrid(host.page);

    await submitHomeForm(off.page, "/?diag_packets=0", meetingId, "diag-nav");
    expect(new URL(off.page.url()).searchParams.has("diag_packets")).toBe(false);
    await expect(off.page.getByText("Waiting to be admitted")).toBeVisible({ timeout: 30_000 });
    await enterGrid(off.page, { admitBy: host.page });
    expect(new URL(off.page.url()).searchParams.has("diag_packets")).toBe(false);

    await expectSwitchLog(host, "ENABLED", "default");
    await expectSwitchLog(off, "DISABLED", "url");

    await waitForDiagnosticsWindow(host, [off]);
    await expectStillSendsAndReceives(off);
    expectNoDiagnostics(off);
  });
});
