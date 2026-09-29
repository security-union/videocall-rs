import { test, expect, chromium, BrowserContext, Page } from "@playwright/test";
import { BROWSER_ARGS, createAuthenticatedContext } from "../helpers/auth-context";
import { wakeControls } from "../helpers/controls";
import {
  MeetingMember,
  admitGuestIfNeeded,
  clickJoinAndEnterGrid,
  joinMeetingAs,
} from "../helpers/screen-share-meeting";
import { waitForServices } from "../helpers/wait-for-services";

/**
 * Issue 2783: a portrait phone camera must reach peers upright, and fit the stage.
 *
 * The camera publishes one picture: quadrants TL red, TR green, BL blue, BR white.
 * Upright it is 480x640 portrait (or 640x480 once "landscape"). The raw sensor
 * buffer is the portrait picture turned a quarter-turn counter-clockwise, which a
 * `rotation: 90` tag restores. The receiver reads its peer canvas quadrants.
 */

const DEFAULT_UI_URL = "http://localhost:3001";
const PEER_VIDEO_CANVAS = "#grid-container .grid-item .canvas-container.video-on canvas";
const STAGE_VIDEO_CANVAS =
  "#grid-container .grid-item.full-bleed .canvas-container.video-on canvas";
const UPRIGHT_QUADRANTS = "TL=red TR=green BL=blue BR=white";
const CANVAS_POLYFILL_LOG = "Using canvas-based polyfill";
const DIRECT_POLYFILL_LOG = "Using direct VideoFrame(video)";

type CameraLayout = "sensor" | "upright" | "landscape";
type CameraControl = { setLayout: (layout: CameraLayout) => void };

function installCanvasCamera(initial: CameraLayout): void {
  const md = navigator.mediaDevices;
  if (!md) return;
  let layout = initial;
  let canvas: HTMLCanvasElement | null = null;
  let track: MediaStreamTrack | null = null;
  let n = 0;
  const paint = (): void => {
    if (!canvas) return;
    const portrait = layout !== "landscape";
    const pw = portrait ? 480 : 640;
    const ph = portrait ? 640 : 480;
    const [cw, ch] = layout === "sensor" ? [ph, pw] : [pw, ph];
    if (canvas.width !== cw || canvas.height !== ch) {
      canvas.width = cw;
      canvas.height = ch;
    }
    const ctx = canvas.getContext("2d")!;
    n += 1;
    ctx.save();
    if (layout === "sensor") {
      ctx.translate(0, pw);
      ctx.rotate(-Math.PI / 2);
    }
    ctx.fillStyle = "#ff0000";
    ctx.fillRect(0, 0, pw / 2, ph / 2);
    ctx.fillStyle = "#00ff00";
    ctx.fillRect(pw / 2, 0, pw / 2, ph / 2);
    ctx.fillStyle = "#0000ff";
    ctx.fillRect(0, ph / 2, pw / 2, ph / 2);
    ctx.fillStyle = "#ffffff";
    ctx.fillRect(pw / 2, ph / 2, pw / 2, ph / 2);
    ctx.fillStyle = "#000000";
    ctx.fillRect(pw / 2 - 8, (n * 16) % ph, 16, 16);
    ctx.restore();
  };
  const cameraTrack = (): MediaStreamTrack => {
    if (!track) {
      canvas = document.createElement("canvas");
      paint();
      setInterval(paint, 66);
      track = canvas.captureStream(15).getVideoTracks()[0];
    }
    return track;
  };
  (window as unknown as { __e2e2783Camera: CameraControl }).__e2e2783Camera = {
    setLayout: (next) => {
      layout = next;
    },
  };
  const original = md.getUserMedia.bind(md);
  md.getUserMedia = async (constraints?: MediaStreamConstraints): Promise<MediaStream> => {
    if (!constraints?.video) return original(constraints);
    const stream = new MediaStream([cameraTrack().clone()]);
    if (constraints.audio) {
      const mic = await original({ audio: constraints.audio });
      mic.getAudioTracks().forEach((t) => stream.addTrack(t));
    }
    return stream;
  };
}

