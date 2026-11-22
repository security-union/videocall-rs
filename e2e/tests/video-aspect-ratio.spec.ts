import { test, expect, chromium, Browser, Page } from "@playwright/test";
import { waitForServices } from "../helpers/wait-for-services";
import {
  BROWSER_ARGS,
  controlButton,
  createAuthenticatedContext,
  startTwoPartyMeeting,
} from "../helpers/meeting";
import {
  CanvasSize,
  FakeCameraOptions,
  peerCanvasSizes,
  recordPeerCanvasSizes,
  useFakeCamera,
} from "../helpers/fake-camera";

// The camera encoder fits each frame under the current quality tier cap
// (VIDEO_QUALITY_TIERS: 1280x720 down to 426x240) with one scale factor, so
// the remote peer gets the sender's aspect ratio at every tier. The sender
// here has a camera bigger than the top tier, so every tier has to scale it.
// Clamping width and height separately, as the encoder used to, turns a
// 1280x960 frame into 1280x720 at the top tier and 426x240 at the bottom.
//
// The viewer's canvas backing size is set from each decoded frame's display
// size, so it is the encoder's output size. The test records every size the
// canvas takes for a while, which covers the tier steps the adaptive quality
// controller makes after video starts (it starts at the lowest tier).

const SENDER = "AspectSender";
const VIEWER = "AspectViewer";

// Even rounding moves the ratio by at most one pixel in 240.
const RATIO_TOLERANCE = 0.01;
// How long to keep watching after the first decoded frame, so tier changes
// are recorded too.
const OBSERVE_MS = 10_000;

interface Call {
  senderBrowser: Browser;
  viewerBrowser: Browser;
  sender: Page;
  viewer: Page;
}

async function startCall(tag: string, uiURL: string, camera: FakeCameraOptions): Promise<Call> {
  const meetingId = `e2e_aspect_${tag}_${Date.now()}`;
  const senderBrowser = await chromium.launch({ args: BROWSER_ARGS });
  const viewerBrowser = await chromium.launch({ args: BROWSER_ARGS });

  const senderCtx = await createAuthenticatedContext(
    senderBrowser,
    `aspect-sender-${tag}@videocall.rs`,
    SENDER,
    uiURL,
  );
  await useFakeCamera(senderCtx, camera);
  const viewerCtx = await createAuthenticatedContext(
    viewerBrowser,
    `aspect-viewer-${tag}@videocall.rs`,
    VIEWER,
    uiURL,
  );

  const sender = await senderCtx.newPage();
  const viewer = await viewerCtx.newPage();
  // The viewer hosts and the sender joins as the guest. With the roles the
  // other way round, the admitted guest intermittently kept the host's tile
  // on "Video Disabled" with no canvas at all while the host was encoding
  // (7 of 15 tests), which is a separate bug from the one tested here.
  await startTwoPartyMeeting(viewer, VIEWER, sender, SENDER, meetingId);
  await expect(
    viewer.locator("#grid-container .floating-name", { hasText: SENDER }).first(),
  ).toBeVisible({ timeout: 30_000 });
  return { senderBrowser, viewerBrowser, sender, viewer };
}

async function endCall(call: Call | undefined): Promise<void> {
  await call?.senderBrowser.close();
  await call?.viewerBrowser.close();
}

/** Start the sender's video and return every size the viewer's canvas took. */
async function observeRemoteVideoSizes(call: Call): Promise<CanvasSize[]> {
  const { sender, viewer } = call;
  await recordPeerCanvasSizes(viewer, SENDER);

  await controlButton(sender, "Start Video").click();
  await expect(controlButton(sender, "Stop Video")).toBeVisible({ timeout: 10_000 });

  await expect
    .poll(async () => (await peerCanvasSizes(viewer)).length, {
      timeout: 30_000,
      message: "the viewer never decoded a frame from the sender",
    })
    .toBeGreaterThan(0);
  // A fixed observation window on purpose: the assertion is about every
  // size the stream takes while the tier controller settles, not one state.
  await viewer.waitForTimeout(OBSERVE_MS);
  return peerCanvasSizes(viewer);
}

function expectAspectAndCap(sizes: CanvasSize[], camera: FakeCameraOptions): void {
  const expected = camera.width / camera.height;
  const report = sizes.map((s) => `${s.width}x${s.height}`).join(", ");
  for (const size of sizes) {
    const ratio = size.width / size.height;
    expect(
      Math.abs(ratio - expected) / expected,
      `remote canvas ${size.width}x${size.height} (ratio ${ratio.toFixed(3)}) does not keep the ` +
        `${camera.width}x${camera.height} camera's ratio ${expected.toFixed(3)}; sizes seen: ${report}`,
    ).toBeLessThanOrEqual(RATIO_TOLERANCE);
    // Fits the top tier (1280x720), in either orientation, and never upscales.
    expect(Math.max(size.width, size.height), report).toBeLessThanOrEqual(1280);
    expect(Math.min(size.width, size.height), report).toBeLessThanOrEqual(720);
    expect(size.width, report).toBeLessThanOrEqual(camera.width);
    expect(size.height, report).toBeLessThanOrEqual(camera.height);
    expect(size.width % 2, `odd width in ${report}`).toBe(0);
    expect(size.height % 2, `odd height in ${report}`).toBe(0);
  }
}

test.describe("Remote video keeps the sender's aspect ratio", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test("a 4:3 camera bigger than every tier arrives 4:3", async ({ baseURL }) => {
    test.setTimeout(120_000);
    const camera = { width: 1280, height: 960 };
    let call: Call | undefined;
    try {
      call = await startCall("landscape", baseURL || "http://localhost:3001", camera);
      const sizes = await observeRemoteVideoSizes(call);
      console.log(`4:3 camera, remote canvas sizes: ${JSON.stringify(sizes)}`);
      expectAspectAndCap(sizes, camera);
    } finally {
      await endCall(call);
    }
  });

  test("a portrait 9:16 camera arrives portrait, not squashed to landscape", async ({
    baseURL,
  }) => {
    test.setTimeout(120_000);
    // A phone held upright. The tier caps are written landscape and are
    // applied orientation-agnostic.
    const camera = { width: 720, height: 1280 };
    let call: Call | undefined;
    try {
      call = await startCall("portrait", baseURL || "http://localhost:3001", camera);
      const sizes = await observeRemoteVideoSizes(call);
      console.log(`9:16 camera, remote canvas sizes: ${JSON.stringify(sizes)}`);
      expectAspectAndCap(sizes, camera);
    } finally {
      await endCall(call);
    }
  });
});
