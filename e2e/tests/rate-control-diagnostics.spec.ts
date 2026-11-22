import { test, expect, chromium, Browser, Locator, Page } from "@playwright/test";
import { waitForServices } from "../helpers/wait-for-services";
import {
  BROWSER_ARGS,
  controlButton,
  createAuthenticatedContext,
  mute,
  startTwoPartyMeeting,
  unmute,
} from "../helpers/meeting";
import { useFakeCamera } from "../helpers/fake-camera";
import { REVEAL_TAPS, openDiagnostics, shapingSection, tapTitle } from "../helpers/traffic-shaping";

// The camera's rate controller adapts to what viewers report. The viewer's
// Diagnostics shows one Reception Stats row per remote peer and media type,
// with the capacity and backlog it reports back on VIDEO rows; the sender's
// Diagnostics shows its Encoder Settings, including the combined capacity
// its viewers report.
//
// Transport: the native e2e stack runs only the WebSocket server, so this
// covers the WebSocket path.
//
// Not tested here: the camera adapting to a constrained link. The e2e
// config caps video at 100 kbps, under every traffic shaping preset. With
// the cap raised per test (helpers/app-config.ts, cap 1500) the camera
// holds 1500 kbps on 720p30 with capacity "not limited", but the canvas
// camera's simple picture only encodes to about 20-50 kbps, so a 250 kbps
// shaped link never queues and nothing adapts. It needs a camera whose
// picture actually costs around 1 Mbps.

const SENDER = "RateSender";
const VIEWER = "RateViewer";

// Rows go stale 10 s after the last media (`RECEPTION_STALE_MS` in
// dioxus-ui/src/components/reception_stats.rs) and the panel republishes
// every second; the extra is for a slow CI machine.
const ROW_GONE_TIMEOUT_MS = 25_000;

// The Encoders card's video rows, present only while the camera is on.
const VIDEO_ROW_IDS = ["encoders-video-bitrate", "encoders-video-target", "encoders-video-tier"];

// The Opus encoder's fixed bitrate (`AUDIO_QUALITY` in videocall-client).
const AUDIO_BITRATE = "50 kbps";

// The cell text for "nothing to show" (an en dash).
const EN_DASH = "\u2013";

interface Call {
  senderBrowser: Browser;
  viewerBrowser: Browser;
  sender: Page;
  viewer: Page;
}

async function startCall(tag: string, uiURL: string): Promise<Call> {
  const meetingId = `e2e_rate_${tag}_${Date.now()}`;
  const senderBrowser = await chromium.launch({ args: BROWSER_ARGS });
  const viewerBrowser = await chromium.launch({ args: BROWSER_ARGS });
  const senderCtx = await createAuthenticatedContext(
    senderBrowser,
    `rate-sender-${tag}@videocall.rs`,
    SENDER,
    uiURL,
  );
  const viewerCtx = await createAuthenticatedContext(
    viewerBrowser,
    `rate-viewer-${tag}@videocall.rs`,
    VIEWER,
    uiURL,
  );
  // The canvas camera, as in video-aspect-ratio.spec.ts and
  // traffic-shaping.spec.ts.
  await useFakeCamera(senderCtx, { width: 1280, height: 720 });
  const sender = await senderCtx.newPage();
  const viewer = await viewerCtx.newPage();
  // The viewer hosts and the sender joins as the guest (see
  // video-aspect-ratio.spec.ts for why this way round).
  await startTwoPartyMeeting(viewer, VIEWER, sender, SENDER, meetingId);
  await expect(senderNameOn(viewer)).toBeVisible({ timeout: 30_000 });
  return { senderBrowser, viewerBrowser, sender, viewer };
}

async function endCall(call: Call | undefined): Promise<void> {
  await call?.senderBrowser.close();
  await call?.viewerBrowser.close();
}

function senderNameOn(viewer: Page): Locator {
  return viewer.locator("#grid-container .floating-name", { hasText: SENDER }).first();
}

