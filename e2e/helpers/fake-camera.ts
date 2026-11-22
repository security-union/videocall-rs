import { BrowserContext, Page } from "@playwright/test";

/**
 * A camera with a chosen frame size, for tests that care about geometry.
 *
 * Chromium's `--use-fake-device-for-media-stream` camera is 640x480, which
 * fits under every quality tier cap but the lowest ones, so it can't show
 * what the encoder does to a frame that has to be scaled down. This replaces
 * the video half of `getUserMedia` with a `canvas.captureStream()` of the
 * requested size. Audio requests still go to the real (fake) microphone, and
 * device enumeration is untouched, so the app still sees Chromium's fake
 * camera in its device list and asks for it by `deviceId`; the constraints
 * are ignored and the canvas stream comes back instead.
 *
 * The canvas is redrawn every 33 ms with a moving bar, because a canvas
 * capture track only emits a frame when the canvas changes.
 */
export interface FakeCameraOptions {
  width: number;
  height: number;
}

function installFakeCamera({ width, height }: FakeCameraOptions): void {
  const mediaDevices = navigator.mediaDevices;
  const realGetUserMedia = mediaDevices.getUserMedia.bind(mediaDevices);

  function canvasVideoTrack(): MediaStreamTrack {
    const canvas = document.createElement("canvas");
    canvas.width = width;
    canvas.height = height;
    const context = canvas.getContext("2d");
    if (!context) {
      throw new Error("fake camera: no 2d context");
    }
    let frame = 0;
    const draw = () => {
      frame += 1;
      context.fillStyle = "#204060";
      context.fillRect(0, 0, width, height);
      context.fillStyle = "#f0c040";
      context.fillRect((frame * 8) % width, 0, width / 10, height);
    };
    draw();
    const timer = setInterval(draw, 33);
    const track = canvas.captureStream(30).getVideoTracks()[0];
    track.addEventListener("ended", () => clearInterval(timer));
    const realStop = track.stop.bind(track);
    track.stop = () => {
      clearInterval(timer);
      realStop();
    };
    // The camera encoder sizes its first config from the track settings, so
    // report the canvas size like a real camera would.
    const realGetSettings = track.getSettings.bind(track);
    track.getSettings = () => ({ ...realGetSettings(), width, height, frameRate: 30 });
    return track;
  }

  mediaDevices.getUserMedia = async (constraints?: MediaStreamConstraints) => {
    if (!constraints?.video) {
      return realGetUserMedia(constraints);
    }
    const stream = new MediaStream([canvasVideoTrack()]);
    if (constraints.audio) {
      const audio = await realGetUserMedia({ audio: constraints.audio });
      audio.getAudioTracks().forEach((track) => stream.addTrack(track));
    }
    return stream;
  };
}

/** Install the fake camera in every page of `context`, before the app loads. */
export async function useFakeCamera(
  context: BrowserContext,
  options: FakeCameraOptions,
): Promise<void> {
  await context.addInitScript(installFakeCamera, options);
}

/** A backing size a remote peer's video canvas took. */
export interface CanvasSize {
  width: number;
  height: number;
}

declare global {
  interface Window {
    __peerCanvasSizes?: CanvasSize[];
  }
}

/**
 * Record every backing size the video canvas on `peerName`'s tile takes.
 *
 * The decoder sets the canvas `width`/`height` attributes to each decoded
 * frame's display size when it changes, so the recorded sizes are the sizes
 * the sender's encoder produced, tier changes included. A MutationObserver
 * catches each resize instead of sampling and hoping to land on it. Call it
 * before the sender starts video; read the result with `peerCanvasSizes`.
 */
export async function recordPeerCanvasSizes(page: Page, peerName: string): Promise<void> {
  await page.evaluate((name) => {
    const sizes: CanvasSize[] = [];
    window.__peerCanvasSizes = sizes;
    const isPeerVideoCanvas = (node: Node): node is HTMLCanvasElement => {
      if (!(node instanceof HTMLCanvasElement) || node.id.startsWith("screen-share-")) {
        return false;
      }
      const tile = node.closest("#grid-container .canvas-container");
      const label = tile?.querySelector(".floating-name")?.textContent ?? "";
      return label.includes(name);
    };
    new MutationObserver((mutations) => {
      for (const mutation of mutations) {
        const canvas = mutation.target;
        if (isPeerVideoCanvas(canvas) && canvas.hasAttribute("width")) {
          const size = { width: canvas.width, height: canvas.height };
          const last = sizes[sizes.length - 1];
          if (!last || last.width !== size.width || last.height !== size.height) {
            sizes.push(size);
          }
        }
      }
    }).observe(document.body, {
      subtree: true,
      attributes: true,
      attributeFilter: ["width", "height"],
    });
  }, peerName);
}

export async function peerCanvasSizes(page: Page): Promise<CanvasSize[]> {
  return page.evaluate(() => window.__peerCanvasSizes ?? []);
}