interface RetagState {
  retagged: number;
  rotation: number;
  display: string;
  coded: string;
  error: string;
}

// Chrome 137+ VideoFrame orientation: the camera reader receives the sensor
// buffer tagged `rotation: 90`, as a rotated capture device delivers it.
function tagCameraFramesQuarterTurned(): void {
  type Processor = { readable: ReadableStream<VideoFrame> };
  type ProcessorCtor = new (init: { track: MediaStreamTrack }) => Processor;
  const w = window as unknown as {
    MediaStreamTrackProcessor?: ProcessorCtor;
    __e2e2783?: RetagState;
  };
  const Native = w.MediaStreamTrackProcessor;
  if (!Native) return;
  const seen: RetagState = { retagged: 0, rotation: -1, display: "", coded: "", error: "" };
  w.__e2e2783 = seen;
  const Retagging = function (init: { track: MediaStreamTrack }): Processor {
    const inner = new Native(init);
    if (init.track.kind !== "video") return inner;
    const retag = new TransformStream<VideoFrame, VideoFrame>({
      transform(frame, controller) {
        try {
          const turned = new VideoFrame(frame, { rotation: 90 } as unknown as VideoFrameInit);
          frame.close();
          seen.retagged += 1;
          seen.rotation = (turned as unknown as { rotation: number }).rotation;
          seen.display = `${turned.displayWidth}x${turned.displayHeight}`;
          seen.coded = `${turned.codedWidth}x${turned.codedHeight}`;
          controller.enqueue(turned);
        } catch (e) {
          seen.error = String(e);
          controller.enqueue(frame);
        }
      },
    });
    return { readable: inner.readable.pipeThrough(retag) };
  };
  w.MediaStreamTrackProcessor = Retagging as unknown as ProcessorCtor;
}

interface PolyfilledShape {
  vendor: string;
  videoElementFramesAreRawSensor: boolean;
}

// iOS Safari as traced in WebKit source: `new VideoFrame(video)` wraps the
// unrotated sensor buffer while the <video> element itself renders upright.
const WEBKIT: PolyfilledShape = {
  vendor: "Apple Computer, Inc.",
  videoElementFramesAreRawSensor: true,
};
// Firefox uprights camera frames before any consumer sees them.
const FIREFOX: PolyfilledShape = { vendor: "", videoElementFramesAreRawSensor: false };

// A browser without main-thread MediaStreamTrackProcessor or VideoFrame
// `rotation`/`flip`, so index.html's polyfill reads the camera.
function emulatePolyfilledBrowser(shape: PolyfilledShape): void {
  const w = window as unknown as {
    MediaStreamTrackProcessor?: unknown;
    VideoFrame: typeof VideoFrame;
  };
  delete w.MediaStreamTrackProcessor;
  const Native = w.VideoFrame;
  const proto = Native.prototype as unknown as Record<string, unknown>;
  delete proto.rotation;
  delete proto.flip;
  Object.defineProperty(Navigator.prototype, "vendor", {
    get: () => shape.vendor,
    configurable: true,
  });
  if (!shape.videoElementFramesAreRawSensor) return;
  const RawSensorVideoFrame = function (
    source: CanvasImageSource,
    init?: VideoFrameInit,
  ): VideoFrame {
    if (!(source instanceof HTMLVideoElement)) return new Native(source, init);
    const vw = source.videoWidth;
    const vh = source.videoHeight;
    const sensor = new OffscreenCanvas(vh, vw);
    const ctx = sensor.getContext("2d")!;
    ctx.translate(0, vw);
    ctx.rotate(-Math.PI / 2);
    ctx.drawImage(source, 0, 0, vw, vh);
    return new Native(sensor, {
      ...init,
      timestamp: init?.timestamp ?? Math.round(source.currentTime * 1e6),
    });
  };
  RawSensorVideoFrame.prototype = Native.prototype;
  w.VideoFrame = RawSensorVideoFrame as unknown as typeof VideoFrame;
}

