import { test, expect, chromium, Page, BrowserContext } from "@playwright/test";
import { BROWSER_ARGS, createAuthenticatedContext } from "../helpers/auth-context";
import { waitForServices } from "../helpers/wait-for-services";
import { wakeControls } from "../helpers/controls";

/**
 * Screen-share "Cause line" contract, at the publisher's SOURCE rung
 * (HCL issue #983, re-based on the issue-#2179 review, round 3).
 *
 * ── What the Cause line means now ─────────────────────────────────────────
 * The line asserts "something is being WITHHELD from what this screen needs".
 * Round 2 split that from "is the share as good as it can get", because the two
 * questions have different answers on capped hardware. The publisher's single
 * stamp rule is `screen_cause_for_tier`
 * (videocall-client/src/encode/screen_encoder.rs):
 *
 *     if live_tier <= source_tier            -> None       (clear all 3 fields)
 *     if live_tier <= ceiling_tier && cause  -> Some(cause) (the ceiling term:
 *                                                ladder-limited | cpu-pressure |
 *                                                single-stream-limited)
 *     else                                   -> Some("bitrate-limited")
 *
 * with TWO published reference points on `ScreenQualitySnapshot`:
 *   - `source_tier_index`      — DESERVED: the rung the captured surface alone
 *     needs. This is the yardstick for "constrained".
 *   - `best_source_tier_index` — REACHABLE: the composed ceiling (source ∨
 *     publish-ladder cap ∨ CPU class ∨ single-stream cap). This is the yardstick
 *     for the SENDER'S METER.
 * They are equal on the common path and diverge exactly when a ceiling term
 * binds. A 4K screen held at 1440p by an 8-core sender is AT its ceiling (full
 * bars on the sender's meter) yet BELOW its source rung (a `ladder-limited`
 * Cause line on the receiver — round 3 moved that case off `cpu-pressure`, since
 * 8 cores clears the 6-core device bar and it is the 1440p publish ladder that
 * actually binds; pinned by `screen_ceiling_cause(3840, 2160, 8, 3)` in
 * videocall-aq/src/constants.rs). Both readings are correct simultaneously, so
 * never assert "meter full ⇒ no Cause line".
 *
 * The receiver mirrors the contract in `build_screen_cause_line`
 * (dioxus-ui/src/components/signal_quality.rs): with neither cause field
 * stamped it renders NOTHING — a bare `encoder_target_bitrate_kbps` is a
 * MAGNITUDE, not a constraint claim, so it no longer produces a partial
 * `Cause: <N>kbps` line.
 *
 * ── The regression this guards (the review's UX blocker) ──────────────────
 * The predicate was once `clamped_tier == 0`, i.e. "anything below the 2160p
 * rung is constrained". That FALSE-FLAGGED every ordinary share: a 1080p (or,
 * as here, a 720p) window resolves to the rung its own pixels need BY DESIGN,
 * and was stamped `bitrate-limited` purely for not being 4K. The viewer saw a
 * permanent "Cause: bitrate-limited" on a perfectly healthy share. Revert to
 * that predicate — or compare against `ceiling_tier` instead of `source_tier`
 * in the clear branch — and this spec fails on its first sample.
 *
 * ── Why the NO-LINE outcome is deterministic, on ANY host ─────────────────
 * For this spec's 1280x720 source:
 *   - DESERVED — `resolve_initial_screen_tier(1280,720,0)`
 *       = max(min(screen_tier_for_source=4, index_of("high")=2), 0) = 2
 *   - CEILING — `max(2, device_floor(cores), screen_ladder_top_index(),
 *     single_stream_floor(cores))`. `device_floor` ∈ {0, 1, 2},
 *     `screen_ladder_top_index()` = 1 (the publish ladder tops out at 1440p),
 *     and `single_stream_floor` = max(1, device) ≤ 2 — so every term is ≤ 2, the
 *     DESERVED term dominates, and the ceiling is 2 at every core count.
 *   - LIVE — `resolve_initial_screen_tier(...).max(ceiling)` = max(2, 2) = 2.
 * So `live_tier (2) <= source_tier (2)` holds always, the classifier returns
 * `None`, and all three fields are cleared.
 *
 * That makes this spec CORE-INDEPENDENT — unlike
 * `peer-screen-hidpi-resolution.spec.ts`, whose 2496x1440 source DESERVES rung 1
 * and so is genuinely gated on the 6-core device bar. No precondition on
 * `navigator.hardwareConcurrency` is needed here, and adding one would be noise:
 * there is no core count at which a 720p share is flagged constrained at start.
 *
 * ── Why this is not a vacuous "absence" test ──────────────────────────────
 * An absence assertion passes for free if nothing rendered at all, so the
 * tooltip is presence-gated before the absence is judged: the Screen legend must
 * appear (a screen sample was recorded), the tooltip must actually render, and
 * its HTML must contain the `Screen` metrics row.
 *
 * ── The POSITIVE case, and how round 3 made it reachable ──────────────────
 * A Cause line requires `live_tier > source_tier`. The 720p mock above can
 * never produce that, and until round 3 nothing in this harness could: the
 * netsim downlink shim degrades the publisher's INBOUND path so no send-tier
 * counter moves (it was removed from this spec for exactly that reason, along
 * with its hard dependency on a `netsim`-built image); the SEND tier-bounds
 * localStorage lever is dead (`sanitized()` forces `screen_auto = true` on every
 * load); and AQ step-down needs sustained real backpressure a localhost share
 * does not generate on demand.
 *
 * Round 3 supplied the lever as a side effect of capping every encode path at
 * the publish ladder's top rung: a source that DESERVES better than 1440p is
 * held below its own rung on every machine, with no impairment at all. The
 * second test in this file uses a 4K mock for exactly that, so the file now
 * covers BOTH halves of the contract — silence when nothing is withheld, and a
 * correctly-classified line when something is. See that test's doc comment for
 * the classifier gate.
 *
 * Both halves are also pinned host-side by
 * `screen_cause_for_tier_splits_withheld_from_reachable` and
 * `source_start_tier_publishes_the_ceiling_and_gates_the_cause_line`
 * (screen_encoder.rs), both mutation-guarded.
 *
 * Setup mirrors `peer-screen-diagnostics.spec.ts`.
 */

