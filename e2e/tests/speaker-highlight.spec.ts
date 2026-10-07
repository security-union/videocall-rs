import path from "node:path";
import { test, expect, chromium, Page } from "@playwright/test";
import { BROWSER_ARGS, createAuthenticatedContext } from "../helpers/auth-context";
import { continuousToneWavPath } from "../helpers/audio-fixtures";
import { waitForServices } from "../helpers/wait-for-services";
import { seedShareViewMode } from "../helpers/screen-share-meeting";

/**
 * Speaking-glow E2E tests.
 *
 * The speaking glow is rendered as:
 * - `.speaking-tile` CSS class on the outer tile div (peer: `.grid-item`,
 *    host self-view: `#host-controls-nav`)
 * - Inline `box-shadow` + `transition` via `speak_style()` on the same element
 *
 * Most tests still exercise the silent baseline. Two targeted regressions now
 * use richer fixtures:
 * - a real WAV fixture for fake microphone input so remote speaking can be observed
 * - a synthetic `getDisplayMedia()` shim so host screen share can start without
 *   the native picker
 */

const SPEAKING_AUDIO_FIXTURE = path.resolve(__dirname, "../../dioxus-ui/assets/hi.wav");

function browserArgs(fakeAudioFile?: string) {
  if (!fakeAudioFile) {
    return [...BROWSER_ARGS];
  }

  return [...BROWSER_ARGS, `--use-file-for-fake-audio-capture=${fakeAudioFile}`];
}

async function installSyntheticDisplayCapture(page: Page) {
  await page.addInitScript(() => {
    const mediaDevices = navigator.mediaDevices;
    if (!mediaDevices) {
      return;
    }

    const createSyntheticDisplayStream = () => {
      const canvas = document.createElement("canvas");
      canvas.width = 1280;
      canvas.height = 720;

      const context = canvas.getContext("2d");
      if (!context) {
        throw new Error("2D canvas context unavailable for synthetic display capture");
      }

      let frame = 0;
      const paint = () => {
        frame += 1;
        context.fillStyle = "#0b1220";
        context.fillRect(0, 0, canvas.width, canvas.height);

        context.fillStyle = "#4fd1c5";
        context.fillRect(80 + (frame % 320), 120, 280, 160);

        context.fillStyle = "#f8fafc";
        context.font = "bold 64px sans-serif";
        context.fillText("Synthetic screen share", 80, 420);

        context.fillStyle = "#94a3b8";
        context.font = "32px sans-serif";
        context.fillText(`frame ${frame}`, 80, 480);

        requestAnimationFrame(paint);
      };

      paint();
      return canvas.captureStream(12);
    };

    Object.defineProperty(mediaDevices, "getDisplayMedia", {
      configurable: true,
      value: async () => createSyntheticDisplayStream(),
    });
  });
}

async function navigateToMeeting(page: Page, meetingId: string, username: string) {
  await page.goto("/");
  await page.waitForTimeout(1500);

  await page.locator("#meeting-id").click();
  await page.locator("#meeting-id").pressSequentially(meetingId, { delay: 50 });
  await page.locator("#username").click();
  await page.locator("#username").fill("");
  await page.locator("#username").pressSequentially(username, { delay: 50 });
  await page.waitForTimeout(500);
  await page.locator("#username").press("Enter");
  await expect(page).toHaveURL(new RegExp(`/meeting/${meetingId}`), {
    timeout: 10_000,
  });
  await page.waitForTimeout(1500);
}

async function ensureMicrophoneEnabled(page: Page) {
  const unmuteButton = page.locator("button.video-control-button", {
    has: page.locator("span.tooltip", { hasText: "Unmute" }),
  });
  if (await unmuteButton.count()) {
    await unmuteButton.first().click();
    await page.waitForTimeout(1_000);
  }
}