async function expectPolyfilledShape(page: Page, shape: PolyfilledShape): Promise<void> {
  expect(
    await page.evaluate(() => {
      const processor = (window as unknown as { MediaStreamTrackProcessor?: unknown })
        .MediaStreamTrackProcessor;
      return {
        rotation: "rotation" in VideoFrame.prototype,
        flip: "flip" in VideoFrame.prototype,
        vendor: navigator.vendor,
        polyfilledProcessor:
          typeof processor === "function" &&
          !/\[native code\]/.test(Function.prototype.toString.call(processor)),
      };
    }),
    "the sender must run index.html's polyfill with no VideoFrame rotation",
  ).toEqual({ rotation: false, flip: false, vendor: shape.vendor, polyfilledProcessor: true });
}

function collectPolyfillPaths(context: BrowserContext): () => string[] {
  const lines: string[] = [];
  context.on("page", (p) =>
    p.on("console", (msg) => {
      const text = msg.text();
      if (text.includes(CANVAS_POLYFILL_LOG) || text.includes(DIRECT_POLYFILL_LOG)) {
        lines.push(text);
      }
    }),
  );
  return () => lines;
}

async function expectOnlyPolyfillPath(
  paths: () => string[],
  expected: string,
  message: string,
): Promise<void> {
  await expect
    .poll(() => paths().length, {
      timeout: 20_000,
      message: "the camera never started a frame reader through the index.html polyfill",
    })
    .toBeGreaterThan(0);
  expect(
    paths().filter((l) => !l.includes(expected)),
    message,
  ).toEqual([]);
}

async function startCamera(page: Page): Promise<void> {
  await wakeControls(page);
  await page.waitForTimeout(300);
  const button = page.locator('[data-testid="camera-toggle-button"]');
  await expect(button).toHaveAttribute("aria-label", "Camera — Start Video", { timeout: 10_000 });
  await button.click();
  await expect(button).toHaveAttribute("aria-label", "Camera — Stop Video", { timeout: 30_000 });
}

async function peerCanvasPainted(page: Page): Promise<boolean> {
  return page.evaluate((selector) => {
    const canvases = document.querySelectorAll<HTMLCanvasElement>(selector);
    if (canvases.length !== 1) return false;
    const { width, height } = canvases[0];
    return width > 0 && height > 0 && !(width === 300 && height === 150);
  }, PEER_VIDEO_CANVAS);
}

// "<portrait|landscape> WxH TL=.. TR=.. BL=.. BR=..", each quadrant named by the
// reference colour nearest to the mean of a patch centred in it.
async function describeReceivedPicture(page: Page): Promise<string> {
  return page.evaluate((selector) => {
    const canvases = document.querySelectorAll<HTMLCanvasElement>(selector);
    if (canvases.length !== 1) return `${canvases.length} peer video canvases`;
    const canvas = canvases[0];
    const { width: w, height: h } = canvas;
    const ctx = canvas.getContext("2d");
    if (!ctx) return "peer canvas has no 2d context";
    const refs: [string, number, number, number][] = [
      ["red", 255, 0, 0],
      ["green", 0, 255, 0],
      ["blue", 0, 0, 255],
      ["white", 255, 255, 255],
      ["black", 0, 0, 0],
    ];
    const half = Math.max(1, Math.floor(Math.min(w, h) * 0.1));
    const quadrant = (fx: number, fy: number): string => {
      const x = Math.max(0, Math.round(fx * w) - half);
      const y = Math.max(0, Math.round(fy * h) - half);
      const data = ctx.getImageData(x, y, 2 * half, 2 * half).data;
      let r = 0;
      let g = 0;
      let b = 0;
      for (let i = 0; i < data.length; i += 4) {
        r += data[i];
        g += data[i + 1];
        b += data[i + 2];
      }
      const n = data.length / 4;
      let best = "";
      let bestDist = Infinity;
      for (const [name, rr, gg, bb] of refs) {
        const d = (r / n - rr) ** 2 + (g / n - gg) ** 2 + (b / n - bb) ** 2;
        if (d < bestDist) {
          bestDist = d;
          best = name;
        }
      }
      return best;
    };
    const shape = h > w ? "portrait" : w > h ? "landscape" : "square";
    return (
      `${shape} ${w}x${h} TL=${quadrant(0.25, 0.25)} TR=${quadrant(0.75, 0.25)} ` +
      `BL=${quadrant(0.25, 0.75)} BR=${quadrant(0.75, 0.75)}`
    );
  }, PEER_VIDEO_CANVAS);
}

