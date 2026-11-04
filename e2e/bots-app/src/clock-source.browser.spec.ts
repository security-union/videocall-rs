import { readdirSync, readFileSync } from "node:fs";
import { resolve } from "node:path";

import { chromium, expect, test, type Page } from "@playwright/test";

import { SD_SOURCE, sourceGeometryForIndex } from "./posture";

const CLOCK_SOURCE = readFileSync(resolve(process.cwd(), "bots-app/src/clock-source.js"), "utf8");
const CLOCK_SPEECH = readFileSync(resolve(process.cwd(), "bots-app/src/clock-speech.js"), "utf8");
const ALICE = resolve(process.cwd(), "bots-app/assets/clock-voices/alice");

/** The page side of bot.ts's clock registration (clock-init.ts imports via import.meta, which this runner cannot load). */
function talkingClockScript(): string {
  const clips = Object.fromEntries(
    readdirSync(ALICE).map((f) => [
      f.replace(/\.ogg$/, ""),
      readFileSync(resolve(ALICE, f)).toString("base64"),
    ]),
  );
  return (
    "globalThis.__CLOCK_FLEET_INDEX = 0;\nglobalThis.__CLOCK_FLEET_SIZE = 1;\n" +
    `globalThis.__CLOCK_VOICE_CLIPS = ${JSON.stringify(clips)};\n${CLOCK_SPEECH}\n${CLOCK_SOURCE}`
  );
}

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

test("talking clock: decodes the shipped voice and publishes second-aligned speech over digital silence (issue 3072)", async ({
  page,
}) => {
  test.setTimeout(30_000);
  const statuses: string[] = [];
  page.on("console", (msg) => {
    if (msg.text().startsWith("[talking-clock] ")) statuses.push(msg.text().slice(16));
  });
  await page.addInitScript(talkingClockScript());
  await page.route("https://clock.test/", (route) =>
    route.fulfill({ contentType: "text/html", body: "<title>talking clock</title>" }),
  );
  await page.goto("https://clock.test/");
  await expect.poll(() => statuses.at(-1), { timeout: 10_000 }).toMatch(/^ready: /);

  const levels = await page.evaluate(async () => {
    const stream = await navigator.mediaDevices.getUserMedia({ audio: true });
    const listener = new AudioContext();
    await listener.resume();
    const analyser = listener.createAnalyser();
    analyser.fftSize = 512;
    listener.createMediaStreamSource(stream).connect(analyser);
    const samples = new Float32Array(analyser.fftSize);
    const out: { ms: number; rms: number }[] = [];
    const until = Date.now() + 12_000;
    while (Date.now() < until) {
      analyser.getFloatTimeDomainData(samples);
      let sum = 0;
      for (const v of samples) sum += v * v;
      out.push({ ms: Date.now(), rms: Math.sqrt(sum / samples.length) });
      await new Promise((r) => setTimeout(r, 5));
    }
    for (const track of stream.getTracks()) track.stop();
    await listener.close();
    return out;
  });

  expect(statuses.at(-1)).toBe("ready: 80 clips decoded, 0 failed, speech model rotation");
  const settled = levels.filter((l) => l.ms >= levels[0].ms + 500);
  const rms = settled.map((l) => l.rms).sort((a, b) => a - b);
  const onset = 0.01;
  expect(rms[0], "the mic is not digitally silent between clips").toBe(0);
  expect(rms.at(-1)!, "no speech").toBeGreaterThan(onset * 5);

  const firstOnsetBySecond = new Map<number, number>();
  for (let i = 1; i < settled.length; i += 1) {
    const second = Math.floor(settled[i].ms / 1_000);
    const rising = settled[i].rms > onset && settled[i - 1].rms <= onset;
    if (rising && !firstOnsetBySecond.has(second)) {
      firstOnsetBySecond.set(second, settled[i].ms % 1_000);
    }
  }
  const offsets = [...firstOnsetBySecond.values()].sort((a, b) => a - b);
  console.log(`first onset per second, ms after the boundary: ${offsets.join(",")}`);
  expect(offsets.length).toBeGreaterThanOrEqual(8);
  expect(
    offsets[Math.floor(offsets.length / 2)],
    "speech does not start on the second",
  ).toBeLessThan(250);
});

/**
 * In a strip above the clock text: per frame, one colour channel's spatial spread; per pair of
 * consecutive frames, its mean absolute per-pixel difference.
 */
async function backgroundStats(
  page: Page,
  frames: number,
): Promise<{ spreads: number[]; changes: number[] }> {
  return page.evaluate(async (count) => {
    const stream = await navigator.mediaDevices.getUserMedia({ video: true });
    // Chromium-only, absent from the DOM lib typings.
    const Processor = (
      globalThis as unknown as {
        MediaStreamTrackProcessor: new (init: { track: MediaStreamTrack }) => {
          readable: ReadableStream<VideoFrame>;
        };
      }
    ).MediaStreamTrackProcessor;
    const reader = new Processor({ track: stream.getVideoTracks()[0] }).readable.getReader();
    const spreads: number[] = [];
    const changes: number[] = [];
    let previous: Uint8ClampedArray | null = null;
    try {
      for (let i = 0; i < count; i++) {
        const { value: frame } = await reader.read();
        const canvas = new OffscreenCanvas(frame!.displayWidth, frame!.displayHeight);
        const context = canvas.getContext("2d")!;
        context.drawImage(frame!, 0, 0);
        frame!.close();
        const strip = context.getImageData(100, 10, 200, 40).data;
        let sum = 0;
        let squares = 0;
        let change = 0;
        for (let j = 0; j < strip.length; j += 4) {
          sum += strip[j];
          squares += strip[j] * strip[j];
          if (previous !== null) change += Math.abs(strip[j] - previous[j]);
        }
        const n = strip.length / 4;
        spreads.push(Math.sqrt(squares / n - (sum / n) ** 2));
        if (previous !== null) changes.push(change / n);
        previous = strip;
      }
      return { spreads, changes };
    } finally {
      for (const track of stream.getTracks()) track.stop();
    }
  }, frames);
}

for (const [alpha, noisy] of [
  [0.15, true],
  [0, false],
] as const) {
  test(`captured frames ${noisy ? "carry" : "lack"} the noise background at alpha ${alpha} (issue 3089)`, async () => {
    for (let launch = 0; launch < 3; launch++) {
      const fresh = await chromium.launch({ headless: true });
      try {
        const page = await fresh.newPage();
        await installClockSource(page, `globalThis.__CLOCK_VIDEO_NOISE = ${alpha};`);
        const { spreads, changes } = await backgroundStats(page, 8);
        if (noisy) {
          expect(
            spreads.every((s) => s > 3),
            `launch ${launch}: ${spreads}`,
          ).toBe(true);
          // A static background changes in at most one pair (the per-second hue step).
          expect(
            changes.filter((c) => c > 1).length,
            `launch ${launch} moves: ${changes}`,
          ).toBeGreaterThanOrEqual(changes.length - 2);
        } else expect(spreads).toEqual(Array(8).fill(0));
      } finally {
        await fresh.close();
      }
    }
  });
}
