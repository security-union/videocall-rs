import { test, expect, chromium, BrowserContext, Page } from "@playwright/test";
import {
  BROWSER_ARGS,
  createAuthenticatedContext,
  pinWebSocketTransport,
} from "../helpers/auth-context";
import { joinAndStartCamera } from "../helpers/camera-publisher";
import { setRuntimeLogLevel } from "../helpers/runtime-log-level";
import { CAMERA_AQ_AXES } from "../helpers/rust-mirrored-constants";
import { waitForServices } from "../helpers/wait-for-services";

/**
 * Issue 2809: `permit_forced_step_down` lets at most ONE camera AQ axis force a step-down per
 * tick. Every pair of netsim-drivable axes is made to contend for one tick. Tick boundaries come
 * from the encoder's own end-of-tick marker, so the assertion is pure ORDER: a Node-side receive
 * timestamp would red a healthy tree whenever an event-loop stall or CDP batching collapsed the
 * gap between two lines a full tick apart.
 *
 * Contention witness: a served axis line whose window elapsed >= window + one tick was denied on
 * the previous tick, and the axis served on that tick is the one that spent the budget.
 */

const DEFAULT_UI_URL = "http://localhost:3001";

const {
  AQ_TICK_INTERVAL_MS,
  WS_SELF_CONGESTION_DROP_THRESHOLD,
  WS_SELF_CONGESTION_WINDOW_MS,
  WT_SELF_CONGESTION_DROP_THRESHOLD,
  WT_SELF_CONGESTION_WINDOW_MS,
  CAMERA_WT_STALE_DROP_THRESHOLD,
  CAMERA_WT_STALE_DROP_WINDOW_MS,
  CAMERA_WS_STALE_DROP_THRESHOLD,
  CAMERA_WS_STALE_DROP_WINDOW_MS,
  WT_SATURATION_STALL_THRESHOLD,
  WT_SATURATION_WINDOW_MS,
} = CAMERA_AQ_AXES;

type BumpName =
  | "bumpWsDrop"
  | "bumpWtDrop"
  | "bumpCameraUplinkStall"
  | "bumpStaleDeltaDrop"
  | "bumpWsStaleDeltaDrop";

type Axis = {
  label: string;
  re: RegExp;
  windowMs: number;
  bump?: { hook: BumpName; count: number };
};

/** Every camera axis that consumes the tick's single forced step-down, in encode-loop order.
 *  Group 1 of each `re` is the window elapsed in ms. */
const CAMERA_AXES: ReadonlyArray<Axis> = [
  {
    label: "ws-drop",
    re: /CameraEncoder: client WS backpressure detected \(\d+ drops in ([0-9.]+)ms\), forcing video step-down/,
    windowMs: WS_SELF_CONGESTION_WINDOW_MS,
    bump: { hook: "bumpWsDrop", count: WS_SELF_CONGESTION_DROP_THRESHOLD + 2 },
  },
  {
    label: "wt-write-drop",
    re: /CameraEncoder: client WT uplink backpressure detected \(\d+ unistream media-frame drops in ([0-9.]+)ms\), forcing video step-down/,
    windowMs: WT_SELF_CONGESTION_WINDOW_MS,
    bump: { hook: "bumpWtDrop", count: WT_SELF_CONGESTION_DROP_THRESHOLD + 2 },
  },
  {
    label: "wt-uplink-stall",
    re: /CameraEncoder: client WT uplink saturation detected \(\d+ slow ready\(\) events in ([0-9.]+)ms\), forcing video step-down/,
    windowMs: WT_SATURATION_WINDOW_MS,
    bump: { hook: "bumpCameraUplinkStall", count: WT_SATURATION_STALL_THRESHOLD + 2 },
  },
  {
    label: "wt-stale-delta",
    re: /CameraEncoder: client WT stale-delta backpressure detected \(\d+ camera deltas dropped in ([0-9.]+)ms\), forcing video step-down/,
    windowMs: CAMERA_WT_STALE_DROP_WINDOW_MS,
    bump: { hook: "bumpStaleDeltaDrop", count: CAMERA_WT_STALE_DROP_THRESHOLD + 3 },
  },
  {
    label: "ws-stale-delta",
    re: /CameraEncoder: client WS stale-delta backpressure detected \(\d+ camera deltas dropped in ([0-9.]+)ms\), forcing video step-down/,
    windowMs: CAMERA_WS_STALE_DROP_WINDOW_MS,
    bump: { hook: "bumpWsStaleDeltaDrop", count: CAMERA_WS_STALE_DROP_THRESHOLD + 3 },
  },
];

type DrivenAxis = Axis & { bump: NonNullable<Axis["bump"]> };

const DRIVEN: ReadonlyArray<DrivenAxis> = CAMERA_AXES.filter((a): a is DrivenAxis => !!a.bump);
const STARVER = DRIVEN[0];