async function expectPeerSeesUpright(
  guestPage: Page,
  shape: "portrait" | "landscape",
): Promise<void> {
  await expect
    .poll(() => peerCanvasPainted(guestPage), {
      timeout: 45_000,
      intervals: [500, 1000],
      message: "the guest never painted exactly one peer video canvas from the host's camera",
    })
    .toBe(true);
  await expect
    .poll(() => describeReceivedPicture(guestPage), {
      timeout: 20_000,
      intervals: [500, 1000],
      message:
        `the guest must see the host's ${shape} picture upright (${UPRIGHT_QUADRANTS}); ` +
        "the portrait sensor buffer painted as-is reads TL=green TR=white BL=red BR=blue",
    })
    .toMatch(new RegExp(`^${shape} \\d+x\\d+ ${UPRIGHT_QUADRANTS}$`));
}

async function withMeeting(
  label: string,
  uiURL: string,
  prepareHost: (host: MeetingMember) => Promise<void>,
  body: (host: MeetingMember, guest: MeetingMember | null) => Promise<void>,
  withGuest = true,
): Promise<void> {
  const meetingId = `e2e_portrait_${label}_${Date.now()}`;
  const roles = withGuest ? ["Host", "Guest"] : ["Host"];
  const browsers = await Promise.all(roles.map(() => chromium.launch({ args: BROWSER_ARGS })));
  const members: MeetingMember[] = [];
  try {
    for (const [i, role] of roles.entries()) {
      const email = `${role.toLowerCase()}-2783-${label}@videocall.rs`;
      const name = `Portrait2783${role}${label}`;
      const context = await createAuthenticatedContext(browsers[i], email, name, uiURL);
      members.push({ page: null as unknown as Page, context, email, name });
    }
    await prepareHost(members[0]);

    members[0].page = await joinMeetingAs(members[0].context, meetingId, members[0].name);
    await clickJoinAndEnterGrid(members[0].page);
    if (withGuest) {
      members[1].page = await joinMeetingAs(members[1].context, meetingId, members[1].name);
      await admitGuestIfNeeded(members[0].page, members[1].page);
    }

    await body(members[0], members[1] ?? null);
  } finally {
    for (const m of members) {
      await m.context.close().catch(() => undefined);
    }
    await Promise.all(browsers.map((b) => b.close().catch(() => undefined)));
  }
}

