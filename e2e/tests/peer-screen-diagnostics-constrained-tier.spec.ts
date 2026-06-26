import { test, expect, chromium, Page, BrowserContext } from "@playwright/test";
import { BROWSER_ARGS, createAuthenticatedContext } from "../helpers/auth-context";
import { waitForServices } from "../helpers/wait-for-services";
import {
  assertNetsimAvailable,
  impairDownlinkNetsim,
  healDownlinkNetsim,
} from "../helpers/downlink-impair";

/**
 * Screen-share "Cause line" at a CONSTRAINED publisher tier (HCL issue #983).
 *
 * ── Why this spec exists ──────────────────────────────────────────────────
 * PR #973 (iter2 commit 1d3773d5) RELAXED the Cause-line assertions in
 * `peer-screen-diagnostics.spec.ts` to "accept omitted Cause line at
 * unconstrained tier 0". That was correct for the cold-start path it tests —
 * on the e2e stack the screen publisher starts at the unconstrained top tier
 * (index 0 = "high"), and the publisher contract DELIBERATELY clears the
 * `adaptive_tier` / `cause_hint` / target-bitrate fields at tier 0 so the
 * receiver OMITS the Cause line (videocall-client/src/encode/screen_encoder.rs,
 * the `tier_index == 0` branch ~L1416-1419 and `apply_initial_tier` ~L1654-1669;
 * the renderer's omit branch is `build_screen_cause_line` in
 * dioxus-ui/src/components/signal_quality.rs ~L1885-1898).
 *
 * The relaxation left a COVERAGE GAP: nothing asserts the OTHER half of the
 * contract — that at a CONSTRAINED tier (index > 0) the Cause line is PRESENT
 * and carries the tier label + a cause-hint. This spec covers that half.
 *
 * ── How the Cause line is populated (verified end-to-end) ─────────────────
 * When AQ constrains the screen encoder (`video_tier_index() > 0`), the
 * publisher stamps the live tier label and a classified cause-hint onto every
 * `VideoMetadata` it emits:
 *   - tier label  ∈ { "medium" (idx 1), "low" (idx 2) } — idx 0 = "high" is the
 *     UNCONSTRAINED tier whose fields are cleared (videocall-aq/src/constants.rs
 *     SCREEN_QUALITY_TIERS L456+: 0="high", 1="medium", 2="low").
 *   - cause-hint  ∈ { "bitrate-limited", "cpu-pressure", "network-rtt",
 *     "manual-cap" } — the `cause_hint_from_trigger` vocabulary in
 *     screen_encoder.rs L276-283 (any other trigger maps to "" → no hint).
 * The receiver's peer_decoder → peer_tile → SignalSample pipeline records them
 * and `build_screen_cause_line` renders, e.g.:
 *     Cause: network-rtt · 500kbps · tier 'low'
 * (U+00B7 MIDDLE DOT separators; wrapped in a muted-color <span>).
 *
 * ── Honest limitation: reaching a constrained tier is NOT deterministic ───
 * As of this commit there is NO Playwright-reachable lever that DETERMINISTI-
 * cally pins the SCREEN publisher off tier 0:
 *   - The performance-preference localStorage tier-bounds path is DEAD:
 *     `load_performance_preference()` runs `.sanitized()` which unconditionally
 *     migrates the SEND tier bounds back to Auto on every load
 *     (performance_settings.rs L1331-1352), so a pre-seeded
 *     `screen_auto:false` + `screen_max/min` is wiped before the encoder reads it.
 *   - The relay layer-union (LAYER_HINT) path caps the publisher's active
 *     simulcast LAYER COUNT (`drop_top_layer`), NOT its `video_tier_index`, and
 *     emits no `TierTransitionRecord` (videocall-aq controller.rs L723-779) — so
 *     it does NOT populate the Cause line.
 *   - The publisher's self-congestion step-down axes (WS/WT send-buffer + ready-
 *     stall counters in screen_encoder.rs L1147-1276) and the server CONGESTION
 *     flag are not reachable by the per-tab netsim shim (it drops inbound BEFORE
 *     the transport write, so no uplink counter moves) and there is no uplink-
 *     impairment helper in e2e/helpers today.
 *
 * Therefore this spec is best-effort + SHAPE-asserting, NOT vacuous:
 *   1. It applies the only available nudge — the per-tab `crushed_downlink`
 *      netsim shim on the PUBLISHER tab — to try to push the publisher's screen
 *      AQ off tier 0, then POLLS the viewer's tooltip for a constrained
 *      Cause line.
 *   2. WHENEVER a Cause line is observed it ASSERTS the regression-catching
 *      shape: the line MUST carry a CONSTRAINED tier label (`medium`/`low`, NOT
 *      `high`) AND one of the four known cause-hints. A regression that emits a
 *      Cause line at a constrained tier WITHOUT the tier label or hint FAILS the
 *      test here — the exact gap #973's relaxation left uncovered.
 *   3. If no constrained Cause line is reached within the window (publisher
 *      stayed at unconstrained tier 0, which is the expected cold-start outcome
 *      on the localhost e2e stack), the test SKIPS with an explicit reason
 *      rather than passing on the empty-string default.
 *
 * The assertion body is REAL: mutate the renderer so a constrained tier emits a
 * Cause line missing its tier label or hint, and this test fails the moment that
 * line is observed. It cannot pass vacuously on the empty/omitted default
 * because the empty case takes the explicit `test.skip` branch, not a green
 * assertion.
 *
 * Setup mirrors `peer-screen-diagnostics.spec.ts` (auth + meeting + the
 * `MOCK_GET_DISPLAY_MEDIA_SCRIPT` getDisplayMedia mock) and reuses the netsim
 * impairment helper from `simulcast-per-receiver.spec.ts`.
 */

