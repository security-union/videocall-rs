/**
 * Real-Chromium coverage for the console-log collector's page-close upload.
 *
 * Chromium rejects a keepalive fetch once the keepalive bodies in flight for
 * the document exceed 64 KiB, and the rejection is asynchronous. The collector
 * is loaded from disk into a page served by a loopback server owned by this
 * spec.
 */
import http from "node:http";
import type { AddressInfo } from "node:net";
import { readFileSync } from "node:fs";
import path from "node:path";
import { test, expect, type Page } from "@playwright/test";

const COLLECTOR_JS = readFileSync(
  path.resolve(__dirname, "../../dioxus-ui/scripts/console-log-collector.js"),
  "utf8",
);
const KEEPALIVE_QUOTA_BYTES = 64 * 1024;
const MEETING_ID = "unload-flush";
const AUTH_TOKEN = "unload-flush-room-token";

interface Chunk {
  chunkSeq: number;
  authorization: string | undefined;
  sessionTs: string | undefined;
  bytes: number;
  entries: { seq: number; level: string; msg: string }[];
  responded: boolean;
  abortedBeforeResponse: boolean;
}

interface Harness {
  base: string;
  chunks: Chunk[];
  holdChunkSeqs: Set<number>;
  /** Held chunks that get their headers and part of the body immediately. */
  headersFirstChunkSeqs: Set<number>;
  releaseHeld(): void;
  close(): Promise<void>;
}

async function startHarness(): Promise<Harness> {
  const chunks: Chunk[] = [];
  const holdChunkSeqs = new Set<number>();
  const headersFirstChunkSeqs = new Set<number>();
  const held: { chunk: Chunk; res: http.ServerResponse }[] = [];
  const respond = (chunk: Chunk, res: http.ServerResponse) => {
    if (chunk.abortedBeforeResponse) return;
    res.end("ok", () => (chunk.responded = true));
  };
  const server = http.createServer((req, res) => {
    if (req.method === "POST") {
      const parts: Buffer[] = [];
      req.on("data", (c: Buffer) => parts.push(c));
      req.on("end", () => {
        const body = Buffer.concat(parts);
        const chunkSeq = Number(req.headers["x-chunk-seq"]);
        const chunk: Chunk = {
          chunkSeq,
          authorization: req.headers["authorization"] as string | undefined,
          sessionTs: req.headers["x-session-timestamp"] as string | undefined,
          bytes: body.length,
          entries: body
            .toString("utf8")
            .split("\n")
            .filter((l) => l.length > 0)
            .map((l) => JSON.parse(l)),
          responded: false,
          abortedBeforeResponse: false,
        };
        chunks.push(chunk);
        res.on("close", () => {
          if (!res.writableEnded) chunk.abortedBeforeResponse = true;
        });
        if (headersFirstChunkSeqs.has(chunkSeq)) {
          res.writeHead(200, { "content-type": "text/plain" });
          res.write("o");
        }
        if (holdChunkSeqs.has(chunkSeq)) held.push({ chunk, res });
        else respond(chunk, res);
      });
      return;
    }
    if (req.url === "/collector.js") {
      res.setHeader("content-type", "text/javascript");
      res.end(COLLECTOR_JS);
      return;
    }
    res.setHeader("content-type", "text/html");
    res.end(
      req.url === "/meeting"
        ? `<!doctype html><script>window.__APP_CONFIG={consoleLogUploadEnabled:"true",hardwareMetricsEnabled:false};</script><script src="/collector.js"></script>`
        : "<!doctype html><p>away</p>",
    );
  });
  await new Promise<void>((r) => server.listen(0, "127.0.0.1", r));
  const { port } = server.address() as AddressInfo;
  return {
    base: `http://127.0.0.1:${port}`,
    chunks,
    holdChunkSeqs,
    headersFirstChunkSeqs,
    releaseHeld: () => held.splice(0).forEach(({ chunk, res }) => respond(chunk, res)),
    close: () =>
      new Promise<void>((r) => {
        server.closeAllConnections();
        server.close(() => r());
      }),
  };
}

