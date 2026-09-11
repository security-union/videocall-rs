import { test, expect, chromium, Locator, Page } from "@playwright/test";
import { injectSessionCookie } from "../helpers/auth";
import { BROWSER_ARGS, createAuthenticatedContext } from "../helpers/auth-context";
import { wakeControls } from "../helpers/controls";
import { fillAndSubmitJoinForm } from "../helpers/join-meeting";
import { CQI } from "../helpers/rust-mirrored-constants";
import { PEER_SIGNAL_DISC, SELF_SIGNAL_DISC, SPARK_MIN_POINTS } from "../helpers/signal-meter";
import { setTransportBadgeFlag } from "../helpers/transport-badge-config";
import { enterTwoUserMeeting } from "../helpers/two-user-meeting";
import { waitForVisibleState } from "../helpers/visible-state";
import { waitForServices } from "../helpers/wait-for-services";

// The drawer's "Show diagnostics on tiles" checkbox (#2673) gates the PEER disc, the WS/WT
// transport badge and the media readout — but NOT the self disc, exempted after UX review
// because it carries the only `aria-live` announcement of connection quality in the product.
// So on the self tile the disc and the badge diverge, which is what these tests pin.
// `enableDiagnosticsTileIndicators` is unused on purpose: its re-seed on every navigation
// would erase the default these tests observe first.

const CHECKBOX = '[data-testid="media-metrics-overlay-toggle"]';
const ANY_DISC = "button.signal-indicator";
const ANY_BADGE = ".transport-badge";
const LIVE_REGION = 'span[role="status"][aria-live="polite"]';

const HOOK = "__videocall_inject_server_rtt";
const CRITICAL_RTT = CQI.CRITICAL_THRESHOLD_MS + 150;
const GOOD_RTT = 40;
// `action_bar_announce_text` appends U+00A0 on odd nonces (1765), so never `toHaveText`.
const ANNOUNCE = {
  CRITICAL: "Your connection is poor.",
  RECOVERED: "Your connection is back to normal.",
} as const;

type InjectHookWindow = Window & {
  __videocall_inject_server_rtt?: (rttMs: number, count?: number, tsMs?: number) => boolean;
};

const UNGATED_RENDER_WINDOW_MS = 5000;
const PEER_HISTORY_DWELL_MS = 12_000;

const CHROME_SCOPE = ".host-tile-chrome";
const PEER_TILE_SCOPE = "#grid-container .grid-item";

const chrome = (page: Page): Locator => page.locator(CHROME_SCOPE);

async function joinSoloMeeting(page: Page, label: string): Promise<void> {
  const meetingId = `e2e_tile_gate_${label}_${Date.now()}`;
  await fillAndSubmitJoinForm(page, meetingId, `TileGate${label}`);
  const joinButton = page.getByText(/Start Meeting|Join Meeting/).first();
  const grid = page.locator("#grid-container");
  const which = await waitForVisibleState(
    [
      { name: "join", locator: joinButton },
      { name: "grid", locator: grid },
    ],
    20_000,
  );
  if (which === "join" && (await joinButton.count()) > 0) {
    await joinButton
      .first()
      .click()
      .catch(() => {});
  }
  await expect(grid).toBeVisible({ timeout: 15_000 });
}

async function assertInjectHook(page: Page): Promise<void> {
  const attached = await page.evaluate(
    (hook) => typeof (window as InjectHookWindow)[hook as typeof HOOK] === "function",
    HOOK,
  );
  expect(
    attached,
    `${HOOK} is not attached. The e2e stack must run with MOCK_PEERS_ENABLED=true ` +
      `(docker/docker-compose.e2e.yaml:304). A missing hook means a broken harness, not an ` +
      `unsupported deployment — and every injection below would go nowhere silently.`,
  ).toBe(true);
}

