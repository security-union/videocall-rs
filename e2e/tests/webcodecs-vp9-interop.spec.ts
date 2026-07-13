import { test, expect } from "@playwright/test";
import { readFileSync } from "fs";
import { join } from "path";

/**
 * WebCodecs VP9 interop gate.
 *
 * Feeds a stream produced by the pure-Rust VP9 encoder (videocall-codecs) to a
 * real Chromium `VideoDecoder` — the exact decoder dioxus-ui uses to render
 * remote participants (web_sys `VideoDecoder`, codec `vp09.00.10.08`). If
 * Chromium decodes every frame, our encoder's output is interoperable with the
 * browsers meeting participants actually run.
 *
 * The fixture `fixtures/pure_rust_vp9.ivf` is committed and regenerated with:
 *   cargo run -p videocall-codecs --example dump_vp9_ivf \
 *     --features test-utils -- e2e/fixtures/pure_rust_vp9.ivf
 * It is 90 frames of 640x480@30 (1 keyframe + 89 inter) at 500 kbps.
 *
 * This spec needs no application server: it decodes on a blank page, so it runs
 * independently of the docker stack the rest of the suite targets.
 */

const FIXTURE_WIDTH = 640;
const FIXTURE_HEIGHT = 480;
const FIXTURE_FRAMES = 90;
// Profile 0, level 1.0, 8-bit — the codec string dioxus-ui configures.
const VP9_CODEC = "vp09.00.10.08";

const fixturePath = join(__dirname, "../fixtures/pure_rust_vp9.ivf");
const fixtureBase64 = readFileSync(fixturePath).toString("base64");

interface DecodeReport {
  configSupported: boolean;
  decoded: number;
  dimensions: Array<{ width: number; height: number }>;
  errors: string[];
  nonUniformFrames: number;
}

test.describe("WebCodecs VP9 interop — Chromium decodes pure-Rust encoder streams", () => {
  test("decodes every frame at the correct resolution with real pixel content", async ({
    page,
  }) => {
    // WebCodecs `VideoDecoder` is a secure-context API, so `about:blank` (opaque
    // origin) does not expose it. Serve a blank page from http://localhost via
    // route interception: localhost is a secure context and needs no real server.
    await page.route("**/*", (route) =>
      route.fulfill({
        contentType: "text/html",
        body: "<!doctype html><title>vp9-interop</title>",
      }),
    );
    await page.goto("http://localhost/vp9-interop");

    const hasVideoDecoder = await page.evaluate(() => typeof VideoDecoder !== "undefined");
    test.skip(!hasVideoDecoder, "VideoDecoder (WebCodecs) unavailable in this browser build");

    const report = await page.evaluate(
      async ({ base64, codec, width, height }): Promise<DecodeReport> => {
        // --- Inline IVF parser: 32-byte file header, then per-frame a 12-byte
        // header (u32 LE size, u64 LE pts) followed by the payload. ---
        const bin = atob(base64);
        const bytes = new Uint8Array(bin.length);
        for (let i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i);
        const view = new DataView(bytes.buffer);

        const magic = String.fromCharCode(bytes[0], bytes[1], bytes[2], bytes[3]);
        if (magic !== "DKIF") throw new Error(`bad IVF magic: ${magic}`);
        const headerLen = view.getUint16(6, true);

        const frames: Array<{ data: Uint8Array; pts: number }> = [];
        let off = headerLen;
        while (off + 12 <= bytes.length) {
          const size = view.getUint32(off, true);
          const pts = Number(view.getBigUint64(off + 4, true));
          off += 12;
          if (off + size > bytes.length) break;
          frames.push({ data: bytes.subarray(off, off + size), pts });
          off += size;
        }

        const errors: string[] = [];
        const dimensions: Array<{ width: number; height: number }> = [];
        let nonUniformFrames = 0;

        // Sample a frame's pixels to confirm it is not a flat/uniform image.
        const canvas = new OffscreenCanvas(width, height);
        const ctx = canvas.getContext("2d");

        const config = { codec, codedWidth: width, codedHeight: height };
        let configSupported = false;
        try {
          const support = await VideoDecoder.isConfigSupported(config);
          configSupported = support.supported === true;
        } catch (e) {
          errors.push(`isConfigSupported threw: ${String(e)}`);
        }

        const decoder = new VideoDecoder({
          output: (frame) => {
            dimensions.push({ width: frame.displayWidth, height: frame.displayHeight });
            if (ctx && nonUniformFrames === 0) {
              ctx.drawImage(frame, 0, 0);
              const cx = width >> 1;
              const cy = height >> 1;
              const px = ctx.getImageData(cx - 40, cy - 40, 80, 80).data;
              const first = px[0];
              for (let i = 4; i < px.length; i += 4) {
                if (px[i] !== first) {
                  nonUniformFrames = 1;
                  break;
                }
              }
            }
            frame.close();
          },
          error: (e) => errors.push(String(e)),
        });

        decoder.configure(config);
        for (let i = 0; i < frames.length; i++) {
          const f = frames[i];
          const chunk = new EncodedVideoChunk({
            type: i === 0 ? "key" : "delta",
            timestamp: f.pts,
            data: f.data,
          });
          decoder.decode(chunk);
        }
        await decoder.flush();
        decoder.close();

        return {
          configSupported,
          decoded: dimensions.length,
          dimensions,
          errors,
          nonUniformFrames,
        };
      },
      { base64: fixtureBase64, codec: VP9_CODEC, width: FIXTURE_WIDTH, height: FIXTURE_HEIGHT },
    );

    // The gate: Chromium's VP9 decoder accepted our stream with no errors.
    expect(report.errors, "decoder emitted no error callbacks").toEqual([]);
    expect(report.configSupported, `${VP9_CODEC} reported supported`).toBe(true);

    // Every input frame produced exactly one VideoFrame.
    expect(report.decoded, "all frames decoded").toBe(FIXTURE_FRAMES);

    // Every decoded frame is at the source resolution.
    for (const d of report.dimensions) {
      expect(d.width).toBe(FIXTURE_WIDTH);
      expect(d.height).toBe(FIXTURE_HEIGHT);
    }

    // Decoded pixels carry real image content (not a flat frame).
    expect(report.nonUniformFrames, "at least one frame has non-uniform pixels").toBeGreaterThan(0);
  });
});
