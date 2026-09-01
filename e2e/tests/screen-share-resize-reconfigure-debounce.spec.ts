import { test, expect, chromium, Page, BrowserContext } from "@playwright/test";
import { BROWSER_ARGS, createAuthenticatedContext } from "../helpers/auth-context";
import { waitForServices } from "../helpers/wait-for-services";
import { wakeControls } from "../helpers/controls";

/**
 * Screen-share drag-resize does NOT storm encoder reconfigures (issue #1922) —
 * sharer side.
 *
 * ## What this guards
 *
 * A `getDisplayMedia` track re-negotiates its native capture dimensions on every
 * step of a window drag-resize, delivering a burst of frames whose
 * `display_width()/display_height()` change continuously (the field build saw up
 * to 18 dimension deltas in a single second). Pre-#1922 the screen encode loop
 * rebuilt `VideoEncoderConfig` and called `configure()` IMMEDIATELY on each delta
 * (`videocall-client/src/encode/screen_encoder.rs`). In WebCodecs the first
 * `encode()` after a `configure()` is an IMPLICIT keyframe that bypasses the
 * keyframe cooldown/coalescer, so a single drag became a ~140-keyframe storm —
 * pixelation for every receiver and, when a `configure()` fatally errored
 * mid-storm, a re-prompting restart that dropped the whole share
 * (`ScreenShareEvent::Cancelled`).
 *
 * The #1922 fix adds a source-dimension SETTLE gate (`DimensionSettle` /
 * `SCREEN_DIM_SETTLE_MS = 400ms`): during a drag the encoder holds its config
 * (WebCodecs scales the frames, output stays valid) and applies exactly ONE
 * `configure()` + one keyframe ~400ms after the source dims stop moving. The base
 * frame-arm reconfigure is gated behind `dim_settle.is_settled(..)`, so instead
 * of one `info!("... reconfiguring encoder")` per delta there is ZERO during the
 * drag and exactly one settled apply after it
 * (`info!("... reconfiguring encoder (settled #1922)")`, or the static-timer
 * branch's `"applied settled resize -> WxH ... (issue #1922)"`).
 *
 * ## How the scenario is driven
 *
 * A real 2-peer meeting: the HOST publishes a canvas-backed `getDisplayMedia`
 * mock and the GUEST views it. The mock's canvas is RESIZABLE at runtime and,
 * because it is a `captureStream(0)` source, each `requestFrame()` captures the
 * canvas at its CURRENT size — so resizing the canvas then requesting a frame
 * delivers a `VideoFrame` whose `displayWidth/displayHeight` follow the new size,
 * exactly the source-dimension change a real drag produces. (Verified empirically
 * that canvas resize propagates through `MediaStreamTrackProcessor` — the read
 * path the ScreenEncoder uses — as changed `VideoFrame.displayWidth`.)
 *
 * `runBurst(steps, stepPx, intervalMs)` steps the canvas width up by `stepPx`
 * every `intervalMs`, emitting one frame per step (15 distinct sizes at ~60ms ≈
 * the field's ~18/sec), then holds at the final size and resumes steady emission
 * so a post-settle frame arrives and the single settled reconfigure applies. All
 * sizes stay <= 1280x720, far below the `SCREEN_MAX_ENCODE_*` ceiling, so each
 * encodes at its own size 1:1 (no aspect clamping) and every step is a genuine
 * encode-dim change. (The ceiling's VALUE is deliberately not restated here — it
 * has already moved once, 3840x2160 -> 2560x1440.)
 *
 * ## Assertions (and why they fail on the un-fixed code)
 *
 *  1. The share SURVIVES: the sharer's UI logs no
 *     `"Screen share state changed: Cancelled"` and the guest still shows the
 *     `.split-screen-tile`.
 *  2. The guest's decoded screen canvas is still PAINTING after the storm
 *     (mean luminance clears the blank threshold).
 *  3. The sharer emits at most 2 base `"reconfiguring encoder"` lines across the
 *     burst + hold window. On the un-fixed code every drag delta reconfigures
 *     (~15 here), so this bound is exceeded and the assertion FAILS. On the fixed
 *     code the gate defers all of them and only the single settled apply remains.
 *  4. The sharer emits at least one SETTLED apply line (`settled #1922` /
 *     `applied settled resize`), proving the deferred source-dimension change was
 *     applied exactly once AND that the mock drove a real dim change into the
 *     encoder. The un-fixed code has no such line (0), so this also FAILS on it.
 *
 * (1)+(2) alone are weak discriminators — a well-behaved mock may survive the
 * un-fixed storm without a fatal `configure()` — so (3)+(4) are the load-bearing
 * fails-on-unfixed guards, using the same console-counting technique as
 * `screen-share-static-keyframe-floor.spec.ts`.
 *
 * Mirrors the auth + 2-peer share harness of `peer-screen-static-fps.spec.ts`.
 */