async function joinMeeting(page: Page, h: Harness): Promise<void> {
  await page.goto(`${h.base}/meeting`);
  await page.evaluate(
    ([mid, token]) => {
      const c = (
        window as unknown as { __consoleLogCollector: Record<string, (...a: string[]) => void> }
      ).__consoleLogCollector;
      c.setAuthToken(token);
      c.setContext(mid, "user@example.test", "User");
    },
    [MEETING_ID, AUTH_TOKEN],
  );
}

/** Logs `count` entries of `size` × `ch`, then a final `marker` entry. */
async function logBurst(
  page: Page,
  count: number,
  size: number,
  marker: string,
  ch = "x",
): Promise<void> {
  await page.evaluate(
    ([n, sz, m, c]) => {
      for (let i = 0; i < (n as number); i++)
        console.log("burst " + i + " " + (c as string).repeat(sz as number));
      console.log(m);
    },
    [count, size, marker, ch] as const,
  );
}

async function hangupFlush(page: Page): Promise<void> {
  await page.evaluate(() =>
    (
      window as unknown as { __consoleLogCollector: { flush(): void } }
    ).__consoleLogCollector.flush(),
  );
}

async function setVisibility(page: Page, state: "hidden" | "visible"): Promise<void> {
  await page.evaluate((s) => {
    Object.defineProperty(document, "visibilityState", { configurable: true, get: () => s });
    document.dispatchEvent(new Event("visibilitychange"));
  }, state);
}

/** Waits until every chunk the server has seen was answered and the page had time to read it. */
async function settleResponses(page: Page, h: Harness, count: number): Promise<void> {
  await expect.poll(() => h.chunks.filter((c) => c.responded).length).toBeGreaterThanOrEqual(count);
  await page.waitForTimeout(250);
}

function hasMarker(chunks: Chunk[], marker: string): boolean {
  return chunks.some((c) => c.entries.some((e) => e.msg === marker));
}

function droppedSeqs(chunks: Chunk[]): number[] {
  const out: number[] = [];
  for (const e of chunks.flatMap((c) => c.entries)) {
    if (e.level !== "meta") continue;
    const m = /seq (\d+)\.\.(\d+)/.exec(e.msg);
    expect(m, `meta line must name the dropped seq range: ${e.msg}`).toBeTruthy();
    for (let s = Number(m![1]); s <= Number(m![2]); s++) out.push(s);
  }
  return out;
}

/** Every entry was uploaded exactly once, or is named in a `meta` drop line when `allowDrops`. */
function expectEveryEntryOnce(chunks: Chunk[], allowDrops = false): void {
  const dropped = droppedSeqs(chunks);
  if (!allowDrops) expect(dropped, "no entry may be dropped").toEqual([]);
  const sorted = [...chunks.flatMap((c) => c.entries.map((e) => e.seq)), ...dropped].sort(
    (a, b) => a - b,
  );
  expect(new Set(sorted).size, "no log entry may be uploaded twice").toBe(sorted.length);
  expect(sorted, "every log entry must be uploaded").toEqual(sorted.map((_, i) => i));
}

function expectUploadContract(chunks: Chunk[]): void {
  for (const c of chunks) {
    expect(c.authorization).toBe(`Bearer ${AUTH_TOKEN}`);
    expect(c.sessionTs).toMatch(/^\d+$/);
  }
  const bySeq = [...chunks].sort((a, b) => a.chunkSeq - b.chunkSeq);
  expect(new Set(bySeq.map((c) => c.chunkSeq)).size, "X-Chunk-Seq must be unique").toBe(
    bySeq.length,
  );
  for (let i = 1; i < bySeq.length; i++) {
    const prevMax = Math.max(...bySeq[i - 1].entries.map((e) => e.seq));
    const curMin = Math.min(...bySeq[i].entries.map((e) => e.seq));
    expect(curMin, "a later X-Chunk-Seq must carry only later entries").toBeGreaterThan(prevMax);
  }
}

function chunkWith(chunks: Chunk[], marker: string): Chunk {
  return chunks.find((c) => c.entries.some((e) => e.msg === marker))!;
}