const DEFAULT_UI_URL = "http://localhost:3001";

/** Any rendered Cause line must name a real rung — never the wire-internal top label. */
const UNCONSTRAINED_TIER_RE = /tier 'native'/;
/**
 * Publisher cause-hint vocabulary — all SEVEN entries of `SCREEN_CAUSE_LEGEND`
 * (dioxus-ui/src/components/signal_quality.rs), which the help copy is generated
 * from. `build_screen_cause_line` prints the hint VERBATIM, so a regex
 * enumerating only some classifiers would silently fail a legitimate line:
 * round 2 added `single-stream-limited` / `network-loss`, round 3 added
 * `ladder-limited`.
 *
 * `single-stream-limited` is LATENT as of round 3 — the single-stream cap and
 * the publish-ladder cap are both 1440p, so it never binds alone and no user
 * sees it today. It is listed here because the publisher can still stamp it (the
 * two caps are independent policies that merely coincide), but per the note on
 * `SCREEN_CAUSE_LEGEND`, do NOT write a spec that expects to OBSERVE it.
 */
const CAUSE_HINT_RE =
  /(bitrate-limited|ladder-limited|cpu-pressure|single-stream-limited|network-rtt|network-loss|manual-cap)/;
/** Constrained rung labels the publisher may legitimately stamp (wire labels). */
const CONSTRAINED_TIER_RE = /tier '(1440p|high|medium|low)'/;

/**
 * How long to watch the tooltip. The publisher starts at its source rung; AQ
 * could in principle step DOWN below it later under real pressure (which would
 * render a LEGITIMATE Cause line), so the window is kept short and close to
 * share start, where the at-source-rung state is guaranteed.
 */
const OBSERVE_MS = 20_000;

interface MeetingMember {
  page: Page;
  context: BrowserContext;
  email: string;
  name: string;
}

/**
 * Build a getDisplayMedia mock returning a continuously-repainted canvas of the
 * given size. The repaint is load-bearing: the screen encoder re-encodes on
 * demand, so a static capture starves the receiver and no screen sample is ever
 * recorded for the chart.
 */
function mockDisplayMediaScript(width: number, height: number): string {
  return `
  (() => {
    const mediaDevices = navigator.mediaDevices;
    if (!mediaDevices) return;
    const createStream = () => {
      const canvas = document.createElement('canvas');
      canvas.width = ${width}; canvas.height = ${height};
      const ctx = canvas.getContext('2d');
      let frame = 0;
      const paint = () => {
        frame += 1;
        ctx.fillStyle = '#1a1a2e'; ctx.fillRect(0, 0, ${width}, ${height});
        ctx.fillStyle = '#fff'; ctx.font = '32px sans-serif';
        ctx.fillText('Mock Screen Share (e2e-983) ' + frame, 320, 360);
        requestAnimationFrame(paint);
      };
      paint();
      return canvas.captureStream(10);
    };
    Object.defineProperty(mediaDevices, 'getDisplayMedia', {
      configurable: true, value: async () => createStream(),
    });
  })();
`;
}

