import { Page } from "@playwright/test";

/**
 * A controllable `navigator.mediaDevices.getUserMedia` mock for the
 * device-permission specs (media-device-permission.spec.ts).
 *
 * WHY THIS EXISTS
 * ───────────────
 * The camera/mic permission fix (DeviceInUse classification, the
 * stay-clickable retry button, and the background auto-recovery loop) can only
 * be exercised by forcing `getUserMedia` to REJECT with a specific
 * `DOMException` name — `NotReadableError` (→ DeviceInUse, the auto-recovering
 * case) and `NotAllowedError` (→ PermissionDenied, which does NOT auto-retry).
 * The Chromium `--use-fake-device-for-media-stream` flags only ever RESOLVE
 * getUserMedia, so a real failure has to be injected in-page. There was no
 * existing getUserMedia-rejection mock in `e2e/helpers/` or `e2e/tests/`, so
 * this builds one.
 *
 * HOW IT WORKS
 * ────────────
 * `installGetUserMediaMock` registers a `page.addInitScript` (which Playwright
 * runs BEFORE any page script, so the override is in place before the wasm app
 * boots and before the pre-join auto-request fires). The override:
 *   - Delegates to the REAL (fake-device) getUserMedia by default, so the
 *     pre-join preview and the join permission probe both succeed and the test
 *     can actually enter the meeting.
 *   - Rejects a call once the test opts a side (video/audio) into failure via
 *     `setGumFail`, using the currently-configured `DOMException` name. This
 *     lets a test run the happy pre-join/join path, THEN flip a device into the
 *     blocked state to drive the in-meeting failure + recovery flow.
 *
 * The camera encoder requests `{ video: …, audio: false }`, the microphone
 * encoder requests `{ audio: …, video: false }`, and the background retry loop
 * probes each side independently via `request_video_only` / `request_audio_only`
 * (video-only / audio-only constraints). The mock therefore keys failure off
 * whether a call REQUESTS video vs audio, so a video-only probe can recover
 * while an audio-only failure persists (and vice versa).
 *
 * `failRemaining` semantics per side:
 *   -  0  → never fail (pass through to the real fake device).
 *   -  N  → fail the next N video/audio-requesting calls, then pass through.
 *   - -1  → always fail (models a device held indefinitely by another app).
 */

interface GumSide {
  /** 0 = never fail, N = fail next N calls, -1 = always fail. */
  failRemaining: number;
  /** Count of getUserMedia calls that requested this side (for attribution). */
  calls: number;
}

interface GumState {
  errorName: string;
  video: GumSide;
  audio: GumSide;
}

type GumWindow = Window & {
  __gum?: GumState;
  __gumInstalled?: boolean;
};

export interface GumFailOpts {
  /** Per-side failure budget for video-requesting calls (see failRemaining). */
  video?: number;
  /** Per-side failure budget for audio-requesting calls (see failRemaining). */
  audio?: number;
  /** DOMException name to reject with, e.g. "NotReadableError". */
  errorName?: string;
}

/**
 * Register the getUserMedia override on the page. MUST be called before
 * navigation so the init script is in place before the app boots.
 */
export async function installGetUserMediaMock(page: Page): Promise<void> {
  await page.addInitScript(() => {
    const w = window as GumWindow;
    const md = navigator.mediaDevices;
    // Guard against double-install across re-navigations within one context.
    if (!md || w.__gumInstalled) {
      return;
    }
    w.__gumInstalled = true;

    const orig = md.getUserMedia.bind(md);
    const state: GumState = {
      errorName: "NotReadableError",
      video: { failRemaining: 0, calls: 0 },
      audio: { failRemaining: 0, calls: 0 },
    };
    w.__gum = state;

    const wantsVideo = (c: MediaStreamConstraints): boolean => Boolean(c.video);
    const wantsAudio = (c: MediaStreamConstraints): boolean => Boolean(c.audio);

    // Decrement the per-side failure budget and report whether THIS call fails.
    const consumeFail = (side: GumSide): boolean => {
      if (side.failRemaining === 0) {
        return false;
      }
      if (side.failRemaining > 0) {
        side.failRemaining -= 1;
      }
      return true;
    };

    md.getUserMedia = (constraints?: MediaStreamConstraints): Promise<MediaStream> => {
      const c = constraints ?? {};
      const v = wantsVideo(c);
      const a = wantsAudio(c);
      if (v) {
        state.video.calls += 1;
      }
      if (a) {
        state.audio.calls += 1;
      }
      const failV = v && consumeFail(state.video);
      const failA = a && consumeFail(state.audio);
      if (failV || failA) {
        return Promise.reject(new DOMException("mock " + state.errorName, state.errorName));
      }
      return orig(c);
    };
  });
}

/**
 * Reconfigure the mock at runtime (after the app has booted / joined). Sets the
 * failure budget for either side and/or the DOMException name to reject with.
 */
export async function setGumFail(page: Page, opts: GumFailOpts): Promise<void> {
  await page.evaluate((o: GumFailOpts) => {
    const w = window as GumWindow;
    const s = w.__gum;
    if (!s) {
      throw new Error("getUserMedia mock not installed (call installGetUserMediaMock first)");
    }
    if (o.errorName !== undefined) {
      s.errorName = o.errorName;
    }
    if (o.video !== undefined) {
      s.video.failRemaining = o.video;
    }
    if (o.audio !== undefined) {
      s.audio.failRemaining = o.audio;
    }
  }, opts);
}

/** Read the number of getUserMedia calls that requested each side so far. */
export async function getGumCalls(page: Page): Promise<{ video: number; audio: number }> {
  return page.evaluate(() => {
    const w = window as GumWindow;
    const s = w.__gum;
    if (!s) {
      throw new Error("getUserMedia mock not installed (call installGetUserMediaMock first)");
    }
    return { video: s.video.calls, audio: s.audio.calls };
  });
}
