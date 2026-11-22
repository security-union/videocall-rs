import { test, expect, chromium, Browser, Page } from "@playwright/test";
import { injectSessionCookie } from "../helpers/auth";
import { waitForServices } from "../helpers/wait-for-services";
import {
  BROWSER_ARGS,
  controlButton,
  createAuthenticatedContext,
  joinMeetingFromPage,
  navigateToMeeting,
  startTwoPartyMeeting,
  unmute,
} from "../helpers/meeting";
import { peerCanvasSizes, recordPeerCanvasSizes, useFakeCamera } from "../helpers/fake-camera";
import { firstAudibleSince, installNetEqProbe } from "../helpers/neteq-probe";
import {
  DELAY_PRESETS_MS,
  RATE_PRESETS_KBPS,
  REVEAL_TAPS,
  REVEAL_WINDOW_MS,
  SCOPES,
  STEP_TEST_DURATION_MS,
  closeDiagnostics,
  delayButton,
  diagnosticsSidebar,
  expectOnlyPressed,
  openDiagnostics,
  rateButton,
  revealTrafficShaping,
  scopeButton,
  shapingBadge,
  shapingOff,
  shapingReadout,
  shapingSection,
  shapingToggle,
  stepRemaining,
  stepRemainingSeconds,
  stepTestButton,
  tapTitle,
  turnShapingOff,
  turnShapingOn,
} from "../helpers/traffic-shaping";

// Traffic shaping is a hidden debug section in the Diagnostics panel: seven
// taps on the "Call Diagnostics" title within 3 s reveal it. It delays the
// sender's own outgoing media through a simulated narrow uplink. Every join
// starts hidden and off; nothing is stored.
//
// Transport: the native e2e stack (nix/dev-stack.nix `e2eStack`) runs only the
// WebSocket server and writes `webTransportEnabled: false` into config.js, so
// these tests cover the WebSocket path only.

// Long enough for a render triggered by the last tap to have landed, so a
// "still hidden" check after it means something.
const SETTLE_MS = 500;

// Defaults from `TrafficShapingSettings::default()` in
// videocall-client/src/traffic_shaping/shaper.rs.
const DEFAULT_RATE_KBPS = 1000;
const DEFAULT_DELAY_MS = 0;
const DEFAULT_SCOPE = "both";

async function expectHiddenAndOff(page: Page): Promise<void> {
  await expect(shapingBadge(page)).toHaveCount(0);
  await openDiagnostics(page);
  // Give a stale revealed flag time to render before checking it is absent.
  await page.waitForTimeout(SETTLE_MS);
  await expect(shapingSection(page)).toHaveCount(0);
}

async function expectFreshDefaults(page: Page): Promise<void> {
  await expect(shapingToggle(page)).toHaveAttribute("aria-checked", "false");
  await expect(shapingOff(page)).toBeVisible();
  await expect(shapingBadge(page)).toHaveCount(0);
  await expectOnlyPressed(page, "rate", RATE_PRESETS_KBPS, DEFAULT_RATE_KBPS);
  await expectOnlyPressed(page, "delay", DELAY_PRESETS_MS, DEFAULT_DELAY_MS);
  await expectOnlyPressed(page, "scope", SCOPES, DEFAULT_SCOPE);
}

// The user's own picks, made before starting a step test.
async function pickUserSettings(page: Page): Promise<void> {
  await rateButton(page, 2000).click();
  await delayButton(page, 150).click();
  await scopeButton(page, "video").click();
  await expectUserPicks(page);
}

async function expectUserPicks(page: Page): Promise<void> {
  await expectOnlyPressed(page, "rate", RATE_PRESETS_KBPS, 2000);
  await expectOnlyPressed(page, "delay", DELAY_PRESETS_MS, 150);
  await expectOnlyPressed(page, "scope", SCOPES, "video");
}

async function expectPresetsDisabled(page: Page, disabled: boolean): Promise<void> {
  const buttons = [
    ...RATE_PRESETS_KBPS.map((rate) => rateButton(page, rate)),
    ...DELAY_PRESETS_MS.map((delay) => delayButton(page, delay)),
    ...SCOPES.map((scope) => scopeButton(page, scope)),
  ];
  for (const button of buttons) {
    if (disabled) {
      await expect(button).toBeDisabled();
    } else {
      await expect(button).toBeEnabled();
    }
  }
}

