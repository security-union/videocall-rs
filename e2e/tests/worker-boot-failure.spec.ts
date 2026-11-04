import { test, expect, chromium, Browser, Page, Route } from "@playwright/test";
import { continuousToneWavPath } from "../helpers/audio-fixtures";
import { BROWSER_ARGS, createAuthenticatedContext } from "../helpers/auth-context";
import { fillAndSubmitJoinForm } from "../helpers/join-meeting";
import { createMeeting } from "../helpers/meeting-api";
import { setRuntimeConfig } from "../helpers/runtime-config";
import {
  enterMeetingAsHost,
  guestJoinsMeeting,
  joinMeetingFromPage,
} from "../helpers/two-user-meeting";
import { waitForServices } from "../helpers/wait-for-services";

type Outcome = "workerReady" | `onerror: ${string}` | "timeout";

async function workerUrl(page: Page, linkId: string): Promise<string> {
  return page.evaluate(async (id) => {
    const html = await (await fetch("/")).text();
    const link = new DOMParser().parseFromString(html, "text/html").getElementById(id);
    return new URL(link!.getAttribute("href")!, location.origin).href;
  }, linkId);
}

/** Fill the tab's wasm budget, free `leave` memories, then start `count` workers at `url`. */
async function startWithFreeMemories(
  page: Page,
  url: string,
  leave: number,
  count = 1,
): Promise<Outcome> {
  return page.evaluate(
    async ({ url, leave, count }) => {
      // No maximum, as the app's worker wasm declares.
      const bytes = new Uint8Array([
        0, 97, 115, 109, 1, 0, 0, 0, 5, 3, 1, 0, 18, 7, 7, 1, 3, 109, 101, 109, 2, 0,
      ]);
      const held: WebAssembly.Instance[] = [];
      for (;;) {
        try {
          held.push((await WebAssembly.instantiate(bytes)).instance);
        } catch {
          break;
        }
      }
      held.splice(held.length - leave, leave);
      (globalThis as unknown as { gc: () => void }).gc();
      const workers: Worker[] = [];
      (globalThis as unknown as { __held: unknown }).__held = [held, workers];
      const start = () =>
        new Promise<Outcome>((resolve) => {
          const worker = new Worker(url);
          workers.push(worker);
          const timer = setTimeout(() => resolve("timeout"), 20_000);
          worker.onerror = (e) => {
            clearTimeout(timer);
            resolve(`onerror: ${e.message}`);
          };
          worker.onmessage = (e) => {
            if (e.data && (e.data.type === "workerReady" || e.data.kind === "worker_ready")) {
              clearTimeout(timer);
              resolve("workerReady");
            }
          };
        });
      for (let i = 0; i < count; i++) {
        const outcome = await start();
        if (outcome !== "workerReady") return outcome;
      }
      return "workerReady";
    },
    { url, leave, count },
  );
}

interface Result {
  outcome: Outcome;
  workerConsole: string[];
}

async function outcomeFor(
  baseURL: string,
  linkId: string,
  leave: number,
  count = 1,
): Promise<Result> {
  const browser = await chromium.launch({ args: [...BROWSER_ARGS, "--js-flags=--expose-gc"] });
  try {
    const page = await browser.newPage({ baseURL });
    const workerConsole: string[] = [];
    page.on("worker", (w) => w.on("console", (m) => workerConsole.push(m.text())));
    await page.goto("/config.js");
    const outcome = await startWithFreeMemories(page, await workerUrl(page, linkId), leave, count);
    return { outcome, workerConsole };
  } finally {
    await browser.close();
  }
}