const DEFAULT_UI_URL = "http://localhost:3001";

interface MeetingMember {
  page: Page;
  context: BrowserContext;
  email: string;
  name: string;
}

// The base `info!` substring emitted by the encode loop's frame-arrival
// dimension reconfigure. Pre-#1922 it fires once PER drag delta; with the fix it
// fires only on the single settled apply (as "... reconfiguring encoder (settled
// #1922)"). Counting occurrences across the burst window is the storm gauge.
const RECONFIGURE_LOG_SUBSTRING = "reconfiguring encoder";

// The screen-share state line the Dioxus UI logs on the SHARER. If the storm
// tears the share down (fatal configure -> restart -> getDisplayMedia re-prompt)
// the event becomes `Cancelled` and this exact line appears.
const SHARE_CANCELLED_LINE = "Screen share state changed: Cancelled";

// Runtime-resizable canvas-backed `getDisplayMedia` mock. Unlike the static-fps /
// floor mocks (fixed 1280x720), this exposes `window.__e2e1922` so the test can
// drive a drag-resize burst: `runBurst` steps `canvas.width` and emits one frame
// per step (captureStream(0) captures the current canvas size on requestFrame),
// then holds and resumes steady emission.
const MOCK_RESIZABLE_DISPLAY_MEDIA_SCRIPT = `
  (() => {
    const mediaDevices = navigator.mediaDevices;
    if (!mediaDevices) return;
    // Initial source size — well below the encode ceiling, so the encoder
    // configures at exactly this size (issue #2343: encode geometry IS the
    // capture geometry unless the ceiling bites).
    const state = { w: 1024, h: 640, emitting: true };
    window.__e2e1922 = state;
    const createStream = () => {
      const canvas = document.createElement('canvas');
      canvas.width = state.w; canvas.height = state.h;
      const ctx = canvas.getContext('2d');
      let frame = 0;
      const paint = () => {
        frame++;
        ctx.fillStyle = '#1a1a2e'; ctx.fillRect(0, 0, canvas.width, canvas.height);
        ctx.fillStyle = '#fff'; ctx.font = '28px sans-serif';
        ctx.fillText('Mock Screen Share (e2e-1922)', 60, Math.floor(canvas.height / 2));
        // A moving marker so live frames carry deltas (a solid still would encode
        // to ~zero and the guest canvas would never repaint).
        ctx.fillStyle = '#ff0';
        const x = 40 + ((frame * 12) % Math.max(100, canvas.width - 120));
        ctx.fillRect(x, canvas.height - 60, 18, 18);
      };
      paint();
      const stream = canvas.captureStream(0);
      const track = stream.getVideoTracks()[0];
      const emitOnce = () => {
        paint();
        if (typeof track.requestFrame === 'function') {
          try { track.requestFrame(); } catch (_) { /* ignore */ }
        }
      };
      const resize = (w, h) => { canvas.width = w; canvas.height = h; state.w = w; state.h = h; };
      // Drive a drag-resize: step the width up, emitting one frame per step.
      state.runBurst = async (steps, stepPx, intervalMs) => {
        state.emitting = false; // only the burst emits during the drag
        const baseW = state.w, baseH = state.h;
        for (let i = 1; i <= steps; i++) {
          resize(baseW + i * stepPx, baseH);
          emitOnce();
          await new Promise((r) => setTimeout(r, intervalMs));
        }
        state.emitting = true; // steady emission resumes at the final size
      };
      const tick = () => {
        if (state.emitting) emitOnce();
        setTimeout(tick, 70); // ~14fps steady capture when emitting
      };
      tick();
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

// Sharer clicks "Share Screen"; returns true once the VIEWER transitions to the
// split layout — which confirms encoded screen frames actually reached the guest
// (so the encoder produced a retained frame).
async function startScreenShare(sharerPage: Page, viewerPage: Page): Promise<boolean> {
  await wakeControls(sharerPage);
  await sharerPage.waitForTimeout(300);
  const shareButton = sharerPage.locator("button.video-control-button", {
    has: sharerPage.locator(".tooltip", { hasText: "Share Screen" }),
  });

  await expect(shareButton).toBeVisible({ timeout: 10_000 });
  await shareButton.click();

  try {
    await expect(viewerPage.locator(".split-screen-tile")).toBeVisible({ timeout: 15_000 });
    return true;
  } catch {
    return false;
  }
}

/**
 * Mean luminance (0-255) of the peer's DECODED screen-share canvas, downsampled
 * to 64x36. `null` when the canvas is absent or unsampleable. A never-decoded
 * canvas is transparent/black (mean ~0); a decoded frame of the mock (a `#1a1a2e`
 * field, R≈26, with white text) has a mean clearly above zero, so mean > 8
 * separates "painted a real frame" from "blank". Copied from
 * `peer-screen-static-fps.spec.ts` (the const there is module-scoped).
 */