test.describe("Traffic shaping (hidden diagnostics section)", () => {
  let meetingId: string;

  test.beforeAll(async () => {
    await waitForServices();
  });

  test.beforeEach(async ({ context, page, baseURL }, testInfo) => {
    await injectSessionCookie(context, { baseURL });
    meetingId = `e2e_shaping_${testInfo.workerIndex}_${Date.now()}`;
    await navigateToMeeting(page, meetingId, "shaping-user");
    expect(await joinMeetingFromPage(page)).toBe("in-meeting");
    await expect(controlButton(page, "Open Diagnostics")).toBeVisible();
  });

  test("hidden by default; six quick taps do not reveal it and the seventh does", async ({
    page,
  }) => {
    await expectHiddenAndOff(page);

    await tapTitle(page, REVEAL_TAPS - 1);
    await page.waitForTimeout(SETTLE_MS);
    await expect(shapingSection(page)).toHaveCount(0);

    // Still inside the 3 s window, so one more tap completes the gesture.
    await tapTitle(page, 1);
    await expect(shapingSection(page)).toBeVisible();
    await expect(
      shapingSection(page).getByRole("heading", { name: "Traffic shaping" }),
    ).toBeVisible();
    await expectFreshDefaults(page);
  });

  test("seven taps spread over more than 3 seconds do not reveal it", async ({ page }) => {
    await openDiagnostics(page);

    // Seven taps 600 ms apart span 3.6 s: at most five ever share a window.
    const gapMs = Math.ceil((REVEAL_WINDOW_MS * 1.2) / (REVEAL_TAPS - 1));
    await tapTitle(page, REVEAL_TAPS, gapMs);
    await page.waitForTimeout(SETTLE_MS);
    await expect(shapingSection(page)).toHaveCount(0);

    // Wait out the window so the slow taps have all expired, then a quick
    // burst still works.
    await page.waitForTimeout(REVEAL_WINDOW_MS);
    await tapTitle(page, REVEAL_TAPS);
    await expect(shapingSection(page)).toBeVisible();
  });

  test("the toggle shows a badge that outlives the closed panel and goes away when off", async ({
    page,
  }) => {
    await revealTrafficShaping(page);
    await expect(shapingToggle(page)).toHaveAttribute("role", "switch");
    await expect(shapingOff(page)).toBeVisible();
    await expect(shapingBadge(page)).toHaveCount(0);

    await turnShapingOn(page);
    await expect(shapingBadge(page)).toHaveText("Traffic shaping on");
    // The readout switches from "off" to the live queue numbers.
    await expect(shapingOff(page)).toHaveCount(0);
    const readout = shapingReadout(page);
    await expect(readout.getByTestId("traffic-shaping-queue-bytes")).toBeVisible();
    await expect(readout.getByTestId("traffic-shaping-queue-ms")).toHaveText(/^-?\d+ ms$/);
    await expect(readout.getByTestId("traffic-shaping-dropped")).toHaveText(/^\d+$/);
    // A plain toggle is not a step test.
    await expect(stepRemaining(page)).toHaveCount(0);

    // The badge stays while the panel is closed.
    await closeDiagnostics(page);
    await expect(shapingBadge(page)).toBeVisible();

    // Reopening the panel shows the section again without new taps, still on.
    await openDiagnostics(page);
    await expect(shapingSection(page)).toBeVisible();
    await expect(shapingToggle(page)).toHaveAttribute("aria-checked", "true");

    await turnShapingOff(page);
    await expect(shapingOff(page)).toBeVisible();
    await expect(shapingOff(page)).toHaveText("Shaping is off.");
    await expect(readout.getByTestId("traffic-shaping-queue-bytes")).toHaveCount(0);

    // Still revealed for this join after closing and reopening, and still off.
    await closeDiagnostics(page);
    await expect(shapingBadge(page)).toHaveCount(0);
    await openDiagnostics(page);
    await expect(shapingSection(page)).toBeVisible();
    await expect(shapingToggle(page)).toHaveAttribute("aria-checked", "false");
  });

  test("rate, delay and scope buttons are each an exclusive choice within their group", async ({
    page,
  }) => {
    await revealTrafficShaping(page);
    await expectFreshDefaults(page);

    for (const rate of RATE_PRESETS_KBPS) {
      await rateButton(page, rate).click();
      await expectOnlyPressed(page, "rate", RATE_PRESETS_KBPS, rate);
    }
    // Changing the rate left the other groups alone.
    await expectOnlyPressed(page, "delay", DELAY_PRESETS_MS, DEFAULT_DELAY_MS);
    await expectOnlyPressed(page, "scope", SCOPES, DEFAULT_SCOPE);

    for (const delay of DELAY_PRESETS_MS) {
      await delayButton(page, delay).click();
      await expectOnlyPressed(page, "delay", DELAY_PRESETS_MS, delay);
    }
    for (const scope of SCOPES) {
      await scopeButton(page, scope).click();
      await expectOnlyPressed(page, "scope", SCOPES, scope);
    }

    // Choices also apply while shaping is on.
    await rateButton(page, 500).click();
    await delayButton(page, 50).click();
    await scopeButton(page, "video").click();
    await turnShapingOn(page);
    await rateButton(page, 2000).click();
    await expectOnlyPressed(page, "rate", RATE_PRESETS_KBPS, 2000);
    await expectOnlyPressed(page, "delay", DELAY_PRESETS_MS, 50);
    await expectOnlyPressed(page, "scope", SCOPES, "video");
    await expect(shapingToggle(page)).toHaveAttribute("aria-checked", "true");

    // The choices are kept for this join when the panel closes and reopens.
    await closeDiagnostics(page);
    await openDiagnostics(page);
    await expectOnlyPressed(page, "rate", RATE_PRESETS_KBPS, 2000);
    await expectOnlyPressed(page, "delay", DELAY_PRESETS_MS, 50);
    await expectOnlyPressed(page, "scope", SCOPES, "video");
  });

  test("nothing survives a reload: hidden, off and back to defaults", async ({ page }) => {
    await revealTrafficShaping(page);
    await rateButton(page, 250).click();
    await delayButton(page, 150).click();
    await scopeButton(page, "audio").click();
    await turnShapingOn(page);

    await page.reload();
    expect(await joinMeetingFromPage(page)).toBe("in-meeting");

    await expectHiddenAndOff(page);
    await tapTitle(page, REVEAL_TAPS);
    await expect(shapingSection(page)).toBeVisible();
    await expectFreshDefaults(page);
  });

  test("nothing survives hanging up and joining again", async ({ page }) => {
    await revealTrafficShaping(page);
    await rateButton(page, 250).click();
    await turnShapingOn(page);
    await closeDiagnostics(page);
    await expect(shapingBadge(page)).toBeVisible();

    await controlButton(page, "Hang Up").click();
    await expect(page).not.toHaveURL(new RegExp(`/meeting/${meetingId}`), { timeout: 15_000 });
    await expect(shapingBadge(page)).toHaveCount(0);

    await navigateToMeeting(page, meetingId, "shaping-user");
    expect(await joinMeetingFromPage(page)).toBe("in-meeting");

    await expectHiddenAndOff(page);
    await tapTitle(page, REVEAL_TAPS);
    await expect(shapingSection(page)).toBeVisible();
    await expectFreshDefaults(page);
  });

  test("the step test shapes at 500 kbps, counts down, turns itself off and keeps the user's picks", async ({
    page,
  }) => {
    test.setTimeout(STEP_TEST_DURATION_MS + 60_000);
    await revealTrafficShaping(page);
    await expect(shapingBadge(page)).toHaveCount(0);

    // The user's own picks, which the step test must not overwrite.
    await pickUserSettings(page);

    await stepTestButton(page).click();

    // During the step the presets show the live step settings and are locked.
    await expect(shapingBadge(page)).toBeVisible();
    await expect(shapingToggle(page)).toHaveAttribute("aria-checked", "true");
    await expectOnlyPressed(page, "rate", RATE_PRESETS_KBPS, 500);
    await expectOnlyPressed(page, "delay", DELAY_PRESETS_MS, 0);
    await expectOnlyPressed(page, "scope", SCOPES, "both");
    await expectPresetsDisabled(page, true);
    await expect(stepRemaining(page)).toHaveText(/^Step test ends in \d+ s$/);

    const first = await stepRemainingSeconds(page);
    expect(first).toBeGreaterThan(0);
    expect(first).toBeLessThanOrEqual(STEP_TEST_DURATION_MS / 1000);
    // The readout refreshes every 500 ms; it must tick down.
    await expect.poll(() => stepRemainingSeconds(page), { timeout: 5_000 }).toBeLessThan(first);

    // It ends on its own even with the panel closed: the badge goes away.
    await closeDiagnostics(page);
    await expect(shapingBadge(page)).toBeVisible();
    await expect(shapingBadge(page)).toHaveCount(0, { timeout: STEP_TEST_DURATION_MS + 5_000 });

    await openDiagnostics(page);
    await expect(shapingSection(page)).toBeVisible();
    await expect(shapingToggle(page)).toHaveAttribute("aria-checked", "false");
    await expect(shapingOff(page)).toBeVisible();
    await expect(stepRemaining(page)).toHaveCount(0);
    // Back to the user's own picks, unlocked.
    await expectUserPicks(page);
    await expectPresetsDisabled(page, false);
  });

  test("toggling the switch during a step test cancels it and restores the user's picks", async ({
    page,
  }) => {
    await revealTrafficShaping(page);
    await pickUserSettings(page);
    await stepTestButton(page).click();
    await expect(stepRemaining(page)).toBeVisible();
    await expectPresetsDisabled(page, true);

    // The switch is on during the step; flipping it ends the step and turns
    // shaping off, well before the 15 s are up.
    await turnShapingOff(page);
    await expect(stepRemaining(page)).toHaveCount(0);
    await expect(shapingOff(page)).toBeVisible();
    await expectUserPicks(page);
    await expectPresetsDisabled(page, false);

    // Turning it back on uses the user's picks, not the step's.
    await turnShapingOn(page);
    await expect(stepRemaining(page)).toHaveCount(0);
    await expectUserPicks(page);
    await expectPresetsDisabled(page, false);
  });

  test("hanging up during a step test and rejoining shows no badge and shaping off", async ({
    page,
  }) => {
    await revealTrafficShaping(page);
    await stepTestButton(page).click();
    await expect(stepRemaining(page)).toBeVisible();
    await expect(shapingBadge(page)).toBeVisible();

    // The open sidebar covers the control bar at this viewport.
    await closeDiagnostics(page);
    await expect(shapingBadge(page)).toBeVisible();
    await controlButton(page, "Hang Up").click();
    await expect(page).not.toHaveURL(new RegExp(`/meeting/${meetingId}`), { timeout: 15_000 });
    await expect(shapingBadge(page)).toHaveCount(0);

    // Rejoin well inside the 15 s the step would have run.
    await navigateToMeeting(page, meetingId, "shaping-user");
    expect(await joinMeetingFromPage(page)).toBe("in-meeting");
    await expectHiddenAndOff(page);
    await tapTitle(page, REVEAL_TAPS);
    await expect(shapingSection(page)).toBeVisible();
    await expect(stepRemaining(page)).toHaveCount(0);
    await expectFreshDefaults(page);
    await expectPresetsDisabled(page, false);
  });
});

