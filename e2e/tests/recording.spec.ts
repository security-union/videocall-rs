/**
 * Recording feature E2E regression spec.
 *
 * ## What this spec guards
 *
 * 1. **Host banner**: clicking the record button transitions the host's
 *    `.recording-status-banner` through "Starting recording…" → "Recording".
 * 2. **Meeting-wide status bar**: after the host starts recording, the
 *    persistent `.meeting-status-bar` (an in-flow strip above the tile grid
 *    hosting the `.meeting-status-item--recording` item) appears for the guest
 *    AND for the recorder themselves (it is shown to every participant
 *    uniformly). This tests `PEER_EVENT_RECORDING_STARTED` delivery populating
 *    the session-keyed recording set that the `any_session_recording` aggregate
 *    reads.
 * 3. **Status bar clears on stop**: after the host stops recording, the bar
 *    detaches for everyone (collapsing its reserved top space back into the
 *    tile area).  This directly guards the abort-cleanup fix:
 *    `set_recording_active(false)` + `PEER_EVENT_RECORDING_STOPPED` must fire on
 *    every exit path (stop click, abort, error) via the → Idle callback,
 *    removing the recorder's session from the set.
 * 4. **Name label not occluded**: the status bar sits ABOVE the tiles rather
 *    than floating over them, so a large tile's `.floating-name` label is never
 *    overlapped by the bar (the literal bug this bar replaced — a fixed-position
 *    pill hid the name in a 2-participant meeting).
 *
 * The dedicated "meeting-wide recording status bar" test below covers the
 * multi-recorder reference-counting semantics (appears on the FIRST recorder,
 * unaffected by additional concurrent recorders, disappears only after the
 * LAST one stops) and the non-auto-fade regression guard.
 *
 * ## Harness
 *
 * Two real browser contexts with `--use-fake-device-for-media-stream`.
 * `window.showSaveFilePicker` is stubbed before navigation with an
 * `addInitScript` so the recording never shows a real OS file-picker dialog
 * and the MediaRecorder writes to a no-op in-memory sink instead.
 * Camera-on is seeded via `vc_prejoin_camera_on` so both peers publish video.
 *
 * ## Mutation sensitivity
 *
 * - Remove `set_recording_active(false)` from the → Idle callback → indicator
 *   stays on the guest side even after stop.
 * - Remove `PEER_EVENT_RECORDING_STOPPED` fan-out → indicator never clears.
 */

import { test, expect, chromium, Page } from "@playwright/test";
import { generateSessionToken } from "../helpers/auth";
import { waitForServices } from "../helpers/wait-for-services";
import { fillAndSubmitJoinForm } from "../helpers/join-meeting";

declare global {
  interface Window {
    // Exposed by `dioxus-ui/scripts/recording.js` for driving/observing the
    // recording state machine from E2E.
    __vcRecording: {
      getState(): string;
      // Resolves the display name the compositor extracts for every peer tile
      // in `#grid-container`, via the SAME production getTileName() drawFrame()
      // feeds into the recording name chips. Used to guard the "remote peer
      // names missing from the recording" regression.
      readTileNames(): Array<{ id: string | null; name: string }>;
    };
    // Test-only override for the in-memory fallback byte ceiling (see below).
    __VC_RECORDING_MAX_FALLBACK_BYTES__?: number;
    // File System Access API entry point (not in the TS DOM lib); we probe and
    // null it out to force the in-memory fallback path.
    showSaveFilePicker?: unknown;
  }
}

const COOKIE_NAME = process.env.COOKIE_NAME || "session";

const BROWSER_ARGS = [
  "--ignore-certificate-errors",
  "--origin-to-force-quic-on=127.0.0.1:4433",
  "--use-fake-device-for-media-stream",
  "--use-fake-ui-for-media-stream",
  "--disable-gpu",
  "--disable-dev-shm-usage",
  "--renderer-process-limit=1",
];

/**
 * Stub script injected before page load.
 *
 * Replaces `window.showSaveFilePicker` with a function that returns a fake
 * FileHandle whose `createWritable()` returns a no-op WritableStream.
 * This prevents the real OS save-file dialog from blocking the test and lets
 * the MediaRecorder write to the fake writer without error.
 */
const STUB_FILE_PICKER_SCRIPT = `
  window.showSaveFilePicker = async function () {
    return {
      createWritable: async function () {
        return {
          write:  async function () {},
          close:  async function () {},
          abort:  async function () {},
        };
      },
    };
  };
`;

/**
 * Init script that forces recording.js down its IN-MEMORY FALLBACK path.
 *
 * The streaming-to-disk path is only taken when `window.showSaveFilePicker`
 * resolves a writable stream (`_writer` non-null). Setting the property to
 * `undefined` makes `typeof window.showSaveFilePicker === "function"` false, so
 * recording.js skips the picker block entirely, leaves `_writer` null, and
 * every chunk accumulates in the module's in-memory `_chunks` array — the exact
 * path the RAM ceiling guards. This script is added AFTER
 * `STUB_FILE_PICKER_SCRIPT` in `createAuthenticatedContext`, so on each
 * navigation it runs last and wins.
 */
const FORCE_FALLBACK_SCRIPT = `window.showSaveFilePicker = undefined;`;

/**
 * Test-only override for the in-memory byte ceiling, injected BEFORE navigation
 * so `recording.js`'s `start()` reads it at recording-start time. A tiny value
 * (a few KiB) is exceeded by the first one or two `CHUNK_MS` timeslice chunks of
 * real fake-camera video, so the auto-stop trips within a few seconds instead of
 * the production 100 MiB / ~5 minutes. Mirrors the `window.__VC_WT_CERT_HASHES__`
 * injection convention documented in `playwright.config.ts`.
 */
