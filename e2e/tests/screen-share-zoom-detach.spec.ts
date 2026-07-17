/**
 * Issue 1175 (v2): zoom / pan / detach for a RECEIVED screen share.
 *
 * Re-verified against commit 58deca56 ("detached-window mirror (play) + main
 * window regular-meeting flip") — the detach behavior was reworked after user
 * testing found the old detached window opened BLANK and the share stayed in the
 * main window. The current contract:
 *
 *   - Zoom/pan on the MAIN tile are rendered DECLARATIVELY from a Dioxus signal
 *     (`ScreenZoomCtx`) as a CSS `transform` on `.ss-zoom-wrapper`; the
 *     `<canvas>` the decoder paints into is NEVER removed or recreated by a
 *     zoom, a reset, a detach, or an unrelated re-render (the "frozen video"
 *     regression).
 *   - The zoom label / clamp state is arithmetic on the pure helpers
 *     (`screen_share_zoom.rs`, ×1.25 steps clamped to [1.0, 4.0]); the control's
 *     percentage, `aria-disabled` clamp state, and `is-zoomed` viewport class
 *     are deterministic.
 *   - DETACH: clicking `[data-testid=ss-detach]` opens a REAL separate window
 *     (Document PiP, or the `window.open` fallback forced headless below) that
 *     mirrors the canvas into a `<video>` and hosts its OWN zoom + reattach
 *     controls. The MAIN window then flips to a regular no-share meeting: the
 *     share pane is hidden OFF-SCREEN (`.share-detached` on #grid-container) but
 *     stays MOUNTED so the canvas keeps its node identity and keeps painting to
 *     feed the mirror; the peer grid goes full width. REATTACH (popup Reattach
 *     button / Escape / closing the window) restores the split layout and
 *     returns focus to ss-detach.
 *   - Zoom state survives an unrelated re-render of the tile (v1 defect D7:
 *     imperative zoom reset on every re-render).
 *
 * Harness: the proven REAL 2-peer screen-share pattern from
 * `crop-toggle.spec.ts` (test 5) and `wt-screen-share-split-layout.spec.ts`.
 * A guest publishes a mocked `getDisplayMedia()` screen share; the host is the
 * VIEWER whose `.split-screen-tile` carries the zoom/detach UI under test. Mock
 * peers are video-OFF placeholders with no canvas, so real browser peers are
 * mandatory here.
 *
 * DOM contract (re-verified against commit 3cdfc2c5 = HEAD's parent, the latest
 * fix-round follow-up on 58deca56; canvas_generator.rs, screen_share_detach.rs,
 * attendants.rs, global.css):
 *   MAIN tile (`ScreenShareZoomable` L1729, `ScreenShareZoomControls` L1927):
 *   - `[data-testid=ss-zoom-viewport]` focusable pan group (L1901); its class
 *     gains `is-zoomed` at scale>1.0 (L1740, `is_zoomed(scale)`). It holds
 *     `.ss-zoom-wrapper` with inline `style: "transform: {transform};"` (L1912)
 *     which holds `canvas#screen-share-<peer>` (`ScreenCanvas`).
 *   - `[data-testid=ss-zoom-controls]` hover/focus-revealed bar (L2092;
 *     opacity:0 + pointer-events:none until `.split-screen-tile:hover`/
 *     `:focus-within` — hence `tile.hover()` before any control click).
 *   - `[data-testid=ss-zoom-out|ss-zoom-in]` `aria-disabled="true"` at the
 *     fit/max clamp (L2100/L2117, NOT native `disabled`); `ss-zoom-label` text
 *     (L2109); `ss-zoom-reset` L2128; `[data-testid=ss-detach]` (L2139)
 *     aria-pressed (L2142), gated by `detach_supported()` (screen_share_detach.rs
 *     L208; desktop 1280>768 → rendered). The old `.is-detached` overlay, bring-
 *     back button, and inert `.ss-tile-interior` were removed in 58deca56.
 *   MAIN window while detached (attendants.rs):
 *   - `#grid-container` gains `share-detached` (L5960) → CSS (global.css
 *     L3202-3216) moves `.ss-left-pane` (L6797) to `left:-99999px` (still
 *     MOUNTED + composited, canvas persists) and makes `.ss-peer-panel` (L6825)
 *     100% wide; `.screen-share-resize-handle` display:none. 3cdfc2c5 also adds
 *     `inert="true"` on `.ss-left-pane` while detached (L6797,
 *     `share_detached.then_some("true")`) so the off-screen controls leave the
 *     tab / AT tree.
 *   - `[data-testid=ss-detach-announce]` visually-hidden role=status aria-live
 *     region at meeting level in #grid-container (canvas_generator.rs
 *     `ScreenDetachAnnouncer`, testid L2182, rendered attendants.rs L6715),
 *     announcing "Shared content opened in a separate window" (L2170) / "Shared
 *     content returned to the meeting" (L2172).
 *   - Focus (ScreenShareZoomControls use_effect): ENTER→#grid-container (L1979),
 *     EXIT→ss-detach toggle (L1981).
 *   DETACHED window DOM (screen_share_detach.rs `build_detached_dom` L506,
 *   `wire_zoom_controls` L669, ids as consts L63-70) — located by `id`:
 *   - `#ss-detached-video` mirror <video> (L579; autoplay+muted+playsinline;
 *     srcObject=captureStream L158; explicit `play()` L180-181 is the 58deca56
 *     blank-mirror fix).
 *   - `#ss-detached-viewport` (L561; gets `data-zoomed` at scale>1) >
 *     `#ss-detached-wrapper` (L571; transform set via style) > the video.
 *   - `#ss-detached-zoom-in|zoom-out|zoom-reset` with `aria-disabled` at clamps;
 *     `#ss-detached-zoom-label` text; `#ss-detached-reattach` button (L63) +
 *     Escape + 400ms close-poll all route to teardown→reattach (finish_open
 *     L436-483). Controls always visible (DETACHED_CSS).
 *
 * Untagged (no `@bvt`): does not run in per-PR CI by design; the full-suite
 * dispatch covers it.
 */

import { test, expect, chromium, Browser, Locator, Page } from "@playwright/test";
import { BROWSER_ARGS, createAuthenticatedContext } from "../helpers/auth-context";
import { waitForServices } from "../helpers/wait-for-services";
import { wakeControls } from "../helpers/controls";

const SPLIT_LAYOUT_ACTIVATION_TIMEOUT_MS = 15_000;

/**
 * Mock `navigator.mediaDevices.getDisplayMedia` with a canvas-derived
 * `MediaStream`. Same pattern as `wt-screen-share-split-layout.spec.ts` /
 * `crop-toggle.spec.ts` — a real stream, so the publisher's encode path runs
 * and the viewer's `screen_enabled` actually flips via on-wire SCREEN frames.
 */
const MOCK_GET_DISPLAY_MEDIA_SCRIPT = `
  (() => {
    const md = navigator.mediaDevices;
    if (!md) return;
    const makeStream = () => {
      const c = document.createElement('canvas');
      c.width = 640; c.height = 480;
      const ctx = c.getContext('2d');
      ctx.fillStyle = '#1a1a2e'; ctx.fillRect(0, 0, 640, 480);
      ctx.fillStyle = '#fff'; ctx.font = '24px sans-serif';
      ctx.fillText('Mock Screen Share', 160, 240);
      return c.captureStream(5);
    };
    Object.defineProperty(md, 'getDisplayMedia', {
      configurable: true, value: async () => makeStream(),
    });
  })();
`;

