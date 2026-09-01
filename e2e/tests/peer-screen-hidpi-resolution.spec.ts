import { test, expect, chromium, Page, BrowserContext } from "@playwright/test";
import { BROWSER_ARGS, createAuthenticatedContext } from "../helpers/auth-context";
import { waitForServices } from "../helpers/wait-for-services";
import { wakeControls } from "../helpers/controls";

/**
 * Issue #2179 — "when sharing a screen the text is generally fuzzy".
 *
 * ## The defect
 * A browser window measuring 1248x720 CSS px on a DPR-2 (Retina) display is
 * 2496x1440 REAL pixels. Two ceilings destroyed that detail before the fix:
 *   1. `getDisplayMedia` was requested with `width.max: 1920, height.max: 1080`
 *      (device px), so capture fitted 2496x1440 -> 1872x1080 (x0.75);
 *   2. the screen AQ ladder topped out at a 1920x1080 rung and a fresh share
 *      landed on it (or, once AQ moved, on the 1280x720 `medium` rung), so
 *      `fit_within_preserving_aspect` fitted that again -> 1248x720 (x0.667).
 * Net: a quarter of the source pixels through TWO fractional resamples, which
 * is exactly what shreds glyph stems and antialiasing.
 *
 * ## What the code does NOW (issue #2343 superseded the #2179 ladder)
 * Screen publishes ONE layer at the captured surface's own geometry.
 * `screen_encode_box_for_capture` (videocall-aq/src/aspect.rs) bounds the capture
 * by the `SCREEN_MAX_ENCODE_*` ceiling and returns the capture size UNCHANGED when
 * it fits — `fit_within_preserving_aspect` clamps its scale factor at 1.0, so a
 * fitting source is never upscaled. That identity is also how
 * `capture_exceeds_encode_ceiling` detects the capped case (`box != capture`).
 * There is no tier ladder and no device/CPU term in the geometry decision.
 *
 * NOTE this source is now a BOUNDARY case, not a comfortable fit. The ceiling
 * moved 3840x2160 -> 2560x1440 (4K VP9 keyframes would exceed `actix-api`'s
 * `MAX_FRAME_SIZE`), so 2496x1440 sits 64px under the width ceiling and EXACTLY
 * on the height ceiling. It still returns unchanged — scale = min(2560/2496,
 * 1440/1440, 1.0) = 1.0 — and `videocall-aq/src/aspect.rs`'s
 * `the_encode_box_is_the_captured_surface_up_to_the_ceiling` pins this same
 * 2496x1440 input at the unit layer. Lower the ceiling again and this spec goes
 * red, which is correct: the decoded size would genuinely change.
 *
 * ## What is asserted, and why it cannot pass on the un-fixed code
 * The viewer's canvas buffer is sized to the decoded frame's display dims
 * (`peer_decoder.rs` `render_to_canvas_cached` -> `canvas_buffer_dims`), so
 * `canvas.width` IS the decoded width. This shares 2496x1440 and asserts EXACTLY
 * that back. Pre-#2179 produced 1872x1080 (widest rung was 1920). Pre-#2343 the
 * best case fitted into the 2560x1440 rung and produced 2496x1440 — the right
 * number for the wrong reason, which is why the old `> 1920` form could not tell
 * "a rung happened to fit" from "there is no ladder"; a low-core sharer was
 * clamped to 1920 instead, hence the old 6-core skip gate.
 *
 * ## Mock notes
 * - The `getDisplayMedia` mock ignores the requested constraints (it just
 *   returns a canvas stream), which is deliberate: it isolates the ENCODER-side
 *   half of the fix from the browser's capture-constraint behaviour, so the test
 *   does not depend on how a headless Chrome honours `max`.
 * - The mock track's `getSettings()` IS load-bearing, and is therefore
 *   ASSERTED below rather than assumed. `screen_stream_source_dims` reads
 *   `getSettings().width/height`, and that IS the encode geometry now; a track
 *   that reports no size yields `(0,0)`, which
 *   `screen_encode_box_for_capture` short-circuits to `(0, 0)` — producing a RED
 *   that looks like a geometry regression but is really a broken mock.
 * - The canvas is CONTINUOUSLY REPAINTED. A static `captureStream()` produces
 *   no frames after the first, and the screen encoder re-encodes on demand, so
 *   a still mock starves the receiver's decoder and the canvas never resizes.
 * - Simulcast is OFF on the e2e docker stack because the served
 *   `dioxus-ui/scripts/config.js` pins `experimentalSimulcastMaxLayers: 1`
 *   EXPLICITLY — the Rust `#[serde(default)]` is 3 since #1082, so the pin (not
 *   an omitted key) is what makes it off; see `e2e/helpers/simulcast-config.ts`
 *   and `dioxus-ui/src/constants.rs`. That is also what keeps the single-stream
 *   ceiling term live.
 */