/** The sender's peer key on the viewer's page, from its tile's `peer-video-<key>-div` id. */
async function senderPeerKey(viewer: Page): Promise<string> {
  const tile = viewer
    .locator('#grid-container .grid-item[id^="peer-video-"]', {
      has: viewer.locator(".floating-name", { hasText: SENDER }),
    })
    .first();
  const id = (await tile.getAttribute("id")) ?? "";
  const match = id.match(/^peer-video-(.+)-div$/);
  expect(match, `unexpected tile id "${id}"`).not.toBeNull();
  return match![1];
}

function receptionRow(viewer: Page, peer: string, media: "VIDEO" | "SCREEN" | "AUDIO"): Locator {
  return viewer.locator(
    `[data-testid="reception-stats"] [data-testid="reception-stats-row"][data-peer="${peer}"][data-media-type="${media}"]`,
  );
}

function encoderValue(sender: Page, testId: string): Locator {
  return sender.getByTestId("encoder-settings").getByTestId(testId);
}

/** Whether `later` comes after `earlier` in document order. */
async function follows(earlier: Locator, later: Locator): Promise<boolean> {
  const laterHandle = await later.elementHandle();
  return earlier.evaluate(
    (node, other) =>
      !!other && (node.compareDocumentPosition(other) & Node.DOCUMENT_POSITION_FOLLOWING) !== 0,
    laterHandle,
  );
}

async function startVideo(sender: Page): Promise<void> {
  await controlButton(sender, "Start Video").click();
  await expect(controlButton(sender, "Stop Video")).toBeVisible({ timeout: 10_000 });
}