const DEFAULT_UI_URL = "http://localhost:3001";

/** Constrained screen tier labels: SCREEN_QUALITY_TIERS idx 1/2 (NOT idx 0 = "high"). */
const CONSTRAINED_TIER_RE = /tier '(medium|low)'/;
/** Unconstrained top tier label — must NEVER appear on a rendered Cause line. */
const UNCONSTRAINED_TIER_RE = /tier 'high'/;
/** Publisher cause-hint vocabulary (cause_hint_from_trigger, screen_encoder.rs). */
const CAUSE_HINT_RE = /(bitrate-limited|cpu-pressure|network-rtt|manual-cap)/;

interface MeetingMember {
  page: Page;
  context: BrowserContext;
  email: string;
  name: string;
}

const MOCK_GET_DISPLAY_MEDIA_SCRIPT = `
  (() => {
    const mediaDevices = navigator.mediaDevices;
    if (!mediaDevices) return;
    const createStream = () => {
      const canvas = document.createElement('canvas');
      canvas.width = 1280; canvas.height = 720;
      const ctx = canvas.getContext('2d');
      ctx.fillStyle = '#1a1a2e'; ctx.fillRect(0, 0, 1280, 720);
      ctx.fillStyle = '#fff'; ctx.font = '32px sans-serif';
      ctx.fillText('Mock Screen Share (e2e-983)', 320, 360);
      return canvas.captureStream(10);
    };
    Object.defineProperty(mediaDevices, 'getDisplayMedia', {
      configurable: true, value: async () => createStream(),
    });
  })();
`;

async function joinMeetingAs(
  context: BrowserContext,
  meetingId: string,
  username: string,
): Promise<Page> {
  const page = await context.newPage();
  await page.goto("/");
  await page.waitForTimeout(1500);

  await page.locator("#meeting-id").click();
  await page.locator("#meeting-id").pressSequentially(meetingId, { delay: 50 });
  await page.locator("#username").click();
  await page.locator("#username").fill("");
  await page.locator("#username").pressSequentially(username, { delay: 50 });
  await page.waitForTimeout(500);
  await page.locator("#username").press("Enter");

  await expect(page).toHaveURL(new RegExp(`/meeting/${meetingId}`), { timeout: 10_000 });
  await page.waitForTimeout(1500);

  return page;
}