const DEFAULT_UI_URL = "http://localhost:3001";

/** The HiDPI source size: a 1248x720 CSS-px window on a DPR-2 panel. */
const SOURCE_WIDTH = 2496;
const SOURCE_HEIGHT = 1440;

/**
 * The widest rung the PRE-#2179 ladder could encode, and still the width the
 * pre-#2343 tier ladder would have snapped this source to on a low-core runner.
 * Retained only to name that number in the failure message — the assertion is
 * EXACT equality with the source, which is strictly stronger.
 */
const PRE_2179_MAX_ENCODE_WIDTH = 1920;

/** Window global the mock stamps with the capture track's reported settings. */
const MOCK_SOURCE_SIZE_GLOBAL = "__e2eScreenMockSourceSize";

/**
 * A 4K surface — deliberately ABOVE `SCREEN_MAX_ENCODE_*` so the ceiling bites.
 * 16:9 so the aspect assertion has a clean expected ratio, and the size the
 * MAX_FRAME_SIZE analysis is written against.
 */
const OVERSIZE_WIDTH = 3840;
const OVERSIZE_HEIGHT = 2160;

interface MeetingMember {
  page: Page;
  context: BrowserContext;
  email: string;
  name: string;
}

/**
 * Animated 2496x1440 canvas stand-in for a Retina window of text. The
 * `requestAnimationFrame` repaint is load-bearing: see the mock notes above.
 *
 * The stream's own `getSettings()` is stamped onto a window global so the test
 * can assert the precondition the production code reads (rather than trusting
 * that a canvas-capture track reports its size).
 */
function mockGetDisplayMediaScript(width: number, height: number): string {
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
        ctx.fillStyle = '#1a1a2e';
        ctx.fillRect(0, 0, ${width}, ${height});
        ctx.fillStyle = '#ffffff';
        ctx.font = '48px monospace';
        for (let row = 0; row < 12; row++) {
          ctx.fillText(
            'HiDPI screen share text sample ' + frame + ' row ' + row,
            60,
            120 + row * 100,
          );
        }
        requestAnimationFrame(paint);
      };
      paint();
      const stream = canvas.captureStream(10);
      const track = stream.getVideoTracks()[0];
      if (track && track.getSettings) {
        const s = track.getSettings();
        window.${MOCK_SOURCE_SIZE_GLOBAL} = { width: s.width, height: s.height };
      }
      return stream;
    };
    Object.defineProperty(mediaDevices, 'getDisplayMedia', {
      configurable: true, value: async () => createStream(),
    });
  })();
`;
}

const MOCK_GET_DISPLAY_MEDIA_SCRIPT = mockGetDisplayMediaScript(SOURCE_WIDTH, SOURCE_HEIGHT);

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
 * Read back the size the SHARER's capture track reported via `getSettings()`.
 *
 * This is the exact value `screen_stream_source_dims` feeds into the start-tier
 * resolution, so asserting it separates "the fix regressed" from "the mock
 * stopped reporting a source size" — two failures that would otherwise present
 * identically as a sub-1920 decoded width.
 */
async function mockReportedSourceSize(
  sharerPage: Page,
): Promise<{ width?: number; height?: number } | undefined> {
  return sharerPage.evaluate((key) => {
    return (window as unknown as Record<string, { width?: number; height?: number } | undefined>)[
      key
    ];
  }, MOCK_SOURCE_SIZE_GLOBAL);
}

/**
 * Poll the viewer's screen-share canvas buffer size, returning the WIDEST
 * decoded frame observed within `windowMs`.
 *
 * The maximum (not the final value) is the right statistic: the fix guarantees
 * the share STARTS at the source resolution, and a loaded CI runner may then
 * legitimately drive AQ down mid-window. Observing the high-water mark at any
 * point proves the fix, and the un-fixed code can never reach it at all (its
 * widest rung is 1920).
 */
/**
 * Open the diagnostics drawer, which is where the SEND meters live.
 *
 * Load-bearing for the capped test: `#perf-vu-screen-readout` is rendered by the
 * perf panel (performance_settings.rs `vu_readout_id: SCREEN_READOUT_ID`), and the
 * headless meter driver writes into it BY ID. With the drawer closed the element
 * does not exist at all, so polling its text would time out rather than read a
 * stale value. Mirrors `openPerformancePanel` in simulcast-per-receiver.spec.ts.
 */
