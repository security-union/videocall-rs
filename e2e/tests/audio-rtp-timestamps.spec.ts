import { test, expect, chromium, Browser, Page } from "@playwright/test";
import { generateSessionToken } from "../helpers/auth";
import { waitForServices } from "../helpers/wait-for-services";

/**
 * Audio packets carry a real RTP media timestamp end to end.
 *
 * Before `AudioMetadata.rtp_timestamp` existed the receiver cast the
 * wall-clock `MediaPacket.timestamp` (ms since 1970) to u32, so every packet
 * reached the NetEq jitter buffer with timestamp 4294967295. The sender now
 * stamps each 20 ms Opus frame on a 48 kHz sample clock (+960 per frame) and
 * jumps it forward across a mute, and the receiver hands that value to the
 * NetEq worker.
 *
 * We observe exactly what NetEq is fed: an init script on the receiving page
 * wraps `Worker.prototype.postMessage` and records every
 * `{cmd: "insert", seq, timestamp, payload}` message (shape: `WorkerMsg` in
 * videocall-client/src/decode/neteq_audio_decoder.rs).
 */

const COOKIE_NAME = process.env.COOKIE_NAME || "session";

const BROWSER_ARGS = [
  "--ignore-certificate-errors",
  "--origin-to-force-quic-on=127.0.0.1:4433",
  "--use-fake-device-for-media-stream",
  "--use-fake-ui-for-media-stream",
  "--disable-gpu",
];

/** Samples per 20 ms Opus frame at the 48 kHz RTP clock. */
const FRAME = 960;
const SAMPLES_PER_MS = 48;
const U32 = 2 ** 32;
const U16 = 2 ** 16;
/** What every packet carried before the fix: wall-clock ms saturated to u32. */
const SATURATED_U32 = U32 - 1;
const MIN_PACKETS = 100;

interface Insert {
  worker: number;
  seq: number;
  timestamp: number;
  at: number;
}

interface Pair {
  seqDelta: number;
  tsDelta: number;
  prev: Insert;
  next: Insert;
}

declare global {
  interface Window {
    __netEqInserts?: Insert[];
  }
}

/** Runs in the page before the app loads; records every NetEq insert. */
function captureNetEqInserts() {
  const inserts: Insert[] = [];
  const workerIds = new WeakMap<Worker, number>();
  let nextWorkerId = 0;
  const originalPostMessage = Worker.prototype.postMessage;
  Worker.prototype.postMessage = function (
    this: Worker,
    message: unknown,
    ...rest: unknown[]
  ): void {
    const msg = message as { cmd?: unknown; seq?: unknown; timestamp?: unknown } | null;
    if (msg && typeof msg === "object" && msg.cmd === "insert") {
      let id = workerIds.get(this);
      if (id === undefined) {
        id = nextWorkerId++;
        workerIds.set(this, id);
      }
      inserts.push({
        worker: id,
        seq: Number(msg.seq),
        timestamp: Number(msg.timestamp),
        at: performance.now(),
      });
    }
    return (originalPostMessage as (...args: unknown[]) => void).call(this, message, ...rest);
  };
  window.__netEqInserts = inserts;
}

async function createAuthenticatedContext(
  browser: Browser,
  email: string,
  name: string,
  uiURL: string,
) {
  const context = await browser.newContext({ baseURL: uiURL, ignoreHTTPSErrors: true });
  const url = new URL(uiURL);
  await context.addCookies([
    {
      name: COOKIE_NAME,
      value: generateSessionToken(email, name),
      domain: url.hostname,
      path: "/",
      httpOnly: true,
      secure: false,
      sameSite: "Lax",
    },
  ]);
  return context;
}

async function navigateToMeeting(page: Page, meetingId: string, username: string) {
  await page.goto("/");
  await page.locator("#meeting-id").click();
  await page.locator("#meeting-id").pressSequentially(meetingId, { delay: 50 });
  await page.locator("#username").click();
  await page.locator("#username").fill("");
  await page.locator("#username").pressSequentially(username, { delay: 50 });
  await page.locator("#username").press("Enter");
  await expect(page).toHaveURL(new RegExp(`/meeting/${meetingId}`), { timeout: 10_000 });
}