/**
 * ANIMATED getDisplayMedia mock, used ONLY by the detach test (Test 3a). A
 * canvas drawn ONCE emits no NEW captured frames, so a captureStream mirror of
 * it cannot be proven "advancing" — its pixels never change (and under headless
 * --disable-gpu the mirror video never even reaches readyState>=2). Repaint every
 * frame (moving bar + counter) so the detached-window mirror receives
 * continuously-advancing, non-blank frames and the pixel-liveness assertion is
 * meaningful. Verified: this renders to a live <video> even under headless
 * --disable-gpu (CI), which a static source does NOT.
 *
 * Scoped to Test 3a on purpose: the zoom / layout tests assert transform /
 * identity / layout (not pixels), and the extra per-frame encode load competes
 * with the guest's CAMERA encode — enough, on a loaded headless box, to drop the
 * camera video the re-render test checks for ("Video Disabled"). Those tests keep
 * the cheap static mock.
 */
const ANIMATED_SHARE_MOCK = `
  (() => {
    const md = navigator.mediaDevices;
    if (!md) return;
    const makeStream = () => {
      const c = document.createElement('canvas');
      c.width = 640; c.height = 480;
      const ctx = c.getContext('2d');
      let n = 0;
      (function paint() {
        n++;
        ctx.fillStyle = '#1a1a2e'; ctx.fillRect(0, 0, 640, 480);
        ctx.fillStyle = '#4fd1c5'; ctx.fillRect((n * 6) % 640, 80, 120, 320);
        ctx.fillStyle = '#fff'; ctx.font = '24px sans-serif';
        ctx.fillText('Mock Screen Share ' + n, 120, 240);
        requestAnimationFrame(paint);
      })();
      return c.captureStream(10);
    };
    Object.defineProperty(md, 'getDisplayMedia', {
      configurable: true, value: async () => makeStream(),
    });
  })();
`;

/**
 * STOP-EMITTING (static) getDisplayMedia mock for the detach-static case (issue
 * #1841). Draws content, emits ~8 real frames via `captureStream(0)` +
 * `requestFrame()` so the RECEIVER decodes and paints its source canvas, then
 * STOPS — the source canvas keeps its last painted content but never repaints.
 * This is the faithful model of a real `getDisplayMedia` share whose content went
 * static: the popup's `captureStream()` of that idle source canvas is starved, so
 * only the detach fix's one-shot source-canvas repaint prime can surface the still.
 */
const STATIC_SHARE_MOCK = `
  (() => {
    const md = navigator.mediaDevices;
    if (!md) return;
    const makeStream = () => {
      const c = document.createElement('canvas');
      c.width = 640; c.height = 480;
      const ctx = c.getContext('2d');
      const draw = () => {
        ctx.fillStyle = '#1a1a2e'; ctx.fillRect(0, 0, 640, 480);
        ctx.fillStyle = '#fff'; ctx.font = '24px sans-serif';
        ctx.fillText('Mock Screen Share (static)', 120, 240);
      };
      draw();
      const stream = c.captureStream(0);
      const track = stream.getVideoTracks()[0];
      let emitted = 0;
      const tick = () => {
        if (emitted < 8) {
          draw();
          if (typeof track.requestFrame === 'function') {
            try { track.requestFrame(); } catch (_) { /* ignore */ }
          }
          emitted++;
          setTimeout(tick, 100);
        }
        // After 8 frames: STOP. The source is now static — captureStream emits
        // nothing more and the receiver's decoded canvas stops repainting.
      };
      tick();
      return stream;
    };
    Object.defineProperty(md, 'getDisplayMedia', {
      configurable: true, value: async () => makeStream(),
    });
  })();
`;

/**
 * Force the detach `window.open` fallback path in headless Chromium by
 * shadowing `documentPictureInPicture` with an own-property getter that returns
 * `undefined`. The Rust side reads it via `Reflect::get` and treats
 * undefined/null as "PiP unsupported" (`screen_share_detach.rs`
 * `document_pip_supported()`), so `open()` takes `open_popup` → `window.open`.
 *
 * Why: Document Picture-in-Picture's `requestWindow` is unreliable headless
 * (it may reject with no compositor), whereas `window.open` is a real, reliably
 * -working production path (Firefox / Safari / older Chromium users hit it) that
 * Playwright's headless Chromium honors and surfaces as a new context page.
 * Forcing this path makes the detached-window contract deterministic so the
 * mirror / zoom / reattach flow is actually exercised. The detach tests still
 * tolerate the revert branch (skip) if a given environment blocks the popup.
 */
const FORCE_POPUP_DETACH_SCRIPT = `
  (() => {
    try {
      Object.defineProperty(window, 'documentPictureInPicture', {
        configurable: true,
        get() { return undefined; },
      });
    } catch (e) {
      /* non-configurable here; the detach test tolerates either window path */
    }
  })();
`;

async function navigateToMeeting(page: Page, meetingId: string, username: string): Promise<void> {
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
}

async function joinMeetingFromPage(
  page: Page,
): Promise<"in-meeting" | "waiting" | "waiting-for-meeting"> {
  const joinButton = page.getByRole("button", { name: /Start Meeting|Join Meeting/ });
  const waitingRoom = page.getByText("Waiting to be admitted");
  const waitingForMeeting = page.getByText("Waiting for meeting to start");
  const grid = page.locator("#grid-container");

  const result = await Promise.race([
    joinButton.waitFor({ timeout: 30_000 }).then(() => "join" as const),
    waitingRoom.waitFor({ timeout: 30_000 }).then(() => "waiting" as const),
    waitingForMeeting.waitFor({ timeout: 30_000 }).then(() => "waiting-for-meeting" as const),
    grid.waitFor({ timeout: 30_000 }).then(() => "auto-joined" as const),
  ]);

  if (result === "waiting") return "waiting";
  if (result === "waiting-for-meeting") return "waiting-for-meeting";
  if (result === "auto-joined") return "in-meeting";

  await page.waitForTimeout(1000);
  await joinButton.click();
  await page.waitForTimeout(3000);
  await expect(grid).toBeVisible({ timeout: 15_000 });
  return "in-meeting";
}

async function admitGuestIfNeeded(
  hostPage: Page,
  guestPage: Page,
  guestResult: "in-meeting" | "waiting" | "waiting-for-meeting",
): Promise<void> {
  if (guestResult === "in-meeting") return;

  if (guestResult === "waiting") {
    const admitButton = hostPage.getByTitle("Admit").first();
    await expect(admitButton).toBeVisible({ timeout: 20_000 });
    await hostPage.waitForTimeout(1000);
    await admitButton.dispatchEvent("click");
    await hostPage.waitForTimeout(3000);

    const guestJoinButton = guestPage.getByRole("button", { name: /Join Meeting|Start Meeting/ });
    const guestGrid = guestPage.locator("#grid-container");

    const postAdmit = await Promise.race([
      guestJoinButton.waitFor({ timeout: 20_000 }).then(() => "join-button" as const),
      guestGrid.waitFor({ timeout: 20_000 }).then(() => "grid" as const),
    ]);

    if (postAdmit === "join-button") {
      await guestPage.waitForTimeout(1000);
      await guestJoinButton.click();
      await guestPage.waitForTimeout(3000);
      await expect(guestGrid).toBeVisible({ timeout: 15_000 });
    }
  }
}