test.describe("Rate control diagnostics (WebSocket)", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test("Reception Stats and Encoder Settings follow the sender's camera and mic", async ({
    baseURL,
  }) => {
    test.setTimeout(180_000);
    let call: Call | undefined;
    try {
      call = await startCall("panel", baseURL || "http://localhost:3001");
      const { sender, viewer } = call;
      const peer = await senderPeerKey(viewer);

      // Both panels open before any media; the reception table lives in
      // the panel, so it stays open on the viewer.
      await openDiagnostics(viewer);
      await expect(viewer.getByTestId("reception-stats")).toBeVisible();
      await openDiagnostics(sender);
      const card = sender.getByTestId("encoders-card");
      await expect(card).toBeVisible();
      await expect(card.getByRole("heading", { name: "Encoders" })).toBeVisible();
      await expect(sender.getByTestId("encoder-settings")).toBeVisible();
      await expect(sender.getByTestId("encoder-settings-camera-off")).toBeVisible();
      // Camera and mic off: no video rows, and the audio row says so.
      for (const id of VIDEO_ROW_IDS) {
        await expect(card.getByTestId(id)).toHaveCount(0);
      }
      await expect(card.getByTestId("encoders-audio-bitrate")).toHaveText("mic off");

      // Unlocked, Traffic Shaping sits right above the Encoders card.
      await tapTitle(sender, REVEAL_TAPS);
      await expect(shapingSection(sender)).toBeVisible();
      expect(await follows(shapingSection(sender), card)).toBe(true);

      // The sender's encoder, before any viewer has reported on it.
      await startVideo(sender);
      await expect(sender.getByTestId("encoder-settings-camera-off")).toHaveCount(0);
      await expect(encoderValue(sender, "encoder-target-bitrate")).toHaveText(/^\d+ kbps$/);
      await expect(encoderValue(sender, "encoder-bitrate")).toHaveText(/^\d+ kbps$/);
      await expect(encoderValue(sender, "encoder-tier")).toHaveText(
        /^\S+ \(\d+x\d+ @ [\d.]+ fps\)$/,
      );
      await expect(encoderValue(sender, "encoder-capacity")).toHaveText("no reports");
      await expect(encoderValue(sender, "encoder-frames-skipped")).toHaveText(/^\d+$/);
      await expect(encoderValue(sender, "encoder-reconfigures")).toHaveText(/^\d+\.\d\/s$/);
      // The card's own ids carry the same values as the legacy ones.
      await expect(card.getByTestId("encoders-video-bitrate")).toHaveText(/^\d+ kbps$/);
      await expect(card.getByTestId("encoders-video-target")).toHaveText(/^\d+ kbps$/);
      await expect(card.getByTestId("encoders-video-tier")).toHaveText(
        /^\S+ \(\d+x\d+ @ [\d.]+ fps\)$/,
      );
      await expect(card.getByTestId("encoders-audio-bitrate")).toHaveText("mic off");

      await unmute(sender);
      await expect(card.getByTestId("encoders-audio-bitrate")).toHaveText(AUDIO_BITRATE);

      // The viewer: a VIDEO and an AUDIO row for the sender.
      const video = receptionRow(viewer, peer, "VIDEO");
      const audio = receptionRow(viewer, peer, "AUDIO");
      await expect(video).toBeVisible({ timeout: 30_000 });
      await expect(audio).toBeVisible({ timeout: 30_000 });
      await expect(video.locator(".reception-stats-peer")).toHaveText(SENDER);
      await expect(audio.locator(".reception-stats-peer")).toHaveText(SENDER);
      await expect
        .poll(async () => Number(await video.getByTestId("reception-stats-fps").textContent()), {
          timeout: 15_000,
          message: "the VIDEO row never showed fps > 0",
        })
        .toBeGreaterThan(0);
      await expect(video.getByTestId("reception-stats-bitrate")).toHaveText(/^\d+ kbps$/);
      await expect(audio.getByTestId("reception-stats-fps")).toHaveText(EN_DASH);
      await expect(audio.getByTestId("reception-stats-rate")).toHaveText(EN_DASH);
      await expect(audio.getByTestId("reception-stats-bitrate")).toHaveText(/^\d+ kbps$/);
      // Once the viewer reports on the sender's camera: "<capacity> / <backlog> ms",
      // or "not limited / <backlog> ms" on a path that never queued.
      await expect(video.getByTestId("reception-stats-rate")).toHaveText(
        /^(\d+|not limited) \/ \d+ ms$/,
        { timeout: 30_000 },
      );

      // The sender sees the report.
      await expect(encoderValue(sender, "encoder-viewers")).toHaveText("1", { timeout: 30_000 });
      await expect(encoderValue(sender, "encoder-capacity")).toHaveText(
        /^(\d+ kbps|not limited)$/,
        { timeout: 30_000 },
      );
      await expect(encoderValue(sender, "encoder-backlog")).toHaveText(/^\d+ ms$/);

      // Camera off: the sender's section says so and the viewer's VIDEO
      // row goes stale while AUDIO stays.
      await controlButton(sender, "Stop Video").click();
      await expect(controlButton(sender, "Start Video")).toBeVisible({ timeout: 10_000 });
      await expect(sender.getByTestId("encoder-settings-camera-off")).toBeVisible();
      for (const id of VIDEO_ROW_IDS) {
        await expect(card.getByTestId(id)).toHaveCount(0);
      }
      await expect(card.getByTestId("encoders-audio-bitrate")).toHaveText(AUDIO_BITRATE);
      await expect(video).toHaveCount(0, { timeout: ROW_GONE_TIMEOUT_MS });
      await expect(audio).toBeVisible();

      await mute(sender);
      await expect(card.getByTestId("encoders-audio-bitrate")).toHaveText("mic off");
    } finally {
      await endCall(call);
    }
  });

  test("the sender's rows go away after the sender leaves", async ({ baseURL }) => {
    test.setTimeout(120_000);
    let call: Call | undefined;
    try {
      call = await startCall("leave", baseURL || "http://localhost:3001");
      const { sender, viewer } = call;
      const peer = await senderPeerKey(viewer);
      await openDiagnostics(viewer);
      await startVideo(sender);
      await unmute(sender);
      const video = receptionRow(viewer, peer, "VIDEO");
      const audio = receptionRow(viewer, peer, "AUDIO");
      await expect(video).toBeVisible({ timeout: 30_000 });
      await expect(audio).toBeVisible({ timeout: 30_000 });

      await controlButton(sender, "Hang Up").click();
      await expect(senderNameOn(viewer)).toHaveCount(0, { timeout: 30_000 });
      await expect(video).toHaveCount(0, { timeout: ROW_GONE_TIMEOUT_MS });
      await expect(audio).toHaveCount(0, { timeout: ROW_GONE_TIMEOUT_MS });
    } finally {
      await endCall(call);
    }
  });
});