const FALLBACK_CAP_BYTES = 2000;
const INJECT_FALLBACK_CAP_SCRIPT = `window.__VC_RECORDING_MAX_FALLBACK_BYTES__ = ${FALLBACK_CAP_BYTES};`;

async function createAuthenticatedContext(
  browser: ReturnType<typeof chromium.launch> extends Promise<infer B> ? B : never,
  email: string,
  name: string,
  uiURL: string,
  opts?: { forceFallback?: boolean },
) {
  const context = await browser.newContext({
    baseURL: uiURL,
    ignoreHTTPSErrors: true,
  });
  const token = generateSessionToken(email, name);
  const url = new URL(uiURL);
  await context.addCookies([
    {
      name: COOKIE_NAME,
      value: token,
      domain: url.hostname,
      path: "/",
      httpOnly: true,
      secure: false,
      sameSite: "Lax",
    },
  ]);
  // Seed camera-on so both peers publish video and the record button is visible.
  await context.addInitScript(`localStorage.setItem("vc_prejoin_camera_on", "true");`);
  // Stub out the OS file picker so recording.js doesn't block on a dialog.
  await context.addInitScript(STUB_FILE_PICKER_SCRIPT);
  if (opts?.forceFallback) {
    // Inject the tiny byte ceiling first, then null out showSaveFilePicker so
    // recording.js takes the in-memory fallback path guarded by the ceiling.
    // Added AFTER STUB_FILE_PICKER_SCRIPT so the `undefined` assignment wins.
    await context.addInitScript(INJECT_FALLBACK_CAP_SCRIPT);
    await context.addInitScript(FORCE_FALLBACK_SCRIPT);
  }
  return context;
}