async function openPerfDrawer(page: Page): Promise<void> {
  await wakeControls(page);
  const diagButton = page.locator("button", {
    has: page.locator("span.tooltip", { hasText: "Open Diagnostics" }),
  });
  await expect(diagButton).toBeVisible({ timeout: 10_000 });
  await diagButton.click();
  const drawer = page.locator("#diagnostics-sidebar");
  await expect(drawer).toBeVisible({ timeout: 10_000 });
  // Readiness: the SEND screen meter is the block that owns the readout below.
  await expect(drawer.locator('[data-testid="perf-vu-screen"]')).toBeVisible({
    timeout: 10_000,
  });
}

async function maxDecodedScreenWidth(
  viewerPage: Page,
  windowMs: number,
  /**
   * Width at which polling can stop early — the widest value this share can
   * legitimately reach. Defaults to the un-capped source width. The capped case
   * passes the CEILING instead, because the decode can never reach the source.
   */
  settleAtWidth: number = SOURCE_WIDTH,
): Promise<{ width: number; height: number }> {
  const canvas = viewerPage.locator('canvas[id^="screen-share-"]').first();
  await expect(canvas).toBeVisible({ timeout: 20_000 });

  let best = { width: 0, height: 0 };
  const deadline = Date.now() + windowMs;
  while (Date.now() < deadline) {
    const dims = await canvas.evaluate((el) => {
      const c = el as HTMLCanvasElement;
      return { width: c.width, height: c.height };
    });
    if (dims.width > best.width) {
      best = dims;
    }
    if (best.width >= settleAtWidth) {
      break;
    }
    await viewerPage.waitForTimeout(500);
  }
  return best;
}