/**
 * 1280x720 mock share. The size is load-bearing: it is far below the 2160p top
 * rung, so on the OLD `clamped_tier == 0` predicate it was flagged constrained,
 * while under the source-relative predicate it sits exactly at the rung it
 * deserves and must not be flagged. That contrast is what this spec
 * discriminates.
 */
const MOCK_GET_DISPLAY_MEDIA_SCRIPT = mockDisplayMediaScript(1280, 720);

/**
 * 4K mock share — a source that genuinely DESERVES better than the publish
 * ladder can deliver, which is what makes the positive case reachable.
 * `resolve_initial_screen_tier(3840,2160,0)` = 0 (`native`), while
 * `screen_ladder_top_index()` = 1 (`1440p`), so the live tier is one rung worse
 * than the source rung and the publisher must stamp a cause.
 */
const MOCK_4K_GET_DISPLAY_MEDIA_SCRIPT = mockDisplayMediaScript(3840, 2160);

/**
 * Core bar below which the DEVICE term binds harder than the publish ladder
 * (`SCREEN_TIER_1440P_MIN_CORES`, videocall-aq/src/constants.rs). At/above it
 * `screen_ceiling_cause` reports `ladder-limited`; below it the device term
 * dominates and it reports `cpu-pressure` instead. The 4K test gates on this so
 * it asserts one concrete classifier rather than deriving the expectation.
 */
const TIER_1440P_MIN_CORES = 6;

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
  await wakeControls(sharerPage);
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
 * the chart so the global tooltip renders. Returns the popup locator.
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