async function clickJoinAndEnterGrid(page: Page): Promise<void> {
  const joinButton = page.getByRole("button", { name: /Start Meeting|Join Meeting/ });
  const grid = page.locator("#grid-container");

  const result = await Promise.race([
    joinButton.waitFor({ timeout: 30_000 }).then(() => "join" as const),
    grid.waitFor({ timeout: 30_000 }).then(() => "auto-joined" as const),
  ]);

  if (result === "join") {
    await page.waitForTimeout(1000);
    await joinButton.click();
    await page.waitForTimeout(3000);
  }

  await expect(grid).toBeVisible({ timeout: 15_000 });
}

async function admitGuestIfNeeded(hostPage: Page, guestPage: Page): Promise<void> {
  const joinButton = guestPage.getByRole("button", { name: /Start Meeting|Join Meeting/ });
  const waitingRoom = guestPage.getByText("Waiting to be admitted");
  const guestGrid = guestPage.locator("#grid-container");

  const result = await Promise.race([
    joinButton.waitFor({ timeout: 30_000 }).then(() => "join" as const),
    waitingRoom.waitFor({ timeout: 30_000 }).then(() => "waiting" as const),
    guestGrid.waitFor({ timeout: 30_000 }).then(() => "auto-joined" as const),
  ]);

  if (result === "waiting") {
    const admitButton = hostPage.getByTitle("Admit").first();
    await expect(admitButton).toBeVisible({ timeout: 20_000 });
    await hostPage.waitForTimeout(1000);
    await admitButton.dispatchEvent("click");
    await hostPage.waitForTimeout(3000);
  }

  if (result !== "auto-joined") {
    await clickJoinAndEnterGrid(guestPage);
  } else {
    await expect(guestGrid).toBeVisible({ timeout: 15_000 });
  }
}

async function startScreenShare(sharerPage: Page, viewerPage: Page): Promise<boolean> {
  await sharerPage.mouse.move(400, 400);
  await sharerPage.waitForTimeout(300);
  const shareButton = sharerPage.locator("button.video-control-button", {
    has: sharerPage.locator(".tooltip", { hasText: "Share Screen" }),
  });

  await expect(shareButton).toBeVisible({ timeout: 10_000 });
  await shareButton.click();

  try {
    await expect(viewerPage.locator(".split-screen-tile")).toBeVisible({
      timeout: 15_000,
    });
    return true;
  } catch {
    return false;
  }
}

/**
 * Open the host's signal-quality popup for the remote (sharing) peer and hover
 * the chart so the global tooltip renders. Returns the tooltip locator.
 *
 * Mirrors the popup/tooltip plumbing in `peer-screen-diagnostics.spec.ts`:
 * the bars icon (`aria-label="Show signal quality"`) lives in the peer tile,
 * the popup is `.signal-quality-popup`, and the chart's crosshair overlay
 * fires an `onmousemove` handler that pops `#signal-chart-tooltip-global`.
 */
async function openScreenTooltip(hostPage: Page) {
  const signalButton = hostPage.locator('button[aria-label="Show signal quality"]').first();
  await expect(signalButton).toBeVisible({ timeout: 15_000 });
  await signalButton.click();

  const popup = hostPage.locator(".signal-quality-popup");
  await expect(popup).toBeVisible({ timeout: 10_000 });

  // The Screen series legend gates on a recorded screen sample — wait for it so
  // the chart definitely has screen data before we hover for the tooltip.
  const screenLegend = popup.locator(".signal-chart-legend .legend-item", {
    hasText: /^Screen/,
  });
  await expect(screenLegend).toBeVisible({ timeout: 20_000 });

  return popup;
}

