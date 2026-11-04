/**
 * A main-thread stall must not silence a remote peer's audio.
 *
 * The receiver's NetEq worker posts each 10 ms of decoded audio straight to
 * the peer's AudioWorklet over a MessageChannel, so a busy main thread no
 * longer starves playback. The probe below rewrites the pcm-player module the
 * client registers and counts render quanta that played zeros after audio
 * started; it reads the worklet's own ring buffer, so it measures the same
 * thing on a build without the fix.
 *
 * The stall is a real synchronous busy-wait in `page.evaluate`. The second
 * test switches the direct path off with `window.__VC_AUDIO_DIRECT_PCM = "0"`
 * (the rollback lever) and asserts the opposite outcome on the same build.
 */

import { test, expect, chromium, Locator, Page } from "@playwright/test";
import { createAuthenticatedContext, BROWSER_ARGS } from "../helpers/auth-context";
import { wakeControls } from "../helpers/controls";
import { enterTwoUserMeeting } from "../helpers/two-user-meeting";
import { waitForServices } from "../helpers/wait-for-services";

const STALL_MS = 250;
const STALL_PERIOD_MS = 1_000;
const STALLS = 20;

const WORKLET_PROBE = `(() => {
  const AW = globalThis.AudioWorklet && globalThis.AudioWorklet.prototype;
  if (!AW || AW.__probe) return;
  AW.__probe = true;
  const prefix = \`(() => {
    const register = globalThis.registerProcessor;
    globalThis.registerProcessor = function (name, cls) {
      if (name === "pcm-player") {
        const process = cls.prototype.process;
        cls.prototype.process = function (inputs, outputs, params) {
          if (!this.__probe) {
            this.__probe = { calls: 0, silent: 0, started: false, directPauses: 0, mainPauses: 0 };
            const handle = this.handleMessage.bind(this);
            this.handleMessage = (data, ...rest) => {
              if (data && data.command === "pause") {
                if (rest[0] === true) this.__probe.directPauses++;
                else this.__probe.mainPauses++;
              }
              if (data && data.command === "__probe") {
                this.port.postMessage({
                  type: "__probe",
                  ...this.__probe,
                  counting: this.counting === true,
                  direct: this.direct === true,
                  pausesIn: this.pausesIn || 0,
                  countedMs: ((this.silentSamples || 0) * 1000) / sampleRate,
                });
                return;
              }
              return handle(data, ...rest);
            };
          }
          const p = this.__probe;
          const need = outputs[0] && outputs[0][0] ? outputs[0][0].length : 128;
          if (this.buffer.availableSamples >= need) p.started = true;
          if (p.started) {
            p.calls++;
            if (this.buffer.availableSamples < need) p.silent++;
          }
          return process.call(this, inputs, outputs, params);
        };
      }
      return register.call(this, name, cls);
    };
  })();\\n\`;
  const addModule = AW.addModule;
  AW.addModule = async function (url, options) {
    if (String(url).startsWith("blob:")) {
      const source = await (await fetch(url)).text();
      if (source.includes("pcm-player")) {
        url = URL.createObjectURL(new Blob([prefix + source], { type: "application/javascript" }));
      }
    }
    return addModule.call(this, url, options);
  };
  const ports = [];
  globalThis.__probePorts = ports;
  const Node = globalThis.AudioWorkletNode;
  globalThis.AudioWorkletNode = class extends Node {
    constructor(context, name, options) {
      super(context, name, options);
      if (name === "pcm-player") {
        const entry = { port: this.port, last: null };
        this.port.addEventListener("message", (e) => {
          if (e.data && e.data.type === "__probe") entry.last = e.data;
        });
        this.port.start();
        ports.push(entry);
      }
    }
  };
})();`;

interface ProbeTotals {
  calls: number;
  silent: number;
  worklets: number;
  /** Worklets that count underruns, and the silence they have counted (the production counter). */
  counting: number;
  countedMs: number;
  direct: number;
  pausesIn: number;
  directPauses: number;
  mainPauses: number;
}