test.describe("#3210 a worker that cannot get its wasm memory reports a failed boot", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test("NetEq worker with one wasm memory left reports the opus-decoder failure @bvt1", async ({
    baseURL,
  }) => {
    test.setTimeout(120_000);
    const ui = baseURL || "http://localhost:3001";
    const control = await outcomeFor(ui, "neteq-worker", 5);
    expect(control.outcome, "control: five free memories boot NetEq").toBe("workerReady");

    const oneLeft = await outcomeFor(ui, "neteq-worker", 1);
    expect(oneLeft.outcome).toMatch(/^onerror: .*Out of memory/);
    expect(oneLeft.workerConsole, "the worker's boot-rejection handler fired").toContainEqual(
      expect.stringContaining("[neteq-worker] boot failed"),
    );
  });

  test("two NetEq workers boot with four wasm memories left @bvt1", async ({ baseURL }) => {
    test.setTimeout(120_000);
    const ui = baseURL || "http://localhost:3001";
    expect((await outcomeFor(ui, "neteq-worker", 4, 2)).outcome).toBe("workerReady");
  });

  test("decoder and NetEq workers with no wasm memory left report it @bvt1", async ({
    baseURL,
  }) => {
    test.setTimeout(120_000);
    const ui = baseURL || "http://localhost:3001";
    expect((await outcomeFor(ui, "codecs-worker", 0)).outcome).toMatch(/^onerror: .*Out of memory/);
    expect((await outcomeFor(ui, "neteq-worker", 0)).outcome).toMatch(/^onerror: .*Out of memory/);
  });

  // #3277. Peer worker scripts are served one at a time in the order the client started the
  // workers, so each boot's memory is exact: NetEq 2, decoder 1. The joining tab runs without the
  // test-only mock-peer decoder, as in production.
  test("a peer's screen decoder waits for a share, so a talker fits three free memories @bvt1", async ({
    baseURL,
  }) => {
    test.setTimeout(180_000);
    const ui = baseURL || "http://localhost:3001";
    const meetingId = `boot_lazy_screen_${Date.now()}`;
    const hostBrowser = await chromium.launch({ args: [...BROWSER_ARGS, EXPOSE_GC] });
    const talkerBrowser = await chromium.launch({
      args: [...BROWSER_ARGS, `--use-file-for-fake-audio-capture=${continuousToneWavPath()}`],
    });
    try {
      const host = await joinerPage(hostBrowser, ui, "BootLazyHost", false);
      const held = await holdPeerWorkerScripts(host);
      const failures = failedBoots(host);
      await enterMeetingAsHost(host, meetingId, "BootLazyHost");
      const talker = await joinerPage(talkerBrowser, ui, "BootLazyTalker", true);
      await guestJoinsMeeting(host, talker, meetingId, "BootLazyTalker");

      await collectGarbage(host);
      await host.evaluate(holdAllWasmMemoriesBut, 3);
      await bootOneAtATime(host, held);

      expect(failures, "the talker's NetEq and camera decoder fit").toEqual([]);
      const boots = await readBoots(host);
      expect(started(boots, "decoder"), "no screen decoder boots").toBe(1);
      expect(started(boots, "neteq")).toBe(1);
      await expectEveryTalkerHeard(host, 1);
    } finally {
      await Promise.all([hostBrowser.close(), talkerBrowser.close()]);
    }
  });

  test("a late joiner into a full tab boots every talker's NetEq before the camera decoders", async ({
    baseURL,
  }) => {
    test.setTimeout(240_000);
    const ui = baseURL || "http://localhost:3001";
    const meetingId = `boot_burst_${Date.now()}`;
    const talkers = ["BootBurstA", "BootBurstB", "BootBurstC"];
    await createMeeting(`${talkers[0].toLowerCase()}@videocall.rs`, talkers[0], { meetingId });
    const talkerBrowser = await chromium.launch({
      args: [
        ...BROWSER_ARGS.filter((a) => !a.startsWith("--renderer-process-limit")),
        `--use-file-for-fake-audio-capture=${continuousToneWavPath()}`,
      ],
    });
    const joinerBrowser = await chromium.launch({ args: [...BROWSER_ARGS, EXPOSE_GC] });
    try {
      const [owner, ...rest] = await Promise.all(
        talkers.map((name) => joinerPage(talkerBrowser, ui, name, true)),
      );
      await fillAndSubmitJoinForm(owner, meetingId, talkers[0]);
      expect(await joinMeetingFromPage(owner)).toBe("in-meeting");
      await Promise.all(
        rest.map(async (page, i) => {
          await fillAndSubmitJoinForm(page, meetingId, talkers[i + 1]);
          expect(await joinMeetingFromPage(page)).toBe("in-meeting");
        }),
      );

      const joiner = await joinerPage(joinerBrowser, ui, "BootBurstLate", false);
      const held = await holdPeerWorkerScripts(joiner);
      const failures = failedBoots(joiner);
      await fillAndSubmitJoinForm(joiner, meetingId, "BootBurstLate");
      expect(await joinMeetingFromPage(joiner)).toBe("in-meeting");
      for (const name of talkers) {
        await expect(
          joiner.locator("#grid-container h4.floating-name", { hasText: name }),
        ).toBeVisible({ timeout: 30_000 });
      }
      await collectGarbage(joiner);
      // Three NetEq workers and two camera decoders; the third decoder is refused.
      await joiner.evaluate(holdAllWasmMemoriesBut, 8);
      await bootOneAtATime(joiner, held);

      expect(
        failures.filter((line) => line.startsWith("NetEq")),
        "every talker's NetEq boots",
      ).toEqual([]);
      await expectEveryTalkerHeard(joiner, talkers.length);
      expect(
        failures.filter((line) => line.includes("decoder worker")),
        "the tab was full: one camera decoder was refused",
      ).toHaveLength(1);
    } finally {
      await Promise.all([talkerBrowser.close(), joinerBrowser.close()]);
    }
  });
});