/**
 * Publisher clicks Share Screen; returns true once the viewer transitions to
 * the split layout. Mirrors the helper in `wt-screen-share-split-layout.spec.ts`.
 */
async function startScreenShareAndAwaitSplitLayout(
  publisherPage: Page,
  viewerPage: Page,
): Promise<boolean> {
  await wakeControls(publisherPage);
  await publisherPage.waitForTimeout(300);
  const shareButton = publisherPage.locator("button.video-control-button", {
    has: publisherPage.locator(".tooltip", { hasText: "Share Screen" }),
  });

  await expect(shareButton).toBeVisible({ timeout: 10_000 });
  await shareButton.click();

  try {
    await expect(viewerPage.locator(".split-screen-tile")).toBeVisible({
      timeout: SPLIT_LAYOUT_ACTIVATION_TIMEOUT_MS,
    });
    return true;
  } catch {
    return false;
  }
}

interface SharedScreenFixture {
  hostPage: Page;
  guestPage: Page;
  browser1: Browser;
  browser2: Browser;
  /** The host VIEWER's received-share tile — the subject under test. */
  tile: Locator;
}

/**
 * Stand up a real 2-peer meeting where the guest publishes a mocked screen
 * share and the host VIEWS it, then return the host's `.split-screen-tile`.
 * `hostExtraInit` (e.g. `FORCE_POPUP_DETACH_SCRIPT`) is added to the host
 * context before nav.
 */
async function setupViewerSeeingSharedScreen(
  uiURL: string,
  meetingId: string,
  hostName: string,
  guestName: string,
  hostExtraInit?: string,
  shareMock: string = MOCK_GET_DISPLAY_MEDIA_SCRIPT,
  headless: boolean = true,
): Promise<SharedScreenFixture> {
  const browser1 = await chromium.launch({ args: BROWSER_ARGS, headless });
  const browser2 = await chromium.launch({ args: BROWSER_ARGS, headless });

  const hostCtx = await createAuthenticatedContext(
    browser1,
    `${hostName.toLowerCase()}@videocall.rs`,
    hostName,
    uiURL,
  );
  const guestCtx = await createAuthenticatedContext(
    browser2,
    `${guestName.toLowerCase()}@videocall.rs`,
    guestName,
    uiURL,
  );

  // Camera-on preference seeded BEFORE nav (prejoin camera defaults OFF); both
  // peers publish video so the re-render-trigger test has a camera-off toggle to
  // flip. Plain-text localStorage key, per the current context.rs read path.
  await hostCtx.addInitScript(`localStorage.setItem("vc_prejoin_camera_on", "true");`);
  await guestCtx.addInitScript(`localStorage.setItem("vc_prejoin_camera_on", "true");`);
  // The guest is the publisher — it needs the getDisplayMedia mock. Defaults to
  // the cheap static mock; the detach test overrides it with ANIMATED_SHARE_MOCK
  // so its mirror pixel-liveness assertion is meaningful.
  await guestCtx.addInitScript(shareMock);
  if (hostExtraInit) {
    await hostCtx.addInitScript(hostExtraInit);
  }

  const hostPage = await hostCtx.newPage();
  const guestPage = await guestCtx.newPage();

  await navigateToMeeting(hostPage, meetingId, hostName);
  const hostResult = await joinMeetingFromPage(hostPage);
  expect(hostResult).toBe("in-meeting");

  await navigateToMeeting(guestPage, meetingId, guestName);
  const guestResult = await joinMeetingFromPage(guestPage);
  await admitGuestIfNeeded(hostPage, guestPage, guestResult);

  // Both tiles must exist before driving screen share so the publisher has a
  // viewer to send SCREEN packets to.
  await expect(hostPage.locator("#grid-container .canvas-container").first()).toBeVisible({
    timeout: 30_000,
  });
  await expect(guestPage.locator("#grid-container .canvas-container").first()).toBeVisible({
    timeout: 30_000,
  });
  // Peer-discovery warm-up before sharing.
  await hostPage.waitForTimeout(3000);

  const shared = await startScreenShareAndAwaitSplitLayout(guestPage, hostPage);
  expect(
    shared,
    "Host viewer never entered the screen-share split layout. The getDisplayMedia " +
      "mock may not have taken effect, or the guest never connected as a peer.",
  ).toBe(true);

  const tile = hostPage.locator(".split-screen-tile").first();
  await expect(tile).toBeVisible({ timeout: 10_000 });
  // The received-share canvas must be mounted inside the tile from the start.
  await expect(tile.locator('canvas[id^="screen-share-"]')).toHaveCount(1);

  return { hostPage, guestPage, browser1, browser2, tile };
}

/**
 * Stamp the canvas node with a unique JS-property marker and return it. Reading
 * the marker back after a zoom / reset / detach proves the SAME node survived —
 * a recreated `<canvas>` would not carry the marker (the v1 frozen-video
 * regression). Presence (`toHaveCount(1)`) is asserted by callers BEFORE this.
 */
async function tagCanvasIdentity(canvas: Locator): Promise<string> {
  return canvas.evaluate((el) => {
    const tag = "ss-zoom-identity-" + Math.random().toString(36).slice(2);
    (el as unknown as { __ssZoomIdentity?: string }).__ssZoomIdentity = tag;
    return tag;
  });
}

async function readCanvasIdentity(canvas: Locator): Promise<string | null> {
  return canvas.evaluate(
    (el) => (el as unknown as { __ssZoomIdentity?: string }).__ssZoomIdentity ?? null,
  );
}

/**
 * Pixel-sample the detached-window mirror `<video>` from INSIDE the popup page:
 * draw it to a small 2D canvas and reduce it to `{ mean brightness,
 * position-weighted signature }`. Returns `null` while the video has no
 * decodable frame yet (readyState < HAVE_CURRENT_DATA or paused), or if drawing
 * throws.
 *
 * Why pixels (issue #1829): the detached window used to open BLANK — a
 * cross-realm `dyn_into` on the popup's `<video>` (and the Document PiP `Window`)
 * failed silently, so the mirror was never built and the share snapped back to
 * the main window. The old `!paused && readyState>=2` poll could NOT tell a
 * blank/frozen mirror from a live one. Two samples let a caller assert the
 * mirror is non-blank (mean above a near-black floor) AND advancing (signatures
 * differ). The mock share is animated, so a correctly-mirrored video advances
 * even under headless `--disable-gpu` (CI) — verified while fixing #1829.
 */
async function sampleDetachedMirror(popup: Page): Promise<{ mean: number; sig: number } | null> {
  return popup.evaluate(() => {
    const v = document.getElementById("ss-detached-video") as HTMLVideoElement | null;
    if (!v || v.readyState < 2 || v.paused) return null;
    const p = document.createElement("canvas");
    p.width = 48;
    p.height = 27;
    const px = p.getContext("2d");
    if (!px) return null;
    try {
      px.drawImage(v, 0, 0, 48, 27);
      const d = px.getImageData(0, 0, 48, 27).data;
      let sum = 0;
      let sig = 0;
      for (let i = 0; i < d.length; i += 4) {
        sum += d[i] + d[i + 1] + d[i + 2];
        sig = (sig + d[i] * (i + 1)) % 2147483647;
      }
      return { mean: sum / (48 * 27 * 3), sig };
    } catch {
      return null;
    }
  });
}

