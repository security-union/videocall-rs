import { BrowserContext, Page, expect } from "@playwright/test";

export const WS_BUFFERED_OVERRIDE_BYTES = 131_072;

export const GATE_REAL_DROP_RE =
  /CameraEncoder: dropping stale camera delta\(s\) under WS backpressure \(issue 2809\) — dropped=\d+ in last window, buffered=(\d+), threshold=(\d+)/;
export const WS_ELECTED_RE = /Elected connection ws_/;
export const AQ_TICK_RE = /AQ_STATUS: .*ladder=camera/;

export type TimedLine = { text: string; at: number };

type GateHookName = "setWsBufferedOverride" | "forceCameraKeyframe" | "bumpWsDrop";
type GateHook = Partial<Record<GateHookName, (arg?: number | null) => unknown>> | undefined;

export function collectTimedConsole(page: Page): TimedLine[] {
  const lines: TimedLine[] = [];
  page.on("console", (msg) => lines.push({ text: msg.text(), at: Date.now() }));
  return lines;
}

/** Attach before the first page opens, so a line logged during the join dance is kept. */
export function collectTimedConsoleFromContext(context: BrowserContext): TimedLine[] {
  const lines: TimedLine[] = [];
  context.on("page", (page) =>
    page.on("console", (msg) => lines.push({ text: msg.text(), at: Date.now() })),
  );
  return lines;
}

export async function assertGateHooksPresent(page: Page, names: GateHookName[]): Promise<void> {
  const ready = await page.evaluate((hooks) => {
    const hook = window.__vcNetsim as unknown as GateHook;
    return Object.fromEntries(hooks.map((n) => [n, typeof hook?.[n]]));
  }, names);
  expect(
    ready,
    `window.__vcNetsim.{${names.join(",")}} are missing. Either the dioxus UI image was built ` +
      "WITHOUT the `netsim` cargo feature (docker/docker-compose.e2e.yaml pins " +
      "TRUNK_BUILD_FEATURES=netsim; rebuild with `make e2e-build`), or the image predates the " +
      "issue-2834 hooks in videocall-client/src/connection/netsim_control.rs.",
  ).toEqual(Object.fromEntries(names.map((n) => [n, "function"])));
}

async function callHook(page: Page, name: GateHookName, arg?: number | null): Promise<unknown> {
  return page.evaluate(
    ([n, a]) => (window.__vcNetsim as unknown as GateHook)?.[n as GateHookName]?.(a),
    [name, arg] as const,
  );
}

export async function setWsBufferedOverride(page: Page, bytes: number | null): Promise<void> {
  expect(await callHook(page, "setWsBufferedOverride", bytes)).toBe(true);
}

export async function forceCameraKeyframe(page: Page): Promise<void> {
  expect(
    await callHook(page, "forceCameraKeyframe"),
    "forceCameraKeyframe found no registered camera PLI flag",
  ).toBe(true);
}

export async function bumpWsDrop(page: Page, n: number): Promise<void> {
  expect(await callHook(page, "bumpWsDrop", n)).toBe(true);
}

/** Sets the override and bumps the WS-drop counter in one page task. */
export async function overrideAndBumpWsDrop(page: Page, bytes: number, n: number): Promise<void> {
  const ok = await page.evaluate(
    ([b, count]) => {
      const hook = window.__vcNetsim as unknown as GateHook;
      return [hook?.setWsBufferedOverride?.(b), hook?.bumpWsDrop?.(count)];
    },
    [bytes, n] as const,
  );
  expect(ok).toEqual([true, true]);
}

export function gateDropLines(lines: TimedLine[]): TimedLine[] {
  return lines.filter((l) => GATE_REAL_DROP_RE.test(l.text));
}

/** WS elected and the camera AQ loop ticking; the caller then settles past the AQ warmup. */
export async function waitForWsCameraAq(lines: TimedLine[]): Promise<void> {
  await expect
    .poll(() => lines.some((l) => WS_ELECTED_RE.test(l.text)), {
      timeout: 30_000,
      intervals: [500, 1000],
      message: "the publisher did not elect a WebSocket connection; this spec targets the WS path.",
    })
    .toBe(true);
  await expect
    .poll(() => lines.some((l) => AQ_TICK_RE.test(l.text)), {
      timeout: 30_000,
      intervals: [500, 1000],
      message: "no camera AQ tick line appeared, so the AQ manager never started.",
    })
    .toBe(true);
}

export const AQ_SETTLE_MS = 8_000;