test.describe("issue 2783 portrait camera reaches peers upright", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test("a camera frame tagged rotation 90 is sent upright @bvt1", async ({ baseURL }) => {
    test.setTimeout(240_000);
    await withMeeting(
      "tagged",
      baseURL || DEFAULT_UI_URL,
      async (host) => {
        await host.context.addInitScript(installCanvasCamera, "sensor" as CameraLayout);
        await host.context.addInitScript(tagCameraFramesQuarterTurned);
      },
      async (host, guest) => {
        await startCamera(host.page);

        const readRetag = () =>
          host.page.evaluate(
            () => (window as unknown as { __e2e2783?: RetagState }).__e2e2783 ?? null,
          );
        await expect
          .poll(async () => (await readRetag())?.retagged ?? 0, {
            timeout: 20_000,
            message: "the camera encoder never read a frame through the rotation-tagging reader",
          })
          .toBeGreaterThan(0);
        expect(
          await readRetag(),
          "this Chromium must carry VideoFrame rotation for the tagged frame to mean anything",
        ).toMatchObject({ rotation: 90, display: "480x640", coded: "640x480", error: "" });

        await expectPeerSeesUpright(guest!.page, "portrait");
      },
    );
  });

  test("a WebKit-shaped camera takes the canvas polyfill path and is sent upright @bvt1", async ({
    baseURL,
  }) => {
    test.setTimeout(240_000);
    let paths: () => string[] = () => [];
    await withMeeting(
      "webkit",
      baseURL || DEFAULT_UI_URL,
      async (host) => {
        await host.context.addInitScript(installCanvasCamera, "upright" as CameraLayout);
        await host.context.addInitScript(emulatePolyfilledBrowser, WEBKIT);
        paths = collectPolyfillPaths(host.context);
      },
      async (host, guest) => {
        await expectPolyfilledShape(host.page, WEBKIT);
        await startCamera(host.page);
        await expectOnlyPolyfillPath(
          paths,
          CANVAS_POLYFILL_LOG,
          "a WebKit VideoFrame cannot carry the camera's orientation, so the polyfill must draw " +
            "the upright <video> element instead of wrapping its raw sensor buffer",
        );
        await expectPeerSeesUpright(guest!.page, "portrait");
      },
    );
  });

  test("a Firefox-shaped camera keeps the direct polyfill path @bvt1", async ({ baseURL }) => {
    test.setTimeout(120_000);
    let paths: () => string[] = () => [];
    await withMeeting(
      "firefox",
      baseURL || DEFAULT_UI_URL,
      async (host) => {
        await host.context.addInitScript(installCanvasCamera, "upright" as CameraLayout);
        await host.context.addInitScript(emulatePolyfilledBrowser, FIREFOX);
        paths = collectPolyfillPaths(host.context);
      },
      async (host) => {
        await expectPolyfilledShape(host.page, FIREFOX);
        await startCamera(host.page);
        await expectOnlyPolyfillPath(
          paths,
          DIRECT_POLYFILL_LOG,
          "Firefox uprights camera frames itself, so its polyfill must keep the direct path",
        );
      },
      false,
    );
  });

  test("a portrait camera on the 1:1 stage is letterboxed until it turns landscape @bvt1", async ({
    baseURL,
  }) => {
    test.setTimeout(240_000);
    await withMeeting(
      "stage",
      baseURL || DEFAULT_UI_URL,
      async (host) => {
        await host.context.addInitScript(installCanvasCamera, "upright" as CameraLayout);
      },
      async (host, guest) => {
        await startCamera(host.page);
        const guestPage = guest!.page;
        const stageCanvas = guestPage.locator(STAGE_VIDEO_CANVAS);

        await expectPeerSeesUpright(guestPage, "portrait");
        await expect(
          stageCanvas,
          "the host's camera must be the guest's 1:1 stage tile",
        ).toHaveCount(1);
        await expect(stageCanvas).toHaveClass("uncropped");
        await expect(stageCanvas).toHaveCSS("object-fit", "contain");

        await host.page.evaluate((layout) => {
          (window as unknown as { __e2e2783Camera: CameraControl }).__e2e2783Camera.setLayout(
            layout,
          );
        }, "landscape" as CameraLayout);
        await expectPeerSeesUpright(guestPage, "landscape");
        await expect(stageCanvas).toHaveClass("cropped");
        await expect(stageCanvas).toHaveCSS("object-fit", "cover");
      },
    );
  });
});