/** `[denier, denied]`, denier earlier in encode-loop order. Starver-denied pairs last: every
 *  other phase also witnesses one of them. */
const PAIRS: ReadonlyArray<readonly [DrivenAxis, DrivenAxis]> = DRIVEN.flatMap((d, i) =>
  DRIVEN.slice(i + 1).map((w) => [d, w] as const),
).sort(([a], [b]) => Number(a === STARVER) - Number(b === STARVER));

const pairKey = (denier: string, denied: string) => `${denier}>${denied}`;

/** Held past its window for more than one tick, so the evidence is still unconsumed at release. */
const STARVE_MS = Math.max(...DRIVEN.map((a) => a.windowMs)) + 2 * AQ_TICK_INTERVAL_MS;

const STARVER_WAIT_MS = 15_000;
const RELEASE_WAIT_MS = STARVE_MS + 4 * AQ_TICK_INTERVAL_MS;

/** 7 planned phases, each at its poll-timeout bound. */
const PAIRING_DEADLINE_MS = 7 * (STARVER_WAIT_MS + STARVE_MS + RELEASE_WAIT_MS);

/** Emitted once per tick AFTER all five axes whenever the budget was spent, so it closes every
 *  tick that served one. `log::debug!`, which the stack's `logLevel: "info"` drops. */
const TICK_MARKER_RE = /CameraEncoder: forced video step-down cashed this AQ tick/;

// `complete_election`'s winner line; connection ids are keyed `ws_*` / `wt_*`.
const WS_ELECTED_RE = /Elected connection ws_/;

const AQ_TICK_RE = /AQ_STATUS: .*ladder=camera/;

function collectConsole(page: Page): string[] {
  const lines: string[] = [];
  page.on("console", (msg) => lines.push(msg.text()));
  return lines;
}

type BumpHook = Partial<Record<BumpName, (n: number) => unknown>> | undefined;

async function assertHooksPresent(page: Page): Promise<void> {
  const hooks = DRIVEN.map((a) => a.bump.hook);
  const ready = await page.evaluate((names) => {
    const hook = window.__vcNetsim as unknown as BumpHook;
    return Object.fromEntries(names.map((n) => [n, typeof hook?.[n]]));
  }, hooks);
  expect(
    ready,
    `window.__vcNetsim.{${hooks.join(",")}} are missing. Either the dioxus UI image was built ` +
      "WITHOUT the `netsim` cargo feature (docker/docker-compose.e2e.yaml pins " +
      "TRUNK_BUILD_FEATURES=netsim; rebuild with `make e2e-build`), or the image predates them.",
  ).toEqual(Object.fromEntries(hooks.map((n) => [n, "function"])));
}

async function bump(page: Page, axes: ReadonlyArray<DrivenAxis>): Promise<void> {
  await page.evaluate(
    (calls) => {
      const hook = window.__vcNetsim as unknown as BumpHook;
      for (const { hook: name, count } of calls) hook?.[name]?.(count);
    },
    axes.map((a) => a.bump),
  );
}

type TickEvent =
  | { kind: "axis"; text: string; label: string; elapsedMs: number }
  | { kind: "marker"; text: string };

/** Served-axis lines and end-of-tick markers, in emit order. */
function tickEvents(lines: string[]): TickEvent[] {
  const events: TickEvent[] = [];
  for (const text of lines) {
    for (const { label, re } of CAMERA_AXES) {
      const m = re.exec(text);
      if (m) {
        events.push({ kind: "axis", text, label, elapsedMs: Number(m[1]) });
        break;
      }
    }
    if (TICK_MARKER_RE.test(text)) events.push({ kind: "marker", text });
  }
  return events;
}

/** Two adjacent axis lines mean one tick was cashed twice. */
function unmarkedPairs(events: TickEvent[]): { first: string; second: string }[] {
  return events.flatMap((event, i) =>
    event.kind === "axis" && i > 0 && events[i - 1].kind === "axis"
      ? [{ first: events[i - 1].text, second: event.text }]
      : [],
  );
}

function contendedPairs(events: TickEvent[]): Set<string> {
  const seen = new Set<string>();
  events.forEach((e, i) => {
    if (e.kind !== "axis" || i < 2) return;
    const [denier, marker] = [events[i - 2], events[i - 1]];
    if (denier.kind !== "axis" || marker.kind !== "marker") return;
    const windowMs = CAMERA_AXES.find((a) => a.label === e.label)!.windowMs;
    if (e.elapsedMs >= windowMs + AQ_TICK_INTERVAL_MS) seen.add(pairKey(denier.label, e.label));
  });
  return seen;
}