interface ProbeReply {
  calls: number;
  silent: number;
  counting: boolean;
  countedMs: number;
  direct: boolean;
  pausesIn: number;
  directPauses: number;
  mainPauses: number;
}

async function readProbe(page: Page): Promise<ProbeTotals> {
  await page.evaluate(() => {
    const ports = (globalThis as unknown as { __probePorts: { port: MessagePort }[] }).__probePorts;
    for (const p of ports) p.port.postMessage({ command: "__probe" });
  });
  await page.waitForTimeout(300);
  return page.evaluate(() => {
    const ports = (globalThis as unknown as { __probePorts: { last: ProbeReply | null }[] })
      .__probePorts;
    const totals = {
      calls: 0,
      silent: 0,
      worklets: 0,
      counting: 0,
      countedMs: 0,
      direct: 0,
      pausesIn: 0,
      directPauses: 0,
      mainPauses: 0,
    };
    for (const p of ports) {
      if (!p.last) continue;
      totals.calls += p.last.calls;
      totals.silent += p.last.silent;
      totals.worklets += 1;
      totals.counting += p.last.counting ? 1 : 0;
      totals.countedMs += p.last.countedMs;
      totals.direct += p.last.direct ? 1 : 0;
      totals.pausesIn += p.last.pausesIn;
      totals.directPauses += p.last.directPauses;
      totals.mainPauses += p.last.mainPauses;
    }
    return totals;
  });
}

function micToggle(page: Page): Locator {
  return page.locator('[data-testid="mic-toggle-button"]');
}

async function micOn(page: Page): Promise<void> {
  await wakeControls(page);
  const toggle = micToggle(page);
  await expect(toggle).toBeVisible({ timeout: 15_000 });
  if (!((await toggle.getAttribute("class")) || "").includes("active")) {
    await toggle.click();
  }
  await expect(toggle).toHaveClass(/\bactive\b/, { timeout: 15_000 });
}

/**
 * Runs the PRODUCTION NetEQ worker with a hook that makes its next N direct-port `play` posts throw
 * once the page calls `__failDirectPosts(N)`, and counts PCM the worker still sends to the main
 * thread after its handoff. `__directPostFailures` proves the hook fired.
 */
const FAILING_DIRECT_POSTS = `(() => {
  const CHANNEL = "__e2e_direct_post_failures";
  const workerHook = (channel) => {
    const bc = new BroadcastChannel(channel);
    let armed = 0;
    let failed = 0;
    bc.onmessage = (e) => {
      armed = e.data.arm;
    };
    const post = MessagePort.prototype.postMessage;
    MessagePort.prototype.postMessage = function (m, t) {
      if (armed > 0 && m && m.command === "play") {
        armed--;
        failed++;
        bc.postMessage({ failed });
        throw new DOMException("injected", "DataCloneError");
      }
      return post.call(this, m, t);
    };
  };
  const bc = new BroadcastChannel(CHANNEL);
  globalThis.__directPostFailures = 0;
  globalThis.__pcmToMainAfterHandoff = 0;
  bc.onmessage = (e) => {
    globalThis.__directPostFailures = e.data.failed;
  };
  globalThis.__failDirectPosts = (n) => bc.postMessage({ arm: n });
  const W = globalThis.Worker;
  globalThis.Worker = class extends W {
    constructor(url, opts) {
      let u = String(url);
      const neteq = u.includes("neteq_worker_loader");
      if (neteq) {
        const abs = new URL(u, location.href).href;
        const base = abs.slice(0, abs.lastIndexOf("/") + 1);
        const xhr = new XMLHttpRequest();
        xhr.open("GET", abs, false);
        xhr.send();
        const source = "(" + workerHook + ")(" + JSON.stringify(CHANNEL) + ");\\n" + xhr.responseText.split('"./').join('"' + base);
        u = URL.createObjectURL(new Blob([source], { type: "text/javascript" }));
      }
      super(u, opts);
      if (neteq) {
        let handedOff = false;
        this.addEventListener("message", (e) => {
          if (e.data && e.data.type === "handoff") handedOff = true;
          else if (handedOff && e.data instanceof Float32Array) globalThis.__pcmToMainAfterHandoff++;
        });
      }
    }
  };
})();`;