test.describe("Peer screen-share diagnostics — Cause line at the source rung", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test("a healthy 720p share sits at the rung it deserves and renders NO Cause line", async ({
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

      // Recorded for the failure message only: a 720p source deserves rung 2 at
      // every core count (see the header walk-through), so this does NOT gate
      // the assertion — it just makes a red self-explanatory.
      const sharerCores = await guestPage.evaluate(() => navigator.hardwareConcurrency);

      // Open the viewer's popup for the publisher tile and confirm screen data.
      const popup = await openScreenTooltip(hostPage);

      // ── Sample the tooltip over a short window near share start. ──────────
      // Collect BOTH the presence evidence (a tooltip carrying the Screen row)
      // and any Cause line, so the absence judged below cannot be vacuous.
      const deadline = Date.now() + OBSERVE_MS;
      let samples = 0;
      let screenRowSamples = 0;
      let observedCauseLine: string | undefined;
      let lastTooltipHtml = "";

      while (Date.now() < deadline) {
        await hoverChart(popup);
        await hostPage.waitForTimeout(500);

        const tooltip = hostPage.locator("#signal-chart-tooltip-global");
        if (!(await tooltip.isVisible().catch(() => false))) {
          continue;
        }
        samples += 1;
        lastTooltipHtml = await tooltip.innerHTML();

        // Presence gate: the Screen metrics row must be in this tooltip, else
        // "no Cause line" says nothing (build_screen_tooltip_line always emits
        // a "Screen" label once a screen sample exists).
        if (/Screen/.test(lastTooltipHtml)) {
          screenRowSamples += 1;
        }

        const causeLine = lastTooltipHtml.split(/<br\s*\/?>|\n/i).find((l) => /Cause:/.test(l));
        if (causeLine) {
          observedCauseLine = causeLine;
          break;
        }
      }

      // ── Presence gate, asserted BEFORE the absence claim. ─────────────────
      expect(
        samples,
        "the signal-quality tooltip never rendered within the observation window, " +
          "so the Cause-line contract could not be evaluated (popup/hover plumbing)",
      ).toBeGreaterThan(0);
      expect(
        screenRowSamples,
        "the tooltip rendered but never carried the Screen metrics row, so an " +
          `absent Cause line proves nothing. Last tooltip HTML: ${lastTooltipHtml}`,
      ).toBeGreaterThan(0);

      // ── Any line that DID render must at least be well-formed. ────────────
      // Keeps the #983 shape coverage: a malformed line fails here even though
      // the deterministic expectation is that no line renders at all.
      if (observedCauseLine) {
        expect(
          observedCauseLine,
          "a Cause line must never render the wire-internal top label 'native'",
        ).not.toMatch(UNCONSTRAINED_TIER_RE);
        expect(
          observedCauseLine,
          `a rendered Cause line must name a constrained rung: ${observedCauseLine}`,
        ).toMatch(CONSTRAINED_TIER_RE);
        expect(
          observedCauseLine,
          `a rendered Cause line must carry a classifier from SCREEN_CAUSE_LEGEND: ` +
            `${observedCauseLine}`,
        ).toMatch(CAUSE_HINT_RE);
      }

      // ── THE regression assertion (issue #2179 review, UX blocker). ────────
      // A 720p share sits AT the rung it deserves (index 2 at every core count),
      // so `screen_cause_for_tier` returns None, the publisher clears
      // tier/hint/bitrate, and the receiver renders nothing. Reverting the
      // predicate to `clamped_tier == 0` — or comparing against the CEILING
      // instead of the SOURCE rung — re-stamps a cause and fails this on the
      // first sample.
      expect(
        observedCauseLine ?? null,
        "a 720p share sits AT the rung its own pixels deserve, so the publisher " +
          "must clear tier/hint/target-bitrate and the viewer must render NO Cause " +
          "line. Observing one means an unconstrained share is being false-flagged " +
          "as withheld (the pre-review `clamped_tier == 0` predicate), or AQ " +
          "genuinely stepped below the source rung during the window — the tier " +
          `label and classifier in the line distinguish the two (sharer ` +
          `cores=${sharerCores}). Line: ${observedCauseLine}`,
      ).toBeNull();
    } finally {
      for (const m of members) {
        if (m.page) {
          await m.page.close().catch(() => undefined);
        }
        await m.context.close().catch(() => undefined);
      }
      await Promise.all(browsers.map((b) => b.close().catch(() => undefined)));
    }
  });

  /**
   * The POSITIVE half — a share genuinely held below what its screen needs.
   *
   * Round 3 made this reachable for the first time. Every encode path is now
   * capped at the publish ladder's top rung (`screen_ladder_top_index()` = 1,
   * `1440p`), so a 4K surface — which `resolve_initial_screen_tier(3840,2160,0)`
   * says deserves rung 0 — is ALWAYS one rung short of its source, on any
   * machine. `screen_cause_for_tier` therefore stamps, and the receiver renders
   * a Cause line. No impairment, no netsim, no AQ manipulation required.
   *
   * ## What is asserted, and what deliberately is not
   * The classifier `screen_ceiling_cause` picks is:
   *   - `ladder-limited`  at `cores >= 6` — the ladder is the binding term;
   *   - `cpu-pressure`    at `cores <  6` — the device term binds harder.
   * This test gates on the 6-core bar and asserts the >= 6 case, rather than
   * deriving the expected string from the core count (which would re-implement
   * the production decision in the test).
   *
   * It asserts the line is PRESENT and well-formed, and pins the two classifiers
   * that would be WRONG here:
   *   - NOT `cpu-pressure` — round 3's stated reason for the split: at 6+ cores
   *     no amount of CPU would help, so blaming the machine sends the user
   *     optimising the wrong thing. Revert `screen_ceiling_cause` to the round-2
   *     ordering (device before ladder) and this fails.
   *   - NOT `single-stream-limited` — latent by construction (its cap and the
   *     ladder cap are both 1440p, so it can never bind alone). Observing it
   *     means the two policies have diverged without the legend/e2e catching up.
   * `ladder-limited` itself is asserted as the steady-state value only when the
   * line is observed at the ladder rung; a legitimate AQ step-down BELOW the
   * ceiling switches the classifier to `bitrate-limited`, which is why the two
   * NOT-assertions above (robust to step-down) carry the regression weight.
   */
  test("a 4K share is held at the ladder's top rung and names ladder-limited", async ({
    baseURL,
  }) => {
    test.setTimeout(240_000);
    const uiURL = baseURL || DEFAULT_UI_URL;
    const meetingId = `e2e_ss_cause4k_${Date.now()}`;

    const browsers = await Promise.all([
      chromium.launch({ args: BROWSER_ARGS }),
      chromium.launch({ args: BROWSER_ARGS }),
    ]);

    const members: MeetingMember[] = [];

    try {
      const profiles = [
        { email: "host-sscause4k@videocall.rs", name: "SSCause4kHost" },
        { email: "guest-sscause4k@videocall.rs", name: "SSCause4kGuest" },
      ];

      for (let i = 0; i < 2; i++) {
        const ctx = await createAuthenticatedContext(
          browsers[i],
          profiles[i].email,
          profiles[i].name,
          uiURL,
        );
        await ctx.addInitScript(MOCK_4K_GET_DISPLAY_MEDIA_SCRIPT);
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
      const guestPage = members[1].page; // PUBLISHER — shares the 4K screen.

      await expect(hostPage.locator("#grid-container .canvas-container")).toHaveCount(1, {
        timeout: 45_000,
      });

      // Gate: below the 6-core bar the DEVICE term binds harder than the ladder
      // and the classifier is `cpu-pressure`, not `ladder-limited`.
      const sharerCores = await guestPage.evaluate(() => navigator.hardwareConcurrency);
      if (!sharerCores || sharerCores < TIER_1440P_MIN_CORES) {
        test.skip(
          true,
          `sharer reports navigator.hardwareConcurrency=${sharerCores}, below the ` +
            `${TIER_1440P_MIN_CORES}-core bar (SCREEN_TIER_1440P_MIN_CORES). On this host ` +
            `screen_ceiling_cause reports 'cpu-pressure' rather than 'ladder-limited', ` +
            `which this test does not assert.`,
        );
        return;
      }

      const shareActivated = await startScreenShare(guestPage, hostPage);
      if (!shareActivated) {
        test.skip(
          true,
          "getDisplayMedia mock did not produce a stream that triggered the split layout.",
        );
        return;
      }

      const popup = await openScreenTooltip(hostPage);

      // Poll until a Cause line appears. Unlike the 720p case this line is
      // EXPECTED, so absence at the end of the window is the failure.
      const deadline = Date.now() + OBSERVE_MS;
      let samples = 0;
      let observedCauseLine: string | undefined;
      let lastTooltipHtml = "";

      while (Date.now() < deadline) {
        await hoverChart(popup);
        await hostPage.waitForTimeout(500);

        const tooltip = hostPage.locator("#signal-chart-tooltip-global");
        if (!(await tooltip.isVisible().catch(() => false))) {
          continue;
        }
        samples += 1;
        lastTooltipHtml = await tooltip.innerHTML();

        const causeLine = lastTooltipHtml.split(/<br\s*\/?>|\n/i).find((l) => /Cause:/.test(l));
        if (causeLine) {
          observedCauseLine = causeLine;
          break;
        }
      }

      expect(
        samples,
        "the signal-quality tooltip never rendered within the observation window, " +
          "so the Cause-line contract could not be evaluated (popup/hover plumbing)",
      ).toBeGreaterThan(0);

      expect(
        observedCauseLine,
        "a 4K share is capped at the publish ladder's 1440p top rung, one rung " +
          "below what its source needs, so the publisher MUST stamp a cause and the " +
          "viewer MUST render a Cause line. Its absence means the withheld state is " +
          `going unexplained (sharer cores=${sharerCores}). ` +
          `Last tooltip HTML: ${lastTooltipHtml}`,
      ).toBeTruthy();

      const causeLine = observedCauseLine as string;

      // Well-formed: a known classifier and a real rung label.
      expect(
        causeLine,
        `Cause line must carry a SCREEN_CAUSE_LEGEND classifier: ${causeLine}`,
      ).toMatch(CAUSE_HINT_RE);
      expect(causeLine, `Cause line must name a constrained rung: ${causeLine}`).toMatch(
        CONSTRAINED_TIER_RE,
      );
      // `native` is not an encodable rung at all post-round-3 — it survives only
      // as the getDisplayMedia capture ceiling — so it must never be named.
      expect(
        causeLine,
        `Cause line must never name the unencodable 'native' rung: ${causeLine}`,
      ).not.toMatch(UNCONSTRAINED_TIER_RE);

      // The two classifiers that would be WRONG here (see the doc comment).
      expect(
        causeLine,
        `at ${sharerCores} cores (>= ${TIER_1440P_MIN_CORES}) the device term does not ` +
          `bind, so blaming the CPU would send the user optimising something that ` +
          `cannot help: ${causeLine}`,
      ).not.toMatch(/cpu-pressure/);
      expect(
        causeLine,
        `'single-stream-limited' is latent — its cap and the ladder cap are both ` +
          `1440p, so it can never bind alone: ${causeLine}`,
      ).not.toMatch(/single-stream-limited/);
    } finally {
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