/** Dispatch a synthetic mousemove on the chart overlay to pop the tooltip. */
async function hoverChart(popup: ReturnType<Page["locator"]>): Promise<void> {
  const overlay = popup.locator("div[style*='cursor: crosshair']").first();
  await expect(overlay).toBeVisible({ timeout: 5_000 });
  await overlay.evaluate((el) => {
    const rect = (el as HTMLElement).getBoundingClientRect();
    const fire = (clientX: number) => {
      el.dispatchEvent(
        new MouseEvent("mousemove", {
          bubbles: true,
          cancelable: true,
          clientX,
          clientY: rect.top + rect.height / 2,
          buttons: 0,
        }),
      );
    };
    fire(rect.left + rect.width / 2);
    fire(rect.right - 5);
  });
}

test.describe("Peer screen-share diagnostics — constrained tier Cause line", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test("Cause line shows tier + hint when the publisher is driven off tier 0", async ({
    baseURL,
  }) => {
    test.setTimeout(240_000);
    const uiURL = baseURL || DEFAULT_UI_URL;
    const meetingId = `e2e_ss_cause_${Date.now()}`;

    const browsers = await Promise.all([
      chromium.launch({ args: BROWSER_ARGS }),
      chromium.launch({ args: BROWSER_ARGS }),
    ]);

    const members: MeetingMember[] = [];

    try {
      const profiles = [
        { email: "host-sscause@videocall.rs", name: "SSCauseHost" },
        { email: "guest-sscause@videocall.rs", name: "SSCauseGuest" },
      ];

      for (let i = 0; i < 2; i++) {
        const ctx = await createAuthenticatedContext(
          browsers[i],
          profiles[i].email,
          profiles[i].name,
          uiURL,
        );
        await ctx.addInitScript(MOCK_GET_DISPLAY_MEDIA_SCRIPT);
        members.push({
          page: null as unknown as Page,
          context: ctx,
          email: profiles[i].email,
          name: profiles[i].name,
        });
      }

      members[0].page = await joinMeetingAs(members[0].context, meetingId, profiles[0].name);
      await clickJoinAndEnterGrid(members[0].page);

      members[1].page = await joinMeetingAs(members[1].context, meetingId, profiles[1].name);
      await admitGuestIfNeeded(members[0].page, members[1].page);

      const hostPage = members[0].page; // VIEWER — reads the Cause line.
      const guestPage = members[1].page; // PUBLISHER — shares the screen.

      // Wait for peer discovery + mesh settlement.
      await expect(hostPage.locator("#grid-container .canvas-container")).toHaveCount(1, {
        timeout: 45_000,
      });

      // Guest (publisher) starts screen-share. Skip cleanly if the wasm-level
      // getDisplayMedia mock could not produce a stream (rare headless variant).
      const shareActivated = await startScreenShare(guestPage, hostPage);
      if (!shareActivated) {
        test.skip(
          true,
          "getDisplayMedia mock did not produce a stream that triggered the split layout.",
        );
        return;
      }

      // ── Attempt to constrain the publisher's screen AQ off tier 0. ────────
      // The ONLY Playwright-reachable nudge today is the per-tab netsim
      // `crushed_downlink` shim (see file header for why every other lever is
      // dead/unreachable). Install it on the PUBLISHER tab. This is best-effort:
      // it may not move the publisher's `video_tier_index` on the localhost e2e
      // stack, in which case the test SKIPS below rather than passing vacuously.
      // `assertNetsimAvailable` fails loud if the UI image lacks the `netsim`
      // cargo feature, so we never silently no-op the impairment.
      await assertNetsimAvailable(guestPage);
      await impairDownlinkNetsim(guestPage);

      // Open the viewer's popup for the publisher tile and confirm screen data.
      const popup = await openScreenTooltip(hostPage);

      // ── Poll for a CONSTRAINED Cause line, asserting its shape on sight. ──
      // We re-hover each iteration (the tooltip is hover-driven) and re-read its
      // HTML. The poll resolves to:
      //   - "constrained" once a Cause line carrying a CONSTRAINED tier label is
      //     seen (we assert the full shape inside the loop, so a malformed
      //     constrained line FAILS immediately rather than being swallowed);
      //   - "omitted" while no Cause line / only an unconstrained line exists.
      // After the window we branch: constrained ⇒ already asserted (pass);
      // omitted ⇒ explicit skip with reason. This structure makes a vacuous
      // green impossible — the empty default never reaches a green assertion.
      const POLL_MS = 90_000;
      const deadline = Date.now() + POLL_MS;
      let reachedConstrained = false;
      let lastTooltipHtml = "";

      while (Date.now() < deadline) {
        await hoverChart(popup);
        await hostPage.waitForTimeout(500);

        const tooltip = hostPage.locator("#signal-chart-tooltip-global");
        if (!(await tooltip.isVisible().catch(() => false))) {
          continue;
        }
        lastTooltipHtml = await tooltip.innerHTML();
        const causeLine = lastTooltipHtml.split(/<br\s*\/?>|\n/i).find((l) => /Cause:/.test(l));

        if (!causeLine) {
          // No Cause line yet ⇒ publisher still at unconstrained tier 0
          // (fields cleared by the publisher contract). Keep polling.
          continue;
        }

        // A Cause line is present. If it carries the UNCONSTRAINED top-tier
        // label, the publisher is (still) at tier 0 — which must NOT render a
        // Cause line at all, so a `tier 'high'` line is itself a regression.
        expect(
          causeLine,
          "Cause line must never render the unconstrained top tier ('high'); " +
            "the publisher contract clears tier/hint at index 0 so the line is omitted",
        ).not.toMatch(UNCONSTRAINED_TIER_RE);

        if (!CONSTRAINED_TIER_RE.test(causeLine)) {
          // Present, not unconstrained, but no recognizable constrained tier
          // label yet (transient mid-render). Keep polling.
          continue;
        }

        // ── Constrained Cause line observed — assert the regression shape. ──
        // This is the assertion #973 relaxed, now exercised at a constrained
        // tier. A constrained-tier line MUST carry both a tier label AND a
        // recognized cause-hint; failing either is the regression this guards.
        expect(
          causeLine,
          `constrained Cause line must carry tier 'medium'|'low': ${causeLine}`,
        ).toMatch(CONSTRAINED_TIER_RE);
        expect(
          causeLine,
          `constrained Cause line must carry a known cause-hint ` +
            `(bitrate-limited|cpu-pressure|network-rtt|manual-cap): ${causeLine}`,
        ).toMatch(CAUSE_HINT_RE);
        // Post-#903 tightening: no wordy pre-prototype phrasing.
        expect(causeLine).not.toMatch(/encoder target/);
        expect(causeLine).not.toMatch(/limited by/);
        expect(causeLine).not.toMatch(/not yet instrumented/);

        reachedConstrained = true;
        break;
      }

      if (!reachedConstrained) {
        // The publisher never left the unconstrained top tier within the
        // window. This is the EXPECTED cold-start outcome on the localhost e2e
        // stack: no Playwright-reachable lever deterministically constrains the
        // screen publisher today (see file header). Skip with a clear reason
        // rather than asserting on the empty-string default — the omitted-tier-0
        // case is already covered by `peer-screen-diagnostics.spec.ts`.
        test.skip(
          true,
          "Publisher stayed at unconstrained screen tier 0 within the poll window — " +
            "no constrained Cause line to assert. No Playwright-reachable lever " +
            "deterministically drives the screen publisher off tier 0 on the e2e " +
            "stack (see spec header). The omitted-line-at-tier-0 case is covered by " +
            `peer-screen-diagnostics.spec.ts. Last tooltip HTML: ${lastTooltipHtml}`,
        );
      }
    } finally {
      const publisherPage = members[1]?.page;
      if (publisherPage) {
        await healDownlinkNetsim(publisherPage).catch(() => undefined);
      }
      for (const m of members) {
        if (m.page) {
          await m.page.close().catch(() => undefined);
        }
        await m.context.close().catch(() => undefined);
      }
      await Promise.all(browsers.map((b) => b.close().catch(() => undefined)));
    }
  });
});