/** Starves `members` behind the starver, then releases them into the same tick. */
async function contendPhase(
  page: Page,
  lines: string[],
  members: ReadonlyArray<DrivenAxis>,
): Promise<void> {
  const servedSince = (from: number, label: string) =>
    tickEvents(lines.slice(from)).some((e) => e.kind === "axis" && e.label === label);

  const phaseStart = lines.length;
  await expect
    .poll(
      async () => {
        await bump(page, [STARVER]);
        return servedSince(phaseStart, STARVER.label);
      },
      {
        timeout: STARVER_WAIT_MS,
        intervals: [250],
        message: `the ${STARVER.label} axis never served, so nothing starves the pair.`,
      },
    )
    .toBe(true);

  const membersStart = lines.length;
  await bump(page, members);
  const releaseAt = Date.now() + STARVE_MS;
  await expect
    .poll(
      async () => {
        await bump(page, [STARVER]);
        return Date.now() >= releaseAt;
      },
      { timeout: STARVE_MS + 10_000, intervals: [250] },
    )
    .toBe(true);

  await expect
    .poll(
      async () => {
        const unserved = members.filter((m) => !servedSince(membersStart, m.label));
        await bump(page, unserved);
        return unserved.length === 0;
      },
      {
        timeout: RELEASE_WAIT_MS,
        intervals: [250],
        message: `released axes ${members.map((m) => m.label).join(",")} never all served.`,
      },
    )
    .toBe(true);
}

test.describe("issue 2809 camera AQ cross-axis step-down guard", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test("at most one camera AQ axis forces a step-down per tick @bvt1", async ({ baseURL }) => {
    test.setTimeout(360_000);
    const meetingId = `e2e_cam_aq_guard_${Date.now()}`;
    const browser = await chromium.launch({ args: BROWSER_ARGS });
    try {
      const ctx: BrowserContext = await createAuthenticatedContext(
        browser,
        "cam-aq-guard@videocall.rs",
        "CameraAqGuardPublisher",
        baseURL || DEFAULT_UI_URL,
      );
      await pinWebSocketTransport(ctx);
      await setRuntimeLogLevel(ctx, "debug");
      const page = await ctx.newPage();
      const consoleLines = collectConsole(page);

      await joinAndStartCamera(page, meetingId);
      await assertHooksPresent(page);

      await expect
        .poll(() => consoleLines.some((l) => WS_ELECTED_RE.test(l)), {
          timeout: 30_000,
          intervals: [500, 1000],
          message:
            "the publisher did not elect a WebSocket connection, so a live WT uplink could move " +
            "the injected counters and the per-tick pairing is no longer controlled.",
        })
        .toBe(true);
      await expect
        .poll(() => consoleLines.some((l) => AQ_TICK_RE.test(l)), {
          timeout: 30_000,
          intervals: [500, 1000],
          message: "no camera AQ tick line appeared, so the AQ manager never started.",
        })
        .toBe(true);

      const deadline = Date.now() + PAIRING_DEADLINE_MS;
      const missing = () => {
        const seen = contendedPairs(tickEvents(consoleLines));
        return PAIRS.filter(([d, w]) => !seen.has(pairKey(d.label, w.label)));
      };
      for (
        let next = missing();
        next.length > 0 &&
        Date.now() < deadline &&
        unmarkedPairs(tickEvents(consoleLines)).length === 0;
        next = missing()
      ) {
        const [denier, denied] = next[0];
        await contendPhase(page, consoleLines, denier === STARVER ? [denied] : [denier, denied]);
      }

      const events = tickEvents(consoleLines);
      const cashedTicks = events.filter((e) => e.kind === "marker").length;
      const axisCount = events.length - cashedTicks;
      // The totals alone cannot separate a near-vacuous run from an evenly paired one.
      const perAxis = CAMERA_AXES.map(
        ({ label }) =>
          `${label}=${events.filter((e) => e.kind === "axis" && e.label === label).length}`,
      ).join(" ");
      const contended = contendedPairs(events);
      console.log(
        `issue 2809 axis lines=${axisCount} (${perAxis}), cashed ticks=${cashedTicks}, ` +
          `contended pairs=${contended.size}/${PAIRS.length} (${[...contended].join(" ")})`,
      );

      const cashedTwice = unmarkedPairs(events);
      expect(
        cashedTwice,
        "two camera AQ axes each forced a step-down with no end-of-tick marker between them, so " +
          "one tick's single forced step-down was cashed twice. " +
          `pairs=${JSON.stringify(cashedTwice, null, 2)}`,
      ).toEqual([]);

      const uncontended = missing().map(([d, w]) => pairKey(d.label, w.label));
      expect(
        uncontended,
        "these axis pairs never contended for one tick, so the guard is unexercised for them. " +
          `cashed ticks=${cashedTicks}: zero means the end-of-tick marker is invisible because ` +
          'the served runtime config did not take `logLevel: "debug"` (the stack pins "info").',
      ).toEqual([]);
    } finally {
      await browser.close();
    }
  });
});