async function muteMicrophone(page: Page) {
  const muteButton = page.locator("button.video-control-button", {
    has: page.locator("span.tooltip", { hasText: "Mute" }),
  });
  await expect(muteButton.first()).toBeVisible({ timeout: 10_000 });
  await muteButton.first().click();
  await page.waitForTimeout(1_000);
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

  if (result === "waiting") {
    return "waiting";
  }

  if (result === "waiting-for-meeting") {
    return "waiting-for-meeting";
  }

  if (result === "auto-joined") {
    return "in-meeting";
  }

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
  if (guestResult === "in-meeting") {
    return;
  }

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
 * Set up a two-user meeting (host + guest) and return both pages
 * along with browser handles for cleanup.
 */
async function setupTwoUserMeeting(
  uiURL: string,
  meetingId: string,
  hostName: string,
  guestName: string,
  options?: {
    hostFakeAudioFile?: string;
    guestFakeAudioFile?: string;
    prepareHostPage?: (page: Page) => Promise<void>;
    prepareGuestPage?: (page: Page) => Promise<void>;
  },
) {
  const browser1 = await chromium.launch({
    args: browserArgs(options?.hostFakeAudioFile),
  });
  const browser2 = await chromium.launch({
    args: browserArgs(options?.guestFakeAudioFile),
  });

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
  // The share tests here read the split layout, which a received share opens
  // in only by preference since #2792.
  await seedShareViewMode(guestCtx, "enlarged");

  const hostPage = await hostCtx.newPage();
  const guestPage = await guestCtx.newPage();

  if (options?.prepareHostPage) {
    await options.prepareHostPage(hostPage);
  }
  if (options?.prepareGuestPage) {
    await options.prepareGuestPage(guestPage);
  }

  await navigateToMeeting(hostPage, meetingId, hostName);
  const hostResult = await joinMeetingFromPage(hostPage);
  expect(hostResult).toBe("in-meeting");

  await navigateToMeeting(guestPage, meetingId, guestName);
  const guestResult = await joinMeetingFromPage(guestPage);
  await admitGuestIfNeeded(hostPage, guestPage, guestResult);

  // Wait for peer tile to appear on the host side
  const peerTile = hostPage.locator("#grid-container .grid-item");
  await expect(peerTile.first()).toBeVisible({ timeout: 30_000 });

  return { hostPage, guestPage, browser1, browser2 };
}

async function waitForTileToSpeak(page: Page) {
  // Works in both grid layout (.grid-item) and split layout (.split-peer-tile)
  const peerTile = page.locator(".split-peer-tile, #grid-container .grid-item").first();
  await expect(peerTile).toBeVisible({ timeout: 30_000 });

  await expect
    .poll(
      async () => {
        const className = (await peerTile.getAttribute("class")) || "";
        const style = (await peerTile.getAttribute("style")) || "";
        const hasExplicitGlow = style.includes("box-shadow") && !style.includes("box-shadow: none");
        return className.includes("speaking-tile") || hasExplicitGlow;
      },
      {
        timeout: 30_000,
        message: "expected peer tile to enter the speaking-highlight state",
      },
    )
    .toBe(true);

  return peerTile;
}

/** One rendered state of the tracked tile's glow-bearing attributes. */
interface GlowSample {
  style: string;
  cls: string;
  missing: boolean;
}

/**
 * True when a tile's inline style is `speak_style`'s SILENT output (the glow is
 * off), false when it is either of the glowing outputs.
 *
 * Keyed on the transition DIRECTION rather than a colour literal, because
 * neither alone is reliable:
 *  - `speak_style`'s silent branches emit the tile's default border colour,
 *    which is themed and whose rendered text has already drifted (this spec
 *    used to hard-code `rgba(100, 100, 100, 0.30)`, which current builds never
 *    emit — an assertion on it could only ever time out).
 *  - `box-shadow: none` is emitted by a GLOWING tile too when
 *    `inner_glow_strength` is 0, so the shadow alone cannot mean "silent".
 * Both silent branches fade with `ease-out` and both glowing branches fade in
 * with `ease-in`, in every version of `canvas_generator.rs` that has shipped —
 * so the pair below identifies the silent state exactly.
 */
function isSilentGlowStyle(style: string): boolean {
  return style.includes("box-shadow: none") && style.includes("ease-out");
}

/**
 * Start recording every rendered state of the tile with id `tileId`.
 *
 * Two capture paths run together, because the failure they guard has two
 * possible shapes.
 *
 * The MutationObserver catches a glow that drops and is restored between polls.
 * It records each mutation's `oldValue` rather than re-reading the element,
 * which matters: observer callbacks are batched to a microtask, so if the glow
 * were switched off and back on within one batch, re-reading the DOM would see
 * only the restored value and miss the drop entirely. The recorded `oldValue`
 * chain preserves every intermediate style the tile ever held. It watches the
 * CONTAINER subtree rather than the tile node, so it keeps reporting if Dioxus
 * re-creates the tile element.
 *
 * The interval re-queries the id from scratch each tick, so it keeps reporting
 * even if the tile is rebuilt outside the observed subtree, and it records the
 * "tile is missing" case that would otherwise make a clean result meaningless.
 *
 * THROWS if `#grid-container` is absent. The two paths are not interchangeable
 * — falling back to interval-only sampling would still collect enough samples
 * to satisfy every non-vacuity check in the test while quietly losing the
 * sub-poll capture, so the degraded mode must be an error, not a default.
 */
async function startGlowRecorder(page: Page, tileId: string): Promise<void> {
  await page.evaluate((id) => {
    const w = window as unknown as {
      __glowSamples?: GlowSample[];
      __glowStop?: () => void;
    };
    const samples: GlowSample[] = [];
    w.__glowSamples = samples;

    const sample = () => {
      const el = document.getElementById(id);
      samples.push({
        style: el?.getAttribute("style") || "",
        cls: el?.getAttribute("class") || "",
        missing: el === null,
      });
    };

    sample();

    const container = document.getElementById("grid-container");
    if (!container) {
      // Fail loudly rather than degrading to interval-only sampling: without
      // the observer a glow drop shorter than the poll interval goes
      // unrecorded, and the test would still collect enough samples to look
      // healthy while having lost the capture it exists to perform.
      throw new Error(
        "#grid-container not found — cannot install the glow MutationObserver, so sub-poll " +
          "glow drops would go unrecorded and a pass would be meaningless",
      );
    }

    const observer = new MutationObserver((records) => {
      for (const record of records) {
        if (record.attributeName === "style" && (record.target as Element).id === id) {
          samples.push({
            style: record.oldValue || "",
            cls: "",
            missing: false,
          });
        }
      }
      sample();
    });
    observer.observe(container, {
      subtree: true,
      attributes: true,
      attributeOldValue: true,
      attributeFilter: ["style", "class"],
    });

    const timer = window.setInterval(sample, 50);

    w.__glowStop = () => {
      observer.disconnect();
      window.clearInterval(timer);
    };
  }, tileId);
}

/** Stop the recorder started by {@link startGlowRecorder} and drain its samples. */
async function stopGlowRecorder(page: Page): Promise<GlowSample[]> {
  return page.evaluate(() => {
    const w = window as unknown as {
      __glowSamples?: GlowSample[];
      __glowStop?: () => void;
    };
    w.__glowStop?.();
    return w.__glowSamples ?? [];
  });
}

test.describe("Speaker highlight glow on video tiles", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  // ──────────────────────────────────────────────────────────────────────
  // 1. Glow on outer tile only — silent state
  // ──────────────────────────────────────────────────────────────────────
  test("peer tile outer div has box-shadow:none and no speaking-tile class when silent", async ({
    baseURL,
  }) => {
    test.setTimeout(120_000);
    const uiURL = baseURL || "http://localhost:80";
    const meetingId = `e2e_glow_peer_${Date.now()}`;

    const { hostPage, browser1, browser2 } = await setupTwoUserMeeting(
      uiURL,
      meetingId,
      "GlowHost",
      "GlowGuest",
    );

    try {
      // The outer tile div is a .grid-item inside #grid-container.
      // When silent it must NOT have .speaking-tile class and its inline
      // style must contain "box-shadow: none".
      const outerTile = hostPage.locator("#grid-container .grid-item").first();
      await expect(outerTile).toBeVisible({ timeout: 10_000 });

      const tileClass = await outerTile.getAttribute("class");
      expect(tileClass).toBeTruthy();
      expect(tileClass).not.toContain("speaking-tile");

      const tileStyle = await outerTile.getAttribute("style");
      expect(tileStyle).toBeTruthy();
      expect(tileStyle).toContain("box-shadow: none");
    } finally {
      await browser1.close();
      await browser2.close();
    }
  });

  test("remote participant border resets to the default color after they stop speaking", async ({
    baseURL,
  }) => {
    test.setTimeout(150_000);
    const uiURL = baseURL || "http://localhost:80";
    const meetingId = `e2e_glow_reset_${Date.now()}`;

    const { hostPage, guestPage, browser1, browser2 } = await setupTwoUserMeeting(
      uiURL,
      meetingId,
      "ResetHost",
      "ResetGuest",
      {
        guestFakeAudioFile: SPEAKING_AUDIO_FIXTURE,
      },
    );

    try {
      await ensureMicrophoneEnabled(guestPage);

      const peerTile = await waitForTileToSpeak(hostPage);

      await muteMicrophone(guestPage);

      // Asserted via `isSilentGlowStyle` rather than a border-colour literal:
      // the tile's default border colour is themed and its rendered text has
      // already drifted from the literal this spec used to hard-code, which
      // left this assertion unsatisfiable on current builds.
      await expect
        .poll(
          async () => ({
            className: (await peerTile.getAttribute("class")) || "",
            silent: isSilentGlowStyle((await peerTile.getAttribute("style")) || ""),
          }),
          {
            timeout: 30_000,
            message: "expected remote peer highlight to clear after speech stops",
          },
        )
        .toMatchObject({
          className: expect.not.stringContaining("speaking-tile"),
          silent: true,
        });
    } finally {
      await browser1.close();
      await browser2.close();
    }
  });

  // ──────────────────────────────────────────────────────────────────────
  // 1b. The glow SURVIVES a heartbeat while the peer is STILL talking —
  //     regression for issue 2174.
  //
  // Bug: `resolve_audio_level` (peer_tile.rs) used to prefer the float
  // `audio_level` metric and fall back to the `is_speaking` boolean only when
  // the float was absent. On the heartbeat path the float is NEVER absent —
  // `broadcast_peer_status` (peer_decode_manager.rs) always emits it, and the
  // `Peer::audio_level` field behind it is only ever assigned 0.0 (initialised
  // to 0.0, reset to 0.0 on a not-speaking heartbeat, never written non-zero in
  // production). So every heartbeat for a TALKING peer resolved to Some(0.0)
  // and drove the tile's glow to zero, discarding the genuinely fresh
  // `is_speaking = true` the sender had just put in the packet. The fix makes
  // the boolean authoritative and the float a refinement.
  //
  // WHY THE AMPLITUDE MOVES: a constant loud tone would pin the decoder's
  // reported intensity at a saturated 1.0, and its VAD is edge-triggered
  // (`handle_pcm_data` re-broadcasts only when the speaking boolean flips or
  // the level moves past AUDIO_LEVEL_DELTA_THRESHOLD = 0.02). The fast path
  // would emit once and then go quiet for the rest of the run — which the
  // resolver cannot distinguish from a dead peer, and its no-events deadline
  // (12.5s) would put the glow out on CORRECT code, failing this spec for a
  // reason unrelated to the bug.
  //
  // WHAT THE FAILURE LOOKS LIKE: on the un-fixed code each heartbeat writes the
  // silent style, and the glow stays out until the next fast-path update
  // re-lights it — a gap of up to about a second, repeated every heartbeat. The
  // recorder captures each mutation's `oldValue`, so even a gap shorter than
  // the sampling interval is recorded rather than sampled over.
  //
  // NON-VACUITY: a "the glow never dropped" pass would be worthless if no
  // heartbeat actually reached the host during the window. The guest toggles
  // their camera, which fires `Connection::set_speaking`'s sibling
  // `send_immediate_heartbeat`, and the test waits for the host tile to render
  // a <canvas> — proof that the host PROCESSED a heartbeat carrying the new
  // media state. The window then runs on past two more 5s keepalives.
  //
  // Mutation sensitivity: reverting `resolve_audio_level` to the float-first
  // rule makes that forced heartbeat resolve to Some(0.0) and the tile falls
  // back to `speak_style`'s silent branch. VERIFIED against an un-fixed build.
  //
  // Deliberately asserts only glow PRESENCE (not the silent style), never a
  // brightness or box-shadow magnitude: a heartbeat-sourced level and a
  // fast-path level legitimately differ, so pinning a specific intensity would
  // make this spec fail on a resolver tuning change that is not a regression.
  // ──────────────────────────────────────────────────────────────────────
  test("remote peer glow stays lit across a heartbeat while they keep speaking", async ({
    baseURL,
  }) => {
    test.setTimeout(240_000);
    const uiURL = baseURL || "http://localhost:80";
    const meetingId = `e2e_glow_heartbeat_${Date.now()}`;

    const { hostPage, guestPage, browser1, browser2 } = await setupTwoUserMeeting(
      uiURL,
      meetingId,
      "HbGlowHost",
      "HbGlowGuest",
      {
        guestFakeAudioFile: continuousToneWavPath(),
      },
    );

    try {
      await ensureMicrophoneEnabled(guestPage);

      // Presence before measurement: the tile must be glowing before we can
      // assert anything about the glow being held.
      const peerTile = await waitForTileToSpeak(hostPage);
      const tileId = await peerTile.getAttribute("id");
      expect(tileId, "the guest tile needs a stable id to track across renders").toBeTruthy();

      // Baseline: with the tone running the glow must be settled, not a
      // transient that `waitForTileToSpeak` happened to catch on its way out.
      await expect
        .poll(async () => isSilentGlowStyle((await peerTile.getAttribute("style")) || ""), {
          timeout: 20_000,
          message: "expected a settled glow on the tile before recording",
        })
        .toBe(false);

      await startGlowRecorder(hostPage, tileId as string);

      // Force an immediate heartbeat from the guest (a media-state transition
      // sends one straight away rather than waiting for the 5s keepalive).
      await guestPage.locator('[data-testid="camera-toggle-button"]').click();

      // Liveness receipt: the host renders the guest's video only after
      // processing a heartbeat carrying video_enabled = true. Without this the
      // assertion below could pass simply because no heartbeat ever arrived.
      await expect
        .poll(async () => hostPage.locator(`#${tileId} canvas`).count(), {
          timeout: 60_000,
          message:
            "expected the host to render the guest's camera — proof a heartbeat was processed",
        })
        .toBeGreaterThan(0);

      // Keep watching across at least two more keepalive heartbeats.
      await hostPage.waitForTimeout(13_000);

      const samples = await stopGlowRecorder(hostPage);

      // Non-vacuity: the recorder must actually have observed a live tile.
      expect(samples.length).toBeGreaterThan(50);
      expect(
        samples.filter((s) => s.missing).length,
        "the tracked tile disappeared mid-window — the recording is not meaningful",
      ).toBe(0);
      // ...and it must have seen the glow at least once, or "never silent"
      // would be trivially true for a tile that rendered nothing at all.
      expect(
        samples.filter((s) => !isSilentGlowStyle(s.style)).length,
        "no glowing sample was recorded at all",
      ).toBeGreaterThan(0);

      // The regression assertion: while the peer talks without pause, no
      // rendered frame may show the tile in the silent style.
      const unglowed = samples.filter((s) => isSilentGlowStyle(s.style));
      expect(
        unglowed.length,
        `tile glow dropped in ${unglowed.length}/${samples.length} samples while the peer was ` +
          `continuously speaking (issue 2174: a heartbeat zeroed the level despite is_speaking=true).` +
          ` First silent sample: ${unglowed[0]?.style ?? "n/a"}`,
      ).toBe(0);
    } finally {
      await browser1.close();
      await browser2.close();
    }
  });

  // ──────────────────────────────────────────────────────────────────────
  // 2. Transition property present for smooth glow animation
  // ──────────────────────────────────────────────────────────────────────
  test("peer tile has transition property in inline style for glow animation", async ({
    baseURL,
  }) => {
    test.setTimeout(120_000);
    const uiURL = baseURL || "http://localhost:80";
    const meetingId = `e2e_glow_trans_${Date.now()}`;

    const { hostPage, browser1, browser2 } = await setupTwoUserMeeting(
      uiURL,
      meetingId,
      "TransHost",
      "TransGuest",
    );

    try {
      const outerTile = hostPage.locator("#grid-container .grid-item").first();
      await expect(outerTile).toBeVisible({ timeout: 10_000 });

      const style = await outerTile.getAttribute("style");
      expect(style).toBeTruthy();
      // speak_style() always emits transition: for both silent and active states
      expect(style).toContain("transition:");
      expect(style).toContain("box-shadow");
    } finally {
      await browser1.close();
      await browser2.close();
    }
  });

  // ──────────────────────────────────────────────────────────────────────
  // 3. Host controls nav (#host-controls-nav) — silent state
  // ──────────────────────────────────────────────────────────────────────
  test("host-controls-nav has class 'host' without speaking-tile when silent", async ({
    baseURL,
  }) => {
    test.setTimeout(120_000);
    const uiURL = baseURL || "http://localhost:80";
    const meetingId = `e2e_glow_hostnav_${Date.now()}`;

    const browser = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const ctx = await createAuthenticatedContext(
        browser,
        "host-nav@videocall.rs",
        "HostNav",
        uiURL,
      );
      const page = await ctx.newPage();

      await navigateToMeeting(page, meetingId, "HostNav");
      const result = await joinMeetingFromPage(page);
      expect(result).toBe("in-meeting");

      const hostNav = page.locator("#host-controls-nav");
      await expect(hostNav).toBeVisible({ timeout: 15_000 });

      // Class should be "host" (no "speaking-tile")
      const navClass = await hostNav.getAttribute("class");
      expect(navClass).toBeTruthy();
      expect(navClass).toContain("host");
      expect(navClass).not.toContain("speaking-tile");
    } finally {
      await browser.close();
    }
  });

  test("host-controls-nav inline style has box-shadow:none when silent", async ({ baseURL }) => {
    test.setTimeout(120_000);
    const uiURL = baseURL || "http://localhost:80";
    const meetingId = `e2e_glow_hostbox_${Date.now()}`;

    const browser = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const ctx = await createAuthenticatedContext(
        browser,
        "host-box@videocall.rs",
        "HostBox",
        uiURL,
      );
      const page = await ctx.newPage();

      await navigateToMeeting(page, meetingId, "HostBox");
      const result = await joinMeetingFromPage(page);
      expect(result).toBe("in-meeting");

      const hostNav = page.locator("#host-controls-nav");
      await expect(hostNav).toBeVisible({ timeout: 15_000 });

      const style = await hostNav.getAttribute("style");
      expect(style).toBeTruthy();
      expect(style).toContain("box-shadow: none");
      expect(style).toContain("transition:");
    } finally {
      await browser.close();
    }
  });

  // ──────────────────────────────────────────────────────────────────────
  // 3b. Host controls nav — overflow clipping and video wrapper radius
  //
  // Regression for PR #1844/#1804: outside customize mode the host-controls-nav
  // must clip overflow (hidden) and .host-video-wrapper must have a non-zero
  // border-top-left-radius. The bug removed overflow:hidden and zeroed the
  // radius, causing visual bleed.
  // ──────────────────────────────────────────────────────────────────────
  test("host-controls-nav has overflow hidden and host-video-wrapper has non-zero top-left radius", async ({
    baseURL,
  }) => {
    test.setTimeout(120_000);
    const uiURL = baseURL || "http://localhost:80";
    const meetingId = `e2e_overflow_radius_${Date.now()}`;

    const browser = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const ctx = await createAuthenticatedContext(
        browser,
        "host-overflow@videocall.rs",
        "HostOverflow",
        uiURL,
      );
      const page = await ctx.newPage();

      await navigateToMeeting(page, meetingId, "HostOverflow");
      const result = await joinMeetingFromPage(page);
      expect(result).toBe("in-meeting");

      // Verify #host-controls-nav.host is visible (default non-customize mode)
      const hostNav = page.locator("#host-controls-nav.host");
      await expect(hostNav).toBeVisible({ timeout: 15_000 });

      // Computed overflow must be "hidden" in default (non-customize) mode
      const overflow = await hostNav.evaluate((el) => {
        return window.getComputedStyle(el).overflow;
      });
      expect(overflow).toBe("hidden");

      // .host-video-wrapper must have a non-zero border-top-left-radius
      const videoWrapper = page.locator(".host-video-wrapper");
      await expect(videoWrapper).toBeVisible({ timeout: 10_000 });

      const borderTopLeftRadius = await videoWrapper.evaluate((el) => {
        return window.getComputedStyle(el).borderTopLeftRadius;
      });
      expect(borderTopLeftRadius).not.toBe("0px");
    } finally {
      await browser.close();
    }
  });

  // ──────────────────────────────────────────────────────────────────────
  // 4. Screen-share tiles — no glow
  //
  // The generic no-screen-share baseline is still covered structurally below.
  // For the host-speaking-while-screen-sharing regression, this file now also
  // includes a synthetic `getDisplayMedia()` path that can drive the split
  // layout without the native OS picker.
  //
  // Screen-share tiles themselves should still never receive speaking glow:
  //   - In the grid layout, screen-share tiles are rendered inside a
  //     separate `.grid-item` div WITHOUT speaking-tile or box-shadow.
  //   - In the split layout (TileMode::ScreenOnly), the screen share
  //     renders inside a `.split-screen-tile` div with NO glow props.
  //   - The Rust source (`canvas_generator.rs`) suppresses glow for the
  //     screen-share canvas while leaving the speaker's participant tile
  //     eligible for highlight.
  //
  // The baseline test below verifies that in a normal (no-screen-share)
  // meeting the grid-item tiles do NOT have stale split-layout artifacts.
  // ──────────────────────────────────────────────────────────────────────
  test("grid-item tiles in a normal meeting have no stale screen-share glow artifacts", async ({
    baseURL,
  }) => {
    test.setTimeout(120_000);
    const uiURL = baseURL || "http://localhost:80";
    const meetingId = `e2e_glow_ss_${Date.now()}`;

    const { hostPage, browser1, browser2 } = await setupTwoUserMeeting(
      uiURL,
      meetingId,
      "SSHost",
      "SSGuest",
    );

    try {
      // No .split-screen-tile should exist when nobody is screen-sharing
      const splitScreenTile = hostPage.locator(".split-screen-tile");
      await expect(splitScreenTile).toHaveCount(0);

      // The peer tile should have box-shadow: none (no glow) and no
      // speaking-tile class — confirming normal silent rendering without
      // screen-share-related artifacts.
      const outerTile = hostPage.locator("#grid-container .grid-item").first();
      await expect(outerTile).toBeVisible({ timeout: 10_000 });

      const style = await outerTile.getAttribute("style");
      expect(style).toContain("box-shadow: none");

      const cls = await outerTile.getAttribute("class");
      expect(cls).not.toContain("speaking-tile");
    } finally {
      await browser1.close();
      await browser2.close();
    }
  });

  // FIXME(#741): Requires real VAD-triggered speaking state — fake media
  // devices produce no audio so waitForTileToSpeak() never resolves and
  // the glow border is never applied. Unblock: inject a synthetic audio
  // track with non-zero samples, or mock the VAD signal directly in the
  // Dioxus client.
  test.fixme("host remains highlighted for other participants while screen sharing", async ({
    baseURL,
  }) => {
    test.setTimeout(180_000);
    const uiURL = baseURL || "http://localhost:80";
    const meetingId = `e2e_glow_host_ss_${Date.now()}`;

    const { hostPage, guestPage, browser1, browser2 } = await setupTwoUserMeeting(
      uiURL,
      meetingId,
      "ScreenHost",
      "ScreenGuest",
      {
        hostFakeAudioFile: SPEAKING_AUDIO_FIXTURE,
        prepareHostPage: installSyntheticDisplayCapture,
      },
    );

    try {
      await ensureMicrophoneEnabled(hostPage);

      const shareBtn = hostPage.locator("button.video-control-button", {
        has: hostPage.locator("span.tooltip", { hasText: "Share Screen" }),
      });
      await expect(shareBtn).toBeVisible({ timeout: 10_000 });
      await shareBtn.click();

      await expect(guestPage.locator(".split-screen-tile")).toBeVisible({ timeout: 30_000 });
      await expect(guestPage.locator(".screen-share-resize-handle")).toBeVisible({
        timeout: 30_000,
      });

      const hostParticipantTile = await waitForTileToSpeak(guestPage);
      const highlightedStyle = (await hostParticipantTile.getAttribute("style")) || "";

      expect(isSilentGlowStyle(highlightedStyle)).toBe(false);
    } finally {
      await browser1.close();
      await browser2.close();
    }
  });

  // ──────────────────────────────────────────────────────────────────────
  // 5. The pin marker SURVIVES a reactive rewrite of the tile's class.
  //
  // A peer-count change that flips `full-bleed` makes Dioxus rewrite the tile
  // root's `class` attribute, dropping anything added to it imperatively. This
  // pins the tile, then flips `full-bleed` on it by having the third
  // participant leave, and requires `tile-pinned` to be present on both sides
  // of that rewrite.
  // ──────────────────────────────────────────────────────────────────────
  test("a pinned peer tile keeps tile-pinned across a reactive class rewrite", async ({
    baseURL,
  }) => {
    test.setTimeout(180_000);
    const uiURL = baseURL || "http://localhost:80";
    const meetingId = `e2e_pin_survives_rerender_${Date.now()}`;

    const { hostPage, browser1, browser2 } = await setupTwoUserMeeting(
      uiURL,
      meetingId,
      "PinRerHost",
      "PinRerGuest",
    );

    let browser3: Awaited<ReturnType<typeof chromium.launch>> | undefined;

    try {
      const guestTile = hostPage.locator("#grid-container .grid-item").first();
      await expect(guestTile).toBeVisible({ timeout: 30_000 });
      const guestTileId = await guestTile.getAttribute("id");
      expect(guestTileId).toBeTruthy();
      const pinnedTile = hostPage.locator(`#${guestTileId}`);

      browser3 = await chromium.launch({ args: browserArgs() });
      const guest3Ctx = await createAuthenticatedContext(
        browser3,
        "pinrerguest2@videocall.rs",
        "PinRerGuest2",
        uiURL,
      );
      const guest3Page = await guest3Ctx.newPage();
      await navigateToMeeting(guest3Page, meetingId, "PinRerGuest2");
      const guest3Result = await joinMeetingFromPage(guest3Page);
      await admitGuestIfNeeded(hostPage, guest3Page, guest3Result);

      await expect
        .poll(async () => hostPage.locator("#grid-container .grid-item").count(), {
          timeout: 30_000,
          message: "expected the host grid to show two remote tiles after the 3rd peer joined",
        })
        .toBeGreaterThanOrEqual(2);

      await pinnedTile.locator('[data-testid="tile-pin-button"]').click({ timeout: 10_000 });
      await expect
        .poll(async () => (await pinnedTile.getAttribute("class")) || "", {
          timeout: 10_000,
          message: "expected the tile to gain tile-pinned after clicking pin",
        })
        .toEqual(expect.stringMatching(/^(?!.*\bfull-bleed\b)(?=.*\btile-pinned\b).*$/));

      await browser3.close();
      browser3 = undefined;

      await expect
        .poll(async () => hostPage.locator("#grid-container .grid-item").count(), {
          timeout: 30_000,
          message: "expected the host grid to drop back to one remote tile after the 3rd peer left",
        })
        .toBe(1);

      await expect
        .poll(async () => (await pinnedTile.getAttribute("class")) || "", {
          timeout: 30_000,
          message: "expected the pinned tile to keep tile-pinned after gaining full-bleed",
        })
        .toEqual(expect.stringMatching(/(?=.*\bfull-bleed\b)(?=.*\btile-pinned\b)/));
      await expect(pinnedTile).toHaveAttribute("data-pinned", "true");
    } finally {
      await browser1.close();
      await browser2.close();
      if (browser3) {
        await browser3.close();
      }
    }
  });

  // ──────────────────────────────────────────────────────────────────────
  // 6. Mic icon audio-indicator — no speaking class when silent
  // ──────────────────────────────────────────────────────────────────────
  test("mic icon does not have speaking class when silent", async ({ baseURL }) => {
    test.setTimeout(120_000);
    const uiURL = baseURL || "http://localhost:80";
    const meetingId = `e2e_glow_mic_${Date.now()}`;

    const { hostPage, browser1, browser2 } = await setupTwoUserMeeting(
      uiURL,
      meetingId,
      "MicHost",
      "MicGuest",
    );

    try {
      // The audio-indicator div on peer tiles should NOT have "speaking" class
      const audioIndicator = hostPage.locator("#grid-container .audio-indicator").first();
      await expect(audioIndicator).toBeVisible({ timeout: 10_000 });
      await expect(audioIndicator).not.toHaveClass(/speaking/);
    } finally {
      await browser1.close();
      await browser2.close();
    }
  });

  // ──────────────────────────────────────────────────────────────────────
  // 6b. Peer-list mic icon — muted peer must NOT show speaking glow
  //
  // Regression: the speaking flag and muted flag are independent signals.
  // Before the fix, a muted peer whose VAD hadn't cleared would show the
  // mic icon with the "speaking" CSS class (green glow) in the peer list,
  // contradicting the muted state.
  // ──────────────────────────────────────────────────────────────────────
  test("peer-list mic icon does not show speaking class when peer is muted", async ({
    baseURL,
  }) => {
    test.setTimeout(150_000);
    const uiURL = baseURL || "http://localhost:80";
    const meetingId = `e2e_mutemic_${Date.now()}`;

    const { hostPage, guestPage, browser1, browser2 } = await setupTwoUserMeeting(
      uiURL,
      meetingId,
      "MuteMicHost",
      "MuteMicGuest",
      {
        guestFakeAudioFile: SPEAKING_AUDIO_FIXTURE,
      },
    );

    try {
      await ensureMicrophoneEnabled(guestPage);

      // Wait for the guest to be detected as speaking on the host side
      await waitForTileToSpeak(hostPage);

      // Open the peer list
      await hostPage.locator(".video-controls-container").hover();
      const peerBtn = hostPage.locator(".video-controls-container button", {
        has: hostPage.locator('.tooltip:has-text("Open Peers")'),
      });
      await peerBtn.first().click();
      await expect(hostPage.locator("#peer-list-container")).toHaveClass(/visible/, {
        timeout: 5_000,
      });

      // The guest's peer_item_mic should have "speaking" class while unmuted
      const guestMicIcon = hostPage.locator(".peer_item_mic").last();
      await expect(guestMicIcon).toBeVisible({ timeout: 5_000 });
      await expect(guestMicIcon).toHaveClass(/speaking/, { timeout: 15_000 });

      // Now mute the guest
      await muteMicrophone(guestPage);

      // The mic icon must lose the "speaking" class once muted
      await expect(guestMicIcon).not.toHaveClass(/speaking/, { timeout: 15_000 });
    } finally {
      await browser1.close();
      await browser2.close();
    }
  });

  // ──────────────────────────────────────────────────────────────────────
  // 7. Host controls nav structural pattern — class + inline style
  //
  // Verifies #host-controls-nav carries both a CSS class and an inline
  // style from speak_style(), so that when speaking IS triggered the glow
  // will render correctly via the `.speaking-tile` CSS rule + box-shadow.
  // ──────────────────────────────────────────────────────────────────────
  test("host-controls-nav has both class and inline style from speak_style", async ({
    baseURL,
  }) => {
    test.setTimeout(120_000);
    const uiURL = baseURL || "http://localhost:80";
    const meetingId = `e2e_glow_hostpat_${Date.now()}`;

    const browser = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const ctx = await createAuthenticatedContext(
        browser,
        "host-pattern@videocall.rs",
        "HostPattern",
        uiURL,
      );
      const page = await ctx.newPage();

      await navigateToMeeting(page, meetingId, "HostPattern");
      const result = await joinMeetingFromPage(page);
      expect(result).toBe("in-meeting");

      const hostNav = page.locator("#host-controls-nav");
      await expect(hostNav).toBeVisible({ timeout: 15_000 });

      // Verify the element has BOTH class and style attributes set:
      //   class = "host" (silent) or "host speaking-tile" (speaking)
      //   style = output of speak_style() — always includes transition + box-shadow
      const cls = await hostNav.getAttribute("class");
      expect(cls).toMatch(/\bhost\b/);

      const style = await hostNav.getAttribute("style");
      expect(style).toBeTruthy();
      // speak_style() always emits these two properties
      expect(style).toContain("transition:");
      expect(style).toContain("box-shadow");

      // Specifically for silent state: ease-out transition for fade-out
      expect(style).toContain("ease-out");
    } finally {
      await browser.close();
    }
  });
});
