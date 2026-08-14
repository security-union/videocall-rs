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
 * ## The fix this spec guards
 * `SCREEN_QUALITY_TIERS` gained a `1440p` (2560x1440) and a `native`
 * (3840x2160) rung, and a share now STARTS at the rung resolved from the
 * CAPTURED SOURCE size instead of a flat network-signal answer. A 2496x1440
 * source on a healthy link therefore configures the 2560x1440 rung, and
 * `fit_within_preserving_aspect` — which never upscales — encodes it at
 * 2496x1440: the source's own pixels, no resample at all.
 *
 * ## What is asserted, and why it cannot pass on the un-fixed code
 * The RECEIVER's screen-share canvas buffer is sized to the decoded frame's
 * display dimensions on every size change (`peer_decoder.rs`
 * `render_to_canvas_cached` -> `canvas_buffer_dims`, which passes display dims
 * through unchanged). So `canvas.width` on the viewer IS the decoded width.
 *
 * This spec shares a 2496x1440 surface and asserts the viewer's decoded width
 * EXCEEDS 1920. On the un-fixed code that is unreachable BY CONSTRUCTION: the
 * widest rung in the old ladder was 1920x1080, so the encoder could never be
 * configured wider than 1920 whatever tier it sat on. Concretely, the un-fixed
 * cold-start path (`host.rs` passes `initial_screen_tier(rtt, camera_tier)`,
 * which returns 0 on a healthy localhost link) landed on the old index 0 =
 * `high` (1920x1080), and `fit_within_preserving_aspect` fitted 2496x1440 into
 * it at x0.75 -> 1872x1080. 1872 < 1920, so the assertion fails; any AQ
 * step-down from there only lowers it further.
 *
 * ## The share's quality CEILING, and why this spec gates on core count
 * The #2179 review rounds made the ENCODE tier the composition of four terms
 * (`resolve_screen_tier_ceiling`, videocall-aq/src/constants.rs), most
 * restrictive (highest index) winning. For this spec's 2496x1440 source:
 *   - SOURCE — `resolve_initial_screen_tier(2496,1440,0)` = 1 (`1440p`)
 *   - DEVICE — `screen_tier_device_floor(cores)`: 0 at `cores >= 10`
 *     (`SCREEN_TIER_NATIVE_MIN_CORES`), 1 at `cores >= 6`
 *     (`SCREEN_TIER_1440P_MIN_CORES`), else 2 (`high`)
 *   - PUBLISH LADDER — `screen_ladder_top_index()` = 1; the ladder tops out at
 *     the 1440p rung, and round 3 made that an unconditional floor on the index
 *     so no path can encode better than the ladder's own top
 *   - STREAM COUNT — `screen_tier_single_stream_floor(cores)` =
 *     `max(1, device_floor(cores))`, live here because the e2e stack is
 *     single-stream (see the simulcast note below)
 * Composing:
 *   - cores >= 6 → ceiling = max(1, {0|1}, 1, 1) = 1 → encode 2496x1440 →
 *     decoded 2496. Discriminates, and this spec asserts it. (Pinned host-side
 *     by the 8-core "issue-reporter case" in screen_encoder.rs.) Note the
 *     ladder term does NOT bind here: this source deserves exactly the 1440p
 *     rung the ladder tops out at, so nothing is withheld.
 *   - cores <  6 → the DEVICE term is 2, so ceiling = 2 (`high`) → encode
 *     1920x1080 → decoded 1872. That is the FIX BEHAVING CORRECTLY — a low-core
 *     sender is deliberately held to 1080p — but it is also exactly what the
 *     un-fixed code produced, so the spec has NO discriminator and would report
 *     a misleading red. It therefore SKIPS below the bar, naming the observed
 *     core count.
 *
 * NOTE the bar is 6, not 10: at 6-9 cores the DEVICE term is `1440p`, which is
 * the very rung this source needs, so the share is unconstrained there. Gating
 * on the native/2160p bar instead would skip on ordinary consumer hardware that
 * this spec can and should assert on.
 *
 * `cores` is `navigator.hardwareConcurrency` read via
 * `videocall_client::utils::hardware_concurrency_cores` (memoized in a
 * `OnceLock`, with NO test override), so the test reads the same fact from the
 * SHARER page to decide. Playwright launches its browsers on the HOST (the
 * Makefile runs `npx playwright test` outside the compose stack), so this is the
 * host's core count, not the UI container's.
 *
 * ## Mock notes
 * - The `getDisplayMedia` mock ignores the requested constraints (it just
 *   returns a canvas stream), which is deliberate: it isolates the ENCODER-side
 *   half of the fix from the browser's capture-constraint behaviour, so the test
 *   does not depend on how a headless Chrome honours `max`.
 * - The mock track's `getSettings()` IS load-bearing, and is therefore
 *   ASSERTED below rather than assumed. `screen_stream_source_dims` reads
 *   `getSettings().width/height` to resolve the start tier; a track that
 *   reports no size yields `(0,0)`, which maps to the `high` (1920x1080) rung —
 *   producing a 1872x1080 encode and a RED that looks exactly like a #2179
 *   regression but is really a broken mock.
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
 * The widest rung the PRE-#2179 ladder could encode. A decoded width above
 * this is only reachable with the new rungs + source-driven start.
 */