/** A publisher and a receiver whose pcm-player worklet is playing the publisher's audio. */
async function withPlayingPair(
  directPcm: boolean,
  tag: string,
  uiURL: string,
  body: (pubPage: Page, rxPage: Page) => Promise<void>,
  extraInit?: string,
): Promise<void> {
  const pubBrowser = await chromium.launch({ args: BROWSER_ARGS });
  const rxBrowser = await chromium.launch({
    args: [...BROWSER_ARGS, "--autoplay-policy=no-user-gesture-required"],
  });
  try {
    const pubCtx = await createAuthenticatedContext(
      pubBrowser,
      `stall-pub-${tag}@videocall.rs`,
      "HostUser",
      uiURL,
    );
    const rxCtx = await createAuthenticatedContext(
      rxBrowser,
      `stall-rx-${tag}@videocall.rs`,
      "GuestUser",
      uiURL,
    );
    await rxCtx.addInitScript(WORKLET_PROBE);
    if (extraInit) {
      await rxCtx.addInitScript(extraInit);
    }
    if (!directPcm) {
      await rxCtx.addInitScript(`window.__VC_AUDIO_DIRECT_PCM = "0";`);
    }
    const pubPage = await pubCtx.newPage();
    const rxPage = await rxCtx.newPage();

    await enterTwoUserMeeting(pubPage, rxPage, `e2e_audio_stall_${tag}_${Date.now()}`);
    await micOn(pubPage);

    await expect
      .poll(async () => (await readProbe(rxPage)).calls, {
        timeout: 60_000,
        intervals: [1_000, 2_000],
        message:
          "the receiver's pcm-player worklet never started playing the publisher's audio; " +
          "every reading below would be vacuous",
      })
      .toBeGreaterThan(375);
    await rxPage.waitForTimeout(3_000);
    await body(pubPage, rxPage);
  } finally {
    await pubBrowser.close();
    await rxBrowser.close();
  }
}

/** Fraction of the receiver's worklet output that was silence across the stall schedule. */
async function silenceUnderStalls(directPcm: boolean, uiURL: string): Promise<number> {
  let fraction = NaN;
  await withPlayingPair(directPcm, directPcm ? "on" : "off", uiURL, async (_pub, rxPage) => {
    const before = await readProbe(rxPage);
    for (let i = 0; i < STALLS; i++) {
      await rxPage.evaluate((ms) => {
        const t = performance.now();
        while (performance.now() - t < ms) {
          /* block the renderer's main thread */
        }
      }, STALL_MS);
      await rxPage.waitForTimeout(STALL_PERIOD_MS - STALL_MS);
    }
    const after = await readProbe(rxPage);

    const calls = after.calls - before.calls;
    const silent = after.silent - before.silent;
    expect(after.worklets, "exactly one remote peer is playing").toBe(1);
    expect(calls, "the worklet must have rendered through the whole stall window").toBeGreaterThan(
      (STALLS * STALL_PERIOD_MS * 375) / 1000 / 2,
    );
    fraction = silent / calls;
    console.log(
      `[audio-stall] direct=${directPcm} silent quanta ${silent}/${calls} = ${(fraction * 100).toFixed(2)}%`,
    );
  });
  return fraction;
}

