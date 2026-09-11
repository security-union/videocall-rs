import { test, expect, chromium, Locator, Page } from "@playwright/test";
import { createAuthenticatedContext, BROWSER_ARGS } from "../helpers/auth-context";
import { wakeControls } from "../helpers/controls";
import { openPerformancePanel, readNetEqPacketsPerSec } from "../helpers/diagnostics-drawer";
import { enterTwoUserMeeting } from "../helpers/two-user-meeting";
import { waitForServices } from "../helpers/wait-for-services";

/**
 * E2E: the receiver's NetEq actually consumes the publisher's audio (issue 2622).
 * `packets_per_sec` is incremented only in `NetEq::insert_packet`, so no
 * heartbeat can move it. The three phases must stay ONE test: A and C both
 * assert 0 and either alone passes on a build where audio never works.
 */

const PHASE_POLL_INTERVALS = [1000, 2000, 3000];

/** One reading per second, all of which must be 0 while the publisher is muted. */
const MUTED_WINDOW_SAMPLES = 15;

/** Unlike `helpers/controls.ts::enableCamera`, waits for capture to start. */
async function startCameraAndConfirm(page: Page): Promise<void> {
  await wakeControls(page);
  const startBtn = page.locator("button.video-control-button", {
    has: page.locator("span.tooltip", { hasText: "Start Video" }),
  });
  await expect(startBtn).toBeVisible({ timeout: 15_000 });
  await startBtn.click();
  await expect(
    page.locator("button.video-control-button", {
      has: page.locator("span.tooltip", { hasText: "Stop Video" }),
    }),
  ).toBeVisible({ timeout: 30_000 });
}

function micToggle(page: Page): Locator {
  return page.locator('[data-testid="mic-toggle-button"]');
}

/**
 * Positive on both of `MicButton`'s outputs (`video_control_buttons.rs`), so an
 * absent button or one whose device is unavailable fails — `not.toHaveClass`
 * passes on both, which is what would make the muted window vacuous.
 */
async function expectMicState(toggle: Locator, on: boolean): Promise<void> {
  await expect(toggle).toHaveClass(on ? /\bactive\b/ : /\boff\b/, { timeout: 15_000 });
  await expect(toggle).toHaveAttribute(
    "aria-label",
    on ? "Microphone — Mute" : "Microphone — Unmute",
    { timeout: 15_000 },
  );
}

async function setMic(page: Page, on: boolean): Promise<void> {
  await wakeControls(page);
  const toggle = micToggle(page);
  await expect(toggle).toBeVisible({ timeout: 15_000 });
  const isOn = ((await toggle.getAttribute("class")) || "").includes("active");
  if (isOn !== on) {
    await toggle.click();
  }
  await expectMicState(toggle, on);
}

test.describe("#2622 receiver NetEq audio liveness", () => {
  test.describe.configure({ mode: "serial", retries: 1 });

  test.beforeAll(async () => {
    await waitForServices();
  });

  test("NetEq packets/s is 0 muted, rises when the publisher unmutes, and returns to 0 @bvt1", async ({
    baseURL,
  }) => {
    test.setTimeout(300_000);
    const uiURL = baseURL || "http://localhost:3001";
    const meetingId = `e2e_audio_liveness_${Date.now()}`;

    const pubBrowser = await chromium.launch({ args: BROWSER_ARGS });
    const rxBrowser = await chromium.launch({ args: BROWSER_ARGS });

    try {
      const pubCtx = await createAuthenticatedContext(
        pubBrowser,
        "audio-pub@videocall.rs",
        "HostUser",
        uiURL,
      );
      const rxCtx = await createAuthenticatedContext(
        rxBrowser,
        "audio-rx@videocall.rs",
        "GuestUser",
        uiURL,
      );

      const pubPage = await pubCtx.newPage();
      const rxPage = await rxCtx.newPage();

      const t0 = Date.now();
      const mark = (label: string) =>
        console.log(`[2622] ${label} at +${Math.round((Date.now() - t0) / 1000)}s`);

      await enterTwoUserMeeting(pubPage, rxPage, meetingId);
      mark("both in meeting");

      await startCameraAndConfirm(pubPage);
      await setMic(pubPage, false);
      mark("publisher camera ON, mic OFF");

      const drawer = await openPerformancePanel(rxPage);
      mark("receiver diagnostics drawer open");

      // A WINDOW, not `expect.poll(...).toBe(0)`: poll succeeds on its first
      // reading, which is 0 at drawer-open even with the publisher UNMUTED.
      await expect
        .poll(async () => Number.isFinite(await readNetEqPacketsPerSec(drawer)), {
          timeout: 45_000,
          intervals: PHASE_POLL_INTERVALS,
          message:
            "issue 2622 phase A1: the receiver must have a NetEq sample for the " +
            "publisher — i.e. a single peer is selected and its row renders; the row " +
            "reads '--' (NaN) until then. This does NOT depend on audio: with the mic " +
            "off the receiver's Peer (and its eager NetEq worker) is created from the " +
            "VIDEO stream, so a camera or peer-selection regression fails here too.",
        })
        .toBe(true);

      const mutedReadings: number[] = [];
      for (let i = 0; i < MUTED_WINDOW_SAMPLES; i++) {
        mutedReadings.push(await readNetEqPacketsPerSec(drawer));
        await rxPage.waitForTimeout(1000);
      }
      expect(
        mutedReadings.filter((v) => v !== 0),
        "issue 2622 phase A2: every reading over the muted window must be exactly 0. " +
          "A non-zero entry means a muted publisher is still sending audio packets; " +
          "NaN means the sample vanished mid-window. " +
          `readings=[${mutedReadings.join(", ")}]`,
      ).toEqual([]);
      mark("phase A green");

      await setMic(pubPage, true);

      await expect
        .poll(async () => readNetEqPacketsPerSec(drawer), {
          timeout: 45_000,
          intervals: PHASE_POLL_INTERVALS,
          message:
            "issue 2622 phase B: after the publisher unmutes, the receiver's NetEq " +
            "packets/s must rise above 0. This is the whole audio path — publish, " +
            "relay forward, Peer::decode audio arm, WorkerMsg::Insert, insert_packet. " +
            "No threshold above 0 is asserted: pinning a rate would break on a " +
            "bitrate or frame-duration change for reasons unrelated to liveness.",
        })
        .toBeGreaterThan(0);
      mark("phase B green");

      await setMic(pubPage, false);

      await expect
        .poll(async () => readNetEqPacketsPerSec(drawer), {
          timeout: 45_000,
          intervals: PHASE_POLL_INTERVALS,
          message:
            "issue 2622 phase C: after the publisher re-mutes, packets/s must return " +
            "to 0 rather than latching at its last non-zero value. Two paths zero it " +
            "— the get_audio roll and the audio-off flush()->reset() — so this fails " +
            "only if BOTH stop running.",
        })
        .toBe(0);
      mark("phase C green");
    } finally {
      await pubBrowser.close();
      await rxBrowser.close();
    }
  });
});