const PRE_2179_MAX_ENCODE_WIDTH = 1920;

/** Window global the mock stamps with the capture track's reported settings. */
const MOCK_SOURCE_SIZE_GLOBAL = "__e2eScreenMockSourceSize";

/**
 * Logical-core bar at/above which the DEVICE term of the share's quality
 * ceiling stops binding a 1440p-class source — `SCREEN_TIER_1440P_MIN_CORES`
 * in `videocall-aq/src/constants.rs`.
 *
 * This is a SKIP GATE, not the assertion, which is why it may restate a
 * production constant: below this bar the fix DELIBERATELY caps the share at
 * the 1080p rung, so a >1920 decode is unreachable BY DESIGN and there is no
 * discriminator to measure. See the ceiling walk-through in the header.
 */
const TIER_1440P_MIN_CORES = 6;

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
const MOCK_GET_DISPLAY_MEDIA_SCRIPT = `
  (() => {
    const mediaDevices = navigator.mediaDevices;
    if (!mediaDevices) return;
    const createStream = () => {
      const canvas = document.createElement('canvas');
      canvas.width = ${SOURCE_WIDTH}; canvas.height = ${SOURCE_HEIGHT};
      const ctx = canvas.getContext('2d');
      let frame = 0;
      const paint = () => {
        frame += 1;
        ctx.fillStyle = '#1a1a2e';
        ctx.fillRect(0, 0, ${SOURCE_WIDTH}, ${SOURCE_HEIGHT});
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
async function maxDecodedScreenWidth(
  viewerPage: Page,
  windowMs: number,
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
    if (best.width >= SOURCE_WIDTH) {
      break;
    }
    await viewerPage.waitForTimeout(500);
  }
  return best;
}

test.describe("Peer screen-share HiDPI resolution (issue #2179)", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test("a 2496x1440 Retina share is decoded above the pre-#2179 1920px ceiling", async ({
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

      // ── Precondition: the SHARER's device class must allow the 1440p rung. ─
      // Below `SCREEN_TIER_1440P_MIN_CORES` the DEVICE term of the ceiling pins
      // the share to 1080p by design, which is indistinguishable from the
      // un-fixed behaviour — no discriminator, so skip rather than report a red
      // the fix did not cause.
      const sharerCores = await guestPage.evaluate(() => navigator.hardwareConcurrency);
      if (!sharerCores || sharerCores < TIER_1440P_MIN_CORES) {
        test.skip(
          true,
          `sharer reports navigator.hardwareConcurrency=${sharerCores}, below the ` +
            `${TIER_1440P_MIN_CORES}-core bar (SCREEN_TIER_1440P_MIN_CORES). The device ` +
            `term of resolve_screen_tier_ceiling pins this share to the 1080p rung BY ` +
            `DESIGN, so a >${PRE_2179_MAX_ENCODE_WIDTH}px decode is unreachable and this ` +
            `spec has no #2179 discriminator on this host.`,
        );
        return;
      }

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
      // The source term of the ceiling is resolved from exactly this. Without
      // it the resolved rung is `high` (1920x1080) and the decoded-width
      // assertion below would fail for a reason unrelated to #2179.
      const reportedSource = await mockReportedSourceSize(guestPage);
      expect(
        reportedSource,
        "the getDisplayMedia mock never stamped a capture size — the mock did not run",
      ).toBeTruthy();
      expect(
        { width: reportedSource?.width, height: reportedSource?.height },
        "the capture track must report its source size through getSettings(); the " +
          "source term of the share's quality ceiling is resolved from it",
      ).toEqual({ width: SOURCE_WIDTH, height: SOURCE_HEIGHT });

      const decoded = await maxDecodedScreenWidth(hostPage, 60_000);

      // The canvas must have been sized by a real decoded frame at all — a 0
      // width means no frame ever painted and the assertion below would be
      // vacuous.
      expect(
        decoded.width,
        "the viewer's screen-share canvas was never sized by a decoded frame",
      ).toBeGreaterThan(0);

      // THE regression assertion. Unreachable on the pre-#2179 ladder, whose
      // widest rung was 1920x1080.
      expect(
        decoded.width,
        `decoded screen width ${decoded.width}x${decoded.height} must exceed the ` +
          `pre-#2179 ${PRE_2179_MAX_ENCODE_WIDTH}px encode ceiling for a ` +
          `${SOURCE_WIDTH}x${SOURCE_HEIGHT} source (sharer cores=${sharerCores}, ` +
          `so the device term does not bind)`,
      ).toBeGreaterThan(PRE_2179_MAX_ENCODE_WIDTH);

      // And the aspect ratio must be the source's — the new rung is a bounding
      // box, not a fixed output size, so a 2496x1440 source must not be
      // letterboxed or stretched into 2560x1440.
      const sourceAspect = SOURCE_WIDTH / SOURCE_HEIGHT;
      const decodedAspect = decoded.width / decoded.height;
      expect(
        Math.abs(decodedAspect - sourceAspect) / sourceAspect,
        `decoded aspect ${decoded.width}x${decoded.height} must match the source ` +
          `${SOURCE_WIDTH}x${SOURCE_HEIGHT}`,
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