// Two participants. The sender (A) shapes their own video to 250 kbps and the
// viewer (B) keeps seeing them in the call. What the UI exposes reliably is
// presence (A's tile on B), decoded video (B's canvas for A gets a size from
// a decoded frame) and decoded audio (the NetEq probe). There is no reliable
// observable for "A's video arrives late" on B's page: the e2e config caps
// video at 100 kbps, under the 250 kbps link, so the shaper's queue stays
// near empty and adds no measurable delay. This test does not assert latency.

const SENDER = "ShapingSender";
const VIEWER = "ShapingViewer";

// How long shaping stays on before checking the call is still up. Fixed on
// purpose: the claim is "survives a while under shaping", not one state.
const SHAPED_OBSERVE_MS = 10_000;

interface Call {
  senderBrowser: Browser;
  viewerBrowser: Browser;
  sender: Page;
  viewer: Page;
}

async function startCall(uiURL: string): Promise<Call> {
  const meetingId = `e2e_shaping_pair_${Date.now()}`;
  const senderBrowser = await chromium.launch({ args: BROWSER_ARGS });
  const viewerBrowser = await chromium.launch({ args: BROWSER_ARGS });
  const senderCtx = await createAuthenticatedContext(
    senderBrowser,
    "shaping-sender@videocall.rs",
    SENDER,
    uiURL,
  );
  // A canvas camera, as in video-aspect-ratio.spec.ts: the viewer's canvas
  // size recorder is proven against it.
  await useFakeCamera(senderCtx, { width: 640, height: 480 });
  const viewerCtx = await createAuthenticatedContext(
    viewerBrowser,
    "shaping-viewer@videocall.rs",
    VIEWER,
    uiURL,
  );
  await installNetEqProbe(viewerCtx);
  const sender = await senderCtx.newPage();
  const viewer = await viewerCtx.newPage();
  // The viewer hosts and the sender joins as the guest, as in
  // video-aspect-ratio.spec.ts (the other way round, the guest sometimes
  // never gets the host's video canvas).
  await startTwoPartyMeeting(viewer, VIEWER, sender, SENDER, meetingId);
  await expect(senderTile(viewer)).toBeVisible({ timeout: 30_000 });
  return { senderBrowser, viewerBrowser, sender, viewer };
}