const EXPOSE_GC = "--js-flags=--expose-gc";

const WORKER_PROBE = `(() => {
  const W = globalThis.Worker;
  const boots = [];
  globalThis.Worker = class extends W {
    constructor(url, opts) {
      const kind = /neteq_worker/.test(String(url)) ? "neteq"
        : /worker_decoder/.test(String(url)) ? "decoder" : null;
      super(kind ? String(url) + "?boot=" + boots.length : url, opts);
      if (!kind) return;
      const boot = { kind, settled: false, terminated: false, packetsPerSec: 0, decodedSamples: 0 };
      boots.push(boot);
      const terminate = this.terminate.bind(this);
      this.terminate = () => {
        boot.terminated = true;
        terminate();
      };
      this.addEventListener("error", () => (boot.settled = true));
      this.addEventListener("message", (e) => {
        const d = e.data || {};
        if (d.type === "workerReady" || d.kind === "worker_ready") boot.settled = true;
        if (d.cmd === "stats" && d.stats) {
          const life = d.stats.lifetime || {};
          boot.packetsPerSec = Math.max(boot.packetsPerSec, Number(d.stats.packets_per_sec) || 0);
          boot.decodedSamples =
            (Number(life.jitter_buffer_emitted_count) || 0) - (Number(life.concealed_samples) || 0);
        }
      });
    }
  };
  globalThis.__peerWorkerBoots = () => JSON.parse(JSON.stringify(boots));
})();`;

interface Boot {
  kind: "neteq" | "decoder";
  settled: boolean;
  terminated: boolean;
  packetsPerSec: number;
  decodedSamples: number;
}

async function readBoots(page: Page): Promise<Boot[]> {
  return page.evaluate(() =>
    (globalThis as unknown as { __peerWorkerBoots: () => Boot[] }).__peerWorkerBoots(),
  );
}

function started(boots: Boot[], kind: Boot["kind"]): number {
  return boots.filter((b) => b.kind === kind && !b.terminated).length;
}

async function joinerPage(browser: Browser, ui: string, name: string, micOn: boolean) {
  const ctx = await createAuthenticatedContext(
    browser,
    `${name.toLowerCase()}@videocall.rs`,
    name,
    ui,
  );
  await ctx.addInitScript(
    `localStorage.setItem("vc_prejoin_camera_on", "false");` +
      `localStorage.setItem("vc_prejoin_mic_on", "${micOn}");`,
  );
  await ctx.addInitScript(WORKER_PROBE);
  return ctx.newPage();
}

