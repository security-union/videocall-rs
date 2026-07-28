import { resolve } from "node:path";

import { expect, test } from "@playwright/test";

test("clock getUserMedia track publishes advancing frames", async ({ page }) => {
  await page.addInitScript(
    `globalThis.__CLOCK_PARTICIPANT = ${JSON.stringify("Clock Browser Test")};`,
  );
  await page.addInitScript({
    path: resolve(process.cwd(), "bots-app/src/clock-source.js"),
  });
  await page.route("https://clock.test/", async (route) => {
    await route.fulfill({
      contentType: "text/html",
      body: "<title>clock source browser test</title>",
    });
  });
  await page.goto("https://clock.test/");

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