async function endCall(call: Call | undefined): Promise<void> {
  await call?.senderBrowser.close();
  await call?.viewerBrowser.close();
}

function senderTile(viewer: Page) {
  return viewer.locator("#grid-container .floating-name", { hasText: SENDER }).first();
}

async function expectAudibleSince(viewer: Page, since: number): Promise<void> {
  await expect
    .poll(() => firstAudibleSince(viewer, since), {
      timeout: 20_000,
      message: "the viewer never played audio from the sender",
    })
    .not.toBeNull();
}

test.describe("Traffic shaping with a second participant (WebSocket)", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test("shaping the sender's video at 250 kbps keeps the call up, then turns off", async ({
    baseURL,
  }) => {
    test.setTimeout(180_000);
    let call: Call | undefined;
    try {
      call = await startCall(baseURL || "http://localhost:3001");
      const { sender, viewer } = call;

      // Media flowing before shaping: decoded video and audio from A on B.
      await recordPeerCanvasSizes(viewer, SENDER);
      await controlButton(sender, "Start Video").click();
      await expect(controlButton(sender, "Stop Video")).toBeVisible({ timeout: 10_000 });
      await unmute(sender);
      await expect
        .poll(async () => (await peerCanvasSizes(viewer)).length, {
          timeout: 30_000,
          message: "the viewer never decoded a video frame from the sender",
        })
        .toBeGreaterThan(0);
      await expectAudibleSince(viewer, Date.now());

      // A shapes their video only, at the narrowest preset.
      await revealTrafficShaping(sender);
      await rateButton(sender, 250).click();
      await scopeButton(sender, "video").click();
      await turnShapingOn(sender);
      await expectOnlyPressed(sender, "rate", RATE_PRESETS_KBPS, 250);
      await expectOnlyPressed(sender, "scope", SCOPES, "video");
      await expect(shapingReadout(sender).getByTestId("traffic-shaping-queue-bytes")).toBeVisible();
      // The badge is the sender's alone.
      await expect(shapingBadge(viewer)).toHaveCount(0);

      const shapedAt = Date.now();
      await sender.waitForTimeout(SHAPED_OBSERVE_MS);

      // The call survived: both are still in it, B still shows A, and A's
      // unshaped audio still plays on B.
      await expect(sender.locator("#grid-container")).toBeVisible();
      await expect(viewer.locator("#grid-container")).toBeVisible();
      await expect(senderTile(viewer)).toBeVisible();
      await expect(controlButton(sender, "Stop Video")).toBeVisible();
      await expect(shapingBadge(sender)).toBeVisible();
      await expectAudibleSince(viewer, shapedAt);

      await turnShapingOff(sender);
      await expect(shapingOff(sender)).toBeVisible();
      const unshapedAt = Date.now();
      await expect(senderTile(viewer)).toBeVisible();
      await expectAudibleSince(viewer, unshapedAt);
      await expect(diagnosticsSidebar(viewer)).toHaveCount(0);
    } finally {
      await endCall(call);
    }
  });
});