async function injectRtt(page: Page, rttMs: number, count: number): Promise<void> {
  const accepted = await page.evaluate(
    ({ rtt, n }) => {
      const fn = (window as InjectHookWindow).__videocall_inject_server_rtt;
      return typeof fn === "function" ? fn(rtt, n) : false;
    },
    { rtt: rttMs, n: count },
  );
  expect(accepted, `${HOOK}(${rttMs}, ${count}) was rejected by the hook`).toBe(true);
}

async function openDrawerAndFindCheckbox(page: Page): Promise<Locator> {
  await wakeControls(page);
  await page.waitForTimeout(300);
  await page
    .locator("button", { has: page.locator("span.tooltip", { hasText: "Open Diagnostics" }) })
    .click();
  await expect(page.locator("#diagnostics-sidebar")).toBeVisible({ timeout: 10_000 });
  const checkbox = page.locator(CHECKBOX);
  await expect(checkbox).toBeVisible({ timeout: 10_000 });
  await expect(
    checkbox,
    "the preference must start OFF, or the gate is untested",
  ).not.toBeChecked();
  return checkbox;
}

test.describe("Tile indicators are diagnostics-only (#2673)", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test("the self disc is exempt while the self badge follows the checkbox @bvt1", async ({
    page,
    context,
    baseURL,
  }) => {
    test.setTimeout(120_000);
    await injectSessionCookie(context, { baseURL });
    await setTransportBadgeFlag(context, "true");

    await joinSoloMeeting(page, "self");

    await expect(chrome(page)).toHaveCount(1, { timeout: 15_000 });
    await expect(
      chrome(page).locator(".connection-led.connected"),
      "the badge needs `on_connected`, so an unconnected client would absent it for free",
    ).toBeVisible({ timeout: 30_000 });
    await expect(page.locator(SELF_SIGNAL_DISC)).toHaveCount(1, { timeout: 30_000 });
    // A badge leaking past the gate would have rendered inside this window.
    await page.waitForTimeout(UNGATED_RENDER_WINDOW_MS);

    // ONE in-page read: "disc present, badge absent" is the divergence, and querying them
    // one after another would let a late badge slip between the two reads.
    const off = await page.evaluate(
      (sel) => ({
        discs: document.querySelectorAll(sel.disc).length,
        badges: document.querySelectorAll(sel.badge).length,
        liveRegions: document.querySelectorAll(sel.live).length,
      }),
      { disc: SELF_SIGNAL_DISC, badge: ANY_BADGE, live: `${CHROME_SCOPE} ${LIVE_REGION}` },
    );
    expect(
      off.discs,
      "the self disc is EXEMPT from the gate: re-adding the early return hides it and takes " +
        "the quality announcement with it",
    ).toBe(1);
    expect(
      off.badges,
      `the transport badge is still gated, but ${off.badges} rendered with the checkbox off`,
    ).toBe(0);
    // The reason for the exemption: gating the disc dropped this region, leaving a 12x12
    // LED that encodes connect state, not quality, as the only self-tile affordance.
    expect(
      off.liveRegions,
      "the self tile must keep its `role=status` region with diagnostics off",
    ).toBe(1);

    const checkbox = await openDrawerAndFindCheckbox(page);
    await checkbox.check();
    await expect(checkbox).toBeChecked();

    const badge = chrome(page).locator(ANY_BADGE);
    await expect(badge).toHaveCount(1, { timeout: 30_000 });
    await expect(badge).toBeVisible();
    await expect(badge).toHaveText(/^(WT|WS)$/);
    await expect(page.locator(SELF_SIGNAL_DISC), "the disc is unmoved by the tick").toHaveCount(1);

    await checkbox.uncheck();
    await expect(checkbox).not.toBeChecked();
    await expect(page.locator(ANY_BADGE)).toHaveCount(0, { timeout: 20_000 });
    await expect(
      page.locator(SELF_SIGNAL_DISC),
      "the disc must survive the untick that removes the badge beside it",
    ).toHaveCount(1);
    await expect(chrome(page).locator(LIVE_REGION)).toHaveCount(1);
  });

  // Unpinnable at the unit level (a late event racing the toggle mimics accrued history);
  // against a real 1 Hz stream one stray event cannot reach SPARK_MIN_POINTS but a saturated
  // series always does. Cameras ON: `spark_paint` zeroes the series for an unmeasured peer,
  // however long it accrued.
  test("a peer disc revealed mid-call carries the history it accrued while hidden @bvt1", async ({
    baseURL,
  }) => {
    test.setTimeout(240_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_tile_gate_accrual_${Date.now()}`;

    const hostBrowser = await chromium.launch({ args: BROWSER_ARGS });
    const guestBrowser = await chromium.launch({ args: BROWSER_ARGS });
    try {
      const hostCtx = await createAuthenticatedContext(
        hostBrowser,
        "tilegateaccrualhost@videocall.rs",
        "AccrualHost",
        uiURL,
      );
      const guestCtx = await createAuthenticatedContext(
        guestBrowser,
        "tilegateaccrualguest@videocall.rs",
        "AccrualGuest",
        uiURL,
      );
      await hostCtx.addInitScript(`localStorage.setItem("vc_prejoin_camera_on", "true");`);
      await guestCtx.addInitScript(`localStorage.setItem("vc_prejoin_camera_on", "true");`);

      const hostPage = await hostCtx.newPage();
      const guestPage = await guestCtx.newPage();
      await enterTwoUserMeeting(hostPage, guestPage, meetingId);

      const peerTile = hostPage.locator(PEER_TILE_SCOPE).first();
      await expect(peerTile).toBeVisible({ timeout: 45_000 });
      await expect(peerTile.locator(".tile-top-icons")).toHaveCount(1, { timeout: 30_000 });
      await expect(peerTile.locator(PEER_SIGNAL_DISC)).toHaveCount(0);

      // Dwell hidden long enough to saturate the peer series (SPARK_POINTS at 1 Hz).
      await hostPage.waitForTimeout(PEER_HISTORY_DWELL_MS);

      const checkbox = await openDrawerAndFindCheckbox(hostPage);
      await checkbox.check();

      // Read IN-PAGE on the first tick the disc exists. Polling the count afterwards would
      // pass on a disc that revealed empty and filled during the wait, which is the opposite
      // of the property: the history must already be there AT the reveal.
      const handle = await hostPage.waitForFunction(
        (sel) => {
          const peer = document.querySelector(sel);
          const raw = peer?.getAttribute("data-signal-samples");
          return raw && /^\d+$/.test(raw) ? Number(raw) : null;
        },
        `${PEER_TILE_SCOPE} ${PEER_SIGNAL_DISC}`,
        { timeout: 20_000, polling: 50 },
      );
      const peerSamples = await handle.jsonValue();
      expect(
        peerSamples,
        `the peer disc revealed with only ${peerSamples} samples. Its sampler is never torn ` +
          `down and \`maybe_push_signal_sample\` sits LEFT of the \`&&\`, so history must ` +
          `survive the hidden period — swapping those operands is what this catches`,
      ).toBeGreaterThanOrEqual(SPARK_MIN_POINTS);
    } finally {
      await hostBrowser.close().catch(() => undefined);
      await guestBrowser.close().catch(() => undefined);
    }
  });

  // The exemption exists for THIS: gating the disc took the only spoken notice of a
  // quality drop with it. The container is asserted above, but `announcement` starts
  // empty and is only written on a transition, so proving the text needs one driven.
  test("with diagnostics off the exempt disc still announces a quality drop @bvt1", async ({
    page,
    context,
    baseURL,
  }) => {
    test.setTimeout(120_000);
    await injectSessionCookie(context, { baseURL });

    await joinSoloMeeting(page, "announce");
    await expect(chrome(page)).toHaveCount(1, { timeout: 15_000 });
    // The checkbox is never touched here; prove that rather than assume it.
    const stored = await page.evaluate(() =>
      localStorage.getItem("diagnostics.media_metrics_overlay"),
    );
    expect(stored, "this test must run the untouched default path").toBeNull();
    await expect(page.locator(SELF_SIGNAL_DISC)).toHaveCount(1, { timeout: 30_000 });

    // AFTER joining: the hook is registered from a `use_hook` in AttendantsComponent.
    await assertInjectHook(page);
    const region = chrome(page).locator(LIVE_REGION);
    await expect(region).toHaveCount(1);

    await injectRtt(page, CRITICAL_RTT, CQI.ENTER_COUNT);
    await expect(
      region,
      "with diagnostics off the drop must still be SPOKEN, not merely rendered",
    ).toContainText(ANNOUNCE.CRITICAL, { timeout: 10_000 });
    await expect(
      page.locator(SELF_SIGNAL_DISC),
      "the disc itself must reach Critical, or the region is announcing a state it does not show",
    ).toHaveAttribute("data-signal-level", "1", { timeout: 10_000 });

    // Both directions: a widget that latches at "poor" would pass a one-way assertion.
    await injectRtt(page, GOOD_RTT, CQI.EXIT_COUNT);
    await expect(region).toContainText(ANNOUNCE.RECOVERED, { timeout: 10_000 });
  });

  test("a peer tile carries no disc until the checkbox is ticked @bvt1", async ({ baseURL }) => {
    test.setTimeout(180_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_tile_gate_peer_${Date.now()}`;

    const hostBrowser = await chromium.launch({ args: BROWSER_ARGS });
    const guestBrowser = await chromium.launch({ args: BROWSER_ARGS });
    try {
      const hostCtx = await createAuthenticatedContext(
        hostBrowser,
        "tilegatehost@videocall.rs",
        "TileGateHost",
        uiURL,
      );
      const guestCtx = await createAuthenticatedContext(
        guestBrowser,
        "tilegateguest@videocall.rs",
        "TileGateGuest",
        uiURL,
      );
      await setTransportBadgeFlag(hostCtx, "true");

      const hostPage = await hostCtx.newPage();
      const guestPage = await guestCtx.newPage();
      await enterTwoUserMeeting(hostPage, guestPage, meetingId);

      const peerTile = hostPage.locator(PEER_TILE_SCOPE).first();
      await expect(peerTile).toBeVisible({ timeout: 45_000 });
      await expect(
        peerTile.locator(".tile-top-icons"),
        "the peer tile's icon cluster must be mounted (cameras stay off; it renders on a " +
          "placeholder tile as on a canvas one), or the absence below is vacuous",
      ).toHaveCount(1, { timeout: 30_000 });
      await hostPage.waitForTimeout(UNGATED_RENDER_WINDOW_MS);

      await expect(
        peerTile.locator(PEER_SIGNAL_DISC),
        "the peer disc is not a default affordance",
      ).toHaveCount(0);
      await expect(
        chrome(hostPage).locator(SELF_SIGNAL_DISC),
        "the exempt self disc is the ONLY disc on the page by default",
      ).toHaveCount(1);
      await expect(hostPage.locator(ANY_DISC)).toHaveCount(1);
      await expect(hostPage.locator(ANY_BADGE), "no badge of any kind by default").toHaveCount(0);

      const checkbox = await openDrawerAndFindCheckbox(hostPage);
      await checkbox.check();
      await expect(checkbox).toBeChecked();

      await expect(peerTile.locator(PEER_SIGNAL_DISC)).toHaveCount(1, { timeout: 20_000 });
      await expect(peerTile.locator(PEER_SIGNAL_DISC)).toBeVisible();
      await expect(chrome(hostPage).locator(ANY_BADGE)).toHaveCount(1, { timeout: 30_000 });

      await checkbox.uncheck();
      await expect(checkbox).not.toBeChecked();
      await expect(peerTile.locator(PEER_SIGNAL_DISC)).toHaveCount(0, { timeout: 20_000 });
      await expect(hostPage.locator(ANY_DISC), "the self disc alone remains").toHaveCount(1);
      await expect(hostPage.locator(ANY_BADGE)).toHaveCount(0, { timeout: 20_000 });
    } finally {
      await hostBrowser.close().catch(() => undefined);
      await guestBrowser.close().catch(() => undefined);
    }
  });
});