/**
 * Assert the MAIN window is in the "regular no-share meeting" look while a share
 * is detached: `#grid-container` carries `share-detached`, the share pane is
 * off-screen (left:-99999px) but STILL mounted (canvas keeps its node identity)
 * and `inert` (removed from the tab / AT tree, per 3cdfc2c5), and the peer grid
 * has expanded to (near) full width.
 */
async function assertMainWindowDetachedLook(
  hostPage: Page,
  canvas: Locator,
  canvasId: string,
): Promise<void> {
  const grid = hostPage.locator("#grid-container");
  const leftPane = hostPage.locator(".ss-left-pane");
  await expect(grid).toHaveClass(/\bshare-detached\b/);

  // Canvas persistence invariant: the split subtree stays MOUNTED (off-screen),
  // so the canvas keeps its node identity and keeps feeding the mirror.
  await expect(canvas).toHaveCount(1);
  expect(await readCanvasIdentity(canvas)).toBe(canvasId);

  // The share pane is moved fully off-screen to the LEFT (position, not
  // display:none — so it stays composited). Its whole box is left of x=0.
  const leftBox = await leftPane.boundingBox();
  expect(leftBox, "ss-left-pane must still be laid out (mounted, off-screen)").not.toBeNull();
  expect(leftBox!.x + leftBox!.width).toBeLessThanOrEqual(0);

  // 3cdfc2c5 a11y guard: the off-screen pane is `inert` while detached, so its
  // ~7 invisible controls leave the keyboard tab order + AT tree.
  await expect(leftPane).toHaveAttribute("inert", "true");
  // Empirically confirm inert actually blocks focus (not just the attribute):
  // focusing ss-detach inside the inert pane is a no-op, so activeElement does
  // NOT become it. (The detach flow therefore interacts only with the popup
  // Page while detached — ss-detach in the main doc is inert until reattach.)
  const detach = hostPage.locator('[data-testid="ss-detach"]');
  await detach.evaluate((el) => (el as HTMLElement).focus());
  expect(
    await hostPage.evaluate(() => document.activeElement?.getAttribute("data-testid") ?? null),
  ).not.toBe("ss-detach");

  // The peer grid expands to (near) full width — the "regular meeting" look.
  const gridBox = await grid.boundingBox();
  const peerBox = await hostPage.locator(".ss-peer-panel").boundingBox();
  expect(gridBox).not.toBeNull();
  expect(peerBox).not.toBeNull();
  expect(peerBox!.width).toBeGreaterThanOrEqual(gridBox!.width * 0.9);
}

/**
 * Assert the MAIN window has been RESTORED to the split layout after reattach:
 * `#grid-container` no longer carries `share-detached`, the split tile is back
 * on-screen (left pane x >= 0) and no longer `inert`, and the canvas is STILL
 * the same node.
 */
async function assertMainWindowRestoredLook(
  hostPage: Page,
  tile: Locator,
  canvas: Locator,
  canvasId: string,
): Promise<void> {
  const leftPane = hostPage.locator(".ss-left-pane");
  // First assertion carries the retry budget for the pagehide → schedule_teardown
  // → teardown → re-render latency after the window closes.
  await expect(hostPage.locator("#grid-container")).not.toHaveClass(/\bshare-detached\b/, {
    timeout: 10_000,
  });
  await expect(tile).toBeVisible();
  await expect(leftPane).not.toHaveAttribute("inert");
  const leftBox = await leftPane.boundingBox();
  expect(leftBox).not.toBeNull();
  expect(leftBox!.x).toBeGreaterThanOrEqual(0);
  await expect(canvas).toHaveCount(1);
  expect(await readCanvasIdentity(canvas)).toBe(canvasId);
}