/** Click through Start/Join (admitting from the waiting room if needed). */
async function enterMeeting(page: Page, hostPage?: Page) {
  const joinButton = page.getByText(/Start Meeting|Join Meeting/);
  const waitingRoom = page.getByText("Waiting to be admitted");
  const grid = page.locator("#grid-container");

  const state = await Promise.race([
    joinButton.waitFor({ timeout: 20_000 }).then(() => "join" as const),
    waitingRoom.waitFor({ timeout: 20_000 }).then(() => "waiting" as const),
  ]);

  if (state === "waiting") {
    expect(hostPage, "guest landed in the waiting room").toBeDefined();
    const admit = hostPage!.getByTitle("Admit").first();
    await expect(admit).toBeVisible({ timeout: 20_000 });
    await admit.dispatchEvent("click");
    const afterAdmit = await Promise.race([
      joinButton.waitFor({ timeout: 20_000 }).then(() => "join" as const),
      grid.waitFor({ timeout: 20_000 }).then(() => "grid" as const),
    ]);
    if (afterAdmit === "grid") return;
  }

  // The button can render before its handler is wired; retry the click
  // until the grid shows up instead of sleeping.
  await expect(async () => {
    if (await joinButton.isVisible()) await joinButton.click();
    await expect(grid).toBeVisible({ timeout: 3_000 });
  }).toPass({ timeout: 20_000 });
}

/** The mic control button whose tooltip currently reads `label`. */
function micButton(page: Page, label: "Mute" | "Unmute") {
  return page
    .locator(".video-controls-container button")
    .filter({ has: page.locator(".tooltip", { hasText: new RegExp(`^${label}$`) }) });
}

async function readInserts(page: Page): Promise<Insert[]> {
  return page.evaluate(() => window.__netEqInserts ?? []);
}

/**
 * The inserts fed to the one NetEq worker that got audio, deduplicated by
 * sequence (a RED-recovered frame can repeat a sequence) and sorted by it.
 */
function streamOf(inserts: Insert[]): Insert[] {
  const counts = new Map<number, number>();
  for (const i of inserts) counts.set(i.worker, (counts.get(i.worker) ?? 0) + 1);
  const workers = [...counts.keys()];
  expect(workers, "exactly one NetEq worker received inserts").toHaveLength(1);

  const bySeq = new Map<number, Insert>();
  for (const i of inserts) {
    const seen = bySeq.get(i.seq);
    if (seen) {
      expect(i.timestamp, `seq ${i.seq} inserted twice with different timestamps`).toBe(
        seen.timestamp,
      );
    } else {
      bySeq.set(i.seq, i);
    }
  }
  // Sequences stay far below 2^16 in this test, so plain numeric order is
  // sequence order.
  return [...bySeq.values()].sort((a, b) => a.seq - b.seq);
}

function pairsOf(stream: Insert[]): Pair[] {
  const pairs: Pair[] = [];
  for (let k = 1; k < stream.length; k++) {
    const prev = stream[k - 1];
    const next = stream[k];
    pairs.push({
      seqDelta: (next.seq - prev.seq + U16) % U16,
      tsDelta: (next.timestamp - prev.timestamp + U32) % U32,
      prev,
      next,
    });
  }
  return pairs;
}

/** Longest run of consecutive sequences whose timestamps step by exactly 960. */
function longestContiguousRun(pairs: Pair[]): number {
  let best = 0;
  let run = 0;
  for (const p of pairs) {
    run = p.seqDelta === 1 && p.tsDelta === FRAME ? run + 1 : 0;
    best = Math.max(best, run);
  }
  return best === 0 ? 0 : best + 1;
}

function histogram(values: number[]): Record<string, number> {
  const out: Record<string, number> = {};
  for (const v of values) out[v] = (out[v] ?? 0) + 1;
  return out;
}