test.describe("Remote audio survives main-thread stalls", () => {
  test.describe.configure({ mode: "serial" });

  test.beforeAll(async () => {
    await waitForServices();
  });

  test("with PCM going straight from NetEq to the worklet, stalls leave under 1% silence", async ({
    baseURL,
  }) => {
    test.setTimeout(240_000);
    const fraction = await silenceUnderStalls(true, baseURL || "http://localhost:3001");
    expect(fraction).toBeLessThan(0.01);
  });

  test("with the direct path switched off, the same stalls are heard as silence", async ({
    baseURL,
  }) => {
    test.setTimeout(240_000);
    const fraction = await silenceUnderStalls(false, baseURL || "http://localhost:3001");
    expect(fraction).toBeGreaterThan(0.03);
  });

  test("a 2 s publisher mute is not counted as silence by the worklet @bvt1", async ({
    baseURL,
  }) => {
    test.setTimeout(240_000);
    await withPlayingPair(
      true,
      "mute",
      baseURL || "http://localhost:3001",
      async (pubPage, rxPage) => {
        const before = await readProbe(rxPage);
        expect(
          before.counting,
          "the worklet must be counting underruns; else zero counted silence proves nothing",
        ).toBe(before.worklets);
        expect(before.direct, "the worklet must be on the direct path").toBe(before.worklets);
        const toggle = micToggle(pubPage);
        await wakeControls(pubPage);
        await toggle.click();
        await expect(toggle).toHaveClass(/\boff\b/, { timeout: 15_000 });
        await pubPage.waitForTimeout(2_000);
        await micOn(pubPage);
        const atUnmute = await readProbe(rxPage);
        expect(
          atUnmute.pausesIn - before.pausesIn,
          "the mute must reach the worklet as a pause, else nothing here tests it",
        ).toBeGreaterThan(0);
        expect(
          atUnmute.directPauses - before.directPauses,
          "the pause must arrive on the direct port",
        ).toBeGreaterThan(0);
        expect(
          atUnmute.mainPauses - before.mainPauses,
          "no pause may come through the main thread",
        ).toBe(0);
        await rxPage.waitForTimeout(6_000);
        const after = await readProbe(rxPage);
        expect(
          after.calls - after.silent - (atUnmute.calls - atUnmute.silent),
          "fresh audio must play after the unmute, else a gap could never end and be counted",
        ).toBeGreaterThan(375);
        console.log(
          `[audio-stall] counted silence across the mute: ${after.countedMs - before.countedMs} ms`,
        );
        expect(after.countedMs - before.countedMs).toBeLessThan(100);
      },
    );
  });

  test("a direct post that fails is dropped, not re-sent to the main thread @bvt1", async ({
    baseURL,
  }) => {
    test.setTimeout(240_000);
    await withPlayingPair(
      true,
      "post-fail",
      baseURL || "http://localhost:3001",
      async (_pub, rxPage) => {
        const counters = () =>
          rxPage.evaluate(() => {
            const g = globalThis as unknown as {
              __directPostFailures: number;
              __pcmToMainAfterHandoff: number;
            };
            return { failed: g.__directPostFailures, toMain: g.__pcmToMainAfterHandoff };
          });
        const before = await readProbe(rxPage);
        await rxPage.evaluate(() =>
          (globalThis as unknown as { __failDirectPosts: (n: number) => void }).__failDirectPosts(
            100,
          ),
        );
        await expect
          .poll(async () => (await counters()).failed, {
            timeout: 10_000,
            message:
              "the injected post failures never fired in the NetEQ worker; the test is vacuous",
          })
          .toBe(100);
        await rxPage.waitForTimeout(1_000);
        const after = await readProbe(rxPage);
        const { toMain } = await counters();
        console.log(
          `[audio-stall] failed direct posts: 100, silent quanta ${after.silent - before.silent}, PCM sent to the main thread after the handoff: ${toMain}`,
        );
        expect(
          after.silent - before.silent,
          "a second of dropped frames must be heard",
        ).toBeGreaterThan(200);
        expect(toMain, "a frame the worklet can no longer play must not be re-posted").toBe(0);
      },
      FAILING_DIRECT_POSTS,
    );
  });
});