/** Holds each peer worker's script until `bootOneAtATime` serves it, keyed by start order. */
async function holdPeerWorkerScripts(page: Page): Promise<Map<number, Route>> {
  const held = new Map<number, Route>();
  await setRuntimeConfig(page.context(), { mockPeersEnabled: "false" });
  await page.context().route(/_loader[^/]*\.js\?boot=\d+$/, (route) => {
    held.set(Number(/boot=(\d+)$/.exec(route.request().url())![1]), route);
  });
  return held;
}

function failedBoots(page: Page): string[] {
  const failures: string[] = [];
  page.on("console", (m) => {
    const failed = /(NetEq worker|decoder worker) for peer .* failed to start/.exec(m.text());
    if (failed) failures.push(m.text().slice(failed.index));
  });
  return failures;
}

async function collectGarbage(page: Page): Promise<void> {
  const gc = () => (globalThis as unknown as { gc: () => void }).gc();
  const bounded = (p: Promise<unknown>) =>
    Promise.race([p.catch(() => undefined), new Promise((r) => setTimeout(r, 2_000))]);
  await Promise.all([page, ...page.workers()].map((target) => bounded(target.evaluate(gc))));
}

async function until(done: () => Promise<boolean>, ms: number): Promise<boolean> {
  for (const end = Date.now() + ms; Date.now() < end; ) {
    if (await done()) return true;
    await new Promise((resolve) => setTimeout(resolve, 100));
  }
  return false;
}

/** Serves held scripts in the order the client started the workers, each after the last settles. */
async function bootOneAtATime(page: Page, held: Map<number, Route>): Promise<void> {
  const boot = async (i: number) => (await readBoots(page))[i];
  for (let next = 0; next < 40; next += 1) {
    if (!(await until(async () => (await readBoots(page)).length > next, 5_000))) return;
    if (!(await until(async () => held.has(next) || (await boot(next)).terminated, 10_000))) {
      throw new Error(`worker ${next} never requested its script`);
    }
    if ((await boot(next)).terminated) continue;
    await held.get(next)!.continue();
    const settled = async () => {
      const b = await boot(next);
      return b.settled || b.terminated;
    };
    if (!(await until(settled, 30_000))) throw new Error(`worker ${next} never settled`);
  }
  throw new Error("peer workers kept restarting");
}

/** Each NetEq worker's own stats: packets inserted, and samples decoded rather than concealed. */
async function expectEveryTalkerHeard(page: Page, talkers: number): Promise<void> {
  await expect
    .poll(
      async () =>
        (await readBoots(page)).filter((b) => b.packetsPerSec > 0 && b.decodedSamples > 0).length,
      { timeout: 30_000, message: "each talker's audio is decoded by its own NetEq worker" },
    )
    .toBe(talkers);
}

/** Fill the tab's budget in a probe worker, free it, then hold all but `leave` memories. */
async function holdAllWasmMemoriesBut(leave: number): Promise<void> {
  const src = `
const bytes = new Uint8Array([0,97,115,109,1,0,0,0,5,3,1,0,18,7,7,1,3,109,101,109,2,0]);
const held = [];
self.onmessage = async (e) => {
  while (e.data < 0 || held.length < e.data) {
    try { held.push((await WebAssembly.instantiate(bytes)).instance); } catch { break; }
  }
  self.postMessage(held.length);
};`;
  const url = URL.createObjectURL(new Blob([src], { type: "text/javascript" }));
  const run = (want: number) =>
    new Promise<[Worker, number]>((resolve) => {
      const w = new Worker(url);
      w.onmessage = (e) => resolve([w, e.data as number]);
      w.postMessage(want);
    });
  const [probe, total] = await run(-1);
  probe.terminate();
  const [filler, held] = await run(total - leave);
  (globalThis as unknown as { __bootQueueFiller: Worker }).__bootQueueFiller = filler;
  if (held !== total - leave) throw new Error(`held ${held} of ${total - leave} wasm memories`);
}