async function screenCanvasMean(page: Page): Promise<number | null> {
  return page.evaluate(() => {
    const canvas = document.querySelector(
      '.split-screen-tile canvas[id^="screen-share-"]',
    ) as HTMLCanvasElement | null;
    if (!canvas || canvas.width === 0 || canvas.height === 0) {
      return null;
    }
    const off = document.createElement("canvas");
    off.width = 64;
    off.height = 36;
    const octx = off.getContext("2d");
    if (!octx) {
      return null;
    }
    try {
      octx.drawImage(canvas, 0, 0, 64, 36);
      const data = octx.getImageData(0, 0, 64, 36).data;
      let sum = 0;
      let n = 0;
      for (let i = 0; i < data.length; i += 4) {
        sum += data[i];
        n += 1;
      }
      return n === 0 ? null : sum / n;
    } catch {
      return null;
    }
  });
}

test.describe("Screen-share drag-resize reconfigure debounce (issue #1922)", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test("a drag-resize burst survives without a reconfigure/keyframe storm", async ({ baseURL }) => {
    test.setTimeout(240_000);
    const uiURL = baseURL || DEFAULT_UI_URL;
    const meetingId = `e2e_ss_resize_${Date.now()}`;

    const browsers = await Promise.all([
      chromium.launch({ args: BROWSER_ARGS }),
      chromium.launch({ args: BROWSER_ARGS }),
    ]);

    const members: MeetingMember[] = [];

    // Live capture of the SHARER's (host's) console — the ScreenEncoder
    // reconfigure `info!` lines and the UI's screen-share state line.
    const sharerConsole: string[] = [];

    try {
      const profiles = [
        { email: "host-1922@videocall.rs", name: "Resize1922Host" },
        { email: "guest-1922@videocall.rs", name: "Resize1922Guest" },
      ];

      for (let i = 0; i < 2; i++) {
        const ctx = await createAuthenticatedContext(
          browsers[i],
          profiles[i].email,
          profiles[i].name,
          uiURL,
        );
        await ctx.addInitScript(MOCK_RESIZABLE_DISPLAY_MEDIA_SCRIPT);
        if (i === 0) {
          ctx.on("page", (p) => {
            p.on("console", (msg) => {
              sharerConsole.push(msg.text());
            });
          });
        }
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

      const hostPage = members[0].page; // sharer
      const guestPage = members[1].page; // viewer

      const shareActivated = await startScreenShare(hostPage, guestPage);
      if (!shareActivated) {
        test.skip(
          true,
          "getDisplayMedia mock did not produce a stream that triggered the split layout.",
        );
        return;
      }

      // Phase 1 — LIVE. Let real frames flow at the initial (steady) size so the
      // encoder is fully live, has a retained frame, and the guest has painted a
      // real decoded frame BEFORE the drag.
      await hostPage.waitForTimeout(4000);

      expect(
        sharerConsole.some((l) => l.includes("Start screen share encoder")),
        "expected the screen-share encoder to have started",
      ).toBe(true);

      const guestMeanBefore = (await screenCanvasMean(guestPage)) ?? 0;
      expect(
        guestMeanBefore,
        "guest should be painting the shared canvas before the drag",
      ).toBeGreaterThan(8);

      // Mark the console position so reconfigure counting covers ONLY the
      // burst + hold window (the steady LIVE phase produces no dim change).
      const mark = sharerConsole.length;

      // Phase 2 — DRAG-RESIZE BURST. 15 width steps of +8px every 60ms (~0.9s of
      // dragging; 15 distinct fitted sizes at ~17/sec, matching the field's peak
      // ~18/sec). Awaited: resolves when the burst has emitted all steps.
      await hostPage.evaluate(() =>
        (
          window as unknown as {
            __e2e1922: { runBurst: (a: number, b: number, c: number) => Promise<void> };
          }
        ).__e2e1922.runBurst(15, 8, 60),
      );

      // Phase 3 — HOLD. Steady emission has resumed at the final size; wait past
      // the 400ms settle window (+ static poll + encoder settle) so the single
      // deferred reconfigure applies and its keyframe reaches the guest.
      await hostPage.waitForTimeout(3000);

      const burstWindow = sharerConsole.slice(mark);
      const reconfigureCount = burstWindow.filter((l) =>
        l.includes(RECONFIGURE_LOG_SUBSTRING),
      ).length;
      const settleApplyCount = burstWindow.filter(
        (l) => l.includes("settled #1922") || /applied settled resize .*#1922/.test(l),
      ).length;
      const cancelledCount = sharerConsole.filter((l) => l.includes(SHARE_CANCELLED_LINE)).length;

      // (1) The share SURVIVED: no Cancelled event, and the guest still shows the
      // split screen-share tile.
      expect(
        cancelledCount,
        `screen share must not be torn down by the resize burst; saw ${cancelledCount} ` +
          `"${SHARE_CANCELLED_LINE}" line(s) on the sharer`,
      ).toBe(0);
      await expect(
        guestPage.locator(".split-screen-tile"),
        "guest must still be viewing the screen share after the burst",
      ).toBeVisible({ timeout: 5_000 });

      // (2) The guest's decoded screen canvas is still PAINTING after the storm.
      const guestMeanAfter = (await screenCanvasMean(guestPage)) ?? 0;
      expect(
        guestMeanAfter,
        "guest screen canvas must still be painting a real frame after the resize burst",
      ).toBeGreaterThan(8);

      // (3) NO reconfigure storm. The fix defers every per-delta reconfigure; only
      // the single settled apply remains. On the un-fixed code this is ~15 (one
      // per drag delta) and this bound is exceeded.
      expect(
        reconfigureCount,
        `expected at most 2 base "${RECONFIGURE_LOG_SUBSTRING}" lines across the ` +
          `burst+hold window (the #1922 settle gate defers per-delta reconfigures); ` +
          `saw ${reconfigureCount}. On the un-fixed code every drag delta reconfigures ` +
          `(~15), so a high count here means the settle gate did not engage.`,
      ).toBeLessThanOrEqual(2);

      // (4) The deferred source-dimension change WAS applied — exactly once,
      // coalesced. This also proves the mock drove a real fitted-dim change into
      // the encoder (a no-op mock would apply nothing). The un-fixed code emits no
      // "#1922" line, so this is 0 there.
      expect(
        settleApplyCount,
        `expected at least one SETTLED reconfigure apply line ("settled #1922" / ` +
          `"applied settled resize ... #1922") after the drag settled; saw ${settleApplyCount}. ` +
          `Its absence means the deferred reconfigure never applied (or the mock drove no dim change).`,
      ).toBeGreaterThanOrEqual(1);
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