async function joinMeetingFromPage(
  page: Page,
): Promise<"in-meeting" | "waiting" | "waiting-for-meeting"> {
  const joinButton = page.getByRole("button", {
    name: /Start Meeting|Join Meeting/,
  });
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

/**
 * Open the peer-list sidebar by clicking the "Open Peers" video-controls button.
 * The button lives in `.controls-secondary`, which auto-hides after ~1s of mouse
 * inactivity, so wake the controls bar by hovering + moving the mouse first.
 * Mirrors the helper in `host-controls-menu-ux.spec.ts`.
 */
async function openPeerListSidebar(page: Page): Promise<void> {
  await page.locator(".video-controls-container").hover();
  await page.mouse.move(400, 400);
  await page.waitForTimeout(300);

  const openPeersBtn = page.locator("button.video-control-button", {
    has: page.locator("span.tooltip", { hasText: "Open Peers" }),
  });
  await expect(openPeersBtn).toBeVisible({ timeout: 10_000 });
  await openPeersBtn.click();
  await expect(page.locator("#peer-list-container.visible")).toBeVisible({ timeout: 10_000 });
}

test.describe("Recording feature", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test("host record→stop updates own banner and guest notification banner", async ({ baseURL }) => {
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_recording_${Date.now()}`;

    const browser1 = await chromium.launch({ args: BROWSER_ARGS });
    const browser2 = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const hostCtx = await createAuthenticatedContext(
        browser1,
        "host-rec@videocall.rs",
        "RecHost",
        uiURL,
      );
      const guestCtx = await createAuthenticatedContext(
        browser2,
        "guest-rec@videocall.rs",
        "RecGuest",
        uiURL,
      );

      const hostPage = await hostCtx.newPage();
      const guestPage = await guestCtx.newPage();

      // ── Join meeting ──────────────────────────────────────────────────
      await fillAndSubmitJoinForm(hostPage, meetingId, "RecHost");
      await hostPage.waitForTimeout(1500);
      const hostResult = await joinMeetingFromPage(hostPage);
      expect(hostResult).toBe("in-meeting");

      await fillAndSubmitJoinForm(guestPage, meetingId, "RecGuest");
      await guestPage.waitForTimeout(1500);
      const guestResult = await joinMeetingFromPage(guestPage);

      if (guestResult === "waiting") {
        const admitButton = hostPage.getByTitle("Admit").first();
        await expect(admitButton).toBeVisible({ timeout: 20_000 });
        await hostPage.waitForTimeout(500);
        await admitButton.dispatchEvent("click");
        await guestPage.locator("#grid-container").waitFor({ timeout: 20_000 });
      }

      await expect(hostPage.locator("#grid-container")).toBeVisible({
        timeout: 10_000,
      });
      await expect(guestPage.locator("#grid-container")).toBeVisible({
        timeout: 10_000,
      });

      // Wait until the host can see the guest's tile so the WebRTC session
      // is established before we start recording.
      await expect(hostPage.locator("#grid-container .canvas-container").first()).toBeVisible({
        timeout: 30_000,
      });

      // ── Start recording ───────────────────────────────────────────────
      const recordBtn = hostPage.getByTestId("record-button");
      await expect(recordBtn).toBeVisible({ timeout: 10_000 });
      await recordBtn.click();

      // Host must see its own local status banner transition to "Recording".
      // The banner renders as `.recording-status-banner` > `.toast-name`.
      const hostStatusBanner = hostPage.locator(".recording-status-banner .toast-name");
      await expect(hostStatusBanner).toHaveText(/Recording/, {
        timeout: 15_000,
      });

      // ── Assert the persistent meeting-wide status bar appears ─────────
      // `.meeting-status-bar` is driven by the `any_session_recording`
      // aggregate over the session-keyed recording set. It is shown to EVERY
      // participant — the guest (non-recorder) AND the host (the recorder).
      await expect(guestPage.locator(".meeting-status-bar")).toBeVisible({
        timeout: 20_000,
      });
      await expect(hostPage.locator(".meeting-status-bar")).toBeVisible({
        timeout: 20_000,
      });

      // ── Regression: status bar does NOT occlude the tile name label ───
      // The literal bug this bar fixed: the previous fixed-position recording
      // pill (top-left) overlapped the per-tile `.floating-name` label in a
      // 2-participant meeting where the lone remote tile is large. That tile is
      // `.grid-item.full-bleed` — `position: fixed; inset: 0; height: 100vh` — so
      // it escapes the grid's `pad_top`; attendants.rs tags `#grid-container`
      // with `status-bar-active` and sets `--status-bar-reserve`, and the CSS
      // rule `#grid-container.status-bar-active .grid-item.full-bleed` insets the
      // tile's top by that reserve so the tile (and its name label) sit BELOW the
      // bar. The name label must therefore render at or below the bar's bottom
      // edge with no vertical overlap. Assert this geometrically on the host's
      // view of the guest tile.
      //
      // Mutation sensitivity: drop the `status-bar-active` class push (or the
      // full-bleed CSS inset rule) and the fixed tile snaps back to `top: 0`, so
      // the name label rises to y≈12 and overlaps the bar → `nameBox.y >=
      // barBottom` fails. (Verified in-band: with the fix the name box is at
      // y≈52 for a 40px bar; without it, y≈12.)
      const hostName = hostPage.locator("#grid-container .floating-name").first();
      await expect(hostName).toBeVisible({ timeout: 15_000 });
      {
        const barBox = await hostPage.locator(".meeting-status-bar").boundingBox();
        const nameBox = await hostName.boundingBox();
        expect(barBox).not.toBeNull();
        expect(nameBox).not.toBeNull();
        const barBottom = barBox!.y + barBox!.height;
        // Name label starts at or below the bar's bottom edge (1px tolerance for
        // sub-pixel rounding). No vertical intersection ⇒ the name is not hidden.
        expect(nameBox!.y).toBeGreaterThanOrEqual(barBottom - 1);
      }

      // ── Regression guard: remote peer names present in the recording ──────
      // The recording compositor labels each tile by scraping its name from the
      // DOM via getTileName().  When the peer name was wrapped in
      // `<span class="floating-name-text">` (commit 38943640, a text-overflow
      // fix) the old direct-text-node scan started returning "" for every tile,
      // so ONLY the local recorder's tile (whose name is passed as an explicit
      // override, not scraped) was labelled — every remote peer appeared
      // nameless in the recording.  Assert the host resolves the GUEST's name
      // through the exact production path the compositor uses.
      //
      // Mutation sensitivity: revert getTileName() in recording.js to the
      // pre-fix direct-text-node scan and every entry's `name` becomes "", so
      // this poll never observes "RecGuest" and times out.  (Verified out of
      // band against a synthetic-DOM harness loading the real recording.js:
      // fixed → ["Alice","Bob"], reverted → ["",""].)
      await expect
        .poll(
          () => hostPage.evaluate(() => window.__vcRecording.readTileNames().map((t) => t.name)),
          { timeout: 15_000 },
        )
        .toContain("RecGuest");

      // ── Stop recording ────────────────────────────────────────────────
      // The record button is still `[data-testid="record-button"]` in the
      // Recording state — clicking it triggers stop().
      const stopBtn = hostPage.getByTestId("record-button");
      await expect(stopBtn).toBeVisible({ timeout: 5_000 });
      await stopBtn.click();

      // ── Assert the meeting-wide status bar detaches ───────────────────
      // PEER_EVENT_RECORDING_STOPPED must arrive and the `on_peer_event`
      // handler must remove the host's session from the recording set, which
      // flips `any_session_recording` false and unmounts the whole bar on
      // every participant (collapsing its reserved top space). This fails if:
      //   • set_recording_active(false) is missing from the → Idle callback
      //   • PEER_EVENT_RECORDING_STOPPED fan-out is missing
      await expect(guestPage.locator(".meeting-status-bar")).toHaveCount(0, {
        timeout: 20_000,
      });
      await expect(hostPage.locator(".meeting-status-bar")).toHaveCount(0, {
        timeout: 20_000,
      });

      // Host banner must also be gone after the recording finishes.
      await expect(hostPage.locator(".recording-status-banner")).not.toBeVisible({
        timeout: 10_000,
      });
    } finally {
      await browser1.close();
      await browser2.close();
    }
  });

  test("in-memory fallback auto-stops and saves when the RAM ceiling is exceeded", async ({
    baseURL,
  }) => {
    // Guards the in-memory fallback RAM ceiling in `dioxus-ui/scripts/recording.js`.
    //
    // Background: on every non-Chromium-desktop browser (Firefox, Safari/iOS,
    // Chrome for Android) `window.showSaveFilePicker` is unavailable, so the
    // recorder falls back to accumulating every MediaRecorder chunk in a
    // module-level `_chunks` array. Without a ceiling a long recording could OOM
    // a mobile tab. The fix tracks cumulative chunk bytes in `_fallbackBytes` and,
    // once it crosses `_fallbackMaxBytes`, auto-invokes the module's own
    // `window.__vcRecording.stop()` — driving the SAME
    // `stopping → saving → saved → idle` sequence as a manual stop click.
    //
    // This test forces the fallback path (showSaveFilePicker = undefined) and
    // sets a tiny ceiling (2000 bytes) via `__VC_RECORDING_MAX_FALLBACK_BYTES__`
    // so a couple of CHUNK_MS (3s) timeslice chunks trip it within a few seconds.
    // It then asserts the recording reaches "saved"/"idle" ON ITS OWN, with NO
    // stop click.
    //
    // Mutation sensitivity: revert the `_fallbackBytes`/`_fallbackCapTripped`
    // cap-check block in `ondataavailable` and the recording keeps running
    // indefinitely — getState() stays "recording" and this test times out.
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_rec_cap_${Date.now()}`;

    const browser1 = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const hostCtx = await createAuthenticatedContext(
        browser1,
        "host-rec-cap@videocall.rs",
        "RecCapHost",
        uiURL,
        { forceFallback: true },
      );

      const hostPage = await hostCtx.newPage();

      // Capture the auto-stop warning so we can prove the cap check actually ran
      // (not just that the recording happened to stop for some other reason).
      let sawCapWarning = false;
      hostPage.on("console", (msg) => {
        if (msg.text().includes("In-memory fallback exceeded")) {
          sawCapWarning = true;
        }
      });

      // ── Join meeting as a solo host ───────────────────────────────────
      await fillAndSubmitJoinForm(hostPage, meetingId, "RecCapHost");
      await hostPage.waitForTimeout(1500);
      const hostResult = await joinMeetingFromPage(hostPage);
      expect(hostResult).toBe("in-meeting");

      await expect(hostPage.locator("#grid-container")).toBeVisible({
        timeout: 10_000,
      });

      // Confirm the fallback path is actually in force before recording: with
      // showSaveFilePicker undefined, recording.js cannot open a disk writer.
      const pickerIsUndefined = await hostPage.evaluate(
        () => typeof window.showSaveFilePicker !== "function",
      );
      expect(pickerIsUndefined).toBe(true);

      // ── Start recording (do NOT click stop) ───────────────────────────
      const recordBtn = hostPage.getByTestId("record-button");
      await expect(recordBtn).toBeVisible({ timeout: 15_000 });
      await recordBtn.click();

      // The recording must first actually reach the "recording" state so we know
      // chunks are being produced into the in-memory fallback.
      await expect
        .poll(() => hostPage.evaluate(() => window.__vcRecording.getState()), {
          timeout: 15_000,
        })
        .toBe("recording");

      // ── Core proof: it auto-stops and saves WITHOUT a stop click ───────
      // After the cap trips: recording → stopping → saving → saved → idle.
      // The "saved" window is brief (~3s) before it settles on "idle", so accept
      // either terminal state. On the pre-fix code this poll never leaves
      // "recording" and times out.
      await expect
        .poll(() => hostPage.evaluate(() => window.__vcRecording.getState()), {
          timeout: 20_000,
        })
        .toMatch(/^(saved|idle)$/);

      // The host's own status banner must also detach once the recording
      // finishes on its own.
      await expect(hostPage.locator(".recording-status-banner")).not.toBeVisible({
        timeout: 10_000,
      });

      // The auto-stop warning must have fired — this is the direct signal that
      // the byte-ceiling check tripped (rather than some unrelated stop).
      expect(sawCapWarning).toBe(true);
    } finally {
      await browser1.close();
    }
  });

  test("record button is hidden for non-host guests by default (recording_allowed_for_all=false)", async ({
    baseURL,
  }) => {
    // Guards the `recording_allowed_for_all` meeting setting:
    // when off (the default) the record button MUST NOT render for the guest.
    // Mutation sensitivity: removing the `is_owner || recording_allowed_for_all_toggle()`
    // gate on the Recording slot would make the guest see the button.
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_rec_hidden_${Date.now()}`;

    const browser1 = await chromium.launch({ args: BROWSER_ARGS });
    const browser2 = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const hostCtx = await createAuthenticatedContext(
        browser1,
        "host-rec-hidden@videocall.rs",
        "RecHostHidden",
        uiURL,
      );
      const guestCtx = await createAuthenticatedContext(
        browser2,
        "guest-rec-hidden@videocall.rs",
        "RecGuestHidden",
        uiURL,
      );

      const hostPage = await hostCtx.newPage();
      const guestPage = await guestCtx.newPage();

      await fillAndSubmitJoinForm(hostPage, meetingId, "RecHostHidden");
      await hostPage.waitForTimeout(1500);
      await joinMeetingFromPage(hostPage);

      await fillAndSubmitJoinForm(guestPage, meetingId, "RecGuestHidden");
      await guestPage.waitForTimeout(1500);
      const guestResult = await joinMeetingFromPage(guestPage);
      if (guestResult === "waiting") {
        const admitButton = hostPage.getByTitle("Admit").first();
        await expect(admitButton).toBeVisible({ timeout: 20_000 });
        await hostPage.waitForTimeout(500);
        await admitButton.dispatchEvent("click");
        await guestPage.locator("#grid-container").waitFor({ timeout: 20_000 });
      }

      await expect(guestPage.locator("#grid-container")).toBeVisible({
        timeout: 10_000,
      });

      // ── Assertions ────────────────────────────────────────────────────
      // Host: record button IS visible (owner sees it regardless of setting).
      await expect(hostPage.getByTestId("record-button")).toBeVisible({
        timeout: 15_000,
      });

      // Guest: record button MUST NOT render.  We wait a short beat to let
      // the action bar mount, then assert the button is absent.  The
      // `count() === 0` check is stricter than `toBeHidden()` because it
      // fails if the button exists at all (hidden or not).
      await guestPage.waitForTimeout(2000);
      const guestRecordCount = await guestPage.getByTestId("record-button").count();
      expect(guestRecordCount).toBe(0);
    } finally {
      await browser1.close();
      await browser2.close();
    }
  });

  test("record button is hidden for unauthenticated guests even when recording_allowed_for_all is enabled", async ({
    baseURL,
  }) => {
    // Guards the `is_guest` check in `record_slot_visible`:
    // Unauthenticated guests (no session token) MUST NOT see the record button
    // regardless of the `recording_allowed_for_all` setting.
    // Mutation sensitivity: removing the `!is_guest` check would make the guest
    // see the button when the setting is enabled.
    const uiURL = baseURL || "http://localhost:3001";
    const apiURL = process.env.API_BASE_URL || "http://localhost:8081";
    const meetingId = `e2e_guest_rec_${Date.now()}`;

    const browser1 = await chromium.launch({ args: BROWSER_ARGS });
    const browser2 = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const hostCtx = await createAuthenticatedContext(
        browser1,
        "host-guest-rec@videocall.rs",
        "RecGuestHost",
        uiURL,
      );
      const hostPage = await hostCtx.newPage();

      // Host creates meeting with recording_allowed_for_all enabled.
      // We need to create the meeting via API with the setting enabled.
      const hostToken = generateSessionToken("host-guest-rec@videocall.rs", "RecGuestHost");
      const createResp = await fetch(`${apiURL}/api/v1/meetings`, {
        method: "POST",
        headers: {
          "Content-Type": "application/json",
          Cookie: `${COOKIE_NAME}=${hostToken}`,
        },
        body: JSON.stringify({
          meeting_id: meetingId,
          recording_allowed_for_all: true,
          // The guest in this test joins with NO session cookie (a true
          // unauthenticated guest) — `allow_guests` defaults to `false`
          // server-side (meeting-api/src/search.rs), which would otherwise
          // block the join before the record-button visibility assertion
          // this test exists to guard is ever reached. Mirrors the same
          // explicit `allow_guests: true` used by every other spec in this
          // repo that exercises a truly unauthenticated guest join (e.g.
          // cross-transport-display-name.spec.ts).
          allow_guests: true,
        }),
      });
      if (!createResp.ok) {
        throw new Error(
          `POST /api/v1/meetings failed (${createResp.status}): ${await createResp.text()}`,
        );
      }

      // Host joins the meeting
      await fillAndSubmitJoinForm(hostPage, meetingId, "RecGuestHost");
      await hostPage.waitForTimeout(1500);
      await joinMeetingFromPage(hostPage);

      // Guest joins WITHOUT authentication (no session cookie).
      // This is a true unauthenticated guest (is_guest=true).
      const guestCtx = await browser2.newContext({
        baseURL: uiURL,
        ignoreHTTPSErrors: true,
      });
      await guestCtx.addInitScript(`localStorage.setItem("vc_prejoin_camera_on", "true");`);
      await guestCtx.addInitScript(STUB_FILE_PICKER_SCRIPT);
      const guestPage = await guestCtx.newPage();

      await fillAndSubmitJoinForm(guestPage, meetingId, "UnauthGuest");
      await guestPage.waitForTimeout(1500);

      // Unlike an authenticated participant, a NO-COOKIE guest's initial join
      // attempt is rejected server-side (401 on POST .../join) and the app
      // redirects to a dedicated "Join as Guest" confirmation page
      // (`/meeting/<id>/guest`, a `#guest-name` input + "Join as Guest"
      // button) that `joinMeetingFromPage` — built for the authenticated
      // "Start Meeting"/"Join Meeting" flow — does not know about. Complete
      // that extra step here before falling through to the shared helper.
      const guestNameInput = guestPage.locator("#guest-name");
      if (await guestNameInput.isVisible({ timeout: 5_000 }).catch(() => false)) {
        await guestNameInput.fill("UnauthGuest");
        await guestPage.getByRole("button", { name: "Join as Guest" }).click();
      }

      const guestResult = await joinMeetingFromPage(guestPage);
      if (guestResult === "waiting") {
        const admitButton = hostPage.getByTitle("Admit").first();
        await expect(admitButton).toBeVisible({ timeout: 20_000 });
        await hostPage.waitForTimeout(500);
        await admitButton.dispatchEvent("click");
        await guestPage.locator("#grid-container").waitFor({ timeout: 20_000 });
      }

      await expect(guestPage.locator("#grid-container")).toBeVisible({
        timeout: 10_000,
      });

      // ── Assertions ────────────────────────────────────────────────────
      // Host: record button IS visible.
      await expect(hostPage.getByTestId("record-button")).toBeVisible({
        timeout: 15_000,
      });

      // Unauthenticated guest: record button MUST NOT render, even though
      // recording_allowed_for_all is enabled. The `is_guest` check must
      // override the setting.
      await guestPage.waitForTimeout(2000);
      const guestRecordCount = await guestPage.getByTestId("record-button").count();
      expect(guestRecordCount).toBe(0);
    } finally {
      await browser1.close();
      await browser2.close();
    }
  });

  test("per-recorder icon shows on the recorder's tile + peer-list entry and clears on stop", async ({
    baseURL,
  }) => {
    // Guards the per-recorder recording indicator (RecordingSetCtx). When the
    // host starts recording:
    //   • the GUEST sees a 🔴 `.recording-indicator` on the HOST's video tile
    //     (`#grid-container .floating-name`) — driven by PEER_EVENT_RECORDING_STARTED
    //     inserting the host's session_id into the reactive recording set
    //     (RecordingSetCtx is keyed by session_id, not user_id — see
    //     recording-same-account.spec.ts for why);
    //   • the HOST sees its OWN 🔴 indicator in the peer list (self is the first
    //     entry) — driven by the local → Recording JS state callback inserting
    //     rec_client.get_own_session_id();
    //   • the GUEST sees the host's 🔴 indicator in the peer list too.
    // On stop, every indicator clears (Idle callback removes the local id; the
    // guest's set entry is removed by PEER_EVENT_RECORDING_STOPPED).
    //
    // Note: a user's OWN camera tile is filtered out of `#grid-container`
    // (display_peers excludes own_session), so the recorder's own tile is not
    // asserted here — the recorder's own indicator surfaces in the peer list,
    // which IS asserted. The remote-tile indicator is asserted from the peer's
    // view (the guest), which is where the recorder's tile actually renders.
    //
    // Mutation sensitivity: drop the `recording_peer_ids.write().insert(...)` in
    // the RECORDING_STARTED arm and the guest's tile/list icons never appear;
    // drop the local `LocalRecordingSetOp::Insert` branch and the host's own
    // peer-list icon never appears; drop the `.remove(...)` paths and the icons
    // never clear on stop.
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_rec_icon_${Date.now()}`;

    const browser1 = await chromium.launch({ args: BROWSER_ARGS });
    const browser2 = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const hostCtx = await createAuthenticatedContext(
        browser1,
        "host-rec-icon@videocall.rs",
        "RecIconHost",
        uiURL,
      );
      const guestCtx = await createAuthenticatedContext(
        browser2,
        "guest-rec-icon@videocall.rs",
        "RecIconGuest",
        uiURL,
      );

      const hostPage = await hostCtx.newPage();
      const guestPage = await guestCtx.newPage();

      // ── Join meeting ──────────────────────────────────────────────────
      await fillAndSubmitJoinForm(hostPage, meetingId, "RecIconHost");
      await hostPage.waitForTimeout(1500);
      expect(await joinMeetingFromPage(hostPage)).toBe("in-meeting");

      await fillAndSubmitJoinForm(guestPage, meetingId, "RecIconGuest");
      await guestPage.waitForTimeout(1500);
      const guestResult = await joinMeetingFromPage(guestPage);
      if (guestResult === "waiting") {
        const admitButton = hostPage.getByTitle("Admit").first();
        await expect(admitButton).toBeVisible({ timeout: 20_000 });
        await hostPage.waitForTimeout(500);
        await admitButton.dispatchEvent("click");
        await guestPage.locator("#grid-container").waitFor({ timeout: 20_000 });
      }

      await expect(hostPage.locator("#grid-container")).toBeVisible({ timeout: 10_000 });
      await expect(guestPage.locator("#grid-container")).toBeVisible({ timeout: 10_000 });

      // Wait until the host can see the guest's tile so the session is up.
      await expect(hostPage.locator("#grid-container .canvas-container").first()).toBeVisible({
        timeout: 30_000,
      });
      // And until the guest can see the host's tile (that's where the host's
      // recording indicator will render for the guest).
      await expect(guestPage.locator("#grid-container .canvas-container").first()).toBeVisible({
        timeout: 30_000,
      });

      // Open the peer-list sidebar on both pages so we can assert the list icon.
      await openPeerListSidebar(hostPage);
      await openPeerListSidebar(guestPage);

      // Scoped selectors: tile icons live under `#grid-container .floating-name`,
      // peer-list icons under `#peer-list-container .peer_item_name_container`.
      const guestSeesHostTileIcon = guestPage.locator(
        "#grid-container .floating-name .recording-indicator",
      );
      const hostSeesGuestTileIcon = hostPage.locator(
        "#grid-container .floating-name .recording-indicator",
      );
      const hostOwnListIcon = hostPage.locator(
        "#peer-list-container .peer_item_name_container .recording-indicator",
      );
      const guestSeesHostListIcon = guestPage.locator(
        "#peer-list-container .peer_item_name_container .recording-indicator",
      );

      // ── Before recording: no indicators anywhere ──────────────────────
      await expect(guestSeesHostTileIcon).toHaveCount(0);
      await expect(hostSeesGuestTileIcon).toHaveCount(0);
      await expect(hostOwnListIcon).toHaveCount(0);
      await expect(guestSeesHostListIcon).toHaveCount(0);

      // ── Host starts recording ─────────────────────────────────────────
      const recordBtn = hostPage.getByTestId("record-button");
      await expect(recordBtn).toBeVisible({ timeout: 10_000 });
      await recordBtn.click();

      // Recording must be live (Recording state) before the local insert fires.
      await expect(hostPage.locator(".recording-status-banner .toast-name")).toHaveText(
        /Recording/,
        { timeout: 15_000 },
      );

      // Guest sees the host's recording icon ON THE HOST'S TILE.
      await expect(guestSeesHostTileIcon).toHaveCount(1, { timeout: 20_000 });
      await expect(guestSeesHostTileIcon.first()).toBeVisible();

      // Host sees its OWN recording icon in the peer list (self = first entry).
      await expect(hostOwnListIcon).toHaveCount(1, { timeout: 15_000 });
      await expect(hostOwnListIcon.first()).toBeVisible();

      // Guest sees the host's recording icon in the peer list.
      await expect(guestSeesHostListIcon).toHaveCount(1, { timeout: 15_000 });
      await expect(guestSeesHostListIcon.first()).toBeVisible();

      // Negative: the host must NOT see a recording icon on the (non-recording)
      // guest's tile — the indicator is per-recorder, not a global banner.
      await expect(hostSeesGuestTileIcon).toHaveCount(0);

      // ── Host stops recording ──────────────────────────────────────────
      await hostPage.getByTestId("record-button").click();

      // Every indicator clears on both views.
      await expect(guestSeesHostTileIcon).toHaveCount(0, { timeout: 20_000 });
      await expect(hostOwnListIcon).toHaveCount(0, { timeout: 15_000 });
      await expect(guestSeesHostListIcon).toHaveCount(0, { timeout: 15_000 });
    } finally {
      await browser1.close();
      await browser2.close();
    }
  });

  test("two concurrent recorders have independent icons: stopping one leaves the other's", async ({
    baseURL,
  }) => {
    // The literal acceptance criterion: "Icon doesn't have dependencies between
    // each other." With `recording_allowed_for_all` enabled, BOTH the host and an
    // authenticated guest record at the same time; each sees the OTHER's tile
    // recording icon. When only the host stops, the guest's icon (as seen on the
    // host's page) must remain — the set is keyed by session_id, so removing the
    // host's entry cannot touch the guest's.
    //
    // Mutation sensitivity: make RECORDING_STOPPED clear the WHOLE set (instead
    // of removing a single key) and stopping the host would wrongly clear the
    // guest's still-live icon, failing the post-stop assertion.
    const uiURL = baseURL || "http://localhost:3001";
    const apiURL = process.env.API_BASE_URL || "http://localhost:8081";
    const meetingId = `e2e_rec_dual_${Date.now()}`;

    const browser1 = await chromium.launch({ args: BROWSER_ARGS });
    const browser2 = await chromium.launch({ args: BROWSER_ARGS });

    try {
      // Create the meeting with recording_allowed_for_all=true so BOTH
      // authenticated participants get a record button.
      const hostToken = generateSessionToken("host-rec-dual@videocall.rs", "RecDualHost");
      const createResp = await fetch(`${apiURL}/api/v1/meetings`, {
        method: "POST",
        headers: {
          "Content-Type": "application/json",
          Cookie: `${COOKIE_NAME}=${hostToken}`,
        },
        body: JSON.stringify({
          meeting_id: meetingId,
          attendees: [],
          recording_allowed_for_all: true,
        }),
      });
      if (!createResp.ok) {
        throw new Error(
          `POST /api/v1/meetings failed (${createResp.status}): ${await createResp.text()}`,
        );
      }

      const hostCtx = await createAuthenticatedContext(
        browser1,
        "host-rec-dual@videocall.rs",
        "RecDualHost",
        uiURL,
      );
      const guestCtx = await createAuthenticatedContext(
        browser2,
        "guest-rec-dual@videocall.rs",
        "RecDualGuest",
        uiURL,
      );

      const hostPage = await hostCtx.newPage();
      const guestPage = await guestCtx.newPage();

      // ── Join meeting ──────────────────────────────────────────────────
      await fillAndSubmitJoinForm(hostPage, meetingId, "RecDualHost");
      await hostPage.waitForTimeout(1500);
      expect(await joinMeetingFromPage(hostPage)).toBe("in-meeting");

      await fillAndSubmitJoinForm(guestPage, meetingId, "RecDualGuest");
      await guestPage.waitForTimeout(1500);
      const guestResult = await joinMeetingFromPage(guestPage);
      if (guestResult === "waiting") {
        const admitButton = hostPage.getByTitle("Admit").first();
        await expect(admitButton).toBeVisible({ timeout: 20_000 });
        await hostPage.waitForTimeout(500);
        await admitButton.dispatchEvent("click");
        await guestPage.locator("#grid-container").waitFor({ timeout: 20_000 });
      }

      await expect(hostPage.locator("#grid-container .canvas-container").first()).toBeVisible({
        timeout: 30_000,
      });
      await expect(guestPage.locator("#grid-container .canvas-container").first()).toBeVisible({
        timeout: 30_000,
      });

      const hostSeesGuestTileIcon = hostPage.locator(
        "#grid-container .floating-name .recording-indicator",
      );
      const guestSeesHostTileIcon = guestPage.locator(
        "#grid-container .floating-name .recording-indicator",
      );

      // ── Both start recording ──────────────────────────────────────────
      const hostRecordBtn = hostPage.getByTestId("record-button");
      await expect(hostRecordBtn).toBeVisible({ timeout: 15_000 });
      await hostRecordBtn.click();
      await expect(hostPage.locator(".recording-status-banner .toast-name")).toHaveText(
        /Recording/,
        { timeout: 15_000 },
      );

      const guestRecordBtn = guestPage.getByTestId("record-button");
      await expect(guestRecordBtn).toBeVisible({ timeout: 15_000 });
      await guestRecordBtn.click();
      await expect(guestPage.locator(".recording-status-banner .toast-name")).toHaveText(
        /Recording/,
        { timeout: 15_000 },
      );

      // Each peer sees the OTHER's recording icon on the other's tile.
      await expect(hostSeesGuestTileIcon).toHaveCount(1, { timeout: 20_000 });
      await expect(guestSeesHostTileIcon).toHaveCount(1, { timeout: 20_000 });

      // ── Stop ONLY the host ────────────────────────────────────────────
      await hostPage.getByTestId("record-button").click();

      // The guest's icon (seen on the host's page) must PERSIST — independence.
      // We first confirm the host's own recording banner cleared (host stop
      // completed), then assert the guest's tile icon is still there.
      await expect(hostPage.locator(".recording-status-banner")).not.toBeVisible({
        timeout: 15_000,
      });
      await expect(hostSeesGuestTileIcon).toHaveCount(1);
      await expect(hostSeesGuestTileIcon.first()).toBeVisible();

      // ── Now stop the guest too — its icon clears everywhere ───────────
      await guestPage.getByTestId("record-button").click();
      await expect(hostSeesGuestTileIcon).toHaveCount(0, { timeout: 20_000 });
    } finally {
      await browser1.close();
      await browser2.close();
    }
  });

  test("meeting-wide recording status bar persists until the last recorder stops and does not auto-fade", async ({
    baseURL,
  }) => {
    // The literal acceptance criteria for the persistent, meeting-wide
    // `.meeting-status-bar` (Google-Meet style):
    //   (a) it appears when the FIRST recorder starts, visible to a non-recording
    //       peer AND to the recorder themselves;
    //   (b) a SECOND recorder starting while the first is still active causes no
    //       change (bar stays present);
    //   (c) stopping the second (non-last) recorder leaves the bar showing
    //       (the first recorder is still active);
    //   (d) stopping the LAST active recorder makes the bar disappear.
    // Plus a regression guard for the CSS auto-fade bug: the old banner shared
    // `.peer-toast`, whose `toast-exit ... 7.5s forwards` animation drove opacity
    // to 0 after ~8s even while recording continued. We wait past that window and
    // assert the bar's computed opacity is still ~1.
    //
    // Uses the same `recording_allowed_for_all=true` dual-recorder harness as the
    // "two concurrent recorders" test so BOTH host and guest get a record button.
    //
    // Mutation sensitivity:
    //   • Replace the `any_session_recording` set-emptiness aggregate with the old
    //     `Option<String>` "last-writer" banner and step (c) fails: stopping the
    //     second recorder would wrongly clear the indicator while the first still
    //     records.
    //   • Re-add the `.peer-toast` class to `.meeting-status-bar` and the
    //     opacity-after-9s assertion fails (fades to 0).
    const uiURL = baseURL || "http://localhost:3001";
    const apiURL = process.env.API_BASE_URL || "http://localhost:8081";
    const meetingId = `e2e_rec_aggregate_${Date.now()}`;

    const browser1 = await chromium.launch({ args: BROWSER_ARGS });
    const browser2 = await chromium.launch({ args: BROWSER_ARGS });

    try {
      // recording_allowed_for_all=true so BOTH participants get a record button.
      const hostToken = generateSessionToken("host-rec-agg@videocall.rs", "RecAggHost");
      const createResp = await fetch(`${apiURL}/api/v1/meetings`, {
        method: "POST",
        headers: {
          "Content-Type": "application/json",
          Cookie: `${COOKIE_NAME}=${hostToken}`,
        },
        body: JSON.stringify({
          meeting_id: meetingId,
          attendees: [],
          recording_allowed_for_all: true,
        }),
      });
      if (!createResp.ok) {
        throw new Error(
          `POST /api/v1/meetings failed (${createResp.status}): ${await createResp.text()}`,
        );
      }

      const hostCtx = await createAuthenticatedContext(
        browser1,
        "host-rec-agg@videocall.rs",
        "RecAggHost",
        uiURL,
      );
      const guestCtx = await createAuthenticatedContext(
        browser2,
        "guest-rec-agg@videocall.rs",
        "RecAggGuest",
        uiURL,
      );

      const hostPage = await hostCtx.newPage();
      const guestPage = await guestCtx.newPage();

      // ── Join meeting ──────────────────────────────────────────────────
      await fillAndSubmitJoinForm(hostPage, meetingId, "RecAggHost");
      await hostPage.waitForTimeout(1500);
      expect(await joinMeetingFromPage(hostPage)).toBe("in-meeting");

      await fillAndSubmitJoinForm(guestPage, meetingId, "RecAggGuest");
      await guestPage.waitForTimeout(1500);
      const guestResult = await joinMeetingFromPage(guestPage);
      if (guestResult === "waiting") {
        const admitButton = hostPage.getByTitle("Admit").first();
        await expect(admitButton).toBeVisible({ timeout: 20_000 });
        await hostPage.waitForTimeout(500);
        await admitButton.dispatchEvent("click");
        await guestPage.locator("#grid-container").waitFor({ timeout: 20_000 });
      }

      await expect(hostPage.locator("#grid-container .canvas-container").first()).toBeVisible({
        timeout: 30_000,
      });
      await expect(guestPage.locator("#grid-container .canvas-container").first()).toBeVisible({
        timeout: 30_000,
      });

      const hostIndicator = hostPage.locator(".meeting-status-bar");
      const guestIndicator = guestPage.locator(".meeting-status-bar");

      // ── Before recording: no indicator anywhere ───────────────────────
      await expect(hostIndicator).toHaveCount(0);
      await expect(guestIndicator).toHaveCount(0);

      // ── (a) First recorder (host) starts ──────────────────────────────
      const hostRecordBtn = hostPage.getByTestId("record-button");
      await expect(hostRecordBtn).toBeVisible({ timeout: 15_000 });
      await hostRecordBtn.click();
      await expect(hostPage.locator(".recording-status-banner .toast-name")).toHaveText(
        /Recording/,
        { timeout: 15_000 },
      );

      // Visible to the non-recording guest AND to the recorder (host) itself.
      await expect(guestIndicator).toBeVisible({ timeout: 20_000 });
      await expect(hostIndicator).toBeVisible({ timeout: 15_000 });

      // ── Non-fade regression: still fully opaque past the 7.9s window ──
      // `toBeVisible` alone would NOT catch this (Playwright treats opacity:0 as
      // visible), so assert the computed opacity directly.
      await guestPage.waitForTimeout(9000);
      const guestOpacity = await guestPage.evaluate(() => {
        const el = document.querySelector(".meeting-status-bar");
        return el ? parseFloat(getComputedStyle(el).opacity) : -1;
      });
      expect(guestOpacity).toBeGreaterThan(0.5);
      await expect(guestIndicator).toBeVisible();
      await expect(hostIndicator).toBeVisible();

      // ── (b) Second recorder (guest) starts — no change to the indicator ─
      const guestRecordBtn = guestPage.getByTestId("record-button");
      await expect(guestRecordBtn).toBeVisible({ timeout: 15_000 });
      await guestRecordBtn.click();
      await expect(guestPage.locator(".recording-status-banner .toast-name")).toHaveText(
        /Recording/,
        { timeout: 15_000 },
      );
      // Indicator remains present on both — the second start doesn't re-trigger it.
      await expect(hostIndicator).toHaveCount(1);
      await expect(hostIndicator).toBeVisible();
      await expect(guestIndicator).toHaveCount(1);
      await expect(guestIndicator).toBeVisible();

      // ── (c) Stop the host (a NON-last recorder — guest still records) ──
      await hostPage.getByTestId("record-button").click();
      // Host's own detailed status banner clears (host stop completed)…
      await expect(hostPage.locator(".recording-status-banner")).not.toBeVisible({
        timeout: 15_000,
      });
      // …but the meeting-wide indicator MUST remain, because the guest is still
      // recording. This is the exact case the old Option<String> banner got wrong.
      await expect(hostIndicator).toBeVisible();
      await expect(guestIndicator).toBeVisible();

      // ── (d) Stop the guest (the LAST recorder) — indicator disappears ──
      await guestPage.getByTestId("record-button").click();
      await expect(hostIndicator).toHaveCount(0, { timeout: 20_000 });
      await expect(guestIndicator).toHaveCount(0, { timeout: 20_000 });
    } finally {
      await browser1.close();
      await browser2.close();
    }
  });
});