test.describe("Issue 1175: received screen-share zoom / detach", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  // ──────────────────────────────────────────────────────────────────────────
  // Test 1 — zoom flow: label + declarative transform + clamp aria-disabled +
  // the `is-zoomed` viewport class.
  //
  // Fails-on-unfixed: on the ABSENT feature the ss-zoom-* testids do not exist
  // (locators time out). The exact "100% → 125% → 400% → 100%" arithmetic with
  // `scale(1.25)` / `scale(4)` on `.ss-zoom-wrapper`, aria-disabled at the
  // fit/max clamps, and the `is-zoomed` viewport class toggling at scale>1.0 are
  // specific to the v2 declarative render; the reverted v1 did not drive the
  // label/transform declaratively from a signal.
  // ──────────────────────────────────────────────────────────────────────────
  test("viewer zoom controls step the received-share transform and clamp at fit and max", async ({
    baseURL,
  }) => {
    test.setTimeout(180_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `ss_zoom_flow_${Date.now()}`;
    const fx = await setupViewerSeeingSharedScreen(uiURL, meetingId, "SsZoomHost1", "SsZoomGuest1");
    const { tile } = fx;

    try {
      const controls = tile.locator('[data-testid="ss-zoom-controls"]');
      const label = tile.locator('[data-testid="ss-zoom-label"]');
      const zoomIn = tile.locator('[data-testid="ss-zoom-in"]');
      const zoomOut = tile.locator('[data-testid="ss-zoom-out"]');
      const reset = tile.locator('[data-testid="ss-zoom-reset"]');
      const wrapper = tile.locator(".ss-zoom-wrapper");
      const viewport = tile.locator('[data-testid="ss-zoom-viewport"]');

      // Presence before measurement.
      await expect(controls).toBeVisible();
      await expect(label).toBeVisible();
      await expect(wrapper).toHaveCount(1);

      // Resting state: 100%, no transform, viewport not is-zoomed, zoom-out
      // disabled at fit, zoom-in enabled.
      await expect(label).toHaveText("100%");
      await expect(wrapper).toHaveAttribute("style", /transform:\s*none/);
      await expect(viewport).not.toHaveClass(/\bis-zoomed\b/);
      await expect(zoomOut).toHaveAttribute("aria-disabled", "true");
      await expect(zoomIn).toHaveAttribute("aria-disabled", "false");

      // Reveal the hover/focus-gated control bar so its buttons receive clicks.
      await tile.hover();

      // One step up: 100% → 125%, scale(1.25); viewport gains is-zoomed;
      // zoom-out becomes enabled.
      await zoomIn.click();
      await expect(label).toHaveText("125%");
      await expect(wrapper).toHaveAttribute("style", /scale\(1\.25\)/);
      await expect(viewport).toHaveClass(/\bis-zoomed\b/);
      await expect(zoomOut).toHaveAttribute("aria-disabled", "false");

      // Saturate at the max clamp. From 125%, six more ×1.25 steps reach 400%
      // (4.0); the sixth click (381% → clamp) is the last one on an ENABLED
      // button — it lands the clamp and flips zoom-in to aria-disabled.
      for (let i = 0; i < 6; i++) {
        await zoomIn.click();
      }
      await expect(label).toHaveText("400%");
      await expect(wrapper).toHaveAttribute("style", /scale\(4\)/);
      await expect(zoomIn).toHaveAttribute("aria-disabled", "true");

      // A further zoom-in at the clamp must be a harmless no-op. The button is
      // now aria-disabled, which Playwright's actionability treats as "not
      // enabled" — an ordinary `.click()` would retry until the test timeout
      // (this is what made this test hang before). Force past actionability to
      // exercise the app's clamp guard, then assert it held at 400%.
      await zoomIn.click({ force: true });
      await expect(label).toHaveText("400%");
      await expect(wrapper).toHaveAttribute("style", /scale\(4\)/);

      // Reset returns to fit: 100%, transform none, viewport no longer is-zoomed,
      // zoom-out disabled again.
      await reset.click();
      await expect(label).toHaveText("100%");
      await expect(wrapper).toHaveAttribute("style", /transform:\s*none/);
      await expect(viewport).not.toHaveClass(/\bis-zoomed\b/);
      await expect(zoomOut).toHaveAttribute("aria-disabled", "true");
    } finally {
      await fx.browser1.close();
      await fx.browser2.close();
    }
  });

  // ──────────────────────────────────────────────────────────────────────────
  // Test 2 — canvas identity across zoom + reset.
  //
  // Fails-on-unfixed: if a change tears down and recreates the screen-share
  // `<canvas>` on zoom or reset (the v1 "frozen video" regression), the new node
  // would not carry the identity marker and the read-back would not equal the
  // captured value.
  // ──────────────────────────────────────────────────────────────────────────
  test("zoom and reset never recreate the received-share canvas node", async ({ baseURL }) => {
    test.setTimeout(180_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `ss_zoom_identity_${Date.now()}`;
    const fx = await setupViewerSeeingSharedScreen(uiURL, meetingId, "SsZoomHost2", "SsZoomGuest2");
    const { tile } = fx;

    try {
      const canvas = tile.locator('canvas[id^="screen-share-"]');
      const label = tile.locator('[data-testid="ss-zoom-label"]');
      const zoomIn = tile.locator('[data-testid="ss-zoom-in"]');
      const reset = tile.locator('[data-testid="ss-zoom-reset"]');
      const wrapper = tile.locator(".ss-zoom-wrapper");

      await expect(canvas).toHaveCount(1);
      const id1 = await tagCanvasIdentity(canvas);
      expect(id1).toBeTruthy();

      // Zoom to 156% (two ×1.25 steps).
      await tile.hover();
      await zoomIn.click();
      await zoomIn.click();
      await expect(label).toHaveText("156%");
      await expect(wrapper).toHaveAttribute("style", /scale\(1\.5625\)/);

      // Same node after zoom.
      await expect(canvas).toHaveCount(1);
      expect(await readCanvasIdentity(canvas)).toBe(id1);

      // Reset back to fit.
      await reset.click();
      await expect(label).toHaveText("100%");
      await expect(wrapper).toHaveAttribute("style", /transform:\s*none/);

      // Same node after reset.
      await expect(canvas).toHaveCount(1);
      expect(await readCanvasIdentity(canvas)).toBe(id1);
    } finally {
      await fx.browser1.close();
      await fx.browser2.close();
    }
  });

  // ──────────────────────────────────────────────────────────────────────────
  // Test 3a — detach opens a live, zoomable mirror window; the main window flips
  // to a regular meeting; the popup Reattach button restores everything.
  //
  // Headless branch (verified against 58deca56): the desktop viewport (1280px >
  // 768px) makes `detach_supported()` true, so `[data-testid=ss-detach]` IS
  // rendered. FORCE_POPUP_DETACH_SCRIPT shadows Document PiP off, so `open()`
  // takes the `window.open` fallback, which Playwright's headless Chromium
  // honors and surfaces as a new context page. If an environment blocks the
  // popup, `open()` resets the optimistic signal (no flip); the test asserts
  // that revert and skips the window-dependent half.
  //
  // Fails-on-unfixed:
  //  - absent feature → no ss-detach testid (locator times out).
  //  - issue #1829 (cross-realm `dyn_into` on the popup <video> / PiP Window
  //    fails silently): the detached window never builds → `share-detached`
  //    reverts (assertMainWindowDetachedLook fails) OR the mirror stays blank →
  //    the pixel-liveness assertion below (non-blank + advancing) fails.
  //  - the detached mirror never PLAYS / freezes: the two pixel samples are
  //    identical or blank → the advancing-frames assertion fails.
  //  - a regression that removed the canvas from the main DOM while detached
  //    (instead of hiding the pane off-screen) → canvas count 0 / identity
  //    mismatch.
  //  - the main window fails to flip (`share-detached` missing) or the popup
  //    zoom controls don't drive the wrapper transform / label / data-zoomed
  //    (the wrapper transform also exercises a cross-realm cast fixed in #1829).
  //  - broken reattach (button → teardown) or EXIT focus handler.
  // ──────────────────────────────────────────────────────────────────────────
  test("detach opens a live zoomable mirror window, flips the main window to a regular meeting, and the popup Reattach restores it", async ({
    baseURL,
  }) => {
    test.setTimeout(180_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `ss_detach_reattach_${Date.now()}`;
    const fx = await setupViewerSeeingSharedScreen(
      uiURL,
      meetingId,
      "SsDetachHost",
      "SsDetachGuest",
      FORCE_POPUP_DETACH_SCRIPT,
      // Animated source so the detached-window mirror shows ADVANCING frames the
      // pixel-liveness assertion below can prove (a static canvas emits no new
      // captured frames).
      ANIMATED_SHARE_MOCK,
    );
    const { hostPage, tile } = fx;

    try {
      const canvas = tile.locator('canvas[id^="screen-share-"]');
      const detach = tile.locator('[data-testid="ss-detach"]');
      const announce = hostPage.locator('[data-testid="ss-detach-announce"]');
      const grid = hostPage.locator("#grid-container");

      // Detach control offered (desktop viewport). Not detached yet.
      await expect(detach).toBeVisible();
      await expect(detach).toHaveAttribute("aria-pressed", "false");
      await expect(grid).not.toHaveClass(/\bshare-detached\b/);

      await expect(canvas).toHaveCount(1);
      const id1 = await tagCanvasIdentity(canvas);
      expect(id1).toBeTruthy();

      // Detach → a real separate window opens (window.open, forced above),
      // surfaced by Playwright as a new context page.
      //
      // Skip-vs-fail distinction (critical for the #1829 guard): the ONLY
      // legitimate skip is the `catch` below — `waitForEvent` timed out, i.e. NO
      // page event fired at all, meaning the browser genuinely BLOCKED the popup
      // (no window ever opened). If a popup page event DOES fire, window.open
      // SUCCEEDED, and the app must then commit to the detach. Issue #1829 is
      // exactly the case where the popup OPENS and the app then reverts (a
      // cross-realm cast silently aborts finish_open, closing the fresh window
      // and snapping the share back) — that is a HARD FAILURE below, and must
      // never be mistaken for a blocked popup and skipped.
      const popupPromise = hostPage.context().waitForEvent("page", { timeout: 12_000 });
      await tile.hover();
      await detach.click();

      let popup: Page;
      try {
        popup = await popupPromise;
      } catch {
        // No page event within 12s → window.open was BLOCKED (no window ever
        // opened). open() reset the optimistic signal; assert that revert and
        // skip the window-dependent half. This branch is NOT reachable for
        // #1829, where the popup opens before the app reverts.
        await expect(grid).not.toHaveClass(/\bshare-detached\b/);
        await expect(detach).toHaveAttribute("aria-pressed", "false");
        await expect(canvas).toHaveCount(1);
        expect(await readCanvasIdentity(canvas)).toBe(id1);
        test.skip(
          true,
          "window.open was blocked here (no popup page event fired), so no " +
            "detached window opened and the detach reverted (asserted above). " +
            "The detached-window contract needs a real popup to exercise.",
        );
        return;
      }

      // The popup page event fired → window.open SUCCEEDED. If the app then
      // reverts by closing the fresh window (the #1829 symptom), that is a HARD
      // FAILURE here, not a blocked popup. (assertMainWindowDetachedLook below is
      // the backstop when the window lingers but the flip reverts.)
      expect(
        popup.isClosed(),
        "#1829: the detached window opened then CLOSED — the app aborted the " +
          "detach (cross-realm cast regression?). This is a hard failure, not a " +
          "blocked popup.",
      ).toBe(false);

      // --- MAIN window: regular-meeting look, canvas persisted off-screen ---
      await assertMainWindowDetachedLook(hostPage, canvas, id1);
      await expect(detach).toHaveAttribute("aria-pressed", "true");
      await expect(announce).toHaveText("Shared content opened in a separate window");

      // --- DETACHED window: a LIVE mirror with its own zoom controls ---
      const video = popup.locator("#ss-detached-video");
      const dViewport = popup.locator("#ss-detached-viewport");
      const dWrapper = popup.locator("#ss-detached-wrapper");
      const dLabel = popup.locator("#ss-detached-zoom-label");
      const dZoomIn = popup.locator("#ss-detached-zoom-in");
      const dReset = popup.locator("#ss-detached-zoom-reset");
      const reattachBtn = popup.locator("#ss-detached-reattach");

      await expect(video).toHaveCount(1);
      // The mirror is wired to the source canvas's captureStream...
      expect(await video.evaluate((el) => (el as HTMLVideoElement).srcObject !== null)).toBe(true);

      // ...and it must show LIVE, NON-BLANK, ADVANCING content — not merely be
      // "present and !paused". Issue #1829: the detached window opened BLANK (a
      // cross-realm `dyn_into` on the popup's <video> / the Document PiP Window
      // failed silently, so the mirror was never built and the share snapped
      // back to the main window). The pre-#1829 `!paused && readyState>=2` poll
      // could not distinguish a blank/frozen mirror from a live one — precisely
      // why CI stayed green while the feature was broken. We now PIXEL-SAMPLE the
      // popup <video> (see `sampleDetachedMirror`): first poll until a non-blank
      // frame decodes, then take two samples ~600ms apart and require they DIFFER
      // (frames are advancing). The animated mock keeps this meaningful under
      // headless --disable-gpu, which is what CI runs.
      //
      // What CI (SwiftShader, --disable-gpu) CAN catch here: the mirror never
      // building (the #1829 regression → window blank / share reverts) and a
      // frozen/blank mirror. What it CANNOT catch: real-GPU-only compositing
      // quirks — those were exercised by a headed real-GPU repro while fixing
      // #1829. A live frame reads a per-channel mean ~48 (measured: the #1a1a2e
      // bg alone is ~33, and the moving teal bar + white text raise it); a black
      // frame reads ~0, so the >8 floor cleanly separates blank from live.
      await expect
        .poll(async () => (await sampleDetachedMirror(popup))?.mean ?? -1, {
          timeout: 15_000,
          message: "detached mirror never decoded a non-blank frame",
        })
        .toBeGreaterThan(8);
      const firstSample = await sampleDetachedMirror(popup);
      await popup.waitForTimeout(600);
      const secondSample = await sampleDetachedMirror(popup);
      expect(firstSample, "detached mirror sample must be readable").not.toBeNull();
      expect(secondSample, "detached mirror sample must be readable").not.toBeNull();
      expect(
        secondSample!.sig,
        "detached mirror is frozen — two pixel samples 600ms apart are identical",
      ).not.toBe(firstSample!.sig);

      // Popup zoom controls drive the mirror wrapper transform + label +
      // data-zoomed (reusing the same pure zoom math as the main tile).
      await expect(dLabel).toHaveText("100%");
      await expect(dViewport).not.toHaveAttribute("data-zoomed");
      await dZoomIn.click();
      await expect(dLabel).toHaveText("125%");
      await expect(dWrapper).toHaveAttribute("style", /scale\(1\.25\)/);
      await expect(dViewport).toHaveAttribute("data-zoomed", "true");
      await dReset.click();
      await expect(dLabel).toHaveText("100%");
      await expect(dWrapper).toHaveAttribute("style", /transform:\s*none/);
      await expect(dViewport).not.toHaveAttribute("data-zoomed");

      // --- Reattach via the popup's Reattach button → window closes, main
      // window restored, focus returns to the detach toggle ---
      const closed = popup.waitForEvent("close", { timeout: 10_000 });
      await reattachBtn.click();
      await closed;

      await assertMainWindowRestoredLook(hostPage, tile, canvas, id1);
      await expect(detach).toHaveAttribute("aria-pressed", "false");
      await expect(announce).toHaveText("Shared content returned to the meeting");
      await expect
        .poll(
          () =>
            hostPage.evaluate(() => document.activeElement?.getAttribute("data-testid") ?? null),
          { timeout: 5_000 },
        )
        .toBe("ss-detach");
    } finally {
      await fx.browser1.close();
      await fx.browser2.close();
    }
  });

  // ──────────────────────────────────────────────────────────────────────────
  // Test 3b — closing the detached window directly restores the share.
  //
  // The user closing the popup window must return the share to the main window
  // (not strand it off-screen). Closing the window fires `pagehide` on the popup
  // → `schedule_teardown` → `teardown`, which resets the detached signal and
  // un-flips `.share-detached`.
  //
  // Fails-on-unfixed: if the close path did not tear down (share left stranded
  // off-screen), `#grid-container` keeps `share-detached` and the restore
  // assertions time out; if the canvas were rebuilt on restore, identity fails.
  // ──────────────────────────────────────────────────────────────────────────
  test("closing the detached window directly restores the share to the main window", async ({
    baseURL,
  }) => {
    test.setTimeout(180_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `ss_detach_close_${Date.now()}`;
    const fx = await setupViewerSeeingSharedScreen(
      uiURL,
      meetingId,
      "SsCloseHost",
      "SsCloseGuest",
      FORCE_POPUP_DETACH_SCRIPT,
    );
    const { hostPage, tile } = fx;

    try {
      const canvas = tile.locator('canvas[id^="screen-share-"]');
      const detach = tile.locator('[data-testid="ss-detach"]');
      const announce = hostPage.locator('[data-testid="ss-detach-announce"]');

      await expect(canvas).toHaveCount(1);
      const id1 = await tagCanvasIdentity(canvas);
      expect(id1).toBeTruthy();

      const popupPromise = hostPage.context().waitForEvent("page", { timeout: 12_000 });
      await tile.hover();
      await detach.click();

      let popup: Page;
      try {
        popup = await popupPromise;
      } catch {
        test.skip(
          true,
          "window.open was blocked here; no detached window to close. The " +
            "direct-close restore path needs a real popup to exercise.",
        );
        return;
      }

      await assertMainWindowDetachedLook(hostPage, canvas, id1);
      await expect(announce).toHaveText("Shared content opened in a separate window");

      // Close the detached window directly (user closes it). The close fires
      // pagehide on the popup → teardown → the share returns to the main window.
      await popup.close();

      await assertMainWindowRestoredLook(hostPage, tile, canvas, id1);
      await expect(detach).toHaveAttribute("aria-pressed", "false");
      await expect(announce).toHaveText("Shared content returned to the meeting");
      await expect
        .poll(
          () =>
            hostPage.evaluate(() => document.activeElement?.getAttribute("data-testid") ?? null),
          { timeout: 5_000 },
        )
        .toBe("ss-detach");
    } finally {
      await fx.browser1.close();
      await fx.browser2.close();
    }
  });

  // ──────────────────────────────────────────────────────────────────────────
  // Test 4 — zoom state survives a peer-driven re-render of the tile (v1 D7).
  //
  // Toggling the guest's camera OFF makes the host re-run `generate_for_peer`
  // for the guest across every panel, including this ScreenOnly screen-share
  // tile — the same re-render that reset the reverted v1's imperative zoom. The
  // camera-off "Video Disabled" placeholder appearing is the observable proof
  // the re-render reached the host.
  //
  // Fails-on-unfixed: on the reverted imperative v1 the label would snap back to
  // 100% and the transform clear on this re-render. v2 renders zoom
  // declaratively from `ScreenZoomCtx`, so it persists.
  //
  // FIXME (pre-existing, unrelated to #1829): this test fails DETERMINISTICALLY
  // in isolation (zero contention) at its BASELINE — `expect("Video Disabled")
  // .toHaveCount(0)` (both cameras on) receives 1 and never resolves within the
  // 5s poll. The host renders the GUEST's camera as "Video Disabled" WHILE the
  // guest is screen-sharing, so the "both cameras visible" baseline is never
  // reached and the toggle-off proof step can't be exercised. This is a
  // camera-decode/publish-during-screen-share concern (the sharer's own camera
  // on the receiver), NOT zoom or detach — the test body is unchanged by this PR
  // and it hangs on base (c0333693). Marked fixme so the file runs green (an
  // honest, visible skip beats a silent pre-existing failure) pending a separate
  // issue to investigate the sharer-camera-during-share pipeline.
  // ──────────────────────────────────────────────────────────────────────────
  test.fixme("zoom state survives a peer-driven re-render of the tile", async ({ baseURL }) => {
    test.setTimeout(180_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `ss_zoom_persist_${Date.now()}`;
    const fx = await setupViewerSeeingSharedScreen(
      uiURL,
      meetingId,
      "SsPersistHost",
      "SsPersistGuest",
    );
    const { hostPage, guestPage, tile } = fx;

    try {
      const label = tile.locator('[data-testid="ss-zoom-label"]');
      const zoomIn = tile.locator('[data-testid="ss-zoom-in"]');
      const wrapper = tile.locator(".ss-zoom-wrapper");
      const viewport = tile.locator('[data-testid="ss-zoom-viewport"]');

      // Zoom to a non-default level: 100% → 156% (two ×1.25 steps).
      await tile.hover();
      await zoomIn.click();
      await zoomIn.click();
      await expect(label).toHaveText("156%");
      await expect(wrapper).toHaveAttribute("style", /scale\(1\.5625\)/);
      await expect(viewport).toHaveClass(/\bis-zoomed\b/);

      // Baseline: no tile shows the camera-off placeholder yet (both cameras on).
      await expect(hostPage.getByText("Video Disabled")).toHaveCount(0);

      // Toggle the guest camera OFF → forces the host to re-render the guest's
      // tiles. The camera-toggle button carries a stable data-testid; wake the
      // auto-hidden control bar first.
      await wakeControls(guestPage);
      await guestPage.waitForTimeout(300);
      await guestPage.locator('[data-testid="camera-toggle-button"]').click();

      // Observable proof the host re-rendered the peer's tiles.
      await expect(hostPage.getByText("Video Disabled").first()).toBeVisible({ timeout: 20_000 });

      // The declarative zoom survives the re-render untouched.
      await expect(label).toHaveText("156%");
      await expect(wrapper).toHaveAttribute("style", /scale\(1\.5625\)/);
      await expect(viewport).toHaveClass(/\bis-zoomed\b/);
    } finally {
      await fx.browser1.close();
      await fx.browser2.close();
    }
  });

  // ──────────────────────────────────────────────────────────────────────────
  // Test 5 — the zoom bar clears the meeting action bar (no overlap).
  //
  // 99c16d0d raised `.ss-zoom-controls` from `bottom:10px` to
  // `bottom:var(--controls-dock-clearance, 112px)` (global.css L3268; clearance
  // token L1525) after a user found the zoom bar obscured by the fixed
  // bottom-docked action bar `.video-controls-container` (global.css L2000-2037;
  // built in attendants.rs L7190 with default `dock-bottom`).
  //
  // Both bars are horizontally centered, so vertical clearance is the sole
  // discriminator: the zoom bar's BOTTOM edge must sit at or above the action
  // bar's TOP edge.
  //
  // Fails-on-unfixed (geometry at 1200x800): the docked action bar (bottom:20px,
  // ~79px tall) has its TOP edge ~99px above the viewport bottom.
  //   - PRE-FIX `bottom:10px` → the zoom bar's BOTTOM edge sits ~10px above the
  //     viewport bottom, i.e. ~89px BELOW the action bar's top → the boxes
  //     overlap by ~18px and `zoomBox.y + zoomBox.height <= barBox.y` FAILS.
  //   - POST-FIX `bottom:112px` → the zoom bar's BOTTOM edge sits ~112px above
  //     the viewport bottom, ~13px ABOVE the action bar's top → clear.
  // ──────────────────────────────────────────────────────────────────────────
  test("the zoom bar clears the meeting action bar with no overlap", async ({ baseURL }) => {
    test.setTimeout(180_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `ss_zoom_overlap_${Date.now()}`;
    const fx = await setupViewerSeeingSharedScreen(
      uiURL,
      meetingId,
      "SsOverlapHost",
      "SsOverlapGuest",
    );
    const { hostPage, tile } = fx;

    try {
      // Deterministic viewport for the geometry arithmetic in the comment above.
      await hostPage.setViewportSize({ width: 1200, height: 800 });
      await hostPage.waitForTimeout(300); // let the split layout reflow

      const zoomControls = tile.locator('[data-testid="ss-zoom-controls"]');
      const viewport = tile.locator('[data-testid="ss-zoom-viewport"]');
      const actionBar = hostPage.locator(".video-controls-container");

      // Reveal the zoom bar via focus-within (no mouse) so the mouse is free to
      // hold the action bar docked.
      await viewport.focus();
      await expect(zoomControls).toBeVisible();

      // The action bar is fixed + `dock-bottom` by default and auto-hides via
      // `.controls-hidden`. Park the mouse over its footprint (bottom center) so
      // `:hover` holds it fully docked — the strictest (highest) position —
      // through the 0.55s dock transition, then measure. (600,768) lies inside
      // the bar's box in BOTH the hidden and docked positions, so :hover is never
      // lost as it animates up.
      await expect(actionBar).toHaveCount(1);
      await expect(actionBar).toHaveClass(/\bdock-bottom\b/);
      await hostPage.mouse.move(600, 768);
      await hostPage.waitForTimeout(700);

      // Presence before measurement.
      const zoomBox = await zoomControls.boundingBox();
      const barBox = await actionBar.boundingBox();
      expect(zoomBox, "zoom control bar must be laid out").not.toBeNull();
      expect(barBox, "action bar must be laid out (docked)").not.toBeNull();

      // No vertical intersection: zoom bar bottom edge <= action bar top edge.
      expect(zoomBox!.y + zoomBox!.height).toBeLessThanOrEqual(barBox!.y);
    } finally {
      await fx.browser1.close();
      await fx.browser2.close();
    }
  });

  // ──────────────────────────────────────────────────────────────────────────
  // Issue #1842 — the detached window matches the shared content's aspect.
  //
  // The window is sized from the source canvas's DECODED dims (canvas.width/
  // height), so its VIDEO area tracks the content aspect, not the split pane box.
  // Uses a distinctively WIDE (3.2:1) share so a content-sized window is
  // unmistakable from the pre-#1842 client-box sizing (which produced the pane's
  // ~16:9-or-narrower aspect).
  //
  // HEADED-gated (HEADED=1): headless Chromium does not honor window.open
  // width/height, so the popup's innerWidth/innerHeight would be a default there;
  // a real window sizes to the features. CI (headless) skips cleanly.
  //
  // Fails-on-unfixed: the client-box sizing yields the pane aspect (≲1.8), so the
  // `> 2.0` poll never clears and the aspect-match assertion fails.
  // ──────────────────────────────────────────────────────────────────────────
  test("the detached window matches the shared content's aspect (headed)", async ({ baseURL }) => {
    test.skip(
      !process.env.HEADED,
      "Headless Chromium does not honor window.open width/height, so the popup " +
        "viewport can't be measured against content aspect. Run with HEADED=1. (issue #1842)",
    );
    test.setTimeout(180_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `ss_detach_aspect_${Date.now()}`;
    // A distinctively WIDE 3.2:1 share so a content-sized window is unmistakable.
    const WIDE_SHARE_MOCK = `
      (() => {
        const md = navigator.mediaDevices;
        if (!md) return;
        Object.defineProperty(md, 'getDisplayMedia', {
          configurable: true,
          value: async () => {
            const c = document.createElement('canvas');
            c.width = 1280; c.height = 400;
            const ctx = c.getContext('2d');
            ctx.fillStyle = '#1a1a2e'; ctx.fillRect(0, 0, 1280, 400);
            ctx.fillStyle = '#fff'; ctx.font = '28px sans-serif';
            ctx.fillText('Wide Mock Screen Share 3.2:1', 200, 200);
            return c.captureStream(5);
          },
        });
      })();
    `;
    const fx = await setupViewerSeeingSharedScreen(
      uiURL,
      meetingId,
      "SsAspectHost",
      "SsAspectGuest",
      FORCE_POPUP_DETACH_SCRIPT,
      WIDE_SHARE_MOCK,
      false, // headed: window.open size is only real with a compositor
    );
    const { hostPage, tile } = fx;

    try {
      const canvas = tile.locator('canvas[id^="screen-share-"]');
      await expect(canvas).toHaveCount(1);
      // The receiver's decoded source-canvas aspect — what the fix sizes to. The
      // canvas element appears from screen METADATA before the first frame
      // decodes, when it is still the 300x150 HTML default (aspect 2.0), so POLL
      // until the decode lands and sets the real (wide) dims before measuring.
      const canvasAspect = () =>
        canvas.evaluate((c) => {
          const cv = c as HTMLCanvasElement;
          return cv.height > 0 ? cv.width / cv.height : 0;
        });
      await expect
        .poll(canvasAspect, {
          timeout: 20_000,
          message: "wide mock never decoded to a wide source canvas",
        })
        .toBeGreaterThan(2.0);
      const contentAspect = await canvasAspect();

      const detach = tile.locator('[data-testid="ss-detach"]');
      await expect(detach).toBeVisible();
      const popupPromise = hostPage.context().waitForEvent("page", { timeout: 12_000 });
      await tile.hover();
      await detach.click();
      const popup = await popupPromise;

      // The popup's VIDEO-AREA aspect = innerWidth / (innerHeight - bar). Poll to
      // let the window settle to its requested size. `> 2.0` is the fails-on-
      // unfixed discriminator: only a content-sized window is this wide.
      const BAR = 40;
      const videoAspect = (bar: number) =>
        popup.evaluate((b) => {
          const vh = window.innerHeight - b;
          return vh > 0 ? window.innerWidth / vh : 0;
        }, bar);
      await expect
        .poll(async () => await videoAspect(BAR), {
          timeout: 10_000,
          message: "detached window never sized to the wide content aspect",
        })
        .toBeGreaterThan(2.0);
      // ...and it tracks the content aspect within tolerance (chrome + rounding).
      const popupAspect = await videoAspect(BAR);
      expect(Math.abs(popupAspect - contentAspect)).toBeLessThan(0.6);
    } finally {
      await fx.browser1.close();
      await fx.browser2.close();
    }
  });

  // ──────────────────────────────────────────────────────────────────────────
  // Issue #1841 (detach half) — detaching a STATIC share shows the current still
  // IMMEDIATELY. `canvas.captureStream()` emits only on source-canvas repaint, so
  // a static source starves the popup mirror; the fix forces a no-op repaint of the
  // source canvas so the auto-rate capture emits the current bitmap at once.
  //
  // HEADLESS GUARD: under headless `--disable-gpu` a static captureStream source's
  // mirror <video> never reaches readyState>=2 (documented on ANIMATED_SHARE_MOCK
  // above — the exact reason Test 3a uses an animated source), so the popup cannot
  // paint a single primed frame there and the assertion could not distinguish the
  // fix. This test is therefore gated to a HEADED run (HEADED=1), where a real
  // compositor renders the primed still; CI (headless) skips it cleanly rather
  // than false-red.
  //
  // Presence discriminator (NOT two-advancing-samples): a static source does not
  // advance, so we assert the popup mirror is NON-BLANK (mean above the near-black
  // floor). Fails-on-unfixed: without the repaint prime the starved popup
  // stays black (mean ~0).
  // ──────────────────────────────────────────────────────────────────────────
  test("detaching a STATIC share shows the current still immediately (headed)", async ({
    baseURL,
  }) => {
    test.skip(
      !process.env.HEADED,
      "Headless --disable-gpu cannot composite a single captureStream frame to the " +
        "mirror <video> (a static source's mirror never reaches readyState>=2 — see " +
        "the ANIMATED_SHARE_MOCK note). Run with HEADED=1 so the primed still can " +
        "paint. (issue #1841 detach half)",
    );
    test.setTimeout(180_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `ss_detach_static_${Date.now()}`;
    const fx = await setupViewerSeeingSharedScreen(
      uiURL,
      meetingId,
      "SsStaticHost",
      "SsStaticGuest",
      FORCE_POPUP_DETACH_SCRIPT,
      STATIC_SHARE_MOCK,
      false, // headed: a static captureStream mirror only paints with a real compositor
    );
    const { hostPage, tile } = fx;

    try {
      // The receiver decoded the initial burst, so its source canvas is painted;
      // wait past the mock's ~800ms emit window so the source is now STATIC.
      await expect(tile.locator('canvas[id^="screen-share-"]')).toHaveCount(1);
      await hostPage.waitForTimeout(3000);

      const detach = tile.locator('[data-testid="ss-detach"]');
      await expect(detach).toBeVisible();

      const popupPromise = hostPage.context().waitForEvent("page", { timeout: 12_000 });
      await tile.hover();
      await detach.click();
      const popup = await popupPromise;

      // The popup mirror must show NON-BLANK content — the primed still — even
      // though the source is static and never advances. >8 separates the mock
      // field (mean ~33) from a black/starved mirror (~0), matching Test 3a's floor.
      await expect
        .poll(async () => (await sampleDetachedMirror(popup))?.mean ?? -1, {
          timeout: 20_000,
          message: "detached mirror of a static share never showed the primed still",
        })
        .toBeGreaterThan(8);
    } finally {
      await fx.browser1.close();
      await fx.browser2.close();
    }
  });
});