test.describe("Peer screen-share HiDPI resolution (issues #2179, #2343)", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test("a 2496x1440 Retina share is decoded at its native geometry, not a tier size", async ({
    baseURL,
  }) => {
    test.setTimeout(240_000);
    const uiURL = baseURL || DEFAULT_UI_URL;
    const meetingId = `e2e_ss_hidpi_${Date.now()}`;

    const browsers = await Promise.all([
      chromium.launch({ args: BROWSER_ARGS }),
      chromium.launch({ args: BROWSER_ARGS }),
    ]);

    const members: MeetingMember[] = [];

    try {
      const profiles = [
        { email: "host-sshidpi@videocall.rs", name: "SSHiDpiHost" },
        { email: "guest-sshidpi@videocall.rs", name: "SSHiDpiGuest" },
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

      const hostPage = members[0].page;
      const guestPage = members[1].page;

      // No core-count skip gate (issue #2343): `resolve_screen_tier_ceiling`,
      // `screen_tier_device_floor` and `SCREEN_TIER_1440P_MIN_CORES` were deleted, so
      // core count no longer influences geometry and the old gate could only produce
      // a false SKIP — green while asserting nothing.

      await expect(hostPage.locator("#grid-container .canvas-container")).toHaveCount(1, {
        timeout: 30_000,
      });

      const shareActivated = await startScreenShare(guestPage, hostPage);
      if (!shareActivated) {
        test.skip(
          true,
          "getDisplayMedia mock did not produce a stream that triggered the split layout.",
        );
        return;
      }

      // ── Precondition: the capture track must REPORT its size. ─────────────
      // `screen_stream_source_dims` reads `getSettings().width/height` and that IS
      // the encode geometry now, so a track reporting nothing yields (0,0) and the
      // assertion below would fail for a broken mock rather than a real regression.
      const reportedSource = await mockReportedSourceSize(guestPage);
      expect(
        reportedSource,
        "the getDisplayMedia mock never stamped a capture size — the mock did not run",
      ).toBeTruthy();
      expect(
        { width: reportedSource?.width, height: reportedSource?.height },
        "the capture track must report its source size through getSettings(); it is " +
          "the encode geometry the assertion below reads back",
      ).toEqual({ width: SOURCE_WIDTH, height: SOURCE_HEIGHT });

      const decoded = await maxDecodedScreenWidth(hostPage, 60_000);

      // The canvas must have been sized by a real decoded frame at all — a 0
      // width means no frame ever painted and the assertion below would be
      // vacuous.
      expect(
        decoded.width,
        "the viewer's screen-share canvas was never sized by a decoded frame",
      ).toBeGreaterThan(0);

      // THE regression assertion (issue #2343): encoded at the captured surface's
      // OWN pixels. A reintroduced ladder snaps this to a rung (1920 pre-#2179, or
      // a device-clamped one) and lands on a different number. Note it must not
      // snap UP to the 2560-wide ceiling either — the ceiling is a bound, not a
      // target, and `fit_within_preserving_aspect` never upscales. See the header
      // for why equality rather than the old `> 1920`.
      expect(
        { width: decoded.width, height: decoded.height },
        `decoded screen geometry must be the source's own ${SOURCE_WIDTH}x${SOURCE_HEIGHT} ` +
          `pixels. A ${PRE_2179_MAX_ENCODE_WIDTH}-wide (or otherwise snapped) result means ` +
          `a resolution ladder is still selecting the encode box`,
      ).toEqual({ width: SOURCE_WIDTH, height: SOURCE_HEIGHT });
    } finally {
      for (const m of members) {
        await m.context.close().catch(() => {});
      }
      for (const b of browsers) {
        await b.close().catch(() => {});
      }
    }
  });

  // ── The OTHER side of the ceiling: an oversized surface must be capped ─────
  //
  // The encode ceiling is not a quality preference, it is a crash guard. A 4K VP9
  // keyframe (~4-8 MB, extrapolated from the measured 1-2 MB at 1080p) exceeds
  // `actix-api`'s `MAX_FRAME_SIZE` (4_000_000), whose documented failure mode is a
  // protocol error that closes the ENTIRE connection. `screen_tier_single_stream_floor`
  // used to prevent 4K single-stream; #2343 deleted it, and `SCREEN_MAX_ENCODE_*`
  // (2560x1440) is what replaced it.
  //
  // The geometry arithmetic is already pinned at the unit layer
  // (`the_encode_box_is_the_captured_surface_up_to_the_ceiling`, videocall-aq/src/aspect.rs,
  // which covers 3840x2160 / 3840x1600 / 5120x2880). What that CANNOT prove is the
  // end-to-end consequence: that a real oversized capture, encoded by real WebCodecs
  // and pushed over the real transport, still arrives. If the cap regressed, the
  // keyframe would breach MAX_FRAME_SIZE and the viewer would receive NOTHING —
  // which is exactly what the `> 0` assertion below detects.
  //
  // NO CEILING LITERAL. The ceiling has already moved once (3840x2160 -> 2560x1440),
  // so pinning 2560 here would just go red on the next move. The assertions are all
  // relative instead:
  //   * the sender's own readout must report a geometry strictly SMALLER than the
  //     capture (production stating the encode it actually configured);
  //   * the decoded width must be strictly LESS than the source (the cap reached the
  //     viewer, not just the local config);
  //   * the aspect must survive (uniform downscale, not a per-axis clamp — the #1037
  //     defect `fit_within_preserving_aspect` exists to prevent);
  //   * frames must arrive at all (the connection survived).
  //
  // FAILS ON UNFIXED: remove the ceiling and the encode is 3840x2160 — both `< SOURCE`
  // assertions fail, and in the worst case the oversized keyframe breaches
  // MAX_FRAME_SIZE, kills the connection, and the `> 0` assertion fails too.
  //
  // NOT ASSERTED, DELIBERATELY: the ` · capped` suffix `format_screen_readout` appends
  // when `snap.capture_capped` is set. Measured on this exact flow, a 3840x2160 share
  // encodes at 2560x1440 (capped, per the readout below) while the readout does NOT
  // carry the suffix — i.e. `capture_capped` reads false for a share that was capped.
  // That is a production reporting bug, filed separately; asserting the suffix here
  // would make this spec red for a defect it is not about.
  test("an oversized share is capped to the encode ceiling and still reaches the viewer", async ({
    baseURL,
  }) => {
    test.setTimeout(240_000);
    const uiURL = baseURL || DEFAULT_UI_URL;
    const meetingId = `e2e_ss_capped_${Date.now()}`;

    const browsers = await Promise.all([
      chromium.launch({ args: BROWSER_ARGS }),
      chromium.launch({ args: BROWSER_ARGS }),
    ]);
    const members: MeetingMember[] = [];

    try {
      const profiles = [
        { email: "host-sscapped@videocall.rs", name: "SSCappedHost" },
        { email: "guest-sscapped@videocall.rs", name: "SSCappedGuest" },
      ];

      for (let i = 0; i < 2; i++) {
        const ctx = await createAuthenticatedContext(
          browsers[i],
          profiles[i].email,
          profiles[i].name,
          uiURL,
        );
        await ctx.addInitScript(mockGetDisplayMediaScript(OVERSIZE_WIDTH, OVERSIZE_HEIGHT));
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

      const hostPage = members[0].page; // VIEWER
      const guestPage = members[1].page; // SHARER

      await expect(hostPage.locator("#grid-container .canvas-container")).toHaveCount(1, {
        timeout: 30_000,
      });

      const shareActivated = await startScreenShare(guestPage, hostPage);
      if (!shareActivated) {
        test.skip(
          true,
          "getDisplayMedia mock did not produce a stream that triggered the split layout.",
        );
        return;
      }

      // Precondition: the capture really is oversized. Without this the cap has
      // nothing to bite on and every assertion below is vacuous.
      const reportedSource = await mockReportedSourceSize(guestPage);
      expect(
        { width: reportedSource?.width, height: reportedSource?.height },
        "the mock must actually report an oversized capture, or there is nothing to cap",
      ).toEqual({ width: OVERSIZE_WIDTH, height: OVERSIZE_HEIGHT });

      // The SENDER's readout is production stating the geometry it configured. The
      // drawer must be OPEN first — the readout element is rendered by the perf panel
      // and the meter driver writes into it BY ID, so with the drawer closed there is
      // no node to read and the poll below would time out for the wrong reason.
      await openPerfDrawer(guestPage);
      // Poll for a SETTLED readout, not merely a present one. The meter publishes an
      // early frame where the geometry is still the raw capture and the bitrate is
      // `0kbps` — measured: `3840x2160·10fps·0kbps` before the encoder configures,
      // then `2560x1440·10fps·4423kbps` after. Matching on shape alone reads the
      // transient and reports a cap failure that is really a race, so the poll
      // requires a NON-ZERO bitrate, which only the configured encoder publishes.
      let sendReadout = "";
      await expect
        .poll(
          async () => {
            sendReadout =
              (await guestPage.locator("#perf-vu-screen-readout").textContent())?.trim() ?? "";
            return sendReadout;
          },
          { timeout: 90_000, intervals: [1000, 2000] },
        )
        .toMatch(/^\d+x\d+·\d+fps·[1-9]\d*kbps/);

      const sendDims = sendReadout.match(/^(\d+)x(\d+)·/);
      expect(sendDims, `send readout had no {w}x{h} prefix: "${sendReadout}"`).not.toBeNull();
      expect(
        Number(sendDims![1]),
        `the sender configured a ${sendDims![1]}px-wide encode for a ${OVERSIZE_WIDTH}px ` +
          `capture — the encode ceiling did not bite. Readout: "${sendReadout}"`,
      ).toBeLessThan(OVERSIZE_WIDTH);

      // Poll the viewer. `settleAtWidth` is the source width, which this share can
      // never reach — so this deliberately runs the full window and reports the
      // high-water mark rather than exiting early on a partial frame.
      const decoded = await maxDecodedScreenWidth(hostPage, 60_000, OVERSIZE_WIDTH);

      expect(
        decoded.width,
        "the viewer never received a decoded frame — an oversized keyframe that " +
          "breaches actix-api's MAX_FRAME_SIZE closes the connection, which looks " +
          "exactly like this",
      ).toBeGreaterThan(0);

      expect(
        decoded.width,
        `decoded width ${decoded.width} must be strictly below the ${OVERSIZE_WIDTH}px ` +
          `source — the encode ceiling did not bite`,
      ).toBeLessThan(OVERSIZE_WIDTH);

      // Uniform downscale, not a per-axis clamp (issue #1037).
      const sourceAspect = OVERSIZE_WIDTH / OVERSIZE_HEIGHT;
      const decodedAspect = decoded.width / decoded.height;
      expect(
        Math.abs(decodedAspect - sourceAspect) / sourceAspect,
        `capped geometry ${decoded.width}x${decoded.height} must keep the source ` +
          `aspect ${OVERSIZE_WIDTH}x${OVERSIZE_HEIGHT}; a per-axis clamp stretches it`,
      ).toBeLessThan(0.02);
    } finally {
      for (const m of members) {
        await m.context.close().catch(() => {});
      }
      for (const b of browsers) {
        await b.close().catch(() => {});
      }
    }
  });
});
