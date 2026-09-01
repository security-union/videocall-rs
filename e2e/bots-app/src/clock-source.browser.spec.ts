import { readFileSync } from "node:fs";
import { resolve } from "node:path";

import { expect, test, type Page } from "@playwright/test";

import { SD_SOURCE, sourceGeometryForIndex } from "./posture";

const CLOCK_SOURCE = readFileSync(resolve(process.cwd(), "bots-app/src/clock-source.js"), "utf8");

async function installClockSource(page: Page, injected?: string): Promise<void> {
  // One registration, the shape bot.ts uses: globals then the source that reads them.
  await page.addInitScript(
    `globalThis.__CLOCK_PARTICIPANT = ${JSON.stringify("Clock Browser Test")};\n` +
      (injected ?? "") +
      "\n" +
      CLOCK_SOURCE,
  );
  await page.route("https://clock.test/", async (route) => {
    await route.fulfill({
      contentType: "text/html",
      body: "<title>clock source browser test</title>",
    });
  });
  await page.goto("https://clock.test/");
}

/** Settings of the track `getUserMedia` hands the client — what reaches the wire. */
async function capturedTrackSettings(page: Page): Promise<{ width?: number; height?: number }> {
  return page.evaluate(async () => {
    const stream = await navigator.mediaDevices.getUserMedia({ video: true });
    try {
      const settings = stream.getVideoTracks()[0].getSettings();
      return { width: settings.width, height: settings.height };
    } finally {
      for (const track of stream.getTracks()) track.stop();
    }
  });
}

test("captured track reports the injected geometry (#2236)", async ({ page }) => {
  const hd = sourceGeometryForIndex(1);
  expect([hd.width, hd.height]).toEqual([1280, 720]);
  await installClockSource(
    page,
    `globalThis.__CLOCK_WIDTH = ${hd.width};globalThis.__CLOCK_HEIGHT = ${hd.height};`,
  );
  expect(await capturedTrackSettings(page)).toEqual({ width: 1280, height: 720 });
});

test("captured track falls back to SD_SOURCE with no injection (#2236)", async ({ page }) => {
  await installClockSource(page);
  expect(await capturedTrackSettings(page)).toEqual({ ...SD_SOURCE });
});

test("clock getUserMedia track publishes advancing frames", async ({ page }) => {
  await installClockSource(page);

  const result = await page.evaluate(async () => {
    const stream = await navigator.mediaDevices.getUserMedia({ video: true });
    const video = document.createElement("video");
    video.muted = true;
    video.srcObject = stream;
    document.body.append(video);

    try {
      await video.play();
      return await new Promise<{ elapsedMs: number; frameTimes: number[] }>((resolve, reject) => {
        const times: number[] = [];
        const startedAt = performance.now();
        const timeout = window.setTimeout(
          () => reject(new Error(`received only ${times.length} clock frames`)),
          2_000,
        );
        const collect = (now: DOMHighResTimeStamp, metadata: VideoFrameCallbackMetadata): void => {
          times.push(metadata.mediaTime);
          const elapsedMs = now - startedAt;
          if (elapsedMs >= 900 && times.length >= 2) {
            window.clearTimeout(timeout);
            resolve({ elapsedMs, frameTimes: times });
            return;
          }
          video.requestVideoFrameCallback(collect);
        };
        video.requestVideoFrameCallback(collect);
      });
    } finally {
      video.remove();
      for (const track of stream.getTracks()) track.stop();
    }
  });

  expect(result.elapsedMs).toBeGreaterThanOrEqual(900);
  expect(result.frameTimes.length).toBeGreaterThanOrEqual(2);
  expect(new Set(result.frameTimes).size).toBeGreaterThanOrEqual(2);
  expect(result.frameTimes.at(-1)).toBeGreaterThan(result.frameTimes[0]);
});