test.describe("console-log collector page-close upload @bvt1", () => {
  let h: Harness;

  test.beforeEach(async () => {
    h = await startHarness();
  });

  test.afterEach(async () => {
    await h.close();
  });

  test("uploads every entry when ~80 KB of non-ASCII is logged before page close", async ({
    page,
  }) => {
    await joinMeeting(page, h);
    const marker = "FINAL-MARKER-UTF8";
    await logBurst(page, 40, 1000, marker, "é");
    await settleResponses(page, h, 1);

    await page.goto(`${h.base}/away`);

    await expect.poll(() => hasMarker(h.chunks, marker), { timeout: 5_000 }).toBe(true);
    expectEveryEntryOnce(h.chunks);
    expectUploadContract(h.chunks);
  });

  test("sends the newest entries within the keepalive quota and records the dropped range", async ({
    page,
  }) => {
    h.holdChunkSeqs.add(1);
    await joinMeeting(page, h);
    const marker = "FINAL-MARKER-SMALL-LINES";
    await logBurst(page, 2000, 20, marker);

    await page.goto(`${h.base}/away`);

    await expect.poll(() => hasMarker(h.chunks, marker), { timeout: 5_000 }).toBe(true);
    const held = h.chunks.find((c) => c.chunkSeq === 1)!;
    expect(chunkWith(h.chunks, marker).bytes + held.bytes).toBeLessThanOrEqual(
      KEEPALIVE_QUOTA_BYTES,
    );
    expect(droppedSeqs(h.chunks).length).toBeGreaterThan(0);
    expectEveryEntryOnce(h.chunks, true);
    expectUploadContract(h.chunks);
  });

  test("measures the close budget in UTF-8 bytes", async ({ page }) => {
    h.holdChunkSeqs.add(1);
    await joinMeeting(page, h);
    const marker = "FINAL-MARKER-UTF8-HELD";
    await logBurst(page, 40, 1000, marker, "ế");

    await page.goto(`${h.base}/away`);

    await expect.poll(() => hasMarker(h.chunks, marker), { timeout: 5_000 }).toBe(true);
    expectEveryEntryOnce(h.chunks, true);
  });

  test("a regular upload in flight at page close is not cancelled by the unload", async ({
    page,
  }) => {
    h.holdChunkSeqs.add(1);
    await joinMeeting(page, h);
    await logBurst(page, 50, 1000, "TAIL");
    await expect.poll(() => h.chunks.length).toBeGreaterThan(0);

    await page.goto(`${h.base}/away`);
    await expect.poll(() => hasMarker(h.chunks, "TAIL"), { timeout: 5_000 }).toBe(true);
    await page.waitForTimeout(500);
    h.releaseHeld();

    await expect.poll(() => h.chunks.find((c) => c.chunkSeq === 1)!.responded).toBe(true);
    expect(h.chunks.find((c) => c.chunkSeq === 1)!.abortedBeforeResponse).toBe(false);
    expectEveryEntryOnce(h.chunks);
  });

  test("hangup flush still in flight leaves room for the close upload", async ({ page }) => {
    h.holdChunkSeqs.add(1);
    await joinMeeting(page, h);
    await logBurst(page, 40, 1000, "BEFORE-HANGUP");
    await hangupFlush(page);
    await expect.poll(() => hasMarker(h.chunks, "BEFORE-HANGUP")).toBe(true);
    const marker = "FINAL-MARKER-AFTER-HANGUP";
    await logBurst(page, 29, 1000, marker);

    await page.goto(`${h.base}/away`);

    await expect.poll(() => hasMarker(h.chunks, marker), { timeout: 5_000 }).toBe(true);
    expectEveryEntryOnce(h.chunks, true);
    expectUploadContract(h.chunks);
  });

  test("a completed keepalive returns its quota", async ({ page }) => {
    await joinMeeting(page, h);
    await logBurst(page, 38, 1000, "FIRST");
    await setVisibility(page, "hidden");
    await settleResponses(page, h, 1);
    await setVisibility(page, "visible");
    const marker = "SECOND";
    await logBurst(page, 38, 1000, marker);
    await setVisibility(page, "hidden");

    await expect.poll(() => hasMarker(h.chunks, marker), { timeout: 5_000 }).toBe(true);
    expectEveryEntryOnce(h.chunks);
    expectUploadContract(h.chunks);
  });

  test("a keepalive's quota is not reused before its response body is read", async ({ page }) => {
    h.holdChunkSeqs.add(1);
    h.headersFirstChunkSeqs.add(1);
    await joinMeeting(page, h);
    await logBurst(page, 38, 1000, "FIRST");
    // Hide again in the task right after the first keepalive's headers arrive.
    await page.evaluate(() => {
      const orig = window.fetch;
      window.fetch = function (...args: Parameters<typeof fetch>) {
        window.fetch = orig;
        const p = orig.apply(this, args);
        p.then(() =>
          setTimeout(() => {
            for (let i = 0; i < 38; i++) console.log("second " + i + " " + "x".repeat(1000));
            console.log("SECOND");
            document.dispatchEvent(new Event("visibilitychange"));
          }, 0),
        );
        return p;
      };
      Object.defineProperty(document, "visibilityState", {
        configurable: true,
        get: () => "hidden",
      });
      document.dispatchEvent(new Event("visibilitychange"));
    });
    await expect.poll(() => hasMarker(h.chunks, "FIRST")).toBe(true);
    await page.waitForTimeout(250);
    h.releaseHeld();
    await settleResponses(page, h, 2);
    await setVisibility(page, "visible");

    await expect.poll(() => hasMarker(h.chunks, "SECOND"), { timeout: 5_000 }).toBe(true);
    expectEveryEntryOnce(h.chunks);
    expectUploadContract(h.chunks);
  });

  test("a second regular upload follows the first", async ({ page }) => {
    await joinMeeting(page, h);
    await logBurst(page, 50, 1000, "FIRST");
    await settleResponses(page, h, 1);
    await logBurst(page, 50, 1000, "SECOND");

    await expect.poll(() => h.chunks.length, { timeout: 5_000 }).toBeGreaterThanOrEqual(2);
    expectEveryEntryOnce(h.chunks);
    expectUploadContract(h.chunks);
  });

  test("hangup flush with more than 64 KiB buffered keeps the rest for a later upload", async ({
    page,
  }) => {
    h.holdChunkSeqs.add(1);
    await joinMeeting(page, h);
    await logBurst(page, 115, 1000, "BUFFERED");
    await expect.poll(() => h.chunks.length).toBe(1);
    await hangupFlush(page);
    await expect.poll(() => h.chunks.length).toBe(2);
    h.releaseHeld();
    await settleResponses(page, h, 2);
    await logBurst(page, 0, 0, "LATER");

    await expect.poll(() => hasMarker(h.chunks, "LATER"), { timeout: 5_000 }).toBe(true);
    const later = h.chunks.find((c) => c.entries.some((e) => e.msg === "LATER"));
    expect(later?.entries.some((e) => e.msg === "BUFFERED")).toBe(true);
    expectEveryEntryOnce(h.chunks);
    expectUploadContract(h.chunks);
  });

  test("a hide that cannot fit everything keeps the rest for the next upload", async ({ page }) => {
    h.holdChunkSeqs.add(1);
    await joinMeeting(page, h);
    await logBurst(page, 38, 1000, "FIRST");
    await setVisibility(page, "hidden");
    await expect.poll(() => hasMarker(h.chunks, "FIRST")).toBe(true);
    const marker = "SECOND";
    await logBurst(page, 38, 1000, marker);
    await setVisibility(page, "hidden");
    await setVisibility(page, "visible");

    await expect.poll(() => hasMarker(h.chunks, marker), { timeout: 5_000 }).toBe(true);
    expectEveryEntryOnce(h.chunks);
    expectUploadContract(h.chunks);
  });

  test("hide, show, hide, then close uploads every entry exactly once", async ({ page }) => {
    await joinMeeting(page, h);
    await logBurst(page, 5, 100, "A");
    await setVisibility(page, "hidden");
    await expect.poll(() => hasMarker(h.chunks, "A")).toBe(true);

    await logBurst(page, 5, 100, "B");
    await setVisibility(page, "visible");
    await logBurst(page, 5, 100, "C");
    await setVisibility(page, "hidden");
    await expect.poll(() => hasMarker(h.chunks, "C")).toBe(true);

    await logBurst(page, 5, 100, "D-written-while-hidden");
    await page.goto(`${h.base}/away`);

    await expect
      .poll(() => hasMarker(h.chunks, "D-written-while-hidden"), { timeout: 5_000 })
      .toBe(true);
    expectEveryEntryOnce(h.chunks);
    expectUploadContract(h.chunks);
  });
});