test.describe("Audio RTP timestamps reach NetEq", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test("timestamps advance 960 per frame and jump forward across a mute", async ({ baseURL }) => {
    test.setTimeout(180_000);
    const uiURL = baseURL || "http://localhost:80";
    const meetingId = `e2e_rtp_ts_${Date.now()}`;

    const receiverBrowser = await chromium.launch({ args: BROWSER_ARGS });
    const senderBrowser = await chromium.launch({ args: BROWSER_ARGS });

    try {
      // ---- Arrange: receiver hosts (and records NetEq inserts), sender joins ----
      const receiverCtx = await createAuthenticatedContext(
        receiverBrowser,
        "rtp-receiver@videocall.rs",
        "RtpReceiver",
        uiURL,
      );
      await receiverCtx.addInitScript(captureNetEqInserts);
      const senderCtx = await createAuthenticatedContext(
        senderBrowser,
        "rtp-sender@videocall.rs",
        "RtpSender",
        uiURL,
      );
      const receiver = await receiverCtx.newPage();
      const sender = await senderCtx.newPage();

      await navigateToMeeting(receiver, meetingId, "RtpReceiver");
      await enterMeeting(receiver);
      await navigateToMeeting(sender, meetingId, "RtpSender");
      await enterMeeting(sender, receiver);

      await expect(receiver.locator("#grid-container .canvas-container").first()).toBeVisible({
        timeout: 30_000,
      });
      // Both mics start muted, so nothing has reached NetEq yet.
      expect(await readInserts(receiver)).toHaveLength(0);

      // ---- Act 1: sender un-mutes its fake microphone ----
      await micButton(sender, "Unmute").first().click();
      await expect(micButton(sender, "Mute").first()).toBeAttached({ timeout: 10_000 });

      await expect
        .poll(async () => streamOf(await readInserts(receiver)).length, {
          message: `receiver NetEq gets at least ${MIN_PACKETS * 1.5} audio frames`,
          timeout: 30_000,
        })
        .toBeGreaterThanOrEqual(MIN_PACKETS * 1.5);

      // ---- Assert 1: real, frame-spaced timestamps ----
      const before = streamOf(await readInserts(receiver));
      const beforePairs = pairsOf(before);
      const distinct = new Set(before.map((i) => i.timestamp)).size;
      const missing = before[before.length - 1].seq - before[0].seq + 1 - before.length;
      console.log(
        JSON.stringify({
          phase: "unmuted",
          packets: before.length,
          distinctTimestamps: distinct,
          firstSeq: before[0].seq,
          lastSeq: before[before.length - 1].seq,
          missingSeqs: missing,
          tsDeltaPerSeqHistogram: histogram(beforePairs.map((p) => p.tsDelta / p.seqDelta)),
          longestContiguousRun: longestContiguousRun(beforePairs),
        }),
      );

      expect(
        before.filter((i) => i.timestamp === SATURATED_U32),
        "no packet carries the saturated wall-clock timestamp",
      ).toHaveLength(0);
      expect(distinct, "every sequence has its own timestamp").toBe(before.length);
      for (const p of beforePairs) {
        // Across a lost packet the timestamp still advances one frame per
        // sequence; a send pause may only add whole frames, never subtract.
        expect(p.tsDelta % FRAME, `ts delta at seq ${p.next.seq} is whole frames`).toBe(0);
        expect(
          p.tsDelta,
          `ts at seq ${p.next.seq} advances at least one frame per sequence`,
        ).toBeGreaterThanOrEqual(p.seqDelta * FRAME);
      }
      expect(longestContiguousRun(beforePairs)).toBeGreaterThanOrEqual(MIN_PACKETS);
      // Localhost: anything beyond a stray packet is not "genuine loss".
      expect(missing, "sequence numbers are contiguous apart from rare loss").toBeLessThanOrEqual(
        Math.ceil(before.length * 0.02),
      );

      // ---- Act 2: mute for about 2 s, then un-mute ----
      await micButton(sender, "Mute").first().click();
      await expect(micButton(sender, "Unmute").first()).toBeAttached({ timeout: 10_000 });
      const mutedAt = Date.now();
      // Confirm the stream actually stopped at the receiver.
      await expect
        .poll(
          async () =>
            receiver.evaluate(() => {
              const all = window.__netEqInserts ?? [];
              return all.length === 0 ? Infinity : performance.now() - all[all.length - 1].at;
            }),
          { message: "audio stops arriving after mute", timeout: 10_000 },
        )
        .toBeGreaterThan(300);
      const lastSeqBeforeMute = Math.max(...(await readInserts(receiver)).map((i) => i.seq));
      // The mute itself is the stimulus: hold it for ~2 s of wall time.
      await sender.waitForTimeout(Math.max(0, 2_000 - (Date.now() - mutedAt)));
      const unmutingAt = Date.now();
      const mutedMs = unmutingAt - mutedAt;
      await micButton(sender, "Unmute").first().click();
      await expect(micButton(sender, "Mute").first()).toBeAttached({ timeout: 10_000 });

      await expect
        .poll(
          async () =>
            streamOf(await readInserts(receiver)).filter((i) => i.seq > lastSeqBeforeMute).length,
          { message: `receiver gets ${MIN_PACKETS} frames after un-mute`, timeout: 30_000 },
        )
        .toBeGreaterThanOrEqual(MIN_PACKETS);

      // ---- Assert 2: sequence continues, timestamp jumps forward by the muted time ----
      const all = streamOf(await readInserts(receiver));
      const allPairs = pairsOf(all);
      const muteIndex = allPairs.findIndex((p) => p.prev.seq === lastSeqBeforeMute);
      expect(muteIndex, "a frame follows the last pre-mute frame").toBeGreaterThanOrEqual(0);
      const across = allPairs[muteIndex];
      const jumpFrames = across.tsDelta / FRAME;
      const jumpMs = across.tsDelta / SAMPLES_PER_MS;
      const afterPairs = allPairs.slice(muteIndex + 1);
      console.log(
        JSON.stringify({
          phase: "after-unmute",
          lastSeqBeforeMute,
          firstSeqAfterUnmute: across.next.seq,
          tsBefore: across.prev.timestamp,
          tsAfter: across.next.timestamp,
          jumpSamples: across.tsDelta,
          jumpFrames,
          jumpMs,
          mutedWallMs: mutedMs,
          packetsAfter: afterPairs.length + 1,
          tsDeltaPerSeqHistogramAfter: histogram(afterPairs.map((p) => p.tsDelta / p.seqDelta)),
          totalPackets: all.length,
          distinctTimestamps: new Set(all.map((i) => i.timestamp)).size,
        }),
      );

      expect(across.next.seq, "sequence does not restart at 0").not.toBe(0);
      expect(across.seqDelta, "sequence continues where it stopped").toBeLessThanOrEqual(3);
      expect(across.tsDelta % FRAME, "jump is whole frames").toBe(0);
      expect(across.tsDelta, "jump is forward by more than the frames sent").toBeGreaterThan(
        across.seqDelta * FRAME,
      );
      expect(jumpMs, "jump is roughly the muted time").toBeGreaterThanOrEqual(1_000);
      expect(jumpMs, "jump is roughly the muted time").toBeLessThanOrEqual(4_000);
      // No frame was captured while the mic was muted, and the clock
      // truncates to whole frames, so the jump covers the muted wall time.
      expect(jumpMs, "jump covers the measured muted interval").toBeGreaterThanOrEqual(
        mutedMs - 2 * (FRAME / SAMPLES_PER_MS),
      );
      for (const p of afterPairs) {
        expect(p.tsDelta % FRAME).toBe(0);
        expect(p.tsDelta).toBeGreaterThanOrEqual(p.seqDelta * FRAME);
      }
      expect(longestContiguousRun(afterPairs)).toBeGreaterThanOrEqual(MIN_PACKETS * 0.9);
    } finally {
      await receiverBrowser.close();
      await senderBrowser.close();
    }
  });
});
